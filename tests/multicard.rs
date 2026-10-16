// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! Arbiter scheduling across several fake cards. These are the spec for multi-card behaviour:
//! cards[0] is primary, the LLM lives on one card, jobs can be pinned or sent to "any".
//! Time is paused, so the suite is deterministic.

mod support_multi;

use gpud::arbiter::{ArbiterError, CardSel, Phase, Vram, WaiterKind};
use support_multi::*;
use tokio::time::{sleep, timeout, Instant};

/// Fill card `i` with a non-yieldable lease and matching used VRAM.
async fn fill(rig: &MultiRig, i: usize, mib: u64) -> gpud::arbiter::Lease {
    let lease = rig
        .lease_on(named(i), "interactive", mib, false, "filler")
        .await;
    rig.cards[i].set_job_vram(mib);
    lease
}

fn env_of(lease: &gpud::arbiter::Lease) -> Vec<(String, String)> {
    lease.env().to_vec()
}

#[tokio::test(start_paused = true)]
async fn pinned_batch_job_on_second_card_needs_no_idle_wait() {
    let rig = MultiRig::start(2).await;
    rig.chat("big").await;
    let t = Instant::now();
    let lease = rig.lease_on(named(1), "batch", 6000, true, "job").await;
    assert!(t.elapsed() < ms(100), "waited {:?}", t.elapsed());
    assert_eq!(lease.card(), "gpu1");
    assert!(!rig.cards[0].has_event("unload"));
    assert!(rig.cards[0].is_loaded("big"));
    // The LLM keeps serving while the job runs on the other card.
    assert_eq!(rig.chat("big").await, 200);
    assert!(!rig.cards[0].has_event("unload"));
    drop(lease);
}

#[tokio::test(start_paused = true)]
async fn any_goes_to_the_card_with_room() {
    let rig = MultiRig::start(2).await;
    rig.chat("big").await;
    let lease = rig.lease_on(CardSel::Any, "batch", 6000, true, "job").await;
    assert_eq!(lease.card(), "gpu1");
    assert!(env_of(&lease).contains(&("CUDA_VISIBLE_DEVICES".into(), "1".into())));
    assert!(rig.cards[0].is_loaded("big"));
    assert!(!rig.cards[0].has_event("unload"));
    drop(lease);
}

#[tokio::test(start_paused = true)]
async fn any_with_full_second_card_waits_for_llm_card_idle() {
    let rig = MultiRig::start(2).await;
    let blocker = fill(&rig, 1, 20000).await;
    rig.chat("big").await;
    let t = Instant::now();
    let acq = rig.spawn_acquire_on(CardSel::Any, "batch", Vram::Mib(6000), true, "job");
    sleep(ms(100)).await;
    assert!(!acq.is_finished());
    let lease = granted(acq).await;
    assert!(
        t.elapsed() >= secs(0.3 * 0.8),
        "granted after {:?}",
        t.elapsed()
    );
    assert_eq!(lease.card(), "gpu0");
    assert!(env_of(&lease).contains(&("CUDA_VISIBLE_DEVICES".into(), "0".into())));
    assert!(rig.cards[0].has_event("unload"));
    drop(blocker);
}

#[tokio::test(start_paused = true)]
async fn blocked_waiter_on_one_card_does_not_block_lower_priority_on_another() {
    let rig = MultiRig::start(2).await;
    // gpu0 is held by a job that cannot yield.
    let holder = rig
        .lease_on(named(0), "interactive", 20000, false, "holder")
        .await;
    rig.cards[0].set_job_vram(20000);
    let high = rig.spawn_acquire_on(named(0), "interactive", Vram::Mib(20000), false, "high");
    sleep(ms(100)).await;
    assert!(!high.is_finished());
    let t = Instant::now();
    let low = rig.lease_on(named(1), "batch", 6000, true, "low").await;
    assert!(t.elapsed() < ms(200), "waited {:?}", t.elapsed());
    assert_eq!(low.card(), "gpu1");
    assert!(!high.is_finished());
    drop(low);
    drop(holder);
    rig.cards[0].set_job_vram(0);
    drop(granted(high).await);
}

#[tokio::test(start_paused = true)]
async fn llm_request_is_not_held_by_a_job_queued_for_another_card() {
    let rig = MultiRig::start(2).await;
    let blocker = fill(&rig, 1, 24000).await;
    let rt = rig.spawn_acquire_on(named(1), "realtime", Vram::Mib(6000), false, "rt");
    sleep(ms(100)).await;
    assert!(!rt.is_finished());
    let code = timeout(secs(1.0), rig.chat("big"))
        .await
        .expect("chat held by a job queued for gpu1");
    assert_eq!(code, 200);
    assert!(!rig.cards[0].has_event("unload"));
    assert!(!rt.is_finished());
    drop(blocker);
    rig.cards[1].set_job_vram(0);
    drop(granted(rt).await);
}

#[tokio::test(start_paused = true)]
async fn yield_is_requested_only_on_the_target_card() {
    let rig = MultiRig::start(2).await;
    let on1 = rig
        .lease_on(named(1), "background", 20000, true, "j1")
        .await;
    rig.cards[1].set_job_vram(20000);
    let on0 = rig.lease_on(named(0), "background", 6000, true, "j0").await;
    rig.cards[0].set_job_vram(6000);
    let rt = rig.spawn_acquire_on(named(1), "realtime", Vram::Mib(6000), false, "rt");
    rig.wait_for(|_, _| on1.yield_requested()).await;
    sleep(secs(1.0)).await;
    assert!(!on0.yield_requested());
    assert!(!rt.is_finished());
    drop(on1);
    rig.cards[1].set_job_vram(0);
    let lease = granted(rt).await;
    assert_eq!(lease.card(), "gpu1");
    assert!(!on0.yield_requested());
}

#[tokio::test(start_paused = true)]
async fn whole_card_lease_on_second_card_while_llm_is_busy() {
    let rig = MultiRig::start(2).await;
    let busy = rig.spawn_chat_with("big", 20, ms(50));
    rig.wait_for(|st, _| st.inflight_llm == 1).await;
    let acq = rig.spawn_acquire_on(named(1), "batch", Vram::WholeCard, false, "all");
    let lease = granted(acq).await;
    assert_eq!(lease.card(), "gpu1");
    let st = rig.status().await;
    assert_eq!(st.inflight_llm, 1);
    assert!(!rig.cards[0].has_event("unload"));
    drop(lease);
    assert_eq!(done(busy).await, 200);
}

#[tokio::test(start_paused = true)]
async fn probe_failure_on_one_card_leaves_the_other_scheduling() {
    let rig = MultiRig::start(2).await;
    rig.set_probe_failing(1, true);
    rig.handle.wake();
    rig.wait_for(|st, _| st.cards.len() == 2 && st.cards[1].vram_error.is_some())
        .await;
    let st = rig.status().await;
    assert!(st.cards[0].vram_error.is_none());
    assert!(st.vram_error.is_none());
    assert!(st.cards[0].vram.is_some());
    // gpu0 still hands out leases (after the usual idle wait and unload).
    let lease = rig.lease_on(named(0), "batch", 6000, true, "job").await;
    assert_eq!(lease.card(), "gpu0");
    let st = rig.status().await;
    assert!(st.cards[1].vram_error.is_some());
    assert!(st.cards[0].vram_error.is_none());
    drop(lease);
}

#[tokio::test(start_paused = true)]
async fn status_lists_every_card_and_tags_leases_and_waiters() {
    let rig = MultiRig::start(3).await;
    let st = rig.status().await;
    assert_eq!(st.cards.len(), 3);
    for (i, c) in st.cards.iter().enumerate() {
        assert_eq!(c.name, card_name(i));
        assert_eq!(c.device, format!("fake:{i}"));
        assert_eq!(c.llm, i == 0);
    }
    assert_eq!(st.cards[0].state, Phase::Llm);

    // Top-level fields are card 0's.
    let top = serde_json::to_value(&st).unwrap();
    let c0 = serde_json::to_value(&st.cards[0]).unwrap();
    assert_eq!(st.state, st.cards[0].state);
    assert_eq!(st.device, st.cards[0].device);
    for key in ["baseline_mib", "floor_mib", "vram", "vram_error"] {
        assert_eq!(top[key], c0[key], "{key}");
    }

    // Leases carry their card; a queued Any shows "any".
    let l2 = rig.lease_on(named(2), "batch", 1000, true, "on2").await;
    let blocker0 = rig
        .lease_on(named(0), "interactive", 20000, false, "b0")
        .await;
    rig.cards[0].set_job_vram(20000);
    let blocker1 = fill(&rig, 1, 24000).await;
    rig.cards[2].set_job_vram(1000 + 22000);
    let acq = rig.spawn_acquire_on(CardSel::Any, "batch", Vram::Mib(6000), true, "floater");
    rig.wait_for(|st, _| st.queue.iter().any(|w| w.card == "any"))
        .await;
    let st = rig.status().await;
    assert!(st
        .leases
        .iter()
        .any(|l| l.card == "gpu2" && l.name == "on2"));
    assert!(st.leases.iter().any(|l| l.card == "gpu0" && l.name == "b0"));
    assert!(st
        .leases
        .iter()
        .any(|l| l.card == "gpu1" && l.name == "filler"));
    let w = st.queue.iter().find(|w| w.name == "floater").unwrap();
    assert_eq!(w.card, "any");
    assert_eq!(w.kind, WaiterKind::Job);
    acq.abort();
    drop((l2, blocker0, blocker1));
}

#[tokio::test(start_paused = true)]
async fn unknown_card_and_unsteerable_any_fail_at_once() {
    let rig = MultiRig::start(2).await;
    let req = request_on(
        CardSel::Named("nope".into()),
        "batch",
        Vram::Mib(100),
        true,
        "job",
    );
    let err = rig.handle.acquire(req).await.unwrap_err();
    assert!(
        matches!(err, ArbiterError::UnknownCard(ref n, _) if n == "nope"),
        "{err:?}"
    );

    let rig = MultiRig::start_with(Opts::new(2).no_env(), |_| {}).await;
    let req = request_on(CardSel::Any, "batch", Vram::Mib(100), true, "job");
    let err = rig.handle.acquire(req).await.unwrap_err();
    assert!(matches!(err, ArbiterError::NoSteerableCard), "{err:?}");
}

#[tokio::test(start_paused = true)]
async fn any_only_considers_cards_with_env() {
    let rig = MultiRig::start_with(Opts::new(2).no_env_on(0), |_| {}).await;
    let lease = rig.lease_on(CardSel::Any, "batch", 6000, true, "job").await;
    assert_eq!(lease.card(), "gpu1");
}

#[tokio::test(start_paused = true)]
async fn any_works_on_a_single_card_without_env() {
    let rig = MultiRig::start_with(Opts::new(1).no_env(), |_| {}).await;
    let lease = rig.lease_on(CardSel::Any, "batch", 6000, true, "job").await;
    assert_eq!(lease.card(), "gpu0");
    assert!(lease.env().is_empty());
}

#[tokio::test(start_paused = true)]
async fn primary_means_the_first_card_even_if_the_second_is_idle() {
    let rig = MultiRig::start(2).await;
    rig.chat("big").await;
    let lease = rig
        .lease_on(CardSel::Primary, "batch", 6000, true, "job")
        .await;
    assert_eq!(lease.card(), "gpu0");
    assert!(rig.cards[0].has_event("unload"));
    let st = rig.status().await;
    assert!(st.leases.iter().all(|l| l.card == "gpu0"));
}

#[tokio::test(start_paused = true)]
async fn resumed_lease_is_adopted_on_the_named_card() {
    let rig = MultiRig::start(2).await;
    let mut req = request_on(named(1), "batch", Vram::Mib(1000), false, "job");
    req.resume_id = Some("L-old".into());
    let lease = timeout(secs(1.0), rig.handle.acquire(req))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(lease.id(), "L-old");
    assert_eq!(lease.card(), "gpu1");
    let st = rig.status().await;
    assert!(st
        .leases
        .iter()
        .any(|l| l.id == "L-old" && l.card == "gpu1"));
    drop(lease);
}

#[tokio::test(start_paused = true)]
async fn releasing_a_lease_on_a_card_without_llm_touches_nothing() {
    let rig = MultiRig::start(2).await;
    rig.chat("big").await;
    let before = rig.cards[0].events();
    let lease = rig.lease_on(named(1), "batch", 6000, true, "job").await;
    sleep(secs(1.0)).await;
    drop(lease);
    sleep(secs(3.0)).await;
    assert_eq!(rig.cards[0].events(), before);
    assert!(rig.cards[1].events().is_empty());
    assert!(rig.cards[0].is_loaded("big"));
    assert_eq!(rig.status().await.state, Phase::Llm);
}

#[tokio::test(start_paused = true)]
async fn llm_on_second_card_is_only_unloaded_by_jobs_pinned_to_it() {
    let rig = MultiRig::start_with(Opts::new(2).llm(1), |_| {}).await;
    assert_eq!(rig.chat("big").await, 200);
    // A job pinned to gpu0 leaves the LLM on gpu1 alone.
    let t = Instant::now();
    let on0 = rig.lease_on(named(0), "batch", 6000, true, "j0").await;
    assert!(t.elapsed() < ms(100), "waited {:?}", t.elapsed());
    assert!(rig.cards[1].is_loaded("big"));
    assert!(!rig.cards[1].has_event("unload"));
    let st = rig.status().await;
    assert!(st.cards[1].llm && !st.cards[0].llm);

    // A job pinned to gpu1 waits for the idle rule, then unloads the LLM.
    assert_eq!(rig.chat("big").await, 200);
    let t = Instant::now();
    let acq = rig.spawn_acquire_on(named(1), "batch", Vram::Mib(6000), true, "j1");
    sleep(ms(100)).await;
    assert!(!acq.is_finished());
    let on1 = granted(acq).await;
    assert!(
        t.elapsed() >= secs(0.3 * 0.8),
        "granted after {:?}",
        t.elapsed()
    );
    assert_eq!(on1.card(), "gpu1");
    assert!(rig.cards[1].has_event("unload"));
    assert!(!rig.cards[1].is_loaded("big"));
    assert!(!rig.cards[0].has_event("unload"));
    drop((on0, on1));
}
