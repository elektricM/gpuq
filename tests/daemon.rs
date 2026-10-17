// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! The real `gpuq` binary: check-config, then the daemon on loopback ports against a fake
//! llama-swap. Unix only (shell-script probe, signals, unix datagram sockets).
#![cfg(unix)]

#[path = "support/fake_llamaswap.rs"]
mod fake_llamaswap;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use fake_llamaswap::{FakeLlamaSwap, MODELS_YAML};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use serde_json::{json, Value};
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout, Instant};

struct Out {
    rc: i32,
    out: String,
    err: String,
}

/// A port for a listener that is bound later. It is taken below the kernel's ephemeral range
/// (32768 and up), so an outgoing connection cannot grab it before the listener binds it.
/// The check binds without listening: a child forked by another test thread holds a copy of
/// the socket until it execs, and a copy that listened would accept the readiness probes
/// meant for the real listener.
fn free_port() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    static NEXT: AtomicU16 = AtomicU16::new(0);
    let base = 20_000 + (std::process::id() % 12) as u16 * 1_000;
    loop {
        let port = base + NEXT.fetch_add(1, Ordering::Relaxed) % 1_000;
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let bound = tokio::net::TcpSocket::new_v4()
            .and_then(|s| s.set_reuseaddr(true).map(|()| s))
            .and_then(|s| s.bind(addr));
        if bound.is_ok() {
            return port;
        }
    }
}

fn gpuq(args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gpuq"));
    cmd.args(args)
        .env_remove("NOTIFY_SOCKET")
        .env_remove("GPUQ_API")
        .env_remove("GPUQ_LOG")
        .env_remove("RUST_LOG")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd
}

async fn finish(child: Child) -> Out {
    let o = timeout(Duration::from_secs(20), child.wait_with_output())
        .await
        .expect("gpuq did not finish in 20 s")
        .unwrap();
    Out {
        rc: o.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&o.stdout).into_owned(),
        err: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

async fn run_cli(args: &[&str]) -> Out {
    finish(gpuq(args).spawn().unwrap()).await
}

/// Config files in a temp dir, pointing at the fake.
struct Setup {
    dir: tempfile::TempDir,
    gate_port: u16,
    api_port: u16,
}

impl Setup {
    fn new() -> Self {
        Setup {
            dir: tempfile::tempdir().unwrap(),
            gate_port: free_port(),
            api_port: free_port(),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn probe_script(&self) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = self.path("probe.sh");
        std::fs::write(&p, "#!/bin/sh\necho 100 24560\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    /// Writes the config and returns its path. `llm` is the body of the `[llm]` table.
    fn config(&self, backend_url: &str, llm: &str) -> PathBuf {
        let models = self.path("models.yaml");
        std::fs::write(&models, MODELS_YAML).unwrap();
        let text = format!(
            r#"
[gate]
port = {gate}
backend = "{backend_url}"

[api]
port = {api}

[gpu]
vendor = "command"
command = ["{probe}"]
driver_settle_s = 0.05
settle_timeout_s = 2

[llm]
llama_swap_config = "{models}"
idle_s = 0.2
gap_s = 0.05
reload_when_idle = false
resume_grace_s = 0.1
{llm}
"#,
            gate = self.gate_port,
            api = self.api_port,
            probe = self.probe_script().display(),
            models = models.display(),
        );
        let p = self.path("gpuq.toml");
        std::fs::write(&p, text).unwrap();
        p
    }

    fn api(&self) -> String {
        format!("127.0.0.1:{}", self.api_port)
    }
}

struct Daemon {
    child: Child,
    api: String,
    gate: String,
}

impl Daemon {
    async fn start(setup: &Setup, config: &Path) -> Daemon {
        Self::start_with(setup, config, &[]).await
    }

    async fn start_with(setup: &Setup, config: &Path, env: &[(&str, &Path)]) -> Daemon {
        let mut cmd = gpuq(&["daemon", "-c", config.to_str().unwrap()]);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = cmd.spawn().unwrap();
        let d = Daemon {
            child,
            api: setup.api(),
            gate: format!("127.0.0.1:{}", setup.gate_port),
        };
        d.wait_healthy().await;
        d
    }

    async fn wait_healthy(&self) {
        let url = format!("http://{}/healthz", self.api);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(r) = reqwest::get(&url).await {
                if r.status().is_success() {
                    return;
                }
            }
            assert!(Instant::now() < deadline, "daemon never became healthy");
            sleep(Duration::from_millis(50)).await;
        }
    }

    async fn status_json(&self) -> Value {
        reqwest::get(format!("http://{}/status", self.api))
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    fn cli(&self, args: &[&str]) -> Command {
        let mut cmd = gpuq(args);
        cmd.env("GPUQ_API", &self.api);
        cmd
    }

    fn pid(&self) -> Pid {
        Pid::from_raw(self.child.id().expect("daemon already exited") as i32)
    }

    /// SIGTERM, then the exit code (within a few seconds).
    async fn terminate(mut self) -> i32 {
        kill(self.pid(), Signal::SIGTERM).unwrap();
        let status = timeout(Duration::from_secs(5), self.child.wait())
            .await
            .expect("daemon did not exit after SIGTERM")
            .unwrap();
        status.code().unwrap_or(-1)
    }
}

async fn chat(gate: &str, model: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://{gate}/v1/chat/completions"))
        .json(&json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn check_config_accepts_a_good_config_and_reads_the_probe() {
    let fake = FakeLlamaSwap::start().await;
    let setup = Setup::new();
    let cfg = setup.config(&fake.url(), "");
    let d = run_cli(&["check-config", "-c", cfg.to_str().unwrap(), "--strict"]).await;
    assert_eq!(d.rc, 0, "{}", d.err);
    assert!(d.out.contains(cfg.to_str().unwrap()), "{}", d.out);
    assert!(d.out.contains("backend: llama-swap"), "{}", d.out);
    assert!(d.out.contains("100 / 24560 MiB"), "{}", d.out);
}

#[tokio::test]
async fn check_config_lists_every_problem_and_exits_1() {
    let setup = Setup::new();
    let p = setup.path("bad.toml");
    std::fs::write(
        &p,
        "[gate]\nbackend = \"nope\"\n[gpu]\nvendor = \"command\"\n[run]\nyield_signal = \"SIGFOO\"\n",
    )
    .unwrap();
    let d = run_cli(&["check-config", "-c", p.to_str().unwrap()]).await;
    assert_eq!(d.rc, 1);
    let lines: Vec<&str> = d.err.lines().collect();
    assert!(lines.len() >= 3, "{}", d.err);
    assert!(lines.iter().all(|l| l.starts_with("gpuq: ")), "{}", d.err);
    assert!(d.err.contains("gate.backend"));
    assert!(d.err.contains("yield_signal"));
}

#[tokio::test]
async fn check_config_missing_file_exits_1() {
    let d = run_cli(&["check-config", "-c", "/nonexistent/gpuq.toml"]).await;
    assert_eq!(d.rc, 1);
    assert!(d.err.starts_with("gpuq: "), "{}", d.err);
}

#[tokio::test]
async fn check_config_probe_failure_is_a_warning_unless_strict() {
    let setup = Setup::new();
    let p = setup.path("c.toml");
    std::fs::write(
        &p,
        "[gpu]\nvendor = \"command\"\ncommand = [\"/nonexistent/probe\"]\n",
    )
    .unwrap();
    let cfg = p.to_str().unwrap();
    let d = run_cli(&["check-config", "-c", cfg]).await;
    assert_eq!(d.rc, 0, "{}", d.err);
    assert!(d.err.contains("probe"), "{}", d.err);
    let d = run_cli(&["check-config", "-c", cfg, "--strict"]).await;
    assert_eq!(d.rc, 1);
}

#[tokio::test]
async fn daemon_refuses_a_bad_config() {
    let setup = Setup::new();
    let p = setup.path("bad.toml");
    std::fs::write(&p, "[gate]\nport = 0\n").unwrap();
    let d = run_cli(&["daemon", "-c", p.to_str().unwrap()]).await;
    assert_eq!(d.rc, 1);
    assert!(d.err.contains("gate.port"), "{}", d.err);
}

#[tokio::test]
async fn daemon_serves_the_gate_the_api_and_the_cli() {
    let fake = FakeLlamaSwap::start().await;
    let setup = Setup::new();
    let cfg = setup.config(&fake.url(), "");
    let daemon = Daemon::start(&setup, &cfg).await;

    // A chat through the gate reaches the backend and loads the model.
    let r = chat(&daemon.gate, "big").await;
    assert_eq!(r.status(), 200);
    let _ = r.bytes().await;
    assert!(fake.is_loaded("big"));

    // /status carries the backend's models.
    let st = daemon.status_json().await;
    assert_eq!(st["state"], "llm", "{st}");
    assert_eq!(st["backend"]["kind"], "llama-swap", "{st}");
    assert_eq!(st["backend"]["models"][0]["model"], "big", "{st}");

    // `gpuq status` shows it too.
    let d = finish(daemon.cli(&["status"]).spawn().unwrap()).await;
    assert_eq!(d.rc, 0, "{}", d.err);
    assert!(d.out.contains("llm"), "{}", d.out);
    assert!(d.out.contains("big"), "{}", d.out);

    // A batch job takes the card from the idle LLM and gives it back.
    let d = finish(
        daemon
            .cli(&["run", "--class", "batch", "--vram", "1G", "--", "true"])
            .spawn()
            .unwrap(),
    )
    .await;
    assert_eq!(d.rc, 0, "{}", d.err);
    assert!(fake.has_event("unload"), "{:?}", fake.events());

    assert_eq!(daemon.terminate().await, 0);
}

#[tokio::test]
async fn sigterm_exits_zero_quickly_and_drops_open_leases() {
    let fake = FakeLlamaSwap::start().await;
    let setup = Setup::new();
    let cfg = setup.config(&fake.url(), "");
    let daemon = Daemon::start(&setup, &cfg).await;
    let mut hold = daemon
        .cli(&["hold", "--class", "batch", "--vram", "1G"])
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while daemon.status_json().await["leases"]
        .as_array()
        .map_or(true, |l| l.is_empty())
    {
        assert!(Instant::now() < deadline, "lease never granted");
        sleep(Duration::from_millis(50)).await;
    }
    let started = Instant::now();
    assert_eq!(daemon.terminate().await, 0);
    assert!(started.elapsed() < Duration::from_secs(3));
    // The client keeps retrying so it can resume its lease; nothing more to check here.
    let _ = hold.kill().await;
}

#[tokio::test]
async fn sighup_is_ignored() {
    let fake = FakeLlamaSwap::start().await;
    let setup = Setup::new();
    let cfg = setup.config(&fake.url(), "");
    let daemon = Daemon::start(&setup, &cfg).await;
    kill(daemon.pid(), Signal::SIGHUP).unwrap();
    sleep(Duration::from_millis(300)).await;
    daemon.wait_healthy().await;
    assert_eq!(daemon.terminate().await, 0);
}

#[tokio::test]
async fn sends_ready_to_notify_socket() {
    let fake = FakeLlamaSwap::start().await;
    let setup = Setup::new();
    let cfg = setup.config(&fake.url(), "");
    let sock_path = setup.path("notify.sock");
    let sock = tokio::net::UnixDatagram::bind(&sock_path).unwrap();
    let daemon = Daemon::start_with(&setup, &cfg, &[("NOTIFY_SOCKET", &sock_path)]).await;
    let mut buf = [0u8; 64];
    let n = timeout(Duration::from_secs(5), sock.recv(&mut buf))
        .await
        .expect("no notification")
        .unwrap();
    assert_eq!(&buf[..n], b"READY=1");
    assert_eq!(daemon.terminate().await, 0);
}

#[tokio::test]
async fn backend_none_has_no_gate_but_grants_leases() {
    let setup = Setup::new();
    let cfg = setup.config("http://127.0.0.1:1", "backend = \"none\"");
    let daemon = Daemon::start(&setup, &cfg).await;

    let st = daemon.status_json().await;
    assert_eq!(st["backend"]["kind"], "none", "{st}");
    assert_eq!(st["backend"]["models"], json!([]), "{st}");
    assert!(
        tokio::net::TcpStream::connect(&daemon.gate).await.is_err(),
        "the gate should not listen with backend = none"
    );

    for _ in 0..2 {
        let d = finish(
            daemon
                .cli(&["run", "--class", "batch", "--vram", "1G", "--", "true"])
                .spawn()
                .unwrap(),
        )
        .await;
        assert_eq!(d.rc, 0, "{}", d.err);
    }
    // Idle afterwards: nothing spins, and the daemon still answers.
    sleep(Duration::from_millis(600)).await;
    daemon.wait_healthy().await;
    assert_eq!(daemon.terminate().await, 0);
}

#[tokio::test]
async fn immich_section_adds_its_object_to_status_and_the_cli_prints_a_line() {
    let fake = FakeLlamaSwap::start().await;
    let setup = Setup::new();
    // No key file: the adapter stays idle and says why. Docker does not exist and is not needed.
    let immich = format!(
        r#"
[classes]
immich = 3

[immich]
url = "http://127.0.0.1:9"
key_file = "{key}"
compose_dir = "{dir}"
service = "ml-gpu"
container = "ml_gpu"
docker = ["{dir}/no-such-docker"]
state_file = "{dir}/immich.json"
"#,
        key = setup.path("no-key").display(),
        dir = setup.dir.path().display(),
    );
    let cfg = setup.config(&fake.url(), &immich);
    let check = run_cli(&["check-config", "-c", cfg.to_str().unwrap()]).await;
    assert!(
        check.out.contains("immich: http://127.0.0.1:9"),
        "{}",
        check.out
    );
    let daemon = Daemon::start(&setup, &cfg).await;

    let mut st = daemon.status_json().await;
    for _ in 0..50 {
        if st["immich"]["error"].is_string() {
            break;
        }
        sleep(Duration::from_millis(50)).await;
        st = daemon.status_json().await;
    }
    assert_eq!(
        st["immich"],
        json!({"state": "idle", "backlog": null, "paused_by_gpuq": false,
               "error": "no usable API key"}),
        "{st}"
    );
    // The backend key is still there next to it.
    assert_eq!(st["backend"]["kind"], "llama-swap", "{st}");

    let d = finish(daemon.cli(&["status"]).spawn().unwrap()).await;
    assert_eq!(d.rc, 0, "{}", d.err);
    let lines: Vec<&str> = d.out.lines().filter(|l| l.starts_with("immich")).collect();
    assert_eq!(lines.len(), 1, "{}", d.out);
    assert!(
        lines[0].contains("idle") && lines[0].contains("no usable API key"),
        "{}",
        d.out
    );
    assert_eq!(daemon.terminate().await, 0);
}

// ---------------------------------------------------------------------- several cards

impl Setup {
    /// A config with two cards, each with its own command probe.
    fn two_card_config(&self, backend_url: &str, extra: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let probe = |name: &str, used: u32| {
            let p = self.path(name);
            std::fs::write(&p, format!("#!/bin/sh\necho {used} 24560\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        };
        let (p0, p1) = (probe("probe0.sh", 100), probe("probe1.sh", 200));
        let models = self.path("models.yaml");
        std::fs::write(&models, MODELS_YAML).unwrap();
        let text = format!(
            r#"
[gate]
port = {gate}
backend = "{backend_url}"

[api]
port = {api}

[gpu]
driver_settle_s = 0.05
settle_timeout_s = 2

[[gpu.card]]
name = "left"
vendor = "command"
command = ["{p0}"]
visible_devices = "0"

[[gpu.card]]
name = "right"
vendor = "command"
command = ["{p1}"]
visible_devices = "1"

[llm]
llama_swap_config = "{models}"
reload_when_idle = false
{extra}
"#,
            gate = self.gate_port,
            api = self.api_port,
            p0 = p0.display(),
            p1 = p1.display(),
            models = models.display(),
        );
        let p = self.path("gpuq.toml");
        std::fs::write(&p, text).unwrap();
        p
    }
}

#[tokio::test]
async fn check_config_reads_every_card() {
    let setup = Setup::new();
    let cfg = setup.two_card_config("http://127.0.0.1:1", "");
    let d = run_cli(&["check-config", "-c", cfg.to_str().unwrap(), "--strict"]).await;
    assert_eq!(d.rc, 0, "{}", d.err);
    assert!(d.out.contains("left: command"), "{}", d.out);
    assert!(d.out.contains("CUDA_VISIBLE_DEVICES=1"), "{}", d.out);
    assert!(d.out.contains("probe left:"), "{}", d.out);
    assert!(d.out.contains("100 / 24560 MiB"), "{}", d.out);
    assert!(d.out.contains("probe right:"), "{}", d.out);
    assert!(d.out.contains("200 / 24560 MiB"), "{}", d.out);
}

#[tokio::test]
async fn check_config_strict_fails_when_any_card_probe_fails() {
    let setup = Setup::new();
    let cfg = setup.two_card_config("http://127.0.0.1:1", "");
    let text = std::fs::read_to_string(&cfg)
        .unwrap()
        .replace("probe1.sh", "missing.sh");
    std::fs::write(&cfg, text).unwrap();
    let c = cfg.to_str().unwrap();
    let d = run_cli(&["check-config", "-c", c]).await;
    assert_eq!(d.rc, 0, "{}", d.err);
    assert!(d.err.contains("card right"), "{}", d.err);
    assert!(d.out.contains("probe left:"), "{}", d.out);
    let d = run_cli(&["check-config", "-c", c, "--strict"]).await;
    assert_eq!(d.rc, 1);
}

#[tokio::test]
async fn check_config_notes_when_any_is_unavailable() {
    let setup = Setup::new();
    let cfg = setup.two_card_config("http://127.0.0.1:1", "");
    let text = std::fs::read_to_string(&cfg)
        .unwrap()
        .lines()
        .filter(|l| !l.starts_with("visible_devices"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&cfg, text).unwrap();
    let d = run_cli(&["check-config", "-c", cfg.to_str().unwrap()]).await;
    assert_eq!(d.rc, 0, "{}", d.err);
    assert!(d.out.contains("--card any"), "{}", d.out);
}

#[tokio::test]
async fn daemon_with_two_cards_starts_and_serves_leases() {
    let setup = Setup::new();
    let cfg = setup.two_card_config("http://127.0.0.1:1", "backend = \"none\"");
    let daemon = Daemon::start(&setup, &cfg).await;
    let d = finish(
        daemon
            .cli(&["run", "--card", "left", "--vram", "1G", "--", "true"])
            .spawn()
            .unwrap(),
    )
    .await;
    assert_eq!(d.rc, 0, "{}", d.err);
    let d = finish(
        daemon
            .cli(&["run", "--card", "nope", "--vram", "1G", "--", "true"])
            .spawn()
            .unwrap(),
    )
    .await;
    assert_eq!(d.rc, 1);
    assert!(d.err.contains("known: left, right"), "{}", d.err);
    assert_eq!(daemon.terminate().await, 0);
}

#[tokio::test]
async fn daemon_with_two_cards_lists_both_in_status() {
    let setup = Setup::new();
    let cfg = setup.two_card_config("http://127.0.0.1:1", "backend = \"none\"\ncard = \"right\"");
    let daemon = Daemon::start(&setup, &cfg).await;
    let st = daemon.status_json().await;
    let cards = st["cards"].as_array().unwrap();
    assert_eq!(cards.len(), 2, "{st}");
    assert_eq!(cards[0]["name"], "left", "{st}");
    assert_eq!(cards[1]["name"], "right", "{st}");
    assert_eq!(cards[0]["llm"], false, "{st}");
    assert_eq!(cards[1]["llm"], true, "{st}");

    let d = finish(daemon.cli(&["status"]).spawn().unwrap()).await;
    assert_eq!(d.rc, 0, "{}", d.err);
    assert_eq!(d.out.lines().filter(|l| l.starts_with("card ")).count(), 2);
    assert_eq!(daemon.terminate().await, 0);
}

#[tokio::test]
async fn daemon_names_the_card_whose_probe_cannot_be_built() {
    let setup = Setup::new();
    let cfg = setup.two_card_config("http://127.0.0.1:1", "backend = \"none\"");
    let empty = setup.path("sysfs");
    std::fs::create_dir(&empty).unwrap();
    let mut lines: Vec<String> = std::fs::read_to_string(&cfg)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    let at = lines.iter().position(|l| l.contains("probe1.sh")).unwrap();
    lines[at - 1] = "vendor = \"amd\"".into();
    lines[at] = format!("sysfs_root = \"{}\"", empty.display());
    std::fs::write(&cfg, lines.join("\n")).unwrap();
    let d = run_cli(&["daemon", "-c", cfg.to_str().unwrap()]).await;
    assert_eq!(d.rc, 1);
    assert!(d.err.contains("card right"), "{}", d.err);
}

#[tokio::test]
async fn check_config_lists_detected_cards_and_exclusions() {
    let dir = tempfile::tempdir().unwrap();
    let sys = dir.path().join("sys");
    for (pci, total, discrete) in [
        ("0000:03:00.0", 24560u64, true),
        ("0000:15:00.0", 2048, false),
    ] {
        let dev = sys.join(pci);
        std::fs::create_dir_all(&dev).unwrap();
        let mib = 1024 * 1024;
        std::fs::write(dev.join("mem_info_vram_total"), (total * mib).to_string()).unwrap();
        std::fs::write(dev.join("mem_info_vram_used"), (100 * mib).to_string()).unwrap();
        if discrete {
            std::fs::write(dev.join("mem_info_vram_vendor"), "samsung").unwrap();
            std::fs::write(dev.join("unique_id"), "abcd").unwrap();
        }
    }
    let cfg = dir.path().join("config.toml");
    let write = |extra: &str| {
        std::fs::write(
            &cfg,
            format!(
                "[llm]\nbackend = \"none\"\n[gpu]\nsysfs_root = {:?}\nnvidia_smi = \"/nonexistent/nvidia-smi\"\n{extra}",
                sys.to_str().unwrap()
            ),
        )
        .unwrap();
    };
    write("skip_integrated = true\n");
    let d = run_cli(&["check-config", "-c", cfg.to_str().unwrap(), "--strict"]).await;
    assert_eq!(d.rc, 0, "{}", d.err);
    assert!(d.out.contains("gpus: 2 detected, 1 in use"), "{}", d.out);
    assert!(
        d.out
            .contains("gpu0: amd pci=0000:03:00.0 24560 MiB, ROCR_VISIBLE_DEVICES=GPU-abcd"),
        "{}",
        d.out
    );
    assert!(d.out.contains("excluded: amd 0000:15:00.0"), "{}", d.out);
    assert!(
        d.out.contains("probe: amd:0000:03:00.0, 100 / 24560 MiB"),
        "{}",
        d.out
    );
    // A card name that does not exist among the detected ones fails the check.
    write("skip_integrated = true\n[[service]]\nname = \"s\"\nvram = \"1G\"\ncard = \"gpu1\"\n");
    let d = run_cli(&["check-config", "-c", cfg.to_str().unwrap()]).await;
    assert_eq!(d.rc, 1, "{}", d.out);
    assert!(d.err.contains("unknown card \"gpu1\""), "{}", d.err);
}
