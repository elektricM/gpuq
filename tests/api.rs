// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! Control API over a real socket, in front of the arbiter and the in-memory fakes.
//! Ported from tests/test_api.py. Real time (sockets), with a short ping interval.

mod support_api;

use std::time::{Duration, Instant};

use serde_json::{json, Value};
use support_api::*;

const PING: Duration = Duration::from_millis(150);

async fn api() -> TestApi {
    TestApi::start(PING).await
}

#[tokio::test]
async fn lease_released_when_connection_closes() {
    let api = api().await;
    let mut ev = api
        .lease_ok(json!({"class": "batch", "vram_mib": 1000, "name": "api"}))
        .await;
    let seen: Vec<_> = ev
        .until("granted")
        .await
        .iter()
        .map(|v| v["event"].as_str().unwrap().to_owned())
        .filter(|e| e != "ping")
        .collect();
    assert_eq!(seen, ["queued", "granted"]);
    assert_eq!(api.rig.status().await.leases.len(), 1);
    let t = Instant::now();
    ev.close();
    assert!(
        status_within(&api.rig, PING * 3, |s| s.leases.is_empty()).await,
        "lease still held {:?} after the client left",
        t.elapsed()
    );
}

#[tokio::test]
async fn yield_event_is_sent_to_a_yieldable_lease() {
    let api = api().await;
    let mut ev = api
        .lease_ok(json!({"class": "batch", "vram_mib": 1000, "yieldable": true, "name": "api"}))
        .await;
    ev.granted().await;
    let chat = api.rig.spawn_chat("big");
    ev.until("yield").await;
    ev.close();
    assert_eq!(done(chat).await, 200);
}

#[tokio::test]
async fn yield_is_sent_once() {
    let api = api().await;
    let mut ev = api
        .lease_ok(json!({"class": "batch", "vram_mib": 1000, "yieldable": true}))
        .await;
    ev.granted().await;
    let chat = api.rig.spawn_chat("big");
    ev.until("yield").await;
    // Several pings later there is still no second yield.
    for _ in 0..3 {
        assert_eq!(ev.event().await, "ping");
    }
    ev.close();
    assert_eq!(done(chat).await, 200);
}

#[tokio::test]
async fn lease_is_released_promptly_when_client_vanishes_after_a_yield() {
    let api = api().await;
    let mut ev = api
        .lease_ok(json!({"class": "batch", "vram_mib": 1000, "yieldable": true, "name": "api"}))
        .await;
    ev.granted().await;
    let chat = api.rig.spawn_chat("big");
    ev.until("yield").await;
    ev.close();
    assert!(status_within(&api.rig, PING * 3, |s| s.leases.is_empty()).await);
    assert_eq!(done(chat).await, 200);
}

#[tokio::test]
async fn unknown_class_is_rejected() {
    let api = api().await;
    let (status, body) = api.post_raw(r#"{"class": "nope", "vram_mib": 100}"#).await;
    assert_eq!(status, 400);
    assert_eq!(
        body["error"],
        "unknown class 'nope'; known: realtime, llm, interactive, background, batch"
    );
    assert!(api.rig.status().await.leases.is_empty());
}

#[tokio::test]
async fn malformed_request_is_rejected() {
    let api = api().await;
    for raw in [
        "not json",
        r#"{"vram_mib": "lots"}"#,
        r#"{"vram_mib": -5}"#,
        r#"{"vram": "lots"}"#,
        r#"{"vram": true}"#,
        r#"{"class": 3}"#,
        r#"{"pid": "x"}"#,
        "[1, 2]",
        "",
    ] {
        let (status, body) = api.post_raw(raw).await;
        assert_eq!(status, 400, "{raw:?}");
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .starts_with("bad lease request: "),
            "{raw:?}: {body}"
        );
    }
}

#[tokio::test]
async fn status_endpoint() {
    let api = api().await;
    let (code, text) = api.get("/status").await;
    assert_eq!(code, 200);
    let st: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(st["state"], "llm");
    assert_eq!(st["device"], "fake:0");
    assert_eq!(st["backend"]["models"][0]["model"], "big");
    assert!(st["vram"]["total_mib"].as_u64().unwrap() > 0);
    assert!(st["leases"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn healthz_and_unknown_routes() {
    let api = api().await;
    assert_eq!(api.get("/healthz").await, (200, "ok".to_owned()));
    assert_eq!(api.get("/nope").await.0, 404);
    assert_eq!(api.get("/lease").await.0, 405);
}

#[tokio::test]
async fn vram_strings_and_whole_card() {
    let api = api().await;
    let mut ev = api
        .lease_ok(json!({"class": "batch", "vram": "6G", "name": "str"}))
        .await;
    ev.granted().await;
    let st = api.rig.status().await;
    assert_eq!(st.leases[0].vram.to_string(), "6144MiB");
    assert_eq!(st.leases[0].name, "str");
    ev.close();
    assert!(status_within(&api.rig, PING * 3, |s| s.leases.is_empty()).await);

    let mut ev = api
        .lease_ok(json!({"class": "batch", "vram": "all", "pid": 4242}))
        .await;
    ev.granted().await;
    let st = api.rig.status().await;
    assert_eq!(st.leases[0].vram.to_string(), "all");
    assert_eq!(st.leases[0].pid, Some(4242));
    ev.close();

    let mut ev = api.lease_ok(json!({"vram_mib": -1})).await;
    ev.granted().await;
    assert_eq!(api.rig.status().await.leases[0].class, "batch");
}

#[tokio::test]
async fn pings_arrive_while_queued_and_while_holding() {
    let api = api().await;
    let mut first = api
        .lease_ok(json!({"vram_mib": 1000, "name": "first"}))
        .await;
    first.granted().await;
    // The whole card cannot be granted next to the first lease, so this one waits.
    let mut second = api.lease_ok(json!({"vram": "all", "name": "second"})).await;
    assert_eq!(second.event().await, "queued");
    let t = Instant::now();
    assert_eq!(second.event().await, "ping");
    assert_eq!(second.event().await, "ping");
    assert!(
        t.elapsed() >= PING,
        "pings came too fast: {:?}",
        t.elapsed()
    );
    assert!(t.elapsed() < PING * 6);
    first.close();
    let seen = second.until("granted").await;
    assert_eq!(seen.last().unwrap()["event"], "granted");
    // And pings keep coming while holding.
    assert_eq!(second.event().await, "ping");
    assert_eq!(second.event().await, "ping");
}

#[tokio::test]
async fn client_leaving_while_queued_leaves_the_queue() {
    let api = api().await;
    let mut first = api
        .lease_ok(json!({"vram_mib": 1000, "name": "first"}))
        .await;
    first.granted().await;
    let mut second = api.lease_ok(json!({"vram": "all", "name": "second"})).await;
    assert_eq!(second.event().await, "queued");
    assert!(status_within(&api.rig, Duration::from_secs(2), |s| s.queue.len() == 1).await);
    second.close();
    assert!(
        status_within(&api.rig, PING * 3, |s| s.queue.is_empty()).await,
        "waiter still queued after the client left"
    );
    assert_eq!(api.rig.status().await.leases.len(), 1);
}

#[tokio::test]
async fn resumed_lease_keeps_its_id() {
    let api = api().await;
    let mut ev = api
        .lease_ok(json!({"class": "batch", "vram_mib": 1000, "name": "old", "resume_id": "abc123"}))
        .await;
    assert_eq!(ev.granted().await, "abc123");
    let st = api.rig.status().await;
    assert_eq!(st.leases.len(), 1);
    assert_eq!(st.leases[0].id, "abc123");
    ev.close();
    assert!(status_within(&api.rig, PING * 3, |s| s.leases.is_empty()).await);
}

#[tokio::test]
async fn open_leases_are_released_when_the_server_stops() {
    let mut api = api().await;
    let mut ev = api.lease_ok(json!({"vram_mib": 1000})).await;
    ev.granted().await;
    api.stop().await;
    assert!(status_within(&api.rig, Duration::from_secs(1), |s| s.leases.is_empty()).await);
    assert!(ev.next().await.is_none() || ev.next().await.is_none());
}

/// With a ping a minute away, the disconnect must still be noticed through the socket itself.
#[tokio::test]
async fn disconnect_is_noticed_between_pings() {
    let api = TestApi::start(Duration::from_secs(60)).await;
    let mut held = api.lease_ok(json!({"vram_mib": 1000})).await;
    held.granted().await;
    let mut queued = api.lease_ok(json!({"vram": "all"})).await;
    assert_eq!(queued.event().await, "queued");
    assert!(status_within(&api.rig, Duration::from_secs(2), |s| s.queue.len() == 1).await);
    queued.close();
    assert!(status_within(&api.rig, Duration::from_secs(1), |s| s.queue.is_empty()).await);
    held.close();
    assert!(status_within(&api.rig, Duration::from_secs(1), |s| s.leases.is_empty()).await);
}

// ---------------------------------------------------------------------- cards

#[tokio::test]
async fn a_single_card_grant_names_the_card_and_has_an_empty_env() {
    let api = api().await;
    let mut ev = api.lease_ok(json!({"vram_mib": 1000})).await;
    let granted = ev.until("granted").await.pop().unwrap();
    assert_eq!(granted["card"], "gpu0", "{granted}");
    assert_eq!(granted["env"], json!({}), "{granted}");
    ev.close();
    let mut ev = api
        .lease_ok(json!({"vram_mib": 1000, "card": "gpu0"}))
        .await;
    assert_eq!(ev.until("granted").await.pop().unwrap()["card"], "gpu0");
    ev.close();
    let mut ev = api.lease_ok(json!({"vram_mib": 1000, "card": null})).await;
    assert_eq!(ev.until("granted").await.pop().unwrap()["card"], "gpu0");
}

#[tokio::test]
async fn unknown_card_is_a_400_listing_the_known_ones() {
    let (api, _) = TestApi::start_two_cards(&[], &[], PING).await;
    let r = client()
        .post(api.url("/lease"))
        .body(json!({"card": "x"}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"], "unknown card 'x'; known: gpu0, gpu1");
    assert!(api.rig.status().await.queue.is_empty());
}

#[tokio::test]
async fn any_without_a_steerable_card_is_a_400() {
    let (two, _) = TestApi::start_two_cards(&[], &[], PING).await;
    let r = client()
        .post(two.url("/lease"))
        .body(json!({"card": "any"}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let v: Value = r.json().await.unwrap();
    assert!(
        v["error"].as_str().unwrap().contains("visible_devices"),
        "{v}"
    );
    // With one card there is nothing to steer, so "any" is fine.
    let single = api().await;
    let mut ev = single.lease_ok(json!({"card": "any"})).await;
    assert_eq!(ev.until("granted").await.pop().unwrap()["card"], "gpu0");
}

#[tokio::test]
async fn card_must_be_a_name() {
    let api = api().await;
    let (status, v) = api.post_raw(r#"{"card": 3}"#).await;
    assert_eq!(status, 400);
    assert!(v["error"].as_str().unwrap().contains("card"), "{v}");
    let (status, _) = api.post_raw(r#"{"card": ""}"#).await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn a_lease_pinned_to_gpu1_is_granted_there_with_its_env() {
    let (api, _gpu1) = TestApi::start_two_cards(
        &[("CUDA_VISIBLE_DEVICES", "0")],
        &[("CUDA_VISIBLE_DEVICES", "1")],
        PING,
    )
    .await;
    let mut ev = api
        .lease_ok(json!({"vram_mib": 1000, "card": "gpu1"}))
        .await;
    let granted = ev.until("granted").await.pop().unwrap();
    assert_eq!(granted["card"], "gpu1", "{granted}");
    assert_eq!(
        granted["env"],
        json!({"CUDA_VISIBLE_DEVICES": "1"}),
        "{granted}"
    );
    let st = api.rig.status().await;
    assert_eq!(st.leases[0].card, "gpu1");
    // Re-adopting with the same card keeps it there.
    let id = granted["id"].as_str().unwrap().to_owned();
    ev.close();
    let mut again = api
        .lease_ok(json!({"vram_mib": 1000, "card": "gpu1", "resume_id": id}))
        .await;
    assert_eq!(again.until("granted").await.pop().unwrap()["card"], "gpu1");
}
