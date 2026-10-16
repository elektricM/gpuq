// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! The ComfyUI flavor against a fake ComfyUI. Ports tests/test_comfyui.py.

mod common;
#[path = "support_managed/mod.rs"]
mod support;

use std::time::{Duration, Instant};

use common::Rig;
use gpuq::config::{Flavor, ServiceKind, ServiceSpec};
use serde_json::{json, Value};
use support::{eventually, service, FakeComfy, Managed};

/// ComfyUI gate with a fake behind it. `idle` is how long the card is kept after the last job.
async fn setup(render_ms: u64, idle_ms: u64) -> (Managed, FakeComfy) {
    setup_with(render_ms, idle_ms, |_| {}).await
}

async fn setup_with(
    render_ms: u64,
    idle_ms: u64,
    tweak: impl FnOnce(&mut gpuq::config::Service),
) -> (Managed, FakeComfy) {
    let comfy = FakeComfy::start(Duration::from_millis(render_ms)).await;
    let mut svc = service("comfyui", &comfy.url(), "20000M");
    svc.flavor = Flavor::Comfyui;
    svc.idle_release = Duration::from_millis(idle_ms);
    tweak(&mut svc);
    let m = Managed::start(Rig::start(|_| {}).await, svc).await;
    (m, comfy)
}

async fn post_prompt(m: &Managed, prompt: Value, extra: Value) -> String {
    let r = m
        .http
        .post(m.url("/prompt"))
        .json(&json!({"prompt": prompt, "extra_data": extra}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["node_errors"], json!({}));
    v["prompt_id"].as_str().unwrap().to_owned()
}

async fn json_of(m: &Managed, path: &str) -> Value {
    m.http
        .get(m.url(path))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn answers_at_once_and_renders_when_the_llm_pauses() {
    let (m, comfy) = setup(300, 400).await;
    assert_eq!(m.rig.chat("big").await, 200); // the LLM was just used
    let t = Instant::now();
    let pid = post_prompt(&m, json!({"1": {}}), json!({})).await;
    assert!(t.elapsed() < Duration::from_secs(1)); // the UI is answered at once
    eventually("prompt submitted", || comfy.ids().contains(&pid)).await;
    assert!(!m.rig.card.is_loaded("big")); // the card is the render's now
    let waiting = m.rig.spawn_chat("big"); // the LLM waits for the render
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(!waiting.is_finished());
    eventually("models freed", || !comfy.freed().is_empty()).await;
    m.wait_lease("comfyui", false).await;
    assert_eq!(common::done(waiting).await, 200);
    assert_eq!(
        comfy.freed(),
        [json!({"unload_models": true, "free_memory": true})]
    );
}

#[tokio::test]
async fn hands_the_card_to_a_waiting_llm_between_renders() {
    let (m, comfy) = setup(500, 30_000).await;
    let first = post_prompt(&m, json!({"1": {}}), json!({})).await;
    eventually("first submitted", || comfy.ids().contains(&first)).await;
    let waiting = m.rig.spawn_chat("big"); // arrives during the render
    m.rig
        .wait_for(|st, _| st.leases.iter().any(|l| l.yield_requested))
        .await;
    let second = post_prompt(&m, json!({"2": {}}), json!({})).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!comfy.ids().contains(&second)); // held: the LLM goes first
    assert_eq!(common::done(waiting).await, 200);
    assert!(!comfy.ids().contains(&second));
    eventually("second submitted", || comfy.ids().contains(&second)).await;
}

#[tokio::test]
async fn takes_queued_prompts_back_for_a_waiting_llm() {
    let (m, comfy) = setup(500, 30_000).await;
    let secret = json!({"api_key_comfy_org": "k"}); // ComfyUI's /queue hides it
    let mut pids = Vec::new();
    for i in 0..2 {
        let pid = post_prompt(&m, json!({ i.to_string(): {} }), secret.clone()).await;
        eventually("submitted", || comfy.ids().contains(&pid)).await;
        pids.push(pid);
    }
    assert_eq!(comfy.st.lock().unwrap().queue, [pids[1].clone()]);
    let waiting = m.rig.spawn_chat("big");
    assert_eq!(common::done(waiting).await, 200); // served after render 1 only
    assert_eq!(comfy.count(&pids[1]), 1); // taken back, not rerun yet
    eventually("resubmitted", || comfy.count(&pids[1]) == 2).await;
    let body = comfy.st.lock().unwrap().bodies[&pids[1]].clone();
    assert_eq!(body["extra_data"], secret);
}

fn weights(file: &str) -> Value {
    json!({"1": {"class_type": "Loader", "inputs": {"unet_name": file}}})
}

#[tokio::test]
async fn frees_models_when_the_workflow_switches_models() {
    let (m, comfy) = setup(100, 30_000).await;
    let mut pids = Vec::new();
    for f in [
        "a.safetensors",
        "a.safetensors",
        "b.gguf",
        "b.gguf",
        "a.safetensors",
    ] {
        pids.push(post_prompt(&m, weights(f), json!({})).await);
    }
    eventually("all submitted", || comfy.ids().len() == 5).await;
    assert_eq!(comfy.ids(), pids); // order kept
    assert_eq!(comfy.freed().len(), 2);
    assert!(comfy
        .freed()
        .iter()
        .all(|f| *f == json!({"unload_models": true})));
    // A switching prompt only went in once ComfyUI had finished everything before it.
    let busy = comfy.st.lock().unwrap().busy_at_submit.clone();
    assert_eq!([busy[2], busy[4]], [false, false]);
}

#[tokio::test]
async fn restarts_the_service_instead_of_freeing_when_it_has_control() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log");
    let script = |word: &str| {
        vec![
            "sh".to_string(),
            "-c".into(),
            format!("echo {word} >> {}", log.display()),
        ]
    };
    let (m, comfy) = setup_with(50, 30_000, |svc| {
        svc.control = Some(ServiceSpec {
            kind: ServiceKind::Command {
                start: script("start"),
                stop: script("stop"),
                status: None,
            },
            health_url: None,
            start_timeout: Duration::from_secs(3),
        });
    })
    .await;
    let a = post_prompt(&m, weights("a.safetensors"), json!({})).await;
    let b = post_prompt(&m, weights("b.safetensors"), json!({})).await;
    eventually("both submitted", || comfy.ids() == [a.clone(), b.clone()]).await;
    assert!(comfy.freed().is_empty());
    // stop at startup, start for the lease, then stop and start for the model switch.
    let lines = std::fs::read_to_string(&log).unwrap();
    assert_eq!(
        lines.lines().collect::<Vec<_>>(),
        ["stop", "start", "stop", "start"]
    );
}

#[tokio::test]
async fn held_prompts_are_visible_and_cancellable() {
    let (m, comfy) = setup(300, 400).await;
    let blocker = m.rig.lease("realtime", 22000, false, "blocker").await;
    let pid = post_prompt(&m, json!({"1": {}}), json!({"create_time": 5})).await;
    let jobs = json_of(&m, "/api/jobs").await;
    assert_eq!(jobs["jobs"][0]["id"], pid.as_str());
    assert_eq!(jobs["jobs"][0]["status"], "pending");
    assert_eq!(jobs["pagination"]["total"], 1);
    let job = json_of(&m, &format!("/api/jobs/{pid}")).await;
    assert_eq!(job["create_time"], 5);
    let queue = json_of(&m, "/queue").await;
    assert_eq!(queue["queue_pending"][0][1], pid.as_str());
    let waiting = |st: &gpuq::arbiter::Status| st.queue.iter().any(|w| w.name == "comfyui");
    m.rig.wait_for(|st, _| waiting(st)).await;
    let r = m
        .http
        .post(m.url(&format!("/api/jobs/{pid}/cancel")))
        .send()
        .await
        .unwrap();
    assert_eq!(r.json::<Value>().await.unwrap(), json!({"cancelled": true}));
    m.rig.wait_for(|st, _| !waiting(st)).await;
    assert_eq!(json_of(&m, "/queue").await["queue_pending"], json!([]));
    drop(blocker);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(comfy.ids().is_empty()); // never submitted
}

#[tokio::test]
async fn queue_delete_and_clear_cancel_held_prompts() {
    let (m, comfy) = setup(300, 400).await;
    let blocker = m.rig.lease("realtime", 22000, false, "blocker").await;
    let a = post_prompt(&m, json!({"1": {}}), json!({})).await;
    let b = post_prompt(&m, json!({"2": {}}), json!({})).await;
    let c = post_prompt(&m, json!({"3": {}}), json!({})).await;
    let post = |body: Value| m.http.post(m.url("/queue")).json(&body).send();
    assert_eq!(post(json!({"delete": [a]})).await.unwrap().status(), 200);
    let queue = json_of(&m, "/queue").await;
    let ids: Vec<&str> = queue["queue_pending"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p[1].as_str().unwrap())
        .collect();
    assert_eq!(ids, [b.as_str(), c.as_str()]);
    assert_eq!(post(json!({"clear": true})).await.unwrap().status(), 200);
    assert_eq!(json_of(&m, "/queue").await["queue_pending"], json!([]));
    drop(blocker);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(comfy.ids().is_empty());
}

#[tokio::test]
async fn rejected_submissions_show_up_as_failed_jobs() {
    let (m, _comfy) = setup(50, 400).await;
    let pid = post_prompt(&m, json!({"bad": {}}), json!({})).await;
    let end = Instant::now() + Duration::from_secs(5);
    let job = loop {
        let job = json_of(&m, &format!("/api/jobs/{pid}")).await;
        if job["status"] == "failed" || Instant::now() > end {
            break job;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(job["status"], "failed");
    assert!(job["execution_error"]["exception_message"]
        .as_str()
        .unwrap()
        .contains("invalid prompt"));
    let jobs = json_of(&m, "/api/jobs").await;
    assert_eq!(jobs["jobs"][0]["id"], pid.as_str());
}
