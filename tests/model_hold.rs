// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! `hold_other_models`: a class keeps the LLM backend on its model while it works, so a less
//! important class asking for another model waits instead of making the server swap.

mod common;

use common::*;
use gpuq::arbiter::{LlmGuard, WaiterKind};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

/// `realtime` (priority 0) holds other models for 2 s after its last request.
async fn rig() -> Rig {
    Rig::start(|s| {
        s.tuning
            .class_tuning
            .get_mut("realtime")
            .unwrap()
            .hold_other_models = Some(secs(2.0));
    })
    .await
}

async fn enter(rig: &Rig, model: &str, class: &str) -> LlmGuard {
    timeout(secs(0.5), rig.handle.enter_llm(model, class))
        .await
        .unwrap_or_else(|_| panic!("{class} request for {model} was held"))
        .unwrap()
}

fn spawn_enter(rig: &Rig, model: &str, class: &str) -> JoinHandle<LlmGuard> {
    let (h, model, class) = (rig.handle.clone(), model.to_owned(), class.to_owned());
    tokio::spawn(async move { h.enter_llm(&model, &class).await.unwrap() })
}

#[tokio::test(start_paused = true)]
async fn the_same_model_is_never_held() {
    let rig = rig().await;
    let rec = enter(&rig, "rec", "realtime").await;
    let other = enter(&rig, "rec", "llm").await;
    drop((rec, other));
    sleep(secs(0.1)).await;
    let _again = enter(&rig, "rec", "llm").await;
}

#[tokio::test(start_paused = true)]
async fn another_model_waits_until_the_class_has_been_quiet() {
    let rig = rig().await;
    let rec = enter(&rig, "rec", "realtime").await;
    let held = spawn_enter(&rig, "big", "llm");
    sleep(secs(1.0)).await;
    assert!(
        !held.is_finished(),
        "held while realtime has a request in flight"
    );
    let st = rig.status().await;
    let h = st.model_hold.expect("hold shown in status");
    assert_eq!(
        (h.behind_class.as_str(), h.behind_model.as_str()),
        ("realtime", "rec")
    );
    assert_eq!((h.held, h.models), (1, vec!["big".to_owned()]));
    let q = &st.queue[0];
    assert_eq!(q.kind, WaiterKind::Llm);
    assert!(q.why.contains("held while realtime uses rec"), "{}", q.why);
    drop(rec);
    // Quiet for 2 s, not before: the next section may come.
    sleep(secs(1.9)).await;
    assert!(
        !held.is_finished(),
        "released before the class was quiet for 2 s"
    );
    sleep(secs(0.2)).await;
    let _big = timeout(secs(0.1), held)
        .await
        .expect("released after 2 s quiet")
        .unwrap();
    assert!(rig.status().await.model_hold.is_none());
}

#[tokio::test(start_paused = true)]
async fn a_new_request_of_the_class_extends_the_hold() {
    let rig = rig().await;
    drop(enter(&rig, "rec", "realtime").await);
    let held = spawn_enter(&rig, "big", "llm");
    sleep(secs(1.5)).await;
    drop(enter(&rig, "rec", "realtime").await); // the next part of the notes
    sleep(secs(1.5)).await;
    assert!(
        !held.is_finished(),
        "the hold restarts at each request of the class"
    );
    sleep(secs(0.6)).await;
    assert!(held.is_finished());
}

#[tokio::test(start_paused = true)]
async fn more_important_and_equal_classes_are_never_held() {
    // llm holds other models too; realtime (more important) and llm itself still pass.
    let rig = Rig::start(|s| {
        s.tuning.class_tuning.insert(
            "llm".into(),
            gpuq::arbiter::ClassTuning {
                hold_other_models: Some(secs(60.0)),
                ..Default::default()
            },
        );
    })
    .await;
    let _big = enter(&rig, "big", "llm").await;
    let _rec = enter(&rig, "rec", "realtime").await;
    let _other = enter(&rig, "other", "llm").await;
    // Less important classes wait.
    let held = spawn_enter(&rig, "rec", "background");
    sleep(secs(1.0)).await;
    assert!(!held.is_finished());
    held.abort();
}

#[tokio::test(start_paused = true)]
async fn classes_without_the_setting_hold_nothing() {
    let rig = Rig::start(|_| {}).await;
    let _rec = enter(&rig, "rec", "realtime").await;
    let _big = enter(&rig, "big", "llm").await;
    assert!(rig.status().await.model_hold.is_none());
}

#[tokio::test(start_paused = true)]
async fn one_line_when_holding_starts_and_one_when_it_ends() {
    let rig = rig().await;
    let rec = enter(&rig, "rec", "realtime").await;
    let held: Vec<_> = (0..3).map(|_| spawn_enter(&rig, "big", "llm")).collect();
    sleep(secs(0.5)).await;
    drop(rec);
    for h in held {
        let _g = timeout(secs(5.0), h).await.expect("released").unwrap();
    }
    let lines: Vec<String> = rig
        .status()
        .await
        .history
        .into_iter()
        .map(|h| h.msg)
        .filter(|m| m.contains("held") || m.contains("holding") || m.contains("hold "))
        .collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].contains("holding LLM requests for other models while realtime uses rec"));
    assert!(
        lines[1].starts_with("released 3 LLM request(s) held while realtime used rec"),
        "{lines:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_held_client_that_goes_away_leaves_no_trace() {
    let rig = rig().await;
    let rec = enter(&rig, "rec", "realtime").await;
    let held = spawn_enter(&rig, "big", "llm");
    sleep(secs(0.5)).await;
    held.abort();
    sleep(secs(0.1)).await;
    assert!(rig.status().await.queue.is_empty());
    drop(rec);
    sleep(secs(3.0)).await;
    assert_eq!(rig.status().await.inflight_llm, 0);
    let _big = enter(&rig, "big", "llm").await;
}

/// The case from the workstation: the recorder (realtime here) writes notes in parts a few
/// seconds apart; an agent sends requests for another model all along. Returns how often the
/// backend switched model.
async fn recorder_and_agent(rig: Rig) -> usize {
    let agent = {
        let rig = rig.clone();
        tokio::spawn(async move {
            for _ in 0..40 {
                assert_eq!(rig.chat_with("big", 2, ms(100)).await, 200);
                sleep(ms(50)).await;
            }
        })
    };
    sleep(secs(0.3)).await;
    for _ in 0..4 {
        assert_eq!(rig.chat_with("rec", 3, ms(200)).await, 200); // one part: 0.6 s
        sleep(secs(0.6)).await; // writing it out
    }
    timeout(secs(60.0), agent).await.unwrap().unwrap();
    let chats: Vec<String> = rig
        .card
        .events()
        .into_iter()
        .filter(|e| e.starts_with("chat "))
        .collect();
    chats.windows(2).filter(|w| w[0] != w[1]).count()
}

#[tokio::test(start_paused = true)]
async fn the_recorder_and_an_agent_do_not_thrash() {
    // Once to the recorder's model and once back.
    let switches = recorder_and_agent(rig().await).await;
    assert!(switches <= 2, "{switches} model switches");
}

#[tokio::test(start_paused = true)]
async fn without_the_setting_the_same_work_thrashes() {
    let switches = recorder_and_agent(Rig::start(|_| {}).await).await;
    assert!(switches >= 6, "only {switches} model switches");
}
