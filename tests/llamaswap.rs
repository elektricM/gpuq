// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The gpud authors

//! The llama-swap client against the fake server.

#[path = "support/fake_llamaswap.rs"]
mod fake_llamaswap;

use std::time::{Duration, Instant};

use fake_llamaswap::{FakeLlamaSwap, MODELS_YAML};
use gpud::backend::llamaswap::LlamaSwap;
use gpud::backend::models::ModelTable;
use gpud::backend::{Backend, BackendError};

struct Rig {
    fake: FakeLlamaSwap,
    ls: LlamaSwap,
    _dir: tempfile::TempDir,
}

async fn rig(load_timeout_ms: u64) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("llama-swap.yaml");
    std::fs::write(&path, MODELS_YAML).unwrap();
    let fake = FakeLlamaSwap::start().await;
    let ls = LlamaSwap::new(
        &fake.url(),
        ModelTable::new(&path),
        Duration::from_millis(load_timeout_ms),
    )
    .unwrap()
    .with_poll_interval(Duration::from_millis(30));
    Rig {
        fake,
        ls,
        _dir: dir,
    }
}

#[tokio::test]
async fn capabilities_and_names() {
    let r = rig(1000).await;
    let c = r.ls.capabilities();
    assert!(c.unload && c.slots && c.health);
    assert_eq!(r.ls.kind(), "llama-swap");
    assert_eq!(r.ls.resolve("other-alias"), "other");
    assert!(!r.ls.uses_gpu("embed"));
}

#[tokio::test]
async fn running_and_loaded_gpu_models() {
    let r = rig(1000).await;
    r.fake.preload("big");
    r.fake.preload("embed");
    let running = r.ls.running().await.unwrap();
    assert_eq!(running.len(), 2);
    assert!(running.iter().all(|m| m.state == "ready"));
    // The CPU model is running but does not count.
    assert_eq!(r.ls.loaded_gpu_models().await.unwrap(), vec!["big"]);
}

#[tokio::test]
async fn unload_all_unloads() {
    let r = rig(1000).await;
    r.fake.preload("big");
    r.ls.unload_all().await.unwrap();
    assert!(r.fake.loaded().is_empty());
    assert!(r.fake.has_event("unload"));
}

#[tokio::test]
async fn slot_save_and_restore_round_trip() {
    let r = rig(1000).await;
    r.fake.preload("big");
    r.fake.lock().slot_tokens = 300;
    assert!(r.ls.save_slot("big").await.unwrap());
    assert_eq!(r.fake.lock().saved.get("big.slot0.bin"), Some(&300));
    r.fake.lock().slot_tokens = 0;
    assert!(r.ls.restore_slot("big").await.unwrap());
    assert_eq!(r.fake.lock().slot_tokens, 300);
    assert!(r.fake.has_event("restore 300"));
    // Restoring twice makes no second request.
    assert!(!r.ls.restore_slot("big").await.unwrap());
}

#[tokio::test]
async fn slot_calls_skip_models_without_slot_save() {
    let r = rig(1000).await;
    r.fake.preload("other");
    assert!(!r.ls.save_slot("other").await.unwrap());
    assert!(!r.ls.restore_slot("other").await.unwrap());
    assert!(!r.fake.events().iter().any(|e| e.starts_with("save")));
}

#[tokio::test]
async fn empty_slot_is_not_saved() {
    let r = rig(1000).await;
    r.fake.preload("big");
    assert!(!r.ls.save_slot("big").await.unwrap());
    assert!(!r.ls.restore_slot("big").await.unwrap());
}

#[tokio::test]
async fn slot_failure_is_not_fatal() {
    let r = rig(1000).await;
    r.fake.preload("big");
    r.fake.lock().slot_tokens = 10;
    r.fake.lock().slots_fail = true;
    assert!(!r.ls.save_slot("big").await.unwrap());
}

#[tokio::test]
async fn load_waits_for_health() {
    let r = rig(2000).await;
    r.fake.lock().health_503s = 3;
    assert!(r.ls.load("other-alias").await);
    assert!(r.fake.is_loaded("other"));
    assert_eq!(r.fake.lock().health_calls, 4);
}

#[tokio::test]
async fn load_gives_up_at_the_deadline() {
    let r = rig(300).await;
    r.fake.lock().health_503s = 1000;
    let t = Instant::now();
    assert!(!r.ls.load("big").await);
    let took = t.elapsed();
    assert!(
        took >= Duration::from_millis(280),
        "returned early: {took:?}"
    );
    assert!(took < Duration::from_millis(900), "overran: {took:?}");
    assert!(r.fake.lock().health_calls > 2);
}

#[tokio::test]
async fn a_hanging_request_is_cut_at_the_deadline() {
    let r = rig(300).await;
    r.fake.lock().hang_health = true;
    let t = Instant::now();
    assert!(!r.ls.load("big").await);
    let took = t.elapsed();
    assert!(took < Duration::from_millis(900), "overran: {took:?}");
}

#[tokio::test]
async fn unreachable_backend() {
    let dir = tempfile::tempdir().unwrap();
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let ls = LlamaSwap::new(
        &format!("http://127.0.0.1:{port}"),
        ModelTable::new(dir.path().join("none.yaml")),
        Duration::from_millis(200),
    )
    .unwrap()
    .with_poll_interval(Duration::from_millis(30));
    assert!(matches!(
        ls.loaded_gpu_models().await,
        Err(BackendError::Unreachable(_))
    ));
    assert!(ls.unload_all().await.is_err());
    assert!(!ls.load("big").await);
}
