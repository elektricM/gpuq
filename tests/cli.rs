// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! The `gpud` command line, run as a real subprocess against an in-process API server.
//! Ported from tests/test_cli.py.

mod support_api;

use std::net::SocketAddr;
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use support_api::*;
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio::time::timeout;

const PING: Duration = Duration::from_millis(200);

fn gpud(addr: SocketAddr, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gpud"));
    cmd.args(args)
        .env("GPUD_API", addr.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd
}

struct Done {
    rc: i32,
    out: String,
    err: String,
}

async fn finish(child: Child) -> Done {
    let o = timeout(Duration::from_secs(20), child.wait_with_output())
        .await
        .expect("gpud did not finish in 20 s")
        .unwrap();
    Done {
        rc: o.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&o.stdout).into_owned(),
        err: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

async fn run(addr: SocketAddr, args: &[&str]) -> Done {
    finish(gpud(addr, args).spawn().unwrap()).await
}

async fn free_addr() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    l.local_addr().unwrap()
}

async fn api() -> TestApi {
    TestApi::start(PING).await
}

#[tokio::test]
async fn run_returns_the_exit_code_of_the_job() {
    let api = api().await;
    let d = run(
        api.addr,
        &["run", "--class", "batch", "--vram", "1G", "--", "true"],
    )
    .await;
    assert_eq!(d.rc, 0, "{}", d.err);
    let d = run(
        api.addr,
        &["run", "--vram", "1G", "--", "sh", "-c", "exit 7"],
    )
    .await;
    assert_eq!(d.rc, 7);
    assert!(status_within(&api.rig, Duration::from_secs(2), |s| s.leases.is_empty()).await);
}

#[tokio::test]
async fn job_killed_by_a_signal_gives_128_plus_it() {
    let api = api().await;
    let d = run(
        api.addr,
        &["run", "--vram", "1G", "--", "sh", "-c", "kill -TERM $$"],
    )
    .await;
    assert_eq!(d.rc, 128 + 15);
}

#[tokio::test]
async fn run_holds_the_lease_while_the_job_runs() {
    let api = api().await;
    let child = gpud(
        api.addr,
        &[
            "run", "--vram", "1G", "--name", "napper", "--", "sleep", "1",
        ],
    )
    .spawn()
    .unwrap();
    assert!(
        status_within(&api.rig, Duration::from_secs(5), |s| s.leases.iter().any(
            |l| l.name == "napper" && l.pid.is_some() && l.vram.to_string() == "1024MiB"
        ))
        .await
    );
    assert_eq!(finish(child).await.rc, 0);
    assert!(status_within(&api.rig, Duration::from_secs(2), |s| s.leases.is_empty()).await);
}

#[tokio::test]
async fn the_job_sees_its_lease_id_and_a_fresh_yield_file() {
    let api = api().await;
    let script = r#"
        test -n "$GPUD_LEASE_ID" || exit 11
        test -n "$GPUD_YIELD_FILE" || exit 12
        test ! -e "$GPUD_YIELD_FILE" || exit 13
        echo "$GPUD_LEASE_ID" > "$OUT"
        echo "$GPUD_YIELD_FILE" >> "$OUT"
    "#;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let mut cmd = gpud(api.addr, &["run", "--vram", "1G", "--", "sh", "-c", script]);
    cmd.env("OUT", &out);
    let d = finish(cmd.spawn().unwrap()).await;
    assert_eq!(d.rc, 0, "{}", d.err);
    let text = std::fs::read_to_string(&out).unwrap();
    let yield_file = text.lines().nth(1).unwrap();
    assert!(
        !std::path::Path::new(yield_file).exists(),
        "yield file left behind"
    );
}

#[tokio::test]
async fn yieldable_job_gets_the_signal_and_the_file_when_the_llm_needs_the_card() {
    let api = api().await;
    let dir = tempfile::tempdir().unwrap();
    let ready = dir.path().join("ready");
    let script = r#"
        trap 'if [ -e "$GPUD_YIELD_FILE" ]; then exit 0; else exit 3; fi' USR1
        echo up > "$READY"
        while :; do sleep 0.05; done
    "#;
    let mut cmd = gpud(
        api.addr,
        &[
            "run",
            "--class",
            "background",
            "--vram",
            "6G",
            "--yieldable",
            "--",
            "sh",
            "-c",
            script,
        ],
    );
    cmd.env("READY", &ready);
    let child = cmd.spawn().unwrap();
    assert!(eventually_exists(&ready).await, "job never started");
    api.rig.card.set_job_vram(6000);
    let chat = api.rig.spawn_chat("big");
    let d = finish(child).await;
    assert_eq!(
        d.rc, 0,
        "job did not see the signal and the file: {}",
        d.err
    );
    assert!(
        d.err.contains("asked to yield: sent SIGUSR1 to"),
        "{}",
        d.err
    );
    api.rig.card.set_job_vram(0);
    assert_eq!(done(chat).await, 200);
}

#[tokio::test]
async fn the_yield_signal_is_configurable() {
    let api = api().await;
    let dir = tempfile::tempdir().unwrap();
    let ready = dir.path().join("ready");
    let script = r#"
        trap 'exit 5' TERM
        echo up > "$READY"
        while :; do sleep 0.05; done
    "#;
    let mut cmd = gpud(
        api.addr,
        &[
            "run",
            "--class",
            "background",
            "--vram",
            "6G",
            "--yieldable",
            "--yield-signal",
            "term",
            "--",
            "sh",
            "-c",
            script,
        ],
    );
    cmd.env("READY", &ready);
    let child = cmd.spawn().unwrap();
    assert!(eventually_exists(&ready).await);
    api.rig.card.set_job_vram(6000);
    let chat = api.rig.spawn_chat("big");
    let d = finish(child).await;
    assert_eq!(d.rc, 5, "{}", d.err);
    assert!(d.err.contains("sent SIGTERM"), "{}", d.err);
    api.rig.card.set_job_vram(0);
    assert_eq!(done(chat).await, 200);
}

#[tokio::test]
async fn unknown_class_fails_cleanly() {
    let api = api().await;
    let d = run(api.addr, &["run", "--class", "nope", "--", "true"]).await;
    assert_eq!(d.rc, 1);
    assert!(d.err.contains("unknown class"), "{}", d.err);
}

#[tokio::test]
async fn run_without_a_command_is_a_usage_error() {
    let api = api().await;
    let d = run(api.addr, &["run", "--vram", "1G"]).await;
    assert_eq!(d.rc, 2);
    let d = run(api.addr, &["run", "--vram", "lots", "--", "true"]).await;
    assert_eq!(d.rc, 2);
    assert!(d.err.contains("lots"));
}

#[tokio::test]
async fn status_text_and_json() {
    let api = api().await;
    let d = run(api.addr, &["status"]).await;
    assert_eq!(d.rc, 0, "{}", d.err);
    assert!(d.out.contains("state"), "{}", d.out);
    assert!(d.out.contains("big (ready)"), "{}", d.out);
    assert!(d.out.contains("fake:0"), "{}", d.out);
    assert!(d.out.contains("MiB"), "{}", d.out);
    let d = run(api.addr, &["status", "--json"]).await;
    let st: Value = serde_json::from_str(&d.out).unwrap();
    assert_eq!(st["state"], "llm");
}

#[tokio::test]
async fn status_shows_a_lease() {
    let api = api().await;
    let _lease = api.rig.lease("batch", 2000, true, "shown").await;
    let d = run(api.addr, &["status"]).await;
    assert!(d.out.contains("batch/shown vram=2000MiB"), "{}", d.out);
}

#[tokio::test]
async fn exit_code_69_when_gpud_is_down() {
    let addr = free_addr().await;
    let d = run(addr, &["status"]).await;
    assert_eq!(d.rc, 69);
    assert!(d.err.contains("not running"), "{}", d.err);
    let d = run(addr, &["run", "--", "true"]).await;
    assert_eq!(d.rc, 69);
    let d = run(addr, &["hold"]).await;
    assert_eq!(d.rc, 69);
}

#[tokio::test]
async fn fallback_runs_the_job_without_gpud() {
    let addr = free_addr().await;
    let d = run(addr, &["run", "--fallback", "--", "true"]).await;
    assert_eq!(d.rc, 0, "{}", d.err);
    assert!(d.err.contains("without a lease"), "{}", d.err);
    let d = run(addr, &["run", "--fallback", "--", "sh", "-c", "exit 4"]).await;
    assert_eq!(d.rc, 4);
}

#[tokio::test]
async fn hold_keeps_the_card_until_interrupted() {
    let api = api().await;
    let child = gpud(api.addr, &["hold", "--vram", "1G"]).spawn().unwrap();
    let pid = child.id().unwrap() as i32;
    assert!(
        status_within(&api.rig, Duration::from_secs(5), |s| s
            .leases
            .iter()
            .any(|l| l.name == "manual"))
        .await
    );
    // The lease is granted before the message is printed; give the print a moment.
    tokio::time::sleep(Duration::from_millis(100)).await;
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGINT,
    )
    .unwrap();
    let d = finish(child).await;
    assert_eq!(d.rc, 0, "{}", d.err);
    assert!(d.err.contains("holding the GPU as"), "{}", d.err);
    assert!(status_within(&api.rig, Duration::from_secs(2), |s| s.leases.is_empty()).await);
}

#[tokio::test]
async fn run_re_adopts_its_lease_after_gpud_restarts() {
    let mut api = api().await;
    let dir = tempfile::tempdir().unwrap();
    let ready = dir.path().join("ready");
    let stop = dir.path().join("stop");
    let script = r#"
        echo up > "$READY"
        while [ ! -e "$STOP" ]; do sleep 0.05; done
    "#;
    let mut cmd = gpud(
        api.addr,
        &[
            "run", "--vram", "1G", "--name", "survivor", "--", "sh", "-c", script,
        ],
    );
    cmd.env("READY", &ready).env("STOP", &stop);
    let child = cmd.spawn().unwrap();
    assert!(eventually_exists(&ready).await);
    let id = api.rig.status().await.leases[0].id.clone();
    api.restart().await;
    assert!(
        status_within(&api.rig, Duration::from_secs(5), |s| s
            .leases
            .iter()
            .any(|l| l.id == id && l.name == "survivor"))
        .await,
        "lease {id} was not re-adopted"
    );
    std::fs::write(&stop, "").unwrap();
    let d = finish(child).await;
    assert_eq!(d.rc, 0, "{}", d.err);
    assert!(d.err.contains("re-adopting"), "{}", d.err);
}

async fn eventually_exists(path: &std::path::Path) -> bool {
    for _ in 0..250 {
        if path.exists() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test]
async fn the_daemon_announces_its_yield_signal() {
    // No --yield-signal: the job gets what gpud announced with the grant.
    let api = TestApi::start_with_yield_signal(PING, "SIGUSR2").await;
    let dir = tempfile::tempdir().unwrap();
    let ready = dir.path().join("ready");
    let script = r#"
        trap 'exit 6' USR2
        echo up > "$READY"
        while :; do sleep 0.05; done
    "#;
    let mut cmd = gpud(
        api.addr,
        &[
            "run",
            "--class",
            "background",
            "--vram",
            "6G",
            "--yieldable",
            "--",
            "sh",
            "-c",
            script,
        ],
    );
    cmd.env("READY", &ready);
    let child = cmd.spawn().unwrap();
    assert!(eventually_exists(&ready).await);
    api.rig.card.set_job_vram(6000);
    let chat = api.rig.spawn_chat("big");
    let d = finish(child).await;
    assert_eq!(d.rc, 6, "{}", d.err);
    assert!(d.err.contains("sent SIGUSR2"), "{}", d.err);
    api.rig.card.set_job_vram(0);
    assert_eq!(done(chat).await, 200);
}

// ---------------------------------------------------------------------- cards

#[tokio::test]
async fn card_flag_names_the_card_and_the_job_sees_it() {
    let api = api().await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let mut cmd = gpud(
        api.addr,
        &[
            "run",
            "--card",
            "gpu0",
            "--vram",
            "1G",
            "--",
            "sh",
            "-c",
            r#"echo "$GPUD_CARD" > "$OUT""#,
        ],
    );
    cmd.env("OUT", &out);
    let d = finish(cmd.spawn().unwrap()).await;
    assert_eq!(d.rc, 0, "{}", d.err);
    assert_eq!(std::fs::read_to_string(&out).unwrap().trim(), "gpu0");
}

#[tokio::test]
async fn the_card_is_exported_without_the_flag_too() {
    let api = api().await;
    let script = r#"test "$GPUD_CARD" = gpu0 || exit 9"#;
    let d = run(api.addr, &["run", "--vram", "1G", "--", "sh", "-c", script]).await;
    assert_eq!(d.rc, 0, "{}", d.err);
}

#[tokio::test]
async fn an_unknown_card_exits_1() {
    let api = api().await;
    let d = run(
        api.addr,
        &["run", "--card", "nope", "--vram", "1G", "--", "true"],
    )
    .await;
    assert_eq!(d.rc, 1);
    assert!(d.err.contains("unknown card"), "{}", d.err);
    assert!(d.err.contains("known: gpu0"), "{}", d.err);
    let d = run(api.addr, &["hold", "--card", "nope"]).await;
    assert_eq!(d.rc, 1);
    assert!(d.err.contains("unknown card"), "{}", d.err);
}

#[tokio::test]
async fn status_with_one_card_has_no_card_lines() {
    let api = api().await;
    let d = run(api.addr, &["status"]).await;
    assert_eq!(d.rc, 0, "{}", d.err);
    assert!(!d.out.contains("card "), "{}", d.out);
    assert!(d.out.contains("device"), "{}", d.out);
}

#[tokio::test]
async fn status_with_two_cards_has_a_line_each() {
    let (api, _gpu1) = TestApi::start_two_cards(&[], &[], PING).await;
    let d = run(api.addr, &["status"]).await;
    assert_eq!(d.rc, 0, "{}", d.err);
    let cards: Vec<&str> = d.out.lines().filter(|l| l.starts_with("card ")).collect();
    assert_eq!(cards.len(), 2, "{}", d.out);
    assert!(
        cards[0].contains("gpu0") && cards[0].contains("llm"),
        "{}",
        d.out
    );
    assert!(cards[1].contains("gpu1"), "{}", d.out);
}

#[tokio::test]
async fn a_job_on_gpu1_gets_that_cards_env() {
    let (api, _gpu1) = TestApi::start_two_cards(
        &[("CUDA_VISIBLE_DEVICES", "0")],
        &[("CUDA_VISIBLE_DEVICES", "1")],
        PING,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let mut cmd = gpud(
        api.addr,
        &[
            "run",
            "--card",
            "gpu1",
            "--vram",
            "1G",
            "--",
            "sh",
            "-c",
            r#"echo "$GPUD_CARD $CUDA_VISIBLE_DEVICES" > "$OUT""#,
        ],
    );
    cmd.env("OUT", &out);
    let d = finish(cmd.spawn().unwrap()).await;
    assert_eq!(d.rc, 0, "{}", d.err);
    assert_eq!(std::fs::read_to_string(&out).unwrap().trim(), "gpu1 1");
}
