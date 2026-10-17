// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! Leases and an LLM that hold several cards at once, on fake cards with paused time.
//!
//! The rules under test: a lease for several cards is granted all at once or not at all, a
//! waiting one holds nothing, it is not overtaken on its cards by later leases of its own
//! class, a holder yields when any of its cards is wanted by something more important, and an
//! LLM on several cards gives all of them back together.

mod support_multi;

use std::collections::HashMap;

use gpuq::arbiter::{merge_env, ArbiterError, CardSel, Phase, Vram};
use support_multi::*;
use tokio::time::{sleep, Instant};

fn kv(k: &str, v: &str) -> (String, String) {
    (k.to_owned(), v.to_owned())
}

/// Fill card `i` with a lease that cannot yield, and matching used VRAM.
async fn fill(rig: &MultiRig, i: usize, mib: u64) -> gpuq::arbiter::Lease {
    let lease = rig
        .lease_on(named(i), "interactive", mib, false, "filler")
        .await;
    rig.cards[i].set_job_vram(mib);
    lease
}

fn names(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

#[tokio::test(start_paused = true)]
async fn a_set_is_granted_all_at_once_with_the_env_merged() {
    // The LLM lives on gpu2, out of the way.
    let rig = MultiRig::start_with(Opts::new(3).llm(2), |_| {}).await;
    let filler = fill(&rig, 1, 20000).await;
    let pair = rig.spawn_acquire_on(set(&[0, 1]), "batch", Vram::Mib(6000), true, "pair");
    sleep(secs(1.0)).await;
    assert!(!pair.is_finished());
    let st = rig.status().await;
    // Nothing is held on gpu0 while gpu1 is busy: no partial grant.
    assert_eq!(st.leases.len(), 1, "{:?}", st.leases);
    assert_eq!(st.leases[0].name, "filler");
    assert_eq!(st.queue[0].card, "gpu0,gpu1");
    assert!(st.queue[0].why.contains("gpu1"), "{}", st.queue[0].why);
    drop(filler);
    rig.cards[1].set_job_vram(0);
    let lease = granted(pair).await;
    assert_eq!(lease.cards(), names(&["gpu0", "gpu1"]));
    assert_eq!(lease.card(), "gpu0,gpu1");
    assert_eq!(lease.env(), [kv("CUDA_VISIBLE_DEVICES", "0,1")]);
    let st = rig.status().await;
    assert_eq!(st.leases.len(), 1);
    assert_eq!(st.leases[0].cards, names(&["gpu0", "gpu1"]));
    assert!(!rig.cards[2].has_event("unload"));
}

#[tokio::test(start_paused = true)]
async fn a_waiting_set_holds_nothing_so_others_can_use_its_free_card() {
    let rig = MultiRig::start_with(Opts::new(3).llm(2), |_| {}).await;
    let filler = fill(&rig, 1, 20000).await;
    // Two sets in opposite orders: the classic deadlock if either held one card and waited.
    let a = rig.spawn_acquire_on(set(&[0, 1]), "batch", Vram::WholeCard, false, "a");
    sleep(ms(50)).await;
    let b = rig.spawn_acquire_on(set(&[1, 0]), "batch", Vram::WholeCard, false, "b");
    sleep(ms(200)).await;
    // gpu0 is free, so a more important job takes it at once.
    let t = Instant::now();
    let c = rig
        .lease_on(named(0), "interactive", 1000, false, "c")
        .await;
    assert!(t.elapsed() < ms(200), "waited {:?}", t.elapsed());
    assert!(!a.is_finished() && !b.is_finished());
    drop(c);
    drop(filler);
    rig.cards[1].set_job_vram(0);
    let a = granted(a).await;
    assert_eq!(a.cards(), names(&["gpu0", "gpu1"]));
    sleep(secs(1.0)).await;
    assert!(!b.is_finished());
    drop(a);
    let b = granted(b).await;
    assert_eq!(b.cards(), names(&["gpu1", "gpu0"]));
    assert_eq!(b.env(), [kv("CUDA_VISIBLE_DEVICES", "1,0")]);
}

#[tokio::test(start_paused = true)]
async fn later_single_card_jobs_of_the_same_class_do_not_starve_a_set() {
    let rig = MultiRig::start_with(Opts::new(3).llm(2), |_| {}).await;
    let busy = rig.lease_on(named(1), "batch", 20000, false, "long").await;
    rig.cards[1].set_job_vram(20000);
    let pair = rig.spawn_acquire_on(set(&[0, 1]), "batch", Vram::WholeCard, false, "pair");
    sleep(ms(100)).await;
    // A stream of single-card batch jobs for the card the set is waiting on.
    let singles: Vec<_> = (0..5)
        .map(|k| {
            rig.spawn_acquire_on(
                named(0),
                "batch",
                Vram::WholeCard,
                false,
                &format!("single{k}"),
            )
        })
        .collect();
    sleep(secs(2.0)).await;
    // gpu0 is idle, but it is kept for the set that queued first.
    assert!(singles.iter().all(|s| !s.is_finished()));
    assert!(!pair.is_finished());
    drop(busy);
    rig.cards[1].set_job_vram(0);
    let pair = granted(pair).await;
    assert_eq!(pair.cards(), names(&["gpu0", "gpu1"]));
    sleep(ms(500)).await;
    assert!(singles.iter().all(|s| !s.is_finished()));
    drop(pair);
    for s in singles {
        drop(granted(s).await);
    }
}

#[tokio::test(start_paused = true)]
async fn a_higher_class_on_one_card_makes_a_set_holder_yield() {
    let rig = MultiRig::start_with(Opts::new(3).llm(2), |_| {}).await;
    let pair = rig
        .lease_cards(set(&[0, 1]), "batch", Vram::Mib(20000), true, "pair")
        .await;
    rig.cards[0].set_job_vram(20000);
    rig.cards[1].set_job_vram(20000);
    let rt = rig.spawn_acquire_on(named(1), "interactive", Vram::Mib(6000), false, "rt");
    rig.wait_for(|_, _| pair.yield_requested()).await;
    assert!(!rt.is_finished());
    let st = rig.status().await;
    assert!(st
        .leases
        .iter()
        .any(|l| l.name == "pair" && l.yield_requested));
    drop(pair);
    rig.cards[0].set_job_vram(0);
    rig.cards[1].set_job_vram(0);
    let rt = granted(rt).await;
    assert_eq!(rt.card(), "gpu1");
}

#[tokio::test(start_paused = true)]
async fn a_lower_class_never_makes_a_set_holder_yield() {
    let rig = MultiRig::start_with(Opts::new(3).llm(2), |_| {}).await;
    let pair = rig
        .lease_cards(set(&[0, 1]), "interactive", Vram::Mib(20000), true, "pair")
        .await;
    rig.cards[1].set_job_vram(20000);
    let low = rig.spawn_acquire_on(named(1), "batch", Vram::Mib(6000), false, "low");
    sleep(secs(2.0)).await;
    assert!(!pair.yield_requested());
    assert!(!low.is_finished());
    drop(pair);
    rig.cards[1].set_job_vram(0);
    drop(granted(low).await);
}

#[tokio::test(start_paused = true)]
async fn card_count_takes_any_cards_with_room() {
    let rig = MultiRig::start_with(Opts::new(4).llm(3), |_| {}).await;
    let filler = fill(&rig, 0, 24000).await;
    let two = rig
        .lease_cards(CardSel::Count(2), "batch", Vram::Mib(6000), true, "two")
        .await;
    assert_eq!(two.cards(), names(&["gpu1", "gpu2"]));
    assert_eq!(two.env(), [kv("CUDA_VISIBLE_DEVICES", "1,2")]);
    // The LLM card was not needed.
    assert!(!rig.cards[3].has_event("unload"));
    drop(two);
    // Three cards: gpu0 is full, so the LLM gives up gpu3 once it has been idle.
    let three = rig
        .lease_cards(CardSel::Count(3), "batch", Vram::Mib(6000), true, "three")
        .await;
    assert_eq!(three.cards(), names(&["gpu1", "gpu2", "gpu3"]));
    assert!(rig.cards[3].has_event("unload"));
    drop(filler);
}

#[tokio::test(start_paused = true)]
async fn all_means_every_card_and_takes_the_llm_card_too() {
    let rig = MultiRig::start(2).await;
    let all = rig
        .lease_cards(CardSel::All, "batch", Vram::Mib(1000), true, "all")
        .await;
    assert_eq!(all.cards(), names(&["gpu0", "gpu1"]));
    assert!(rig.cards[0].has_event("unload"));
    let st = rig.status().await;
    assert!(st.cards.iter().all(|c| c.state == Phase::Jobs));
    drop(all);
    // Released: the resident model comes back.
    rig.wait_for(|st, _| st.cards[0].state == Phase::Llm).await;
}

#[tokio::test(start_paused = true)]
async fn the_llm_is_not_unloaded_while_the_rest_of_a_set_is_blocked() {
    let rig = MultiRig::start(2).await;
    let filler = fill(&rig, 1, 20000).await;
    let pair = rig.spawn_acquire_on(set(&[0, 1]), "batch", Vram::Mib(6000), true, "pair");
    // Well past the idle time: gpu1 cannot be had, so the LLM keeps gpu0.
    sleep(secs(3.0)).await;
    assert!(!pair.is_finished());
    assert!(!rig.cards[0].has_event("unload"));
    assert_eq!(rig.chat("big").await, 200);
    drop(filler);
    rig.cards[1].set_job_vram(0);
    let pair = granted(pair).await;
    assert!(rig.cards[0].has_event("unload"));
    drop(pair);
}

#[tokio::test(start_paused = true)]
async fn the_offload_hook_runs_for_each_card_of_a_set() {
    let rig = MultiRig::start_with(Opts::new(3).llm(2).hook(), |_| {}).await;
    // Something outside gpuq (a desktop session) fills both cards.
    rig.cards[0].set_job_vram(20000);
    rig.cards[1].set_job_vram(20000);
    let pair = rig
        .lease_cards(set(&[0, 1]), "batch", Vram::Mib(6000), false, "pair")
        .await;
    assert_eq!(pair.cards(), names(&["gpu0", "gpu1"]));
    let mut runs = rig.hook.as_ref().unwrap().runs.lock().unwrap().clone();
    runs.sort();
    assert_eq!(runs, names(&["gpu0", "gpu1"]));
}

#[tokio::test(start_paused = true)]
async fn requests_that_cannot_work_fail_at_once() {
    let rig = MultiRig::start_with(
        Opts::new(4)
            .llm(3)
            .card(0, &[("ROCR_VISIBLE_DEVICES", "GPU-a")], "amd")
            .card(1, &[("CUDA_VISIBLE_DEVICES", "0")], "command")
            .card(2, &[("CUDA_VISIBLE_DEVICES", "1")], "nvidia")
            .no_env_on(3)
            .card(3, &[], "nvidia"),
        |_| {},
    )
    .await;
    let try_on = |sel: CardSel| {
        let handle = rig.handle.clone();
        async move {
            handle
                .acquire(request_on(sel, "batch", Vram::Mib(100), true, "x"))
                .await
        }
    };
    // Two vendors that both use CUDA_VISIBLE_DEVICES: their numbers mean different cards.
    match try_on(set(&[1, 2])).await {
        Err(ArbiterError::MixedVendors(e)) => {
            assert!(e.contains("CUDA_VISIBLE_DEVICES"), "{e}")
        }
        other => panic!("expected MixedVendors, got {other:?}"),
    }
    // AMD and NVIDIA use different variables, so they can share a lease.
    let mixed = try_on(set(&[0, 2])).await.unwrap();
    assert_eq!(
        mixed.env(),
        [
            kv("ROCR_VISIBLE_DEVICES", "GPU-a"),
            kv("CUDA_VISIBLE_DEVICES", "1")
        ]
    );
    drop(mixed);
    assert!(matches!(
        try_on(set(&[0, 3])).await,
        Err(ArbiterError::NoEnv(n)) if n == "gpu3"
    ));
    assert!(matches!(
        try_on(CardSel::All).await,
        Err(ArbiterError::MixedVendors(_) | ArbiterError::NoEnv(_))
    ));
    assert!(matches!(
        try_on(CardSel::Count(4)).await,
        Err(ArbiterError::NotEnoughCards(4, 3))
    ));
    assert!(matches!(
        try_on(set(&[0, 0])).await,
        Err(ArbiterError::DuplicateCard(_))
    ));
    assert!(matches!(
        try_on(CardSel::Set(names(&["gpu0", "nope"]))).await,
        Err(ArbiterError::UnknownCard(..))
    ));
    // card_count picks cards whose env merges: gpu1 and gpu2 would clash.
    let two = try_on(CardSel::Count(2)).await.unwrap();
    assert_eq!(two.cards(), names(&["gpu0", "gpu1"]));
}

#[test]
fn env_merge_joins_values_in_grant_order() {
    let a = [kv("CUDA_VISIBLE_DEVICES", "GPU-x")];
    let b = [kv("CUDA_VISIBLE_DEVICES", "GPU-y")];
    let amd = [
        kv("HIP_VISIBLE_DEVICES", "1"),
        kv("ROCR_VISIBLE_DEVICES", "1"),
    ];
    let merged = merge_env([
        ("b", "nvidia", &b[..]),
        ("a", "nvidia", &a[..]),
        ("c", "amd", &amd[..]),
    ])
    .unwrap();
    assert_eq!(
        merged,
        [
            kv("CUDA_VISIBLE_DEVICES", "GPU-y,GPU-x"),
            kv("HIP_VISIBLE_DEVICES", "1"),
            kv("ROCR_VISIBLE_DEVICES", "1"),
        ]
    );
    let e = merge_env([("a", "nvidia", &a[..]), ("z", "command", &b[..])]).unwrap_err();
    assert!(e.contains("a (nvidia)") && e.contains("z (command)"), "{e}");
}

// ------------------------------------------------------------------------ the LLM

#[tokio::test(start_paused = true)]
async fn an_llm_on_two_cards_unloads_from_both_when_one_is_wanted() {
    let rig = MultiRig::start_with(Opts::new(3), |s| {
        s.llm.cards = names(&["gpu0", "gpu1"]);
    })
    .await;
    let st = rig.status().await;
    assert_eq!(st.llm_cards, names(&["gpu0", "gpu1"]));
    assert!(st.cards[1].llm && st.cards[1].state == Phase::Llm);
    assert!(!st.cards[2].llm);
    // A job for gpu2 does not touch the LLM.
    drop(rig.lease_on(named(2), "batch", 6000, true, "aside").await);
    assert!(!rig.cards[0].has_event("unload"));
    // A job for gpu1 waits for the LLM's idle time, then gets the card from an unload.
    let job = rig.lease_on(named(1), "batch", 6000, true, "job").await;
    assert!(rig.cards[0].has_event("unload"));
    let st = rig.status().await;
    assert!(st.llm_cards.is_empty());
    assert!(st.cards[..2].iter().all(|c| c.state == Phase::Jobs));
    // A chat needs both cards back: the job is asked to yield, then the LLM reloads.
    let chat = rig.spawn_chat("big");
    rig.wait_for(|_, _| job.yield_requested()).await;
    drop(job);
    assert_eq!(chat.await.unwrap(), 200);
    let st = rig.status().await;
    assert_eq!(st.llm_cards, names(&["gpu0", "gpu1"]));
}

#[tokio::test(start_paused = true)]
async fn a_model_bigger_than_one_card_spreads_and_a_small_one_stays() {
    let rig = MultiRig::start_with(Opts::new(3), |s| {
        s.llm.model_vram =
            HashMap::from([("big".to_owned(), 30_000), ("other".to_owned(), 10_000)]);
    })
    .await;
    // big needs 30000 MiB; one 24560 MiB card is not enough, two are.
    let st = rig.status().await;
    assert_eq!(st.llm_cards, names(&["gpu0", "gpu1"]));
    // A job takes gpu1, which unloads the LLM from both cards.
    let job = rig.lease_on(named(1), "batch", 6000, false, "job").await;
    assert!(rig.cards[0].has_event("unload"));
    // "other" fits on gpu0 alone, so it is served while the job keeps gpu1.
    assert_eq!(rig.chat("other").await, 200);
    let st = rig.status().await;
    assert_eq!(st.llm_cards, names(&["gpu0"]));
    assert_eq!(st.leases.len(), 1);
    drop(job);
}

#[tokio::test(start_paused = true)]
async fn a_model_pinned_to_other_cards_takes_them() {
    let rig = MultiRig::start_with(Opts::new(3), |s| {
        s.llm.model_cards = HashMap::from([("other".to_owned(), names(&["gpu0", "gpu2"]))]);
    })
    .await;
    assert_eq!(rig.status().await.llm_cards, names(&["gpu0"]));
    // The LLM is up on gpu0; "other" also needs gpu2, which is free, so it takes it.
    assert_eq!(rig.chat("other").await, 200);
    assert_eq!(rig.status().await.llm_cards, names(&["gpu0", "gpu2"]));
    // A job for gpu2 now has to wait for the LLM like a job for gpu0.
    let t = Instant::now();
    let job = rig.lease_on(named(2), "batch", 6000, true, "job").await;
    assert!(
        t.elapsed() >= secs(0.3 * 0.8),
        "granted after {:?}",
        t.elapsed()
    );
    assert!(rig.cards[0].has_event("unload"));
    drop(job);
}
