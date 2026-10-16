//! Configuration loading, validation and conversion.

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpud::arbiter::Vram;
use gpud::config::{parse_vram, Config, ConfigError, GpuVendor, LlmBackend, ServiceKind};

fn example_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/gpud.toml")
}

fn parse(text: &str) -> Result<Config, ConfigError> {
    Config::from_toml_str(text)
}

fn err_text(text: &str) -> String {
    parse(text)
        .expect_err("config should be rejected")
        .to_string()
}

#[test]
fn defaults_are_valid() {
    assert_eq!(Config::default().validate(), Vec::<String>::new());
}

#[test]
fn example_file_matches_the_defaults() {
    let cfg = Config::load(Some(&example_path())).unwrap();
    assert_eq!(cfg, Config::default());
}

#[test]
fn file_values_override_and_tables_merge() {
    let cfg = parse("[gate]\nport = 9000\n[classes]\nnightly = 7\n").unwrap();
    assert_eq!(cfg.gate.port, 9000);
    assert_eq!(cfg.gate.backend, Config::default().gate.backend);
    assert_eq!(cfg.classes["nightly"], 7);
    assert_eq!(cfg.classes["llm"], 1);
    assert_eq!(cfg.classes["batch"], 4);
}

#[test]
fn user_classes_can_override_default_priorities() {
    let cfg = parse("[classes]\nbatch = 9\n").unwrap();
    assert_eq!(cfg.classes["batch"], 9);
    assert_eq!(cfg.classes.len(), 5);
}

#[test]
fn lists_are_replaced() {
    let cfg = parse("[llm]\nidle_wait_classes = [\"batch\"]\n").unwrap();
    assert_eq!(cfg.llm.idle_wait_classes, ["batch"]);
}

#[test]
fn durations_take_integers_and_floats() {
    let cfg = parse("[gpu]\ndriver_settle_s = 0.5\nsettle_timeout_s = 7\n").unwrap();
    assert_eq!(cfg.gpu.driver_settle, Duration::from_millis(500));
    assert_eq!(cfg.gpu.settle_timeout, Duration::from_secs(7));
    assert!(err_text("[gpu]\ndriver_settle_s = -1\n").contains("duration"));
}

#[test]
fn llm_config_alias_is_accepted() {
    let cfg = parse("[llm]\nconfig = \"/tmp/swap.yaml\"\n").unwrap();
    assert_eq!(cfg.llm.llama_swap_config, Path::new("/tmp/swap.yaml"));
}

#[test]
fn missing_explicit_path_is_an_error() {
    match Config::load(Some(Path::new("/nonexistent/gpud.toml"))) {
        Err(ConfigError::Read { source, .. }) => {
            assert_eq!(source.kind(), std::io::ErrorKind::NotFound)
        }
        other => panic!("expected a read error, got {other:?}"),
    }
}

#[test]
fn explicit_path_is_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.toml");
    std::fs::write(&path, "[api]\nport = 1234\n").unwrap();
    assert_eq!(Config::load(Some(&path)).unwrap().api.port, 1234);
}

#[test]
fn vendors_parse_and_unknown_vendor_is_rejected() {
    assert_eq!(
        parse("[gpu]\nvendor = \"intel\"\n").unwrap().gpu.vendor,
        GpuVendor::Intel
    );
    assert!(parse("[gpu]\nvendor = \"voodoo\"\n").is_err());
}

#[test]
fn command_vendor_needs_a_command() {
    assert!(err_text("[gpu]\nvendor = \"command\"\n").contains("gpu.command"));
    let cfg = parse("[gpu]\nvendor = \"command\"\ncommand = [\"/bin/probe\"]\n").unwrap();
    assert_eq!(cfg.gpu.command, ["/bin/probe"]);
}

#[test]
fn undefined_class_is_rejected() {
    assert!(err_text("[llm.model_classes]\nm = \"ghost\"\n").contains("ghost"));
    assert!(err_text("[llm.class_tuning.ghost]\ngap_s = 1\n").contains("ghost"));
    assert!(err_text("[llm]\nidle_wait_classes = [\"ghost\"]\n").contains("ghost"));
    let svc = "[[service]]\nname = \"s\"\nvram = \"1G\"\nclass = \"ghost\"\n";
    assert!(err_text(svc).contains("ghost"));
}

#[test]
fn llm_class_is_required() {
    let mut cfg = Config::default();
    cfg.classes.remove("llm");
    assert!(cfg.validate().iter().any(|p| p.contains("llm")));
}

#[test]
fn all_problems_are_reported() {
    let text = "[gate]\nport = 0\n[api]\nport = 0\n[run]\nyield_signal = \"SIGNOPE\"\n";
    let ConfigError::Invalid(problems) = parse(text).unwrap_err() else {
        panic!("expected Invalid");
    };
    assert_eq!(problems.len(), 3, "{problems:?}");
}

#[test]
fn unknown_key_is_rejected() {
    assert!(parse("[gate]\nprot = 1\n").is_err());
    assert!(parse("[nonsense]\nx = 1\n").is_err());
}

#[test]
fn python_style_sections_point_at_service() {
    for section in ["comfyui", "immich"] {
        let msg = err_text(&format!("[{section}]\nenabled = true\n"));
        assert!(msg.contains("[[service]]"), "{msg}");
    }
}

#[test]
fn yield_signal_is_validated() {
    assert_eq!(Config::default().run.yield_signal, "SIGUSR1");
    assert!(parse("[run]\nyield_signal = \"SIGUSR2\"\n").is_ok());
    assert!(err_text("[run]\nyield_signal = \"SIGFOO\"\n").contains("SIGFOO"));
}

#[test]
fn parse_vram_cases() {
    assert_eq!(parse_vram("8G").unwrap(), Vram::Mib(8192));
    assert_eq!(parse_vram("8GiB").unwrap(), Vram::Mib(8192));
    assert_eq!(parse_vram("1.5g").unwrap(), Vram::Mib(1536));
    assert_eq!(parse_vram("3000M").unwrap(), Vram::Mib(3000));
    assert_eq!(parse_vram("512").unwrap(), Vram::Mib(512));
    assert_eq!(parse_vram("ALL").unwrap(), Vram::WholeCard);
    assert!(parse_vram("lots").is_err());
    assert!(parse_vram("").is_err());
    assert!(parse_vram("0").is_err());
    assert!(parse_vram("-4G").is_err());
}

#[test]
fn server_backends_need_llm_service() {
    for b in ["vllm", "sglang", "llama-server"] {
        let msg = err_text(&format!("[llm]\nbackend = \"{b}\"\n"));
        assert!(msg.contains("[llm.service]"), "{msg}");
    }
    let ok =
        "[llm]\nbackend = \"vllm\"\n[llm.service]\nkind = \"systemd\"\nunit = \"vllm.service\"\n";
    let cfg = parse(ok).unwrap();
    assert_eq!(cfg.llm.backend, LlmBackend::Vllm);
    let spec = cfg.llm.service.unwrap();
    assert_eq!(
        spec.kind,
        ServiceKind::Systemd {
            unit: "vllm.service".into(),
            user: false
        }
    );
    // Not needed for the others.
    assert!(parse("[llm]\nbackend = \"ollama\"\n").is_ok());
    assert!(parse("[llm]\nbackend = \"none\"\n").is_ok());
}

#[test]
fn service_spec_must_be_complete() {
    let svc = |body: &str| format!("[llm]\nbackend = \"vllm\"\n[llm.service]\n{body}");
    assert!(parse(&svc("kind = \"systemd\"\n")).is_err());
    assert!(parse(&svc("kind = \"docker\"\n")).is_err());
    assert!(parse(&svc("kind = \"command\"\nstart = [\"a\"]\n")).is_err());
    assert!(parse(&svc("kind = \"command\"\nstart = []\nstop = [\"b\"]\n")).is_err());
    assert!(parse(&svc("kind = \"docker\"\ncontainer = \"c\"\nunit = \"u\"\n")).is_err());
    assert!(parse(&svc("kind = \"zzz\"\n")).is_err());
    let ok = parse(&svc(
        "kind = \"command\"\nstart = [\"a\"]\nstop = [\"b\"]\nstatus = [\"c\"]\n\
         health_url = \"http://127.0.0.1:1/h\"\nstart_timeout_s = 5\n",
    ))
    .unwrap();
    let spec = ok.llm.service.unwrap();
    assert_eq!(spec.start_timeout, Duration::from_secs(5));
    assert!(spec.health_url.is_some());
}

const SERVICES: &str = r#"
[[service]]
name = "comfyui"
vram = "16G"
flavor = "comfyui"
[service.control]
kind = "podman"
container = "comfy"
[service.gate]
listen = "127.0.0.1:8188"
backend = "http://127.0.0.1:8189"
gpu_paths = ["/prompt"]
busy_url = "http://127.0.0.1:8189/queue"

[[service]]
name = "tts"
class = "background"
vram = "all"
yieldable = false
stop_on_release = false
idle_release_s = 30
"#;

#[test]
fn services_parse_with_defaults() {
    let cfg = parse(SERVICES).unwrap();
    assert_eq!(cfg.service.len(), 2);
    let comfy = &cfg.service[0];
    assert_eq!(comfy.class, "interactive");
    assert!(comfy.yieldable && comfy.stop_on_release);
    assert_eq!(comfy.idle_release, Duration::from_secs(120));
    assert_eq!(comfy.vram().unwrap(), Vram::Mib(16384));
    assert!(matches!(
        comfy.control.as_ref().unwrap().kind,
        ServiceKind::Podman { .. }
    ));
    let gate = comfy.gate.as_ref().unwrap();
    assert_eq!(gate.host_port().unwrap(), ("127.0.0.1".to_string(), 8188));
    assert_eq!(gate.gpu_paths, ["/prompt"]);
    let tts = &cfg.service[1];
    assert_eq!(tts.vram().unwrap(), Vram::WholeCard);
    assert!(tts.control.is_none() && tts.gate.is_none());
    assert_eq!(tts.idle_release, Duration::from_secs(30));
}

#[test]
fn service_problems_are_caught() {
    assert!(err_text("[[service]]\nname = \"a\"\nvram = \"lots\"\n").contains("vram"));
    assert!(err_text("[[service]]\nvram = \"1G\"\n").contains("name"));
    let dup =
        "[[service]]\nname = \"a\"\nvram = \"1G\"\n[[service]]\nname = \"a\"\nvram = \"1G\"\n";
    assert!(err_text(dup).contains("duplicate"));
}

#[test]
fn listen_collisions_are_caught() {
    let clash = |listen: &str| {
        format!(
            "[[service]]\nname = \"a\"\nvram = \"1G\"\n[service.gate]\nlisten = \"{listen}\"\n\
             backend = \"http://127.0.0.1:1\"\n"
        )
    };
    assert!(err_text(&clash("127.0.0.1:8080")).contains("port 8080"));
    assert!(err_text(&clash("0.0.0.0:9390")).contains("port 9390"));
    assert!(err_text(&clash("nonsense")).contains("host:port"));
    assert!(parse(&clash("127.0.0.1:8188")).is_ok());
    // Same port on different concrete addresses is fine.
    assert!(parse(&format!(
        "[gate]\nhost = \"127.0.0.2\"\n{}",
        clash("127.0.0.1:8080")
    ))
    .is_ok());
    // A wildcard gate collides with any address on its port.
    assert!(err_text(&format!(
        "[gate]\nhost = \"*\"\n{}",
        clash("127.0.0.1:8080")
    ))
    .contains("8080"));
}

#[test]
fn arbiter_settings_mapping() {
    let text = r#"
[gpu]
margin_mib = 300
settle_timeout_s = 11
driver_settle_s = 0.25
whole_card_baseline_mib = 700
[hooks]
offload_settle_s = 4
[llm]
resident = "chat"
idle_s = 90
gap_s = 5
reload_when_idle = false
reload_delay_s = 6
reload_slack_mib = 2048
reload_wait_s = 7
resume_grace_s = 8
batch_hold_s = 2.5
batch_hold_count = 3
idle_wait_classes = ["batch"]
[llm.class_tuning.realtime]
gap_s = 120
[classes]
nightly = 7
"#;
    let s = parse(text).unwrap().arbiter_settings();
    assert_eq!(s.resident.as_deref(), Some("chat"));
    assert_eq!(s.classes.llm(), 1);
    assert_eq!(s.classes.job_prio("nightly"), 7);
    let t = &s.tuning;
    assert_eq!(t.margin_mib, 300);
    assert_eq!(t.settle_timeout, Duration::from_secs(11));
    assert_eq!(t.driver_settle, Duration::from_millis(250));
    assert_eq!(t.whole_card_baseline_mib, 700);
    assert_eq!(t.offload_settle, Duration::from_secs(4));
    assert_eq!(t.idle, Duration::from_secs(90));
    assert_eq!(t.gap, Duration::from_secs(5));
    assert!(!t.reload_when_idle);
    assert_eq!(t.reload_delay, Duration::from_secs(6));
    assert_eq!(t.reload_slack_mib, 2048);
    assert_eq!(t.reload_wait, Duration::from_secs(7));
    assert_eq!(t.resume_grace, Duration::from_secs(8));
    assert_eq!(t.batch_hold, Duration::from_millis(2500));
    assert_eq!(t.batch_hold_count, 3);
    assert_eq!(t.idle_wait_classes.len(), 1);
    assert_eq!(t.gap_after("realtime"), Duration::from_secs(120));
    assert_eq!(t.gap_after("batch"), Duration::from_secs(5));
    assert_eq!(t.reload_delay_after("realtime"), Duration::from_secs(6));
}

#[test]
fn empty_resident_means_none() {
    assert_eq!(Config::default().arbiter_settings().resident, None);
}

#[test]
fn describe_mentions_the_essentials() {
    let text = Config::from_toml_str(SERVICES).unwrap().describe();
    for needle in [
        "llama-swap",
        "127.0.0.1:8080",
        "9390",
        "auto",
        "realtime=0, llm=1, interactive=2",
        "comfyui",
        "tts",
    ] {
        assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
    }
}
