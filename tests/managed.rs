// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The gpud authors

//! Generic managed services: lease on demand, start and stop through command scripts, idle
//! release, yield. Real sockets and short real time; the arbiter runs on in-memory fakes.

mod common;
#[path = "support_managed/mod.rs"]
mod support;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::Rig;
use support::{eventually, service, FakeService, Managed};

const VRAM: &str = "4000M";

async fn rig() -> Rig {
    Rig::start(|_| {}).await
}

/// A gated service named "tts" controlled by the fake's scripts.
async fn start(fake: &FakeService, tweak: impl FnOnce(&mut gpud::config::Service)) -> Managed {
    let mut svc = service("tts", &fake.url(""), VRAM);
    svc.control = Some(fake.spec(false));
    svc.gate.as_mut().unwrap().gpu_paths = vec!["/work".into()];
    tweak(&mut svc);
    Managed::start(rig().await, svc).await
}

#[tokio::test]
async fn first_request_takes_the_lease_starts_and_is_served() {
    let fake = FakeService::start().await;
    let m = start(&fake, |_| {}).await;
    assert!(!fake.is_up());
    let (code, body) = m.get("/work?ms=10").await;
    assert_eq!((code, body.as_str()), (200, "done"));
    assert!(fake.is_up());
    assert_eq!(fake.starts(), 1);
    assert_eq!(m.lease_names().await, ["tts"]);
}

#[tokio::test]
async fn second_request_reuses_the_lease() {
    let fake = FakeService::start().await;
    let m = start(&fake, |s| s.idle_release = Duration::from_secs(30)).await;
    assert_eq!(m.get("/work?ms=10").await.0, 200);
    assert_eq!(m.get("/work?ms=10").await.0, 200);
    assert_eq!(fake.starts(), 1);
    assert_eq!(m.lease_names().await, ["tts"]);
}

#[tokio::test]
async fn non_gpu_paths_pass_but_get_503_while_stopped() {
    let fake = FakeService::start().await;
    let m = start(&fake, |s| s.idle_release = Duration::from_secs(30)).await;
    let (code, body) = m.get("/status").await;
    assert_eq!(code, 503);
    assert!(body.contains("tts"), "{body}");
    assert_eq!(fake.status_hits.load(Ordering::SeqCst), 0);
    assert!(!fake.is_up());
    assert!(m.lease_names().await.is_empty());
    assert_eq!(m.get("/work").await.0, 200);
    assert_eq!(m.get("/status").await, (200, "ok".into()));
}

#[tokio::test]
async fn idle_release_stops_the_service_and_frees_the_card() {
    let fake = FakeService::start().await;
    let m = start(&fake, |s| s.idle_release = Duration::from_millis(300)).await;
    assert_eq!(m.get("/work").await.0, 200);
    m.wait_lease("tts", false).await;
    assert!(!fake.is_up());
    assert!(fake.stops() >= 1);
    // The next request wakes it again.
    assert_eq!(m.get("/work").await.0, 200);
    assert_eq!(fake.starts(), 2);
}

#[tokio::test]
async fn in_flight_requests_keep_the_lease() {
    let fake = FakeService::start().await;
    let m = start(&fake, |s| s.idle_release = Duration::from_millis(100)).await;
    let (code, _) = m.get("/work?ms=700").await;
    assert_eq!(code, 200);
    // The work ran longer than idle_release, yet it was never cut off.
    assert_eq!(fake.starts(), 1);
    assert_eq!(fake.stops(), 1, "only the stop at startup");
}

#[tokio::test]
async fn an_llm_request_makes_the_service_finish_stop_and_release() {
    let fake = FakeService::start().await;
    let m = start(&fake, |s| s.idle_release = Duration::from_secs(30)).await;
    assert_eq!(m.get("/work").await.0, 200);
    let slow = {
        let (http, url) = (m.http.clone(), m.url("/work?ms=400"));
        tokio::spawn(async move { http.get(url).send().await.unwrap().status().as_u16() })
    };
    eventually("slow request running", || {
        fake.active.load(Ordering::SeqCst) == 1
    })
    .await;
    let chat = m.rig.spawn_chat("big");
    // The in-flight request finishes; only then does the service stop.
    assert_eq!(slow.await.unwrap(), 200);
    assert_eq!(common::done(chat).await, 200);
    assert!(fake.stops() >= 1);
    assert!(!m.lease_names().await.contains(&"tts".to_string()));
    // A later request queues for the card again and is served.
    assert_eq!(m.get("/work").await.0, 200);
    assert_eq!(fake.starts(), 2);
}

#[tokio::test]
async fn requests_during_a_yield_are_held_for_the_next_lease() {
    let fake = FakeService::start().await;
    let m = start(&fake, |s| s.idle_release = Duration::from_secs(30)).await;
    assert_eq!(m.get("/work").await.0, 200);
    let slow = {
        let (http, url) = (m.http.clone(), m.url("/work?ms=300"));
        tokio::spawn(async move { http.get(url).send().await.unwrap().status().as_u16() })
    };
    eventually("slow request running", || {
        fake.active.load(Ordering::SeqCst) == 1
    })
    .await;
    let chat = m.rig.spawn_chat("big");
    m.rig
        .wait_for(|st, _| st.leases.iter().any(|l| l.yield_requested))
        .await;
    let late = {
        let (http, url) = (m.http.clone(), m.url("/work"));
        tokio::spawn(async move { http.get(url).send().await.unwrap().status().as_u16() })
    };
    assert_eq!(slow.await.unwrap(), 200);
    assert_eq!(late.await.unwrap(), 200);
    assert_eq!(common::done(chat).await, 200);
    // The late request never overlapped the slow one: it waited for a new lease.
    assert_eq!(fake.max_active.load(Ordering::SeqCst), 1);
    assert_eq!(fake.starts(), 2);
}

#[tokio::test]
async fn a_failed_start_gives_503_and_frees_the_lease() {
    let fake = FakeService::start().await;
    let m = start(&fake, |s| s.control = Some(fake.spec(true))).await;
    let (code, body) = m.get("/work").await;
    assert_eq!(code, 503);
    assert!(body.contains("did not start"), "{body}");
    assert!(m.lease_names().await.is_empty());
    // Backing off: another request is refused at once, with the reason.
    let (code, body) = m.get("/work").await;
    assert_eq!(code, 503);
    assert!(body.contains("did not start"), "{body}");
    assert_eq!(fake.starts(), 1);
}

#[tokio::test]
async fn busy_url_keeps_the_lease_while_it_reports_work() {
    let fake = FakeService::start().await;
    let busy_url = fake.url("/busy");
    let m = start(&fake, |s| {
        s.idle_release = Duration::from_millis(200);
        s.gate.as_mut().unwrap().busy_url = Some(busy_url);
    })
    .await;
    fake.busy.store(true, Ordering::SeqCst);
    assert_eq!(m.get("/work").await.0, 200);
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(m.lease_names().await, ["tts"]);
    assert!(fake.is_up());
    fake.busy.store(false, Ordering::SeqCst);
    m.wait_lease("tts", false).await;
    assert!(!fake.is_up());
}

#[tokio::test]
async fn without_gpu_paths_every_request_needs_the_card() {
    let fake = FakeService::start().await;
    let m = start(&fake, |s| s.gate.as_mut().unwrap().gpu_paths.clear()).await;
    assert_eq!(m.get("/status").await, (200, "ok".into()));
    assert_eq!(fake.starts(), 1);
}

#[tokio::test]
async fn a_service_without_a_gate_holds_its_lease_and_yields_when_asked() {
    let fake = FakeService::start().await;
    let mut svc = service("indexer", &fake.url(""), VRAM);
    svc.gate = None;
    svc.control = Some(fake.spec(false));
    let m = Managed::start(rig().await, svc).await;
    m.wait_lease("indexer", true).await;
    eventually("service up", || fake.is_up()).await;
    // An LLM request wants the card: the service stops and the lease goes.
    let chat = m.rig.spawn_chat("big");
    assert_eq!(common::done(chat).await, 200);
    assert!(fake.stops() >= 1);
    // It queues again afterwards and comes back once the card is free.
    m.wait_lease("indexer", true).await;
    eventually("service up again", || fake.is_up()).await;
    assert_eq!(fake.starts(), 2);
}
