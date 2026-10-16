//! Arbiter scheduling against a fake backend and a fake VRAM counter.
//! Ported from tests/test_arbiter.py; time is paused, so the suite is deterministic.

mod common;

use std::time::Duration;

use common::*;
use gpud::arbiter::{Phase, Vram, WaiterKind};
use tokio::time::{sleep, timeout, Instant};

#[tokio::test(start_paused = true)]
async fn job_waits_for_idle_then_gets_clean_card() {
    let rig = Rig::start(|_| {}).await;
    rig.chat("big").await;
    let t = Instant::now();
    let lease = rig.lease("background", 6000, true, "job").await;
    assert!(t.elapsed() >= secs(0.3 * 0.8));
    assert_eq!(rig.status().await.state, Phase::Jobs);
    assert!(rig.card.loaded().is_empty());
    assert!(rig.card.has_event("save 100")); // slot kept for later
    drop(lease);
}

#[tokio::test(start_paused = true)]
async fn held_request_yields_job_then_restores_slot() {
    let rig = Rig::start(|_| {}).await;
    rig.chat("big").await; // conversation of 100 tokens in slot 0
    let lease = rig.lease("background", 6000, true, "job").await;
    rig.card.set_job_vram(6000);
    let req = rig.spawn_chat("big");
    rig.wait_for(|_, _| lease.yield_requested()).await;
    assert!(!req.is_finished()); // held while the job winds down
    rig.card.set_job_vram(0);
    drop(lease);
    assert_eq!(done(req).await, 200);
    let ev = rig.card.events();
    let restore = ev.iter().position(|e| e == "restore 100").unwrap();
    let last_chat = ev.iter().rposition(|e| e == "chat big").unwrap();
    assert!(restore < last_chat, "{ev:?}");
    assert_eq!(rig.card.slot_tokens(), 200); // restored 100 + this turn
}

#[tokio::test(start_paused = true)]
async fn batch_hold_delays_yield_then_hands_over() {
    let rig = Rig::start(|s| s.tuning.batch_hold = secs(0.6)).await;
    let lease = rig.lease("batch", 6000, true, "job").await;
    rig.card.set_job_vram(6000);
    let req = rig.spawn_chat("big");
    sleep(secs(0.3)).await;
    assert!(!lease.yield_requested()); // still inside the hold window
    rig.wait_for(|_, _| lease.yield_requested()).await;
    rig.card.set_job_vram(0);
    drop(lease);
    assert_eq!(done(req).await, 200);
}

#[tokio::test(start_paused = true)]
async fn batch_hold_ends_early_when_enough_requests_queue() {
    let rig = Rig::start(|s| {
        s.tuning.batch_hold = secs(60.0);
        s.tuning.batch_hold_count = 2;
    })
    .await;
    let lease = rig.lease("batch", 6000, true, "job").await;
    rig.card.set_job_vram(6000);
    let first = rig.spawn_chat("big");
    sleep(secs(0.3)).await;
    assert!(!lease.yield_requested()); // one request: keep holding
    let second = rig.spawn_chat("big");
    rig.wait_for(|_, _| lease.yield_requested()).await;
    rig.card.set_job_vram(0);
    drop(lease);
    assert_eq!(done(first).await, 200);
    assert_eq!(done(second).await, 200);
}

#[tokio::test(start_paused = true)]
async fn batch_hold_does_not_apply_to_other_classes() {
    let rig = Rig::start(|s| s.tuning.batch_hold = secs(60.0)).await;
    let lease = rig.lease("background", 6000, true, "job").await;
    rig.card.set_job_vram(6000);
    let req = rig.spawn_chat("big");
    rig.wait_for(|_, _| lease.yield_requested()).await;
    rig.card.set_job_vram(0);
    drop(lease);
    assert_eq!(done(req).await, 200);
}

#[tokio::test(start_paused = true)]
async fn batch_hold_does_not_apply_to_non_yieldable_batch_job() {
    let rig = Rig::start(|s| s.tuning.batch_hold = secs(60.0)).await;
    let lease = rig.lease("batch", 6000, false, "job").await;
    let req = rig.spawn_chat("big");
    sleep(secs(0.4)).await;
    assert!(!req.is_finished());
    assert!(!lease.yield_requested()); // cannot yield; nothing to schedule
    drop(lease);
    assert_eq!(done(req).await, 200);
}

#[tokio::test(start_paused = true)]
async fn non_yieldable_job_is_not_disturbed() {
    let rig = Rig::start(|_| {}).await;
    let lease = rig.lease("interactive", 20000, false, "job").await;
    let req = rig.spawn_chat("big");
    sleep(secs(0.5)).await;
    assert!(!req.is_finished());
    assert!(!lease.yield_requested());
    drop(lease);
    assert_eq!(done(req).await, 200);
}

#[tokio::test(start_paused = true)]
async fn realtime_job_preempts_waiting_without_idle_delay() {
    // A background job would wait a minute; realtime must not.
    let rig = Rig::start(|s| s.tuning.idle = secs(60.0)).await;
    let busy = rig.spawn_chat_with("big", 10, ms(30));
    rig.wait_for(|st, _| st.inflight_llm == 1).await;
    let acq = rig.spawn_acquire("realtime", Vram::Mib(3000), false, "transcribe");
    sleep(ms(50)).await;
    let late = rig.spawn_chat("big"); // arrives after the realtime job queued: waits
    let lease = granted(acq).await;
    assert!(busy.is_finished()); // the in-flight request finished first
    assert!(!late.is_finished());
    drop(lease);
    assert_eq!(done(late).await, 200);
}

#[tokio::test(start_paused = true)]
async fn job_waits_for_gap_between_pipeline_requests() {
    // Requests of a class with a longer gap keep interactive jobs off the card between them.
    let rig = Rig::start(|_| {}).await;
    rig.chat("rec").await;
    let unloads = rig.card.count("unload");
    let acq = rig.spawn_acquire("interactive", Vram::Mib(20000), false, "render");
    for _ in 0..3 {
        // back-to-back requests, 0.3 s apart: the card stays with the LLM
        sleep(secs(0.3)).await;
        assert_eq!(timeout(secs(5.0), rig.chat("rec")).await.unwrap(), 200);
    }
    assert!(!acq.is_finished());
    assert_eq!(rig.card.count("unload"), unloads);
    let lease = granted(acq).await; // granted once the longer gap has passed
    drop(lease);
}

#[tokio::test(start_paused = true)]
async fn class_reload_delay_keeps_resident_off_until_followup_request() {
    let rig = Rig::start(|_| {}).await;
    rig.chat("big").await; // 100 tokens of conversation on the resident model
    let lease = rig.lease("realtime", 3000, false, "job").await;
    drop(lease);
    assert_eq!(timeout(secs(5.0), rig.chat("rec")).await.unwrap(), 200);
    assert!(!rig.card.has_event("restore 100")); // not wasted on the other model's turn
    assert_eq!(timeout(secs(5.0), rig.chat("big")).await.unwrap(), 200);
    assert!(rig.card.has_event("restore 100"));
    assert_eq!(rig.card.slot_tokens(), 200);
}

#[tokio::test(start_paused = true)]
async fn lower_priority_job_does_not_block_llm() {
    let rig = Rig::start(|s| s.tuning.idle = secs(60.0)).await;
    let acq = rig.spawn_acquire("background", Vram::Mib(6000), true, "bg");
    let status = timeout(secs(2.0), rig.chat("big")).await.unwrap();
    assert_eq!(status, 200);
    assert_eq!(rig.status().await.state, Phase::Llm);
    assert!(!acq.is_finished());
    acq.abort();
}

#[tokio::test(start_paused = true)]
async fn idle_card_reloads_resident() {
    let rig = Rig::start(|_| {}).await;
    rig.chat("big").await;
    let lease = rig.lease("batch", 1000, false, "job").await;
    drop(lease);
    rig.wait_for(|st, c| st.state == Phase::Llm && c.is_loaded("big"))
        .await;
    assert!(rig.card.has_event("restore 100"));
}

#[tokio::test(start_paused = true)]
async fn no_resident_model_means_nothing_is_reloaded() {
    let rig = Rig::start(|s| s.resident = None).await;
    rig.chat("big").await;
    let lease = rig.lease("batch", 1000, false, "job").await;
    let loads = rig.card.count("load big");
    drop(lease);
    rig.wait_for(|st, _| st.state == Phase::Llm).await;
    assert_eq!(rig.card.count("load big"), loads);
    assert!(rig.card.loaded().is_empty());
    // loaded on demand
    assert_eq!(timeout(secs(5.0), rig.chat("other")).await.unwrap(), 200);
}

#[tokio::test(start_paused = true)]
async fn failed_reload_drops_saved_slot() {
    let rig = Rig::start(|_| {}).await;
    rig.chat("big").await;
    let lease = rig.lease("batch", 1000, false, "job").await;
    assert!(rig.card.saved().contains_key("big"));
    rig.card.set_load_fails(true);
    drop(lease);
    rig.wait_for(|st, _| st.state == Phase::Llm).await;
    // The backend trait has no way to drop a saved slot, so the arbiter must stop trying to
    // restore it: later requests don't each wait out a timeout for a slot that cannot come back.
    rig.card.set_load_fails(false);
    assert_eq!(timeout(secs(5.0), rig.chat("big")).await.unwrap(), 200);
    assert!(
        !rig.card.events().iter().any(|e| e.starts_with("restore")),
        "{:?}",
        rig.card.events()
    );
}

#[tokio::test(start_paused = true)]
async fn unload_failure_backs_off() {
    let rig = Rig::start(|_| {}).await;
    rig.card.set_unload_fails(true);
    let acq = rig.spawn_acquire("batch", Vram::Mib(1000), false, "x");
    sleep(secs(1.0)).await;
    let st = rig.status().await;
    let fails = st
        .history
        .iter()
        .filter(|h| h.msg.contains("unload failed"))
        .count();
    assert_eq!(fails, 1); // one attempt, then a 15 s back-off, not a hot loop
    assert_eq!(st.state, Phase::Llm);
    acq.abort();
}

#[tokio::test(start_paused = true)]
async fn reload_waits_for_vram_left_behind_by_a_job() {
    let rig = Rig::start(|_| {}).await;
    rig.chat("big").await;
    let lease = rig.lease("interactive", 20000, false, "job").await;
    rig.card.set_job_vram(17000); // the job "finished" but its process still holds VRAM
    drop(lease);
    rig.wait_for(|st, _| {
        st.history
            .iter()
            .any(|h| h.msg.contains("waiting for VRAM to drain"))
    })
    .await;
    assert!(!rig.card.is_loaded("big")); // would have spilled into system RAM
    rig.card.set_job_vram(0);
    rig.wait_for(|st, c| st.state == Phase::Llm && c.is_loaded("big"))
        .await;
}

#[tokio::test(start_paused = true)]
async fn concurrent_jobs_share_card_by_vram() {
    let rig = Rig::start(|_| {}).await;
    let a = rig.lease("background", 6000, true, "a").await;
    let b = rig.lease("batch", 8000, false, "b").await;
    let c = rig.spawn_acquire("batch", Vram::Mib(15000), false, "c");
    sleep(secs(0.3)).await;
    assert!(!c.is_finished()); // 6000 + 8000 + 15000 > 24560
    drop(b);
    drop(a);
    drop(granted(c).await);
}

#[tokio::test(start_paused = true)]
async fn whole_card_lease_needs_an_otherwise_empty_card() {
    let rig = Rig::start(|_| {}).await;
    let small = rig.lease("batch", 1000, false, "small").await;
    let whole = rig.spawn_acquire("batch", Vram::WholeCard, false, "whole");
    sleep(secs(0.3)).await;
    assert!(!whole.is_finished()); // another lease is active
    drop(small);
    let lease = granted(whole).await;
    let other = rig.spawn_acquire("batch", Vram::Mib(100), false, "late");
    sleep(secs(0.3)).await;
    assert!(!other.is_finished()); // nothing fits next to a whole-card lease
    drop(lease);
    drop(granted(other).await);
}

#[tokio::test(start_paused = true)]
async fn whole_card_waits_while_other_processes_use_vram() {
    let rig = Rig::start(|_| {}).await;
    rig.card.set_job_vram(2000); // e.g. a desktop session
    let whole = rig.spawn_acquire("batch", Vram::WholeCard, false, "whole");
    rig.wait_for(|st, _| st.queue.iter().any(|w| w.why.contains("still in use")))
        .await;
    assert!(!whole.is_finished());
    rig.card.set_job_vram(0);
    rig.handle.wake();
    drop(granted(whole).await);
}

#[tokio::test(start_paused = true)]
async fn offload_hook_runs_once_and_frees_the_card() {
    let card = FakeCard::default();
    let hook = FakeHook::freeing(&card);
    let rig = Rig::start_on(card, true, Some(hook.clone()), |s| {
        s.tuning.offload_settle = Duration::ZERO
    })
    .await;
    rig.card.set_job_vram(10000); // a desktop session holding VRAM
    let lease = rig.lease("batch", 20000, false, "big-job").await;
    assert_eq!(hook.runs(), 1);
    drop(lease);
}

#[tokio::test(start_paused = true)]
async fn offload_hook_failure_leaves_job_waiting() {
    let card = FakeCard::default();
    let hook = FakeHook::failing(&card);
    let rig = Rig::start_on(card, true, Some(hook.clone()), |s| {
        s.tuning.offload_settle = Duration::ZERO
    })
    .await;
    rig.card.set_job_vram(10000);
    let acq = rig.spawn_acquire("batch", Vram::Mib(20000), false, "big-job");
    rig.wait_for(|_, _| hook.runs() >= 1).await;
    sleep(secs(0.3)).await;
    assert!(!acq.is_finished());
    assert_eq!(hook.runs(), 1);
    acq.abort();
}

#[tokio::test(start_paused = true)]
async fn abandoned_held_request_leaves_queue() {
    let rig = Rig::start(|_| {}).await;
    let lease = rig.lease("interactive", 20000, false, "job").await;
    let req = rig.spawn_chat("big");
    rig.wait_for(|st, _| st.queue.iter().any(|w| w.kind == WaiterKind::Llm))
        .await;
    req.abort();
    rig.wait_for(|st, _| st.queue.is_empty()).await;
    drop(lease);
}

#[tokio::test(start_paused = true)]
async fn resumed_lease_is_adopted_without_queueing() {
    let rig = Rig::start(|_| {}).await;
    let mut req = request("batch", Vram::Mib(1000), false, "job");
    req.resume_id = Some("L-old".into());
    let lease = timeout(secs(1.0), rig.handle.acquire(req))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(lease.id(), "L-old");
    assert!(rig.status().await.leases.iter().any(|l| l.id == "L-old"));
    drop(lease);
}

#[tokio::test(start_paused = true)]
async fn start_with_empty_card_waits_for_leases_to_resume() {
    let rig = Rig::start_with(false, None, |s| s.tuning.resume_grace = secs(0.5)).await;
    rig.handle.wake();
    assert_eq!(rig.status().await.state, Phase::Jobs);
    sleep(secs(0.2)).await;
    assert_eq!(rig.status().await.state, Phase::Jobs); // grace period: not reloaded yet
    rig.wait_for(|st, c| st.state == Phase::Llm && c.is_loaded("big"))
        .await;
}

#[tokio::test(start_paused = true)]
async fn status_reports_floor_and_vram() {
    let rig = Rig::start(|_| {}).await;
    rig.chat("big").await;
    let lease = rig.lease("batch", 1000, false, "job").await;
    let st = rig.status().await;
    assert_eq!(st.state, Phase::Jobs);
    assert_eq!(st.floor_mib, Some(FLOOR));
    assert_eq!(st.vram.unwrap().total_mib, 24560);
    assert_eq!(st.leases[0].class, "batch");
    drop(lease);
}

// ------------------------------------------------------------------------ Rust-specific

#[tokio::test(start_paused = true)]
async fn queued_acquire_dropped_before_grant_leaves_queue() {
    let rig = Rig::start(|_| {}).await;
    let holder = rig.lease("batch", 20000, false, "holder").await;
    // Polled once (so it queues), then dropped by the timeout.
    let req = request("batch", Vram::Mib(20000), false, "impatient");
    let res = timeout(secs(0.5), rig.handle.acquire(req)).await;
    assert!(res.is_err());
    rig.wait_for(|st, _| st.queue.is_empty()).await;
    // The abandoned waiter must not be granted a ghost lease.
    drop(holder);
    let again = rig.lease("batch", 20000, false, "again").await;
    assert_eq!(rig.status().await.leases.len(), 1);
    drop(again);
}

#[tokio::test(start_paused = true)]
async fn status_answers_while_unload_is_in_progress() {
    let rig = Rig::start(|_| {}).await;
    rig.card.set_unload_delay(secs(2.0));
    let acq = rig.spawn_acquire("batch", Vram::Mib(1000), false, "job");
    rig.wait_for(|st, _| st.state == Phase::Unloading).await;
    let st = timeout(ms(100), rig.handle.status())
        .await
        .expect("status blocked during unload")
        .unwrap();
    assert_eq!(st.state, Phase::Unloading);
    drop(timeout(secs(10.0), acq).await.unwrap().unwrap().unwrap());
}

#[tokio::test(start_paused = true)]
async fn lease_dropped_while_yield_pending_frees_the_card() {
    let rig = Rig::start(|_| {}).await;
    let lease = rig.lease("background", 6000, true, "job").await;
    let req = rig.spawn_chat("big");
    rig.wait_for(|_, _| lease.yield_requested()).await;
    assert!(!req.is_finished());
    drop(lease); // the job never answered the yield request; it just went away
    assert_eq!(done(req).await, 200);
    assert!(rig.status().await.leases.is_empty());
}

#[tokio::test(start_paused = true)]
async fn unknown_job_class_ranks_as_batch() {
    let rig = Rig::start(|_| {}).await;
    let classes = rig.handle.classes();
    assert_eq!(classes.job_prio("mystery"), classes.job_prio("batch"));
    let holder = rig.lease("interactive", 20000, false, "holder").await;
    // Queued first, but ranked last: background (3) beats the unknown class (as batch, 4).
    let mystery = rig.spawn_acquire("mystery", Vram::Mib(20000), false, "mystery");
    rig.wait_for(|st, _| st.queue.len() == 1).await;
    let bg = rig.spawn_acquire("background", Vram::Mib(20000), false, "bg");
    rig.wait_for(|st, _| st.queue.len() == 2).await;
    let st = rig.status().await;
    assert_eq!(st.queue[0].class, "background");
    assert_eq!(st.queue[1].class, "mystery");
    drop(holder);
    let bg_lease = granted(bg).await;
    assert!(!mystery.is_finished());
    drop(bg_lease);
    drop(granted(mystery).await);
}

#[tokio::test(start_paused = true)]
async fn margin_is_kept_free_on_top_of_declared_vram() {
    let rig = Rig::start(|_| {}).await;
    // After the unload the card holds FLOOR; 256 MiB margin stays free.
    let fits = TOTAL_MIB - FLOOR - 256;
    let lease = rig.lease("batch", fits, false, "exact").await;
    drop(lease);
    let rig = Rig::start(|_| {}).await;
    let over = rig.spawn_acquire("batch", Vram::Mib(fits + 1), false, "over");
    sleep(secs(2.0)).await;
    assert!(!over.is_finished());
    let st = rig.status().await;
    assert!(
        st.queue[0].why.contains(&format!("{} MiB free", fits)),
        "{:?}",
        st.queue
    );
}
