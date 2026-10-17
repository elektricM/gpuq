// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! The Immich ML adapter: queue backlog to lease to container, against a fake Immich API, a
//! fake docker script and the in-memory arbiter rig. Real sockets and processes, short
//! intervals (a poll is 50 ms).

mod common;
#[path = "support_immich/mod.rs"]
mod support;

use std::time::{Duration, Instant};

use common::{granted, Rig};
use gpuq::arbiter::Vram;
use gpuq::config::Config;
use serde_json::json;
use support::{eventually, eventually_async, FakeDocker, Harness, GPU_CONCURRENCY};
use tokio::time::sleep;

const URGENT_MIB: u64 = 20000;

async fn harness() -> Harness {
    Harness::prepare(Rig::start_with(false, None, |_| {}).await).await
}

/// A realtime job that does not fit next to the Immich lease, so the arbiter asks it to yield.
fn urgent(
    h: &Harness,
) -> tokio::task::JoinHandle<Result<gpuq::arbiter::Lease, gpuq::arbiter::ArbiterError>> {
    h.rig
        .spawn_acquire("realtime", Vram::Mib(URGENT_MIB), false, "urgent")
}

// ------------------------------------------------------------------ the trigger

#[tokio::test]
async fn a_small_backlog_takes_no_lease_and_pauses_nothing() {
    let mut h = harness().await;
    h.immich.set_backlog(99); // min_backlog is 100; the 9999 jobs of another queue do not count
    h.start(&[]);
    let polls = || h.immich.st.lock().unwrap().queue_gets;
    eventually("a few polls", || polls() >= 4).await;
    assert!(h.lease_names().await.is_empty());
    assert!(h.queued_names().await.is_empty());
    assert!(h.immich.events().is_empty(), "{:?}", h.immich.events());
    assert!(h.docker.compose_calls().is_empty());
    assert_eq!(h.state_paused(), None);
    assert_eq!(h.view.json()["backlog"], 99);
    assert_eq!(h.view_state(), "idle");
}

#[tokio::test]
async fn the_backlog_counts_waiting_active_delayed_and_paused() {
    let mut h = harness().await;
    {
        let mut s = h.immich.st.lock().unwrap();
        let q = s.queues.get_mut("ocr").unwrap();
        (q.waiting, q.active, q.delayed, q.paused) = (30, 30, 20, 20);
    }
    h.start(&[]);
    // 100 in total, exactly min_backlog.
    eventually("a lease", || h.docker.calls(&FakeDocker::up_line()) > 0).await;
    assert!(h.has_lease().await);
}

#[tokio::test]
async fn a_big_backlog_pauses_the_queues_and_takes_a_lease() {
    let mut h = harness().await;
    h.immich.set_backlog(100);
    h.start(&[]);
    eventually("a lease", || h.docker.calls(&FakeDocker::up_line()) > 0).await;
    assert!(h.has_lease().await);
    let ev = h.immich.events();
    assert_eq!(
        ev[..3],
        ["pause smartSearch", "pause faceDetection", "pause ocr"]
    );
    assert_eq!(h.state_paused(), Some(true));
    let status = h.rig.status().await;
    let lease = status
        .leases
        .iter()
        .find(|l| l.name == "immich-ml")
        .unwrap();
    assert_eq!(lease.class, "background");
    assert_eq!(lease.vram, Vram::Mib(6144));
    assert!(lease.yieldable);
}

#[tokio::test]
async fn a_busy_card_leaves_the_adapter_queued_with_the_queues_paused() {
    let rig = Rig::start_with(false, None, |_| {}).await;
    let blocker = rig.lease("realtime", URGENT_MIB, false, "blocker").await;
    let mut h = Harness::prepare(rig).await;
    h.immich.set_backlog(500);
    h.start(&[]);
    eventually("queued", || h.view_state() == "waiting").await;
    assert!(h.immich.all_paused());
    assert_eq!(h.queued_names().await, ["immich-ml"]);
    assert!(h.docker.compose_calls().is_empty());
    drop(blocker);
    eventually("started", || h.docker.calls(&FakeDocker::up_line()) > 0).await;
    h.wait_serving().await;
}

// ------------------------------------------------------------------ the grant

#[tokio::test]
async fn a_grant_starts_the_container_waits_settles_then_raises_concurrency_and_resumes() {
    let mut h = harness().await;
    h.immich.set_backlog(300);
    h.docker.set_health("starting");
    h.start(&[("settle_s", "0.4")]);

    eventually("compose up", || h.docker.calls(&FakeDocker::up_line()) > 0).await;
    // Not healthy yet: nothing touches the queues beyond the pause.
    sleep(Duration::from_millis(300)).await;
    assert_eq!(h.view_state(), "starting");
    assert!(h.immich.all_paused());
    assert_eq!(h.immich.config_put_count(), 0);
    assert!(h.docker.calls(&FakeDocker::health_line()) >= 2);

    h.docker.set_health("healthy");
    let healthy_at = Instant::now();
    h.wait_serving().await;
    let raised_at = h.immich.first_config_put().unwrap();
    assert!(
        raised_at.duration_since(healthy_at) >= Duration::from_millis(380),
        "concurrency raised {:?} after the container turned healthy",
        raised_at.duration_since(healthy_at)
    );
    // The full config went back, with the GPU values and the sections gpuq does not touch.
    let put = &h.immich.config_puts()[0];
    for (q, n) in GPU_CONCURRENCY {
        assert_eq!(put["job"][q]["concurrency"], n);
    }
    assert!(put["machineLearning"]["urls"].is_array());
    assert_eq!(h.state_paused(), Some(false));

    // The docker calls, exactly as a user would type them.
    assert_eq!(h.docker.compose_calls(), [FakeDocker::up_line()]);
    assert_eq!(
        h.docker.cwd(),
        h.dir.path().canonicalize().unwrap().display().to_string()
    );
    assert!(h.docker.is_running());
    assert!(h.has_lease().await);
    let ev = h.immich.events();
    assert_eq!(ev.iter().filter(|e| *e == "config").count(), 1);
    let config_at = ev.iter().position(|e| e == "config").unwrap();
    let resume_at = ev.iter().position(|e| e == "resume smartSearch").unwrap();
    assert!(config_at < resume_at, "{ev:?}");
}

#[tokio::test]
async fn a_failing_compose_up_gives_the_card_back_and_retries() {
    let mut h = harness().await;
    h.immich.set_backlog(300);
    h.docker.fail("up", true);
    h.start(&[]);
    eventually("two attempts", || {
        h.docker.calls(&FakeDocker::up_line()) >= 2
    })
    .await;
    assert!(
        h.immich.all_paused(),
        "queues stay paused for the next lease"
    );
    assert_eq!(h.immich.config_put_count(), 0);
    assert!(h.view.json()["error"]
        .as_str()
        .unwrap()
        .contains("compose up"));
    h.docker.fail("up", false);
    h.wait_serving().await;
    assert!(h.view.json()["error"].is_null());
}

// ------------------------------------------------------------------ yielding

#[tokio::test]
async fn a_yield_drains_stops_restores_and_keeps_a_big_leftover_paused() {
    let mut h = harness().await;
    h.immich.set_backlog(300);
    h.start(&[]);
    h.wait_serving().await;

    h.immich.set_active(2);
    h.immich.set_drain_on_get(true);
    let polls_before = h.immich.st.lock().unwrap().queue_gets;
    let urgent = urgent(&h);
    let lease = granted(urgent).await;

    // By the time the other job has the card, the container is gone and Immich is as it was.
    assert_eq!(h.docker.calls(&FakeDocker::stop_line()), 1);
    assert!(!h.docker.is_running());
    assert!(h.immich.concurrency_is(&support::OLD_CONCURRENCY));
    assert!(h.immich.all_paused(), "leftover is above cpu_ok_backlog");
    assert_eq!(h.state_paused(), Some(true));
    // The two active jobs were waited for.
    assert!(h.immich.st.lock().unwrap().queue_gets >= polls_before + 3);
    let ev = h.immich.events();
    let last_resume = ev.iter().rposition(|e| e.starts_with("resume")).unwrap();
    let pause_after = ev
        .iter()
        .skip(last_resume)
        .filter(|e| e.starts_with("pause"))
        .count();
    assert_eq!(pause_after, 3, "{ev:?}");
    // The adapter queues again with the queues still paused.
    eventually("waiting again", || h.view_state() == "waiting").await;
    assert_eq!(h.queued_names().await, ["immich-ml"]);
    assert_eq!(h.docker.calls(&FakeDocker::up_line()), 1);
    drop(lease);
}

#[tokio::test]
async fn a_yield_with_a_small_leftover_resumes_the_queues_for_the_cpu() {
    let mut h = harness().await;
    h.immich.set_backlog(300);
    h.start(&[]);
    h.wait_serving().await;

    h.immich.set_backlog(20); // cpu_ok_backlog is 20: at or below resumes
    let lease = granted(urgent(&h)).await;
    assert_eq!(h.docker.calls(&FakeDocker::stop_line()), 1);
    eventually("queues resumed", || h.immich.none_paused()).await;
    assert!(h.immich.concurrency_is(&support::OLD_CONCURRENCY));
    eventually("state cleared", || h.state_paused() == Some(false)).await;
    // 20 jobs is below min_backlog and nothing is paused: the adapter stays out of it.
    sleep(Duration::from_millis(200)).await;
    assert!(h.queued_names().await.is_empty());
    assert_eq!(h.view_state(), "idle");
    drop(lease);
}

#[tokio::test]
async fn one_job_over_the_cpu_limit_stays_paused() {
    let mut h = harness().await;
    h.immich.set_backlog(300);
    h.start(&[]);
    h.wait_serving().await;
    h.immich.set_backlog(21);
    let lease = granted(urgent(&h)).await;
    assert!(h.immich.all_paused());
    assert_eq!(h.state_paused(), Some(true));
    drop(lease);
}

#[tokio::test]
async fn two_empty_polls_end_the_lease() {
    let mut h = harness().await;
    h.immich.set_backlog(300);
    h.start(&[]);
    h.wait_serving().await;

    h.immich.set_backlog(0);
    eventually("lease released", || {
        h.docker.calls(&FakeDocker::stop_line()) == 1
    })
    .await;
    let rig = h.rig.clone();
    eventually_async("lease gone", || async {
        rig.status().await.leases.is_empty()
    })
    .await;
    assert!(h.immich.concurrency_is(&support::OLD_CONCURRENCY));
    eventually("queues resumed", || h.immich.none_paused()).await;
    eventually("state cleared", || h.state_paused() == Some(false)).await;
    sleep(Duration::from_millis(200)).await;
    assert_eq!(h.docker.calls(&FakeDocker::up_line()), 1, "no second start");
    assert_eq!(h.view_state(), "idle");
}

#[tokio::test]
async fn one_empty_poll_is_not_enough() {
    let mut h = harness().await;
    h.immich.set_backlog(300);
    h.start(&[("poll_s", "0.2")]);
    h.wait_serving().await;
    h.immich.set_backlog(0);
    sleep(Duration::from_millis(250)).await;
    h.immich.set_backlog(5);
    sleep(Duration::from_millis(600)).await;
    assert_eq!(h.docker.calls(&FakeDocker::stop_line()), 0);
    assert!(h.has_lease().await);
}

// ------------------------------------------------------------------ the key

#[tokio::test]
async fn a_rejected_key_keeps_the_adapter_idle_until_the_file_changes() {
    let mut h = harness().await;
    h.immich.set_backlog(300);
    std::fs::write(&h.key_file, "stale\n").unwrap();
    h.start(&[]);
    eventually("the 401", || h.immich.unauthorized() >= 1).await;
    sleep(Duration::from_millis(400)).await;
    assert_eq!(h.immich.unauthorized(), 1, "the same key is not sent again");
    assert!(h.lease_names().await.is_empty());
    assert!(h.immich.events().is_empty());
    assert!(h.docker.compose_calls().is_empty());
    let err = h.view.json()["error"].as_str().unwrap().to_owned();
    assert!(err.contains("rejects"), "{err}");

    // Rotated: the new key is read on the next poll.
    std::fs::write(&h.key_file, "secret\n").unwrap();
    h.wait_serving().await;
    assert!(h.view.json()["error"].is_null());
}

#[tokio::test]
async fn a_missing_or_empty_key_file_keeps_the_adapter_idle() {
    let mut h = harness().await;
    h.immich.set_backlog(300);
    std::fs::remove_file(&h.key_file).unwrap();
    h.start(&[]);
    sleep(Duration::from_millis(300)).await;
    assert!(h.lease_names().await.is_empty());
    assert_eq!(h.immich.st.lock().unwrap().queue_gets, 0);
    assert_eq!(h.view.json()["error"], "no usable API key");

    std::fs::write(&h.key_file, "  \n").unwrap();
    sleep(Duration::from_millis(150)).await;
    assert_eq!(h.immich.st.lock().unwrap().queue_gets, 0);

    std::fs::write(&h.key_file, "secret").unwrap();
    h.wait_serving().await;
}

// ------------------------------------------------------------------ restarts

#[tokio::test]
async fn a_pause_left_by_a_previous_run_is_undone_when_the_backlog_is_gone() {
    let mut h = harness().await;
    std::fs::create_dir_all(h.state_file.parent().unwrap()).unwrap();
    std::fs::write(&h.state_file, r#"{"paused_by_gpuq": true}"#).unwrap();
    h.immich.set_paused(true);
    h.immich.set_backlog(0);
    assert!(h.immich.all_paused());
    h.start(&[]);
    eventually("queues resumed", || h.immich.none_paused()).await;
    eventually("state cleared", || h.state_paused() == Some(false)).await;
    assert!(h.lease_names().await.is_empty());
    assert!(h.docker.compose_calls().is_empty());
    assert_eq!(h.view.json()["paused_by_gpuq"], false);
}

#[tokio::test]
async fn a_pause_left_by_a_previous_run_with_a_small_backlog_still_asks_for_the_card() {
    let mut h = harness().await;
    std::fs::create_dir_all(h.state_file.parent().unwrap()).unwrap();
    std::fs::write(&h.state_file, r#"{"paused_by_gpuq": true}"#).unwrap();
    h.immich.set_paused(true);
    h.immich.set_backlog(10);
    h.start(&[]);
    h.wait_serving().await;
}

#[tokio::test]
async fn a_container_running_without_a_lease_is_stopped_at_startup() {
    let mut h = harness().await;
    h.docker.set_running(true);
    h.immich.set_backlog(0);
    h.start(&[]);
    eventually("stopped", || !h.docker.is_running()).await;
    assert_eq!(h.docker.calls(&FakeDocker::stop_line()), 1);
    assert!(h.lease_names().await.is_empty());
}

#[tokio::test]
async fn a_stopped_container_is_left_alone_at_startup() {
    let mut h = harness().await;
    h.immich.set_backlog(0);
    h.start(&[]);
    eventually("a poll", || h.immich.st.lock().unwrap().queue_gets >= 2).await;
    assert!(h.docker.compose_calls().is_empty());
}

// ------------------------------------------------------------------ giving up early

#[tokio::test]
async fn a_yield_during_the_health_wait_gives_up_and_stops_the_container() {
    let mut h = harness().await;
    h.immich.set_backlog(300);
    h.docker.set_health("starting");
    h.start(&[("health_timeout_s", "60")]);
    eventually("started", || h.docker.calls(&FakeDocker::up_line()) > 0).await;
    sleep(Duration::from_millis(100)).await;
    assert_eq!(h.view_state(), "starting");

    let t = Instant::now();
    let lease = granted(urgent(&h)).await;
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    assert_eq!(h.docker.calls(&FakeDocker::stop_line()), 1);
    assert!(!h.docker.is_running());
    assert_eq!(h.immich.config_put_count(), 0);
    assert!(h.immich.all_paused(), "the backlog is still big");
    drop(lease);
}

#[tokio::test]
async fn a_yield_during_the_settle_sleep_is_honoured_at_once() {
    let mut h = harness().await;
    h.immich.set_backlog(300);
    h.start(&[("settle_s", "30")]);
    eventually("started", || h.docker.calls(&FakeDocker::up_line()) > 0).await;
    sleep(Duration::from_millis(200)).await;
    assert_eq!(h.view_state(), "starting");

    let t = Instant::now();
    let lease = granted(urgent(&h)).await;
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    assert_eq!(h.docker.calls(&FakeDocker::stop_line()), 1);
    assert_eq!(h.immich.config_put_count(), 0);
    drop(lease);
}

#[tokio::test]
async fn a_container_that_never_gets_healthy_times_out() {
    let mut h = harness().await;
    h.immich.set_backlog(300);
    h.docker.set_health("unhealthy");
    h.start(&[("health_timeout_s", "0.3")]);
    eventually("stopped", || h.docker.calls(&FakeDocker::stop_line()) >= 1).await;
    assert_eq!(h.immich.config_put_count(), 0);
    assert!(h.immich.all_paused());
}

// ------------------------------------------------------------------ shutdown

#[tokio::test]
async fn shutdown_while_serving_stops_the_container_and_frees_the_card() {
    let mut h = harness().await;
    h.immich.set_backlog(300);
    h.start(&[]);
    h.wait_serving().await;
    h.shutdown().await;
    assert!(!h.docker.is_running());
    assert!(h.immich.concurrency_is(&support::OLD_CONCURRENCY));
    assert!(h.rig.status().await.leases.is_empty());
    assert!(h.immich.all_paused(), "300 jobs left for the next start");
}

#[tokio::test]
async fn shutdown_while_queued_leaves_the_queue_and_stops() {
    let rig = Rig::start_with(false, None, |_| {}).await;
    let _blocker = rig.lease("realtime", URGENT_MIB, false, "blocker").await;
    let mut h = Harness::prepare(rig).await;
    h.immich.set_backlog(300);
    h.start(&[]);
    eventually("queued", || h.view_state() == "waiting").await;
    h.shutdown().await;
    assert!(h.queued_names().await.is_empty());
    assert!(h.docker.compose_calls().is_empty());
}

// ------------------------------------------------------------------ status

#[tokio::test]
async fn the_status_object_has_four_fields_and_follows_the_states() {
    let mut h = harness().await;
    let v = h.view.json();
    assert_eq!(
        v,
        json!({"state": "idle", "backlog": null, "paused_by_gpuq": false, "error": null})
    );
    h.immich.set_backlog(300);
    h.start(&[]);
    h.wait_serving().await;
    let v = h.view.json();
    assert_eq!(v["state"], "serving");
    assert_eq!(v["backlog"], 300);
    assert_eq!(v["paused_by_gpuq"], false);
    assert!(v["error"].is_null());
    assert_eq!(v.as_object().unwrap().len(), 4);

    h.immich.set_backlog(300);
    h.immich.set_active(1);
    h.immich.set_drain_on_get(false);
    let blocker = urgent(&h);
    // Draining waits for the active job, which never ends here.
    eventually("draining", || h.view_state() == "draining").await;
    h.immich.set_active(0);
    drop(granted(blocker).await);
}

// ------------------------------------------------------------------ config

fn parse(extra: &str) -> Result<Config, String> {
    Config::from_toml_str(extra).map_err(|e| e.to_string())
}

const MINIMAL: &str = r#"
[immich]
url = "http://127.0.0.1:2283"
key_file = "/etc/gpuq/immich.key"
compose_dir = "/srv/immich"
service = "ml-gpu"
container = "ml_gpu"
"#;

#[test]
fn the_section_is_optional_and_off_by_default() {
    assert!(Config::default().immich.is_none());
    assert!(parse("").unwrap().immich.is_none());
}

#[test]
fn the_minimal_section_gets_the_documented_defaults() {
    let c = parse(&format!("[classes]\nimmich = 3\n{MINIMAL}"))
        .unwrap()
        .immich
        .unwrap();
    assert_eq!(c.compose_profile, "gpu");
    assert_eq!(c.docker, ["docker"]);
    assert_eq!(c.class, "immich".to_owned());
    assert_eq!(c.vram, "6G");
    assert_eq!(c.card, None);
    assert_eq!(c.poll, Duration::from_secs(30));
    assert_eq!((c.min_backlog, c.cpu_ok_backlog), (200, 50));
    assert_eq!(c.queues, ["smartSearch", "faceDetection", "ocr"]);
    assert_eq!(c.gpu_concurrency["smartSearch"], 8);
    assert_eq!(c.gpu_concurrency["faceDetection"], 4);
    assert_eq!(c.gpu_concurrency["ocr"], 2);
    assert_eq!(c.health_timeout, Duration::from_secs(600));
    assert_eq!(c.settle, Duration::from_secs(35));
    assert_eq!(c.drain_poll, Duration::from_secs(5));
}

#[test]
fn the_class_must_exist_so_the_default_needs_a_classes_entry() {
    let e = parse(MINIMAL).unwrap_err();
    assert!(e.contains("immich.class") && e.contains("[classes]"), "{e}");
    let ok = format!("[classes]\nimmich = 3\n{MINIMAL}");
    assert!(parse(&ok).is_ok());
    let other = MINIMAL.replace("service =", "class = \"batch\"\nservice =");
    assert!(parse(&other).is_ok());
}

#[test]
fn required_keys_are_required() {
    for key in ["url", "key_file", "compose_dir", "service", "container"] {
        let text: String = MINIMAL
            .lines()
            .filter(|l| !l.starts_with(&format!("{key} =")))
            .map(|l| format!("{l}\n"))
            .collect();
        let e = parse(&format!("[classes]\nimmich = 3\n{text}")).unwrap_err();
        assert!(e.contains(&format!("missing field `{key}`")), "{key}: {e}");
    }
}

#[test]
fn bad_values_are_reported() {
    let base = format!("[classes]\nimmich = 3\n{MINIMAL}");
    let with = |line: &str| format!("{base}{line}\n");
    let e = parse(&with("vram = \"lots\"")).unwrap_err();
    assert!(e.contains("immich.vram"), "{e}");
    // Detected cards are only known at run time; a [gpu] table pinned to one card is not.
    let pinned = format!("[gpu]\npci = \"0000:03:00.0\"\n{}", with("card = \"gpu9\""));
    let e = parse(&pinned).unwrap_err();
    assert!(e.contains("immich.card") && e.contains("gpu9"), "{e}");
    let e = parse(&with("url = \"nope\"").replace("url = \"http://127.0.0.1:2283\"\n", ""))
        .unwrap_err();
    assert!(e.contains("immich.url"), "{e}");
    let e = parse(&with("docker = []")).unwrap_err();
    assert!(e.contains("immich.docker"), "{e}");
    let e = parse(&with("queues = []")).unwrap_err();
    assert!(e.contains("immich.queues"), "{e}");
    let e = parse(&with("poll_s = 0")).unwrap_err();
    assert!(e.contains("immich.poll_s"), "{e}");
    let e = parse(&with("gpu_concurrency = { nosuch = 1 }")).unwrap_err();
    assert!(e.contains("nosuch"), "{e}");
    let e = parse(&with("polll_s = 1")).unwrap_err();
    assert!(e.contains("polll_s"), "{e}");
}

#[test]
fn card_names_a_configured_card() {
    let cards = r#"
[[gpu.card]]
name = "main"
vendor = "amd"
[[gpu.card]]
name = "second"
vendor = "amd"
"#;
    let text = |card: &str| format!("[classes]\nimmich = 3\n{cards}{MINIMAL}card = \"{card}\"\n");
    let c = parse(&text("second")).unwrap().immich.unwrap();
    assert_eq!(c.card.as_deref(), Some("second"));
    assert!(parse(&text("third")).is_err());
}

#[test]
fn the_state_file_defaults_follow_xdg_then_home() {
    let c = parse(&format!("[classes]\nimmich = 3\n{MINIMAL}"))
        .unwrap()
        .immich
        .unwrap();
    // One test owns the environment for this binary.
    std::env::set_var("XDG_STATE_HOME", "/xdg/state");
    assert_eq!(
        c.state_path().unwrap().to_str(),
        Some("/xdg/state/gpuq/immich.json")
    );
    std::env::remove_var("XDG_STATE_HOME");
    std::env::set_var("HOME", "/srv/someone");
    assert_eq!(
        c.state_path().unwrap().to_str(),
        Some("/srv/someone/.local/state/gpuq/immich.json")
    );
    let mut c = c;
    c.state_file = Some("/var/lib/gpuq/immich.json".into());
    assert_eq!(
        c.state_path().unwrap().to_str(),
        Some("/var/lib/gpuq/immich.json")
    );
}

#[test]
fn check_config_describes_the_section() {
    let c = parse(&format!("[classes]\nimmich = 3\n{MINIMAL}")).unwrap();
    let text = c.describe();
    let line = text.lines().find(|l| l.starts_with("immich:")).unwrap();
    assert!(line.contains("http://127.0.0.1:2283"), "{line}");
    assert!(line.contains("smartSearch"), "{line}");
    assert!(line.contains(">= 200") && line.contains("<= 50"), "{line}");
    assert!(!Config::default().describe().contains("immich:"));
}

#[test]
fn the_shipped_example_is_valid() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/immich.toml");
    let cfg = Config::load(Some(&path)).unwrap();
    let im = cfg.immich.unwrap();
    assert_eq!(im.class, "immich");
    assert!(cfg.classes.contains_key("immich"));
}
