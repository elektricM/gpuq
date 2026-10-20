// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! The arbiter task: owns all scheduling state and changes it only in response to messages.
//!
//! The loop waits on three things: the message channel, a timer at the next decision deadline
//! (never more than [`TICK`] away, because drain and baseline decisions depend on polled VRAM)
//! and the effects in flight, at most one per card. After each of them it runs a bounded
//! scheduling pass.
//!
//! Every card has its own phase. The cards that are not in `Jobs` belong to the LLM backend
//! and always share one phase: the LLM takes and gives back its cards together, and an unload
//! or a reload is one effect for all of them. Which cards that is depends on the model (see
//! `Actor::placement`): its home cards, or more for a model that needs more VRAM.
//!
//! Leases sit in one list; each names the cards it holds. Waiters sit in one queue ordered by
//! (priority, arrival), each aimed at one card, a set of cards, or any card (or any N cards)
//! that can be steered. A scheduling step looks at one card and only at the waiters that can
//! use it, and only the best of them is served there. A lease for several cards is granted in
//! one step once every card it needs fits, so it never holds one card while it waits for
//! another. Because only the best waiter of a card is served, a waiting multi-card lease is
//! not overtaken on its cards by later or lower-class leases: each card it needs comes to it
//! as soon as its current holders leave.
//!
//! Slow backend work (unload, reload, the offload hook, waiting for VRAM to drain) runs as an
//! effect: a boxed future that lives inside the phase it belongs to and is polled by the loop.
//! So `Status` and every other message is answered while an unload is under way.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::future::join_all;
use serde::Serialize;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{sleep, sleep_until, timeout, Instant};

use super::{
    merge_env, CardSel, CardStatus, Classes, Granted, HistoryEntry, LeaseRequest, LeaseStatus,
    ModelHold, Msg, OffloadHook, QueueStatus, Settings, Status, Tuning, Vram, WaiterKind,
};
use crate::backend::{Backend, BackendError, Placement};
use crate::probe::{VramProbe, VramReading};
use crate::Mib;

/// A scheduling pass does at most this many steps; each one changes state or stops.
const MAX_PASSES: usize = 100;
/// Longest sleep between passes.
const TICK: Duration = Duration::from_secs(5);
/// Pause before trying an unload again after one failed.
const UNLOAD_BACKOFF: Duration = Duration::from_secs(15);
const HISTORY_LEN: usize = 40;
/// How many recent admissions are remembered, to undo one whose client vanished.
const RECENT_ADMITS: usize = 64;
/// Floor for polling sleeps, so a zero setting cannot spin the executor.
const MIN_POLL: Duration = Duration::from_millis(50);
/// A probe read that takes longer counts as failed, for that card only.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Who owns the card.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Llm,
    Unloading,
    Jobs,
    Restoring,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Phase::Llm => "llm",
            Phase::Unloading => "unloading",
            Phase::Jobs => "jobs",
            Phase::Restoring => "restoring",
        }
    }
}

// ------------------------------------------------------------------------ effects

/// An effect in flight: polled by the main loop, owns clones of what it needs.
type Fx<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Why the resident model is being reloaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RestoreKind {
    /// The card goes back to the LLM: wait for VRAM to drain first, restart idle timers.
    Card,
    /// The resident model was swapped out after an unload; a request for it needs its slot back.
    Slot,
}

/// The phase, with the data that only exists in it. An unload or reload covers every card of
/// the LLM; its effect lives on the first of them and the others carry `None`.
enum State {
    /// `retry_at`: no new unload before then, the last one failed.
    Llm {
        retry_at: Option<Instant>,
    },
    Unloading {
        fx: Option<Fx<Unloaded>>,
    },
    /// `offload`: the hook (and its settle pause) is running.
    Jobs {
        offload: Option<Fx<Option<Mib>>>,
    },
    Restoring {
        fx: Option<Fx<Reloaded>>,
    },
}

impl State {
    fn phase(&self) -> Phase {
        match self {
            State::Llm { .. } => Phase::Llm,
            State::Unloading { .. } => Phase::Unloading,
            State::Jobs { .. } => Phase::Jobs,
            State::Restoring { .. } => Phase::Restoring,
        }
    }

    /// Jobs phase with no effect running: the card can be scheduled.
    fn is_idle_jobs(&self) -> bool {
        matches!(self, State::Jobs { offload: None })
    }

    fn is_llm(&self) -> bool {
        !matches!(self, State::Jobs { .. })
    }
}

struct Unloaded {
    /// Models whose slot was saved, even when the unload failed afterwards.
    saved: Vec<String>,
    /// VRAM in use afterwards on each card, in card order (None: the probe failed).
    result: Result<Vec<Option<Mib>>, String>,
    elapsed: Duration,
}

struct Reloaded {
    kind: RestoreKind,
    loaded: bool,
    /// A slot was saved for the resident when the reload began.
    had_saved: bool,
    elapsed: Duration,
}

/// What an effect finished with.
enum Done {
    Unloaded(Unloaded),
    Reloaded(Reloaded),
    Offloaded(Option<Mib>),
}

/// Waits for the first effect of any card to finish; never resolves when none is running.
async fn fx_wait(cards: &mut [CardState]) -> (usize, Done) {
    std::future::poll_fn(|cx| {
        for (i, c) in cards.iter_mut().enumerate() {
            let polled = match &mut c.state {
                State::Unloading { fx: Some(fx) } => fx.as_mut().poll(cx).map(Done::Unloaded),
                State::Restoring { fx: Some(fx) } => fx.as_mut().poll(cx).map(Done::Reloaded),
                State::Jobs { offload: Some(fx) } => fx.as_mut().poll(cx).map(Done::Offloaded),
                _ => continue,
            };
            if let Poll::Ready(done) = polled {
                return Poll::Ready((i, done));
            }
        }
        Poll::Pending
    })
    .await
}

/// Lets an effect add history lines while it runs. Holds a weak sender, so a running effect
/// does not keep the arbiter alive after the last handle is gone.
#[derive(Clone)]
struct Log(mpsc::WeakUnboundedSender<Msg>);

impl Log {
    fn say(&self, msg: impl Into<String>) {
        if let Some(tx) = self.0.upgrade() {
            let _ = tx.send(Msg::Event(msg.into()));
        }
    }
}

/// What effects need from the outside world: the backend and the cards they work on.
#[derive(Clone)]
struct Env {
    backend: Arc<dyn Backend>,
    probes: Vec<Arc<dyn VramProbe>>,
    names: Vec<String>,
    t: Arc<Tuning>,
    log: Log,
}

impl Env {
    /// VRAM in use now on the `k`th card; None (and a history line) when the probe fails.
    async fn used_on(&self, k: usize) -> Option<Mib> {
        match timed_read(self.probes[k].clone()).await {
            Ok(r) => Some(r.used_mib),
            Err(e) => {
                if self.names.len() > 1 {
                    self.log
                        .say(format!("cannot read VRAM on {} ({e})", self.names[k]));
                } else {
                    self.log.say(format!("cannot read VRAM ({e})"));
                }
                None
            }
        }
    }

    /// VRAM in use now on every card, read concurrently.
    async fn used_all(&self) -> Vec<Option<Mib>> {
        join_all((0..self.probes.len()).map(|k| self.used_on(k))).await
    }
}

/// One probe read, cut off after [`PROBE_TIMEOUT`].
async fn timed_read(probe: Arc<dyn VramProbe>) -> Result<VramReading, String> {
    match timeout(PROBE_TIMEOUT, probe.read()).await {
        Ok(Ok(r)) => Ok(r),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!(
            "probe timed out after {}s",
            PROBE_TIMEOUT.as_secs()
        )),
    }
}

/// Save slots, unload, wait for the backend to report empty, let the driver settle, measure.
async fn unload(env: Env) -> Unloaded {
    let started = Instant::now();
    let mut saved = Vec::new();
    let steps = unload_steps(&env, started, &mut saved).await;
    let result = match steps {
        Ok(()) => {
            sleep(env.t.driver_settle).await; // let the driver hand the memory back
            Ok(env.used_all().await)
        }
        Err(e) => Err(e.to_string()),
    };
    Unloaded {
        saved,
        result,
        elapsed: started.elapsed(),
    }
}

async fn unload_steps(
    env: &Env,
    started: Instant,
    saved: &mut Vec<String>,
) -> Result<(), BackendError> {
    let models = env.backend.loaded_gpu_models().await?;
    if env.backend.capabilities().slots {
        for model in models {
            if env.backend.save_slot(&model).await? {
                saved.push(model);
            }
        }
    }
    env.backend.unload_all().await?;
    let deadline = started + env.t.settle_timeout;
    while Instant::now() < deadline && !env.backend.loaded_gpu_models().await?.is_empty() {
        sleep(Duration::from_secs(1)).await;
    }
    Ok(())
}

/// Don't reload a card-filling model while a finished job still holds VRAM: it would spill
/// into system RAM and run several times slower. `floors[k]` is the level of the `k`th card
/// right after the unload; cards without one are not waited for.
async fn wait_for_floor(env: &Env, floors: &[Option<Mib>]) {
    let slack = env.t.reload_slack_mib;
    let deadline = Instant::now() + env.t.reload_wait;
    // (card, used, floor) of every card still above its limit; None when a probe failed.
    let over = |used: &[Option<Mib>]| -> Option<Vec<(usize, Mib, Mib)>> {
        let mut high = Vec::new();
        for (k, floor) in floors.iter().enumerate() {
            let Some(floor) = *floor else { continue };
            let u = used.get(k).copied().flatten()?;
            if u > floor.saturating_add(slack) {
                high.push((k, u, floor));
            }
        }
        Some(high)
    };
    let Some(mut high) = over(&env.used_all().await) else {
        return;
    };
    let on = |k: usize| {
        if env.names.len() > 1 {
            format!(" on {}", env.names[k])
        } else {
            String::new()
        }
    };
    if let Some(&(k, used, floor)) = high.first() {
        env.log.say(format!(
            "waiting for VRAM to drain before reload ({used} MiB used{}, floor {floor})",
            on(k)
        ));
    }
    while !high.is_empty() && Instant::now() < deadline {
        sleep(env.t.driver_settle.max(MIN_POLL)).await;
        match over(&env.used_all().await) {
            Some(h) => high = h,
            None => return,
        }
    }
    if let Some(&(k, used, _)) = high.first() {
        env.log.say(format!(
            "VRAM still {used} MiB{} after {}s; reloading anyway",
            on(k),
            env.t.reload_wait.as_secs_f64()
        ));
    }
}

/// Load the resident model, then give it its saved slot back.
async fn reload(
    env: Env,
    resident: String,
    had_saved: bool,
    floors: Vec<Option<Mib>>,
    kind: RestoreKind,
) -> Reloaded {
    let started = Instant::now();
    if floors.iter().any(Option::is_some) {
        wait_for_floor(&env, &floors).await;
    }
    if kind == RestoreKind::Card {
        env.log.say(format!("reloading {resident}"));
    }
    let loaded = env.backend.load(&resident).await;
    if loaded && had_saved {
        if let Err(e) = env.backend.restore_slot(&resident).await {
            env.log
                .say(format!("restoring the {resident} slot failed ({e})"));
        }
    }
    Reloaded {
        kind,
        loaded,
        had_saved,
        elapsed: started.elapsed(),
    }
}

/// Run the offload hook, let the card settle, measure again.
async fn offload(env: Env, hook: Arc<dyn OffloadHook>) -> Option<Mib> {
    if !hook.run(&env.names[0]).await {
        env.log.say("offload hook failed");
    }
    sleep(env.t.offload_settle).await;
    env.used_on(0).await
}

// ------------------------------------------------------------------------ records

/// Which card or cards a waiter can be served on.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Target {
    Card(usize),
    /// Any card that can be steered (see `Actor::steer`).
    Any,
    /// All of these cards at once (at least two).
    Set(Vec<usize>),
    /// Any this many cards that can be steered, at once (at least two).
    Count(usize),
}

impl Target {
    /// `steer[c]`: card `c` can be picked for an `Any` or `Count` job.
    fn can_use(&self, card: usize, steer: &[bool]) -> bool {
        match self {
            Target::Card(c) => *c == card,
            Target::Set(s) => s.contains(&card),
            Target::Any | Target::Count(_) => steer.get(card).copied().unwrap_or(false),
        }
    }

    /// How many cards it takes at once.
    fn need(&self) -> usize {
        match self {
            Target::Card(_) | Target::Any => 1,
            Target::Set(s) => s.len(),
            Target::Count(n) => *n,
        }
    }

    /// The cards of a fixed target; empty for `Any` and `Count`.
    fn fixed(&self) -> Vec<usize> {
        match self {
            Target::Card(c) => vec![*c],
            Target::Set(s) => s.clone(),
            Target::Any | Target::Count(_) => Vec::new(),
        }
    }
}

/// What one class does with the LLM.
struct ClassUse {
    /// Canonical id of the model it last used.
    model: String,
    inflight: usize,
    last: Instant,
}

struct HoldLog {
    behind_class: String,
    behind_model: String,
    since: Instant,
    released: usize,
    longest: Duration,
}

/// Why a request waits for another class's model.
struct HeldBehind {
    class: String,
    model: String,
    /// When the hold ends if nothing else happens; None while that class has requests in flight.
    until: Option<Instant>,
}

enum Want {
    Job {
        vram: Vram,
        yieldable: bool,
        pid: Option<u32>,
        reply: oneshot::Sender<Granted>,
    },
    Llm {
        model: String,
        reply: oneshot::Sender<()>,
    },
}

/// A job asking for a lease, or an LLM request held at the gate.
struct Waiter {
    /// Handle-side id, for `Cancel`.
    id: u64,
    /// Arrival order inside the actor; orders waiters of equal priority.
    seq: u64,
    class: String,
    prio: u32,
    name: String,
    since: SystemTime,
    note: String,
    /// When it was first held behind a batch lease.
    held_at: Option<Instant>,
    /// Held because a more important class uses another model (see `model_hold`).
    model_held: bool,
    /// For an LLM request: the cards its model runs on.
    target: Target,
    want: Want,
}

impl Waiter {
    fn key(&self) -> (u32, u64) {
        (self.prio, self.seq)
    }

    fn is_llm(&self) -> bool {
        matches!(self.want, Want::Llm { .. })
    }

    fn can_use(&self, card: usize, steer: &[bool]) -> bool {
        self.target.can_use(card, steer)
    }

    fn vram(&self) -> Option<Vram> {
        match self.want {
            Want::Job { vram, .. } => Some(vram),
            Want::Llm { .. } => None,
        }
    }
}

struct LeaseRec {
    id: String,
    /// Handle-side id of the request that got it (None for a resumed lease).
    handle: Option<u64>,
    class: String,
    prio: u32,
    name: String,
    /// Declared per card.
    vram: Vram,
    yieldable: bool,
    yield_tx: watch::Sender<bool>,
    since: SystemTime,
    pid: Option<u32>,
    /// The cards it holds, in grant order.
    cards: Vec<usize>,
}

/// An LLM request is held back by any waiting job that outranks it and could take one of the
/// cards its model needs. Jobs for other cards do not count.
fn blocked(waiters: &[Waiter], key: (u32, u64), cards: &[usize], steer: &[bool]) -> bool {
    waiters
        .iter()
        .any(|w| !w.is_llm() && w.key() < key && cards.iter().any(|&c| w.can_use(c, steer)))
}

/// What a card needs to decide whether a request fits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Limits {
    /// VRAM kept free on top of what leases declare.
    margin_mib: Mib,
    /// A whole-card lease needs at most this much used by other processes.
    whole_card_baseline_mib: Mib,
}

/// `free = total - max(used, baseline_or_used + declared) - margin`, never below zero.
fn free_mib(r: VramReading, baseline: Option<Mib>, declared: Mib, margin: Mib) -> Mib {
    let base = baseline.unwrap_or(r.used_mib);
    let committed = r.used_mib.max(base.saturating_add(declared));
    r.total_mib.saturating_sub(committed).saturating_sub(margin)
}

/// Whether a request of `want` VRAM fits next to the leases holding `held`.
fn fits(want: Vram, held: &[Vram], baseline: Option<Mib>, r: VramReading, lim: Limits) -> bool {
    match want {
        Vram::WholeCard => {
            held.is_empty() && baseline.is_some_and(|b| b < lim.whole_card_baseline_mib)
        }
        Vram::Mib(m) => {
            if held.contains(&Vram::WholeCard) {
                return false;
            }
            free_mib(r, baseline, declared_mib(held), lim.margin_mib) >= m
        }
    }
}

/// Sum of the sized reservations (a whole-card lease counts as zero here; `fits` handles it).
fn declared_mib(held: &[Vram]) -> Mib {
    held.iter()
        .map(|v| match v {
            Vram::Mib(m) => *m,
            Vram::WholeCard => 0,
        })
        .fold(0, Mib::saturating_add)
}

fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn age_s(t: SystemTime) -> f64 {
    t.elapsed().map_or(0.0, |d| d.as_secs_f64())
}

/// Keep the first of each index.
fn dedup(v: impl IntoIterator<Item = usize>) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();
    for c in v {
        if !out.contains(&c) {
            out.push(c);
        }
    }
    out
}

// ------------------------------------------------------------------------ the actor

/// Everything that belongs to one card.
struct CardState {
    name: String,
    probe: Arc<dyn VramProbe>,
    env: Vec<(String, String)>,
    pin_env: Vec<(String, String)>,
    vendor: String,
    limits: Limits,
    state: State,
    /// VRAM in use with no lease held (driver, desktop).
    baseline: Option<Mib>,
    /// VRAM in use right after the last unload.
    floor: Option<Mib>,
    /// Size of the card, from the last good reading.
    total: Option<Mib>,
    grace_until: Instant,
    /// None: no lease has ended yet.
    released_at: Option<Instant>,
    released_class: String,
    /// Waiters (by arrival number) the offload hook already ran for.
    offload_tried: HashSet<u64>,
    probe_error: Option<String>,
}

struct Actor {
    classes: Classes,
    resident: Option<String>,
    t: Arc<Tuning>,
    backend: Arc<dyn Backend>,
    hook: Option<Arc<dyn OffloadHook>>,
    weak: mpsc::WeakUnboundedSender<Msg>,

    cards: Vec<CardState>,
    /// Per card: can it be picked for an `Any` job.
    steer: Vec<bool>,
    leases: Vec<LeaseRec>,

    // The LLM side. Its cards are the ones not in `Jobs`.
    /// Cards a model uses when nothing more specific is known; never empty.
    home: Vec<usize>,
    /// Canonical model id to its cards, from the config.
    model_cards: HashMap<String, Vec<usize>>,
    /// Canonical model id to the VRAM it needs, from the config.
    model_vram: HashMap<String, Mib>,
    inflight: usize,
    last_llm: Instant,
    last_llm_class: String,
    saved: HashSet<String>,
    /// Recent admissions (id, class, model), to undo one whose client vanished.
    recent_admits: VecDeque<(u64, String, String)>,
    /// Per class: the model it last used, its requests in flight, when it was last active.
    llm_use: HashMap<String, ClassUse>,
    /// The hold behind a class's model under way: for one start and one end line.
    hold_log: Option<HoldLog>,

    waiters: Vec<Waiter>,
    history: VecDeque<(SystemTime, String)>,
    next_seq: u64,
    next_lease: u64,
    boot: u64,
    /// Next time a waiting decision can change; collected during a pass.
    deadline: Option<Instant>,
}

pub(super) async fn run(
    settings: Settings,
    cards: Vec<super::Card>,
    llm_card: usize,
    backend: Arc<dyn Backend>,
    offload: Option<Arc<dyn OffloadHook>>,
    tx: mpsc::UnboundedSender<Msg>,
    mut rx: mpsc::UnboundedReceiver<Msg>,
) {
    // Keep only a weak sender: the loop ends once every handle is gone.
    let weak = tx.downgrade();
    drop(tx);
    let mut actor = Actor::new(settings, cards, llm_card, backend, offload, weak);
    actor.start().await;
    loop {
        let now = Instant::now();
        let wake_at = actor.deadline.map_or(now + TICK, |d| d.min(now + TICK));
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(msg) => actor.handle(msg).await,
                None => return,
            },
            () = sleep_until(wake_at) => {}
            (i, done) = fx_wait(&mut actor.cards) => actor.finish(i, done),
        }
        actor.pass().await;
    }
}

impl Actor {
    fn new(
        settings: Settings,
        cards: Vec<super::Card>,
        llm_card: usize,
        backend: Arc<dyn Backend>,
        hook: Option<Arc<dyn OffloadHook>>,
        weak: mpsc::WeakUnboundedSender<Msg>,
    ) -> Self {
        let now = Instant::now();
        let many = cards.len() > 1;
        let steer = cards.iter().map(|c| !many || !c.env.is_empty()).collect();
        let index = |n: &String| cards.iter().position(|c| &c.name == n);
        let mut home = dedup(settings.llm.cards.iter().filter_map(index));
        if home.is_empty() {
            home = vec![llm_card];
        }
        let model_cards = settings
            .llm
            .model_cards
            .iter()
            .filter_map(|(m, names)| {
                let v = dedup(names.iter().filter_map(index));
                (!v.is_empty()).then(|| (m.clone(), v))
            })
            .collect();
        let cards = cards
            .into_iter()
            .enumerate()
            .map(|(i, c)| CardState {
                name: c.name,
                probe: c.probe,
                env: c.env,
                pin_env: c.pin_env,
                vendor: c.vendor,
                limits: Limits {
                    margin_mib: c.margin_mib,
                    whole_card_baseline_mib: c.whole_card_baseline_mib,
                },
                // The LLM starts on its home cards; `start` corrects that from what is loaded.
                state: if home.contains(&i) {
                    State::Llm { retry_at: None }
                } else {
                    State::Jobs { offload: None }
                },
                baseline: None,
                floor: None,
                total: None,
                grace_until: now,
                released_at: None,
                released_class: String::new(),
                offload_tried: HashSet::new(),
                probe_error: None,
            })
            .collect();
        Actor {
            classes: settings.classes,
            resident: settings.resident.filter(|r| !r.is_empty()),
            t: Arc::new(settings.tuning),
            backend,
            hook,
            weak,
            cards,
            steer,
            leases: Vec::new(),
            home,
            model_cards,
            model_vram: settings.llm.model_vram,
            inflight: 0,
            last_llm: now,
            last_llm_class: "llm".into(),
            saved: HashSet::new(),
            recent_admits: VecDeque::new(),
            llm_use: HashMap::new(),
            hold_log: None,
            waiters: Vec::new(),
            history: VecDeque::new(),
            next_seq: 0,
            next_lease: 1,
            // Keeps lease ids unique across restarts.
            boot: unix_secs(SystemTime::now()) & 0xFFFFF,
            deadline: None,
        }
    }

    fn env(&self, cards: &[usize]) -> Env {
        Env {
            backend: self.backend.clone(),
            probes: cards.iter().map(|&c| self.cards[c].probe.clone()).collect(),
            names: cards.iter().map(|&c| self.cards[c].name.clone()).collect(),
            t: self.t.clone(),
            log: Log(self.weak.clone()),
        }
    }

    fn names(&self, cards: &[usize]) -> Vec<String> {
        cards.iter().map(|&c| self.cards[c].name.clone()).collect()
    }

    /// " on gpu1" when there is more than one card, else nothing, so single-card history
    /// keeps its wording.
    fn on(&self, card: usize) -> String {
        self.on_set(&[card])
    }

    /// " on gpu0,gpu1", or nothing with a single card.
    fn on_set(&self, cards: &[usize]) -> String {
        if self.cards.len() > 1 {
            format!(" on {}", self.names(cards).join(","))
        } else {
            String::new()
        }
    }

    /// How a target reads in history and status: "gpu1", "gpu0,gpu1", "any", "any 2".
    fn target_name(&self, t: &Target) -> String {
        match t {
            Target::Card(c) => self.cards[*c].name.clone(),
            Target::Set(s) => self.names(s).join(","),
            Target::Any => "any".to_owned(),
            Target::Count(n) => format!("any {n}"),
        }
    }

    fn event(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        tracing::info!("{msg}");
        self.push_history(msg);
    }

    fn warn(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        tracing::warn!("{msg}");
        self.push_history(msg);
    }

    fn push_history(&mut self, msg: String) {
        if self.history.len() == HISTORY_LEN {
            self.history.pop_front();
        }
        self.history.push_back((SystemTime::now(), msg));
    }

    /// Ask for a pass no later than `at`.
    fn want(&mut self, at: Instant) {
        self.deadline = Some(self.deadline.map_or(at, |d| d.min(at)));
    }

    /// Read the given cards concurrently. A card whose read fails or times out gives None
    /// and nothing else is affected. A failure is logged once per distinct error and means
    /// "cannot decide now": the next tick tries again.
    async fn read_cards(&mut self, which: &[usize]) -> Vec<Option<VramReading>> {
        let reads = which.iter().map(|&i| {
            let probe = self.cards[i].probe.clone();
            async move { (i, timed_read(probe).await) }
        });
        let results = join_all(reads).await;
        let mut out = vec![None; self.cards.len()];
        for (i, result) in results {
            match result {
                Ok(r) => {
                    self.cards[i].probe_error = None;
                    self.cards[i].total = Some(r.total_mib);
                    out[i] = Some(r);
                }
                Err(e) => {
                    if self.cards[i].probe_error.as_deref() != Some(e.as_str()) {
                        let on = self.on(i);
                        self.event(format!("cannot read VRAM{on} ({e}); will retry"));
                    }
                    self.cards[i].probe_error = Some(e);
                }
            }
        }
        out
    }

    // -------------------------------------------------------------- cards and leases

    fn index(&self, name: &str) -> Option<usize> {
        self.cards.iter().position(|c| c.name == name)
    }

    /// The leases holding `card`.
    fn held(&self, card: usize) -> impl Iterator<Item = &LeaseRec> + '_ {
        self.leases.iter().filter(move |l| l.cards.contains(&card))
    }

    fn busy(&self, card: usize) -> bool {
        self.held(card).next().is_some()
    }

    fn held_vram(&self, card: usize) -> Vec<Vram> {
        self.held(card).map(|l| l.vram).collect()
    }

    /// The cards the LLM holds now (unloading and restoring included), in card order.
    fn llm_set(&self) -> Vec<usize> {
        (0..self.cards.len())
            .filter(|&c| self.cards[c].state.is_llm())
            .collect()
    }

    /// The phase of the LLM's cards; None when it holds none.
    fn llm_phase(&self) -> Option<Phase> {
        self.cards
            .iter()
            .find(|c| c.state.is_llm())
            .map(|c| c.state.phase())
    }

    /// The env of several cards merged. A conflict cannot happen for leases (requests are
    /// checked first); for the LLM it is logged and the cards' env is just concatenated.
    fn merged_env(&self, cards: &[usize]) -> Vec<(String, String)> {
        let parts = cards.iter().map(|&c| {
            let k = &self.cards[c];
            (k.name.as_str(), k.vendor.as_str(), k.env.as_slice())
        });
        merge_env(parts).unwrap_or_else(|e| {
            tracing::warn!("{e}");
            cards
                .iter()
                .flat_map(|&c| self.cards[c].env.iter().cloned())
                .collect()
        })
    }

    /// What a lease on `cards` hands its job, see [`super::lease_env`].
    fn lease_env(&self, cards: &[usize]) -> Vec<(String, String)> {
        let pins: Vec<&[(String, String)]> = cards
            .iter()
            .map(|&c| self.cards[c].pin_env.as_slice())
            .collect();
        super::lease_env(self.merged_env(cards), &pins)
    }

    fn env_ok(&self, cards: &[usize]) -> bool {
        merge_env(cards.iter().map(|&c| {
            let k = &self.cards[c];
            (k.name.as_str(), k.vendor.as_str(), k.env.as_slice())
        }))
        .is_ok()
    }

    /// VRAM a model can count on: the card's size minus its margin and what stays in use
    /// without leases. Unknown before the first reading.
    fn usable(&self, card: usize) -> Mib {
        let c = &self.cards[card];
        let used = c.floor.or(c.baseline).unwrap_or(0);
        c.total
            .unwrap_or(0)
            .saturating_sub(c.limits.margin_mib)
            .saturating_sub(used)
    }

    /// The cards a model runs on: its own list from the config, else the home cards, plus the
    /// fewest other cards (largest first) that make room for the VRAM it needs.
    fn placement(&self, model: &str) -> Vec<usize> {
        let model = self.backend.resolve(model);
        if let Some(cards) = self.model_cards.get(&model) {
            return cards.clone();
        }
        let need = self
            .model_vram
            .get(&model)
            .copied()
            .or_else(|| self.backend.vram_estimate(&model));
        let mut set = self.home.clone();
        let Some(need) = need else {
            return set;
        };
        let mut have: Mib = set.iter().map(|&c| self.usable(c)).sum();
        let mut rest: Vec<usize> = (0..self.cards.len()).filter(|c| !set.contains(c)).collect();
        rest.sort_by_key(|&c| std::cmp::Reverse(self.usable(c)));
        for c in rest {
            if have >= need {
                break;
            }
            set.push(c);
            have = have.saturating_add(self.usable(c));
        }
        set
    }

    /// Where the LLM goes when the cards go idle: the resident model's cards, else home.
    fn llm_return(&self) -> Vec<usize> {
        match &self.resident {
            Some(r) => self.placement(r),
            None => self.home.clone(),
        }
    }

    /// Tell the backend which cards the next load uses, for servers gpuq starts itself.
    fn place(&self, cards: &[usize]) {
        let p = Placement {
            cards: self.names(cards),
            env: self.merged_env(cards),
            free_mib: cards.iter().map(|&c| self.usable(c)).collect(),
        };
        self.backend.place(&p);
    }

    /// Decide where to begin from what the backend has loaded.
    async fn start(&mut self) {
        let all: Vec<usize> = (0..self.cards.len()).collect();
        let readings = self.read_cards(&all).await;
        match self.backend.loaded_gpu_models().await {
            Err(e) => {
                // Backend down: assume it owns its cards, it loads on demand.
                let kind = self.backend.kind();
                self.warn(format!(
                    "cannot read {kind} state ({e}); starting in llm state"
                ));
                self.place(&self.home.clone());
            }
            Ok(loaded) if !loaded.is_empty() => {
                let set = dedup(
                    self.home
                        .clone()
                        .into_iter()
                        .chain(loaded.iter().flat_map(|m| self.placement(m))),
                );
                for &c in &set {
                    self.enter_llm_state(c, None);
                }
                self.place(&set);
                let on = if set.len() > 1 {
                    self.on_set(&set)
                } else {
                    String::new()
                };
                self.event(format!("start: llm state{on}, loaded {loaded:?}"));
            }
            Ok(_) => {
                // Nothing loaded: maybe gpuq restarted under a running job. Give `gpu run`
                // clients time to re-adopt their leases before reloading the LLM on top.
                let grace = self.t.resume_grace;
                for c in self.home.clone() {
                    let card = &mut self.cards[c];
                    card.grace_until = Instant::now() + grace;
                    card.state = State::Jobs { offload: None };
                    card.baseline = readings[c].map(|r| r.used_mib);
                    card.floor = card.baseline;
                }
                self.event(format!(
                    "start: card empty, waiting {}s for leases to resume",
                    grace.as_secs_f64()
                ));
            }
        }
    }

    // -------------------------------------------------------------- messages

    async fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::EnterLlm {
                id,
                model,
                class,
                reply,
            } => self.enter_llm(id, model, class, reply),
            Msg::ExitLlm { class, model } => {
                self.inflight = self.inflight.saturating_sub(1);
                self.last_llm = Instant::now();
                self.exit_class(&class, &model);
            }
            Msg::Acquire { id, req, reply } => self.acquire(id, req, reply),
            Msg::Cancel(id) => self.cancel(id),
            Msg::Release(id) => self.release(&id),
            Msg::Status(reply) => {
                let status = self.status().await;
                let _ = reply.send(status);
            }
            Msg::Wake => {}
            Msg::Event(msg) => self.event(msg),
        }
    }

    fn enter_llm(&mut self, id: u64, model: String, class: String, reply: oneshot::Sender<()>) {
        let prio = self.classes.llm_prio(&class);
        let seq = self.take_seq();
        let cards = self.placement(&model);
        let in_llm = cards
            .iter()
            .all(|&c| matches!(self.cards[c].state, State::Llm { .. }));
        let behind = self.held_behind(&model, prio, Instant::now());
        if in_llm
            && behind.is_none()
            && !blocked(&self.waiters, (prio, seq), &cards, &self.steer)
            && !self.needs_slot_restore(&model)
        {
            if reply.send(()).is_ok() {
                self.admit(id, &class, &model);
            }
            return;
        }
        let model_held = behind.is_some();
        let note = match &behind {
            Some(b) => {
                self.log_hold_start(b);
                if let Some(until) = b.until {
                    self.want(until);
                }
                hold_note(b)
            }
            None => {
                let phase = self.cards[cards[0]].state.phase().name();
                self.event(format!("hold {class} request for {model} (state {phase})"));
                String::new()
            }
        };
        let target = match cards[..] {
            [c] => Target::Card(c),
            _ => Target::Set(cards),
        };
        self.waiters.push(Waiter {
            id,
            seq,
            class,
            prio,
            name: model.clone(),
            since: SystemTime::now(),
            note,
            held_at: None,
            model_held,
            target,
            want: Want::Llm { model, reply },
        });
    }

    // -------------------------------------------------------------- model holds

    /// The more important class whose model `model` would push off the backend, if any.
    fn held_behind(&self, model: &str, prio: u32, now: Instant) -> Option<HeldBehind> {
        let want = self.backend.resolve(model);
        let mut best: Option<(u32, HeldBehind)> = None;
        for (class, hold) in self.t.model_holders() {
            let class_prio = self.classes.llm_prio(class);
            if class_prio >= prio {
                continue;
            }
            let Some(u) = self.llm_use.get(class) else {
                continue;
            };
            if u.model == want {
                continue;
            }
            let until = u.last + hold;
            if u.inflight == 0 && now >= until {
                continue;
            }
            if best.as_ref().is_some_and(|(p, _)| *p <= class_prio) {
                continue;
            }
            let b = HeldBehind {
                class: class.to_owned(),
                model: u.model.clone(),
                until: (u.inflight == 0).then_some(until),
            };
            best = Some((class_prio, b));
        }
        best.map(|(_, b)| b)
    }

    fn log_hold_start(&mut self, b: &HeldBehind) {
        if self.hold_log.is_some() {
            return;
        }
        self.hold_log = Some(HoldLog {
            behind_class: b.class.clone(),
            behind_model: b.model.clone(),
            since: Instant::now(),
            released: 0,
            longest: Duration::ZERO,
        });
        self.event(format!(
            "holding LLM requests for other models while {} uses {}",
            b.class, b.model
        ));
    }

    /// Once no request waits behind another class's model any more, say how it went.
    fn log_hold_end(&mut self) {
        if self.waiters.iter().any(|w| w.model_held) {
            return;
        }
        let Some(h) = self.hold_log.take() else {
            return;
        };
        self.event(format!(
            "released {} LLM request(s) held while {} used {} ({:.0}s, longest wait {:.0}s)",
            h.released,
            h.behind_class,
            h.behind_model,
            h.since.elapsed().as_secs_f64(),
            h.longest.as_secs_f64()
        ));
    }

    fn enter_class(&mut self, class: &str, model: &str) {
        let model = self.backend.resolve(model);
        let now = Instant::now();
        let u = self.llm_use.entry(class.to_owned()).or_insert(ClassUse {
            model: model.clone(),
            inflight: 0,
            last: now,
        });
        u.model = model;
        u.inflight += 1;
        u.last = now;
    }

    fn exit_class(&mut self, class: &str, model: &str) {
        if let Some(u) = self.llm_use.get_mut(class) {
            u.inflight = u.inflight.saturating_sub(1);
            u.last = Instant::now();
            if u.inflight == 0 {
                u.model = self.backend.resolve(model);
            }
        }
    }

    /// The cards a request names. Unknown names were refused by the handle already.
    fn resolve(&self, sel: &CardSel) -> Target {
        let set = |v: Vec<usize>| match v[..] {
            [] => Target::Card(0),
            [c] => Target::Card(c),
            _ => Target::Set(v),
        };
        match sel {
            CardSel::Default if self.steer.iter().any(|&s| s) => Target::Any,
            CardSel::Default | CardSel::Primary => Target::Card(0),
            CardSel::Named(n) => Target::Card(self.index(n).unwrap_or(0)),
            CardSel::Any | CardSel::Count(0 | 1) => Target::Any,
            CardSel::Count(n) => Target::Count(*n),
            CardSel::Set(names) => set(dedup(names.iter().filter_map(|n| self.index(n)))),
            CardSel::All => set((0..self.cards.len()).collect()),
        }
    }

    fn acquire(&mut self, id: u64, req: LeaseRequest, reply: oneshot::Sender<Granted>) {
        let prio = self.classes.job_prio(&req.class);
        let target = self.resolve(&req.card);
        if let Some(resume) = req.resume_id.clone().filter(|r| !r.is_empty()) {
            let mut cards = target.fixed();
            if cards.is_empty() {
                cards = (0..self.cards.len())
                    .filter(|&c| self.steer[c])
                    .take(target.need())
                    .collect();
            }
            if cards.is_empty() {
                cards.push(0);
            }
            let lease = LeaseRec::new(resume.clone(), None, &req.class, prio, &req, cards);
            let on = self.on_set(&lease.0.cards);
            if lease
                .0
                .cards
                .iter()
                .any(|&c| matches!(self.cards[c].state, State::Llm { .. }))
            {
                let kind = self.backend.kind();
                self.warn(format!(
                    "lease {resume} resumed while {kind} holds the card"
                ));
            }
            self.event(format!(
                "resume lease {resume} {}/{}{on}",
                req.class, req.name
            ));
            self.install(lease, reply);
            return;
        }
        let seq = self.take_seq();
        let on = if self.cards.len() > 1 {
            format!(" on {}", self.target_name(&target))
        } else {
            String::new()
        };
        self.event(format!(
            "queue {}/{}{on} vram={}",
            req.class, req.name, req.vram
        ));
        self.waiters.push(Waiter {
            id,
            seq,
            prio,
            class: req.class,
            name: req.name,
            since: SystemTime::now(),
            note: String::new(),
            held_at: None,
            model_held: false,
            target,
            want: Want::Job {
                vram: req.vram,
                yieldable: req.yieldable,
                pid: req.pid,
                reply,
            },
        });
    }

    fn take_seq(&mut self) -> u64 {
        self.next_seq += 1;
        self.next_seq
    }

    /// Record a lease on its cards and tell its owner. If the owner is already gone, take it
    /// back.
    fn install(
        &mut self,
        (lease, yield_rx): (LeaseRec, watch::Receiver<bool>),
        reply: oneshot::Sender<Granted>,
    ) {
        let id = lease.id.clone();
        let granted = Granted {
            id: id.clone(),
            cards: self.names(&lease.cards),
            env: self.lease_env(&lease.cards),
            yield_rx,
        };
        self.leases.push(lease);
        if reply.send(granted).is_err() {
            if let Some(i) = self.leases.iter().position(|l| l.id == id) {
                let lease = self.leases.remove(i);
                self.log_release(&lease);
            }
        }
    }

    fn cancel(&mut self, id: u64) {
        if let Some(i) = self.waiters.iter().position(|w| w.id == id) {
            self.waiters.remove(i);
            return;
        }
        if let Some(i) = self.leases.iter().position(|l| l.handle == Some(id)) {
            // Granted in the same breath as the client went away.
            let lease = self.leases.remove(i);
            self.after_release(lease);
        } else if let Some(i) = self.recent_admits.iter().position(|a| a.0 == id) {
            if let Some((_, class, model)) = self.recent_admits.remove(i) {
                self.exit_class(&class, &model);
            }
            self.inflight = self.inflight.saturating_sub(1);
            self.last_llm = Instant::now();
        }
    }

    fn release(&mut self, id: &str) {
        if let Some(i) = self.leases.iter().position(|l| l.id == id) {
            let lease = self.leases.remove(i);
            self.after_release(lease);
        }
    }

    fn log_release(&mut self, lease: &LeaseRec) {
        let on = self.on_set(&lease.cards);
        self.event(format!(
            "release lease {} {}/{}{on} after {:.0}s",
            lease.id,
            lease.class,
            lease.name,
            age_s(lease.since)
        ));
    }

    fn after_release(&mut self, lease: LeaseRec) {
        self.log_release(&lease);
        for &c in &lease.cards {
            if !self.busy(c) {
                let card = &mut self.cards[c];
                card.baseline = None; // re-measured once the job's memory is gone
                card.released_at = Some(Instant::now());
                card.released_class = lease.class.clone();
            }
        }
    }

    fn admit(&mut self, id: u64, class: &str, model: &str) {
        self.inflight += 1;
        self.last_llm = Instant::now();
        self.last_llm_class = class.to_owned();
        self.enter_class(class, model);
        if self.recent_admits.len() == RECENT_ADMITS {
            self.recent_admits.pop_front();
        }
        self.recent_admits
            .push_back((id, class.to_owned(), model.to_owned()));
    }

    // -------------------------------------------------------------- scheduling

    /// Whether card `i` needs a fresh reading this pass.
    fn needs_read(&self, i: usize) -> bool {
        let c = &self.cards[i];
        if !c.state.is_idle_jobs() {
            return false;
        }
        if !self.busy(i) {
            // Not before the driver has had time to hand the memory back.
            return c
                .released_at
                .map_or(true, |at| Instant::now() >= at + self.t.driver_settle);
        }
        self.top_for(i).is_some_and(|k| !self.waiters[k].is_llm())
    }

    /// Readings of every card that needs one, taken concurrently.
    async fn preread(&mut self) -> Vec<Option<VramReading>> {
        let which: Vec<usize> = (0..self.cards.len())
            .filter(|&i| self.needs_read(i))
            .collect();
        self.read_cards(&which).await
    }

    async fn pass(&mut self) {
        self.deadline = None;
        for _ in 0..MAX_PASSES {
            let readings = self.preread().await;
            let llm_was_home = self.llm_phase() == Some(Phase::Llm);
            let mut again = false;
            // Job cards first, so an `Any` job that fits somewhere is placed before the LLM
            // is asked to hand over.
            for i in 0..self.cards.len() {
                if matches!(self.cards[i].state, State::Jobs { .. }) {
                    again |= self.step_jobs(i, &readings);
                }
            }
            if llm_was_home && self.llm_phase() == Some(Phase::Llm) {
                again |= self.step_llm(&readings);
            }
            // Unloading and Restoring are driven by their effect.
            if !again {
                return;
            }
        }
    }

    /// An effect finished: move to the phase it was heading for.
    fn finish(&mut self, card: usize, done: Done) {
        match done {
            Done::Unloaded(u) => self.finish_unload(u),
            Done::Reloaded(r) => self.finish_reload(r),
            Done::Offloaded(measured) => {
                if measured.is_some() {
                    self.cards[card].baseline = measured;
                }
                self.cards[card].state = State::Jobs { offload: None };
            }
        }
    }

    fn finish_unload(&mut self, u: Unloaded) {
        self.saved.extend(u.saved);
        let group: Vec<usize> = (0..self.cards.len())
            .filter(|&c| matches!(self.cards[c].state, State::Unloading { .. }))
            .collect();
        match u.result {
            Err(e) => {
                self.event(format!("unload failed ({e}); staying in llm state"));
                let retry = Some(Instant::now() + UNLOAD_BACKOFF);
                for c in group {
                    self.enter_llm_state(c, retry);
                }
            }
            Ok(used) => {
                let mut levels = Vec::new();
                for (k, &c) in group.iter().enumerate() {
                    let u = used.get(k).copied().flatten();
                    let card = &mut self.cards[c];
                    card.baseline = u;
                    card.floor = u;
                    card.state = State::Jobs { offload: None };
                    levels.push(u.map_or_else(|| "?".to_owned(), |m| m.to_string()));
                }
                let secs = u.elapsed.as_secs_f64();
                if group.len() > 1 {
                    let names = self.names(&group).join(",");
                    self.event(format!(
                        "cards {names} free in {secs:.1}s (VRAM {} MiB still in use)",
                        levels.join(", ")
                    ));
                } else {
                    self.event(format!(
                        "card free in {secs:.1}s (VRAM {} MiB still in use)",
                        levels.join(", ")
                    ));
                }
            }
        }
    }

    fn finish_reload(&mut self, r: Reloaded) {
        if r.had_saved {
            if let Some(resident) = self.resident.clone() {
                self.saved.remove(&resident);
                if !r.loaded {
                    // Don't make every later request wait out another load timeout for it.
                    self.event(format!("{resident} did not load; dropped its saved slot"));
                }
            }
        }
        for c in 0..self.cards.len() {
            if matches!(self.cards[c].state, State::Restoring { .. }) {
                self.enter_llm_state(c, None);
            }
        }
        if r.kind == RestoreKind::Card {
            self.last_llm = Instant::now();
            self.event(format!("llm state ({:.1}s)", r.elapsed.as_secs_f64()));
        }
    }

    fn enter_llm_state(&mut self, card: usize, retry_at: Option<Instant>) {
        self.cards[card].state = State::Llm { retry_at };
        self.cards[card].offload_tried.clear();
    }

    /// The best waiter that can use `card`.
    fn top_for(&self, card: usize) -> Option<usize> {
        self.waiters
            .iter()
            .enumerate()
            .filter(|(_, w)| w.can_use(card, &self.steer))
            .min_by_key(|(_, w)| w.key())
            .map(|(i, _)| i)
    }

    /// Set a waiter's `why`. A waiter that is not pinned to one card also says which card it
    /// is talking about.
    fn note(&mut self, idx: usize, card: usize, text: String) {
        let w = &mut self.waiters[idx];
        w.note = if matches!(w.target, Target::Card(_)) {
            text
        } else {
            format!("{}: {text}", self.cards[card].name)
        };
    }

    /// How long the LLM must have been quiet before `job` may take the card.
    fn required_idle(&self, class: &str, prio: u32) -> Duration {
        if self.t.idle_wait_classes.contains(class) {
            self.t.idle
        } else if prio > self.classes.llm() {
            self.t.gap_after(&self.last_llm_class)
        } else {
            Duration::ZERO // outranks the LLM: no waiting beyond in-flight requests
        }
    }

    fn needs_slot_restore(&self, model: &str) -> bool {
        self.resident
            .as_ref()
            .is_some_and(|r| self.saved.contains(r) && self.backend.resolve(model) == *r)
    }

    /// Let held LLM requests through, unless a better job waits. Returns true when the
    /// resident model has to be reloaded first.
    fn admit_llm(&mut self) -> bool {
        let mut keys: Vec<_> = self
            .waiters
            .iter()
            .filter(|w| w.is_llm())
            .map(Waiter::key)
            .collect();
        keys.sort_unstable();
        for key in keys {
            let Some(idx) = self.waiters.iter().position(|w| w.key() == key) else {
                continue;
            };
            let cards = self.waiters[idx].target.fixed();
            // A model that needs a card the LLM does not hold yet waits for step_jobs.
            if !cards
                .iter()
                .all(|&c| matches!(self.cards[c].state, State::Llm { .. }))
            {
                continue;
            }
            if blocked(&self.waiters, key, &cards, &self.steer) {
                continue;
            }
            if self.still_held(idx) {
                continue;
            }
            let restore = match &self.waiters[idx].want {
                Want::Llm { model, .. } => self.needs_slot_restore(model),
                Want::Job { .. } => continue,
            };
            if restore {
                if let Some(resident) = self.resident.clone() {
                    self.event(format!("restoring {resident} slot before serving it"));
                    let set = self.llm_set();
                    self.start_reload(resident, RestoreKind::Slot, set);
                    return true;
                }
            }
            let w = self.waiters.remove(idx);
            let Want::Llm { model, reply } = w.want else {
                continue;
            };
            if w.model_held {
                if let Some(h) = &mut self.hold_log {
                    h.released += 1;
                    h.longest = h.longest.max(Duration::from_secs_f64(age_s(w.since)));
                }
            } else {
                self.event(format!(
                    "release held {} request for {model} after {:.0}s",
                    w.class,
                    age_s(w.since)
                ));
            }
            if reply.send(()).is_ok() {
                self.admit(w.id, &w.class, &model);
            }
        }
        self.log_hold_end();
        false
    }

    /// Whether waiter `idx` must keep waiting for another class's model. Updates its note
    /// and asks for a pass when the hold runs out.
    fn still_held(&mut self, idx: usize) -> bool {
        let w = &self.waiters[idx];
        let Want::Llm { model, .. } = &w.want else {
            return false;
        };
        match self.held_behind(model, w.prio, Instant::now()) {
            Some(b) => {
                self.log_hold_start(&b);
                if let Some(until) = b.until {
                    self.want(until);
                }
                let w = &mut self.waiters[idx];
                w.model_held = true;
                w.note = hold_note(&b);
                true
            }
            None => false,
        }
    }

    /// Llm phase of the LLM's cards: hand them to the top job once it may have them.
    /// True = phase changed.
    fn step_llm(&mut self, readings: &[Option<VramReading>]) -> bool {
        if self.admit_llm() {
            return true;
        }
        let set = self.llm_set();
        // The best job that is first in line on one of the LLM's cards.
        let best = set
            .iter()
            .filter_map(|&c| {
                self.top_for(c)
                    .filter(|&k| !self.waiters[k].is_llm())
                    .map(|k| (k, c))
            })
            .min_by_key(|&(k, _)| self.waiters[k].key());
        let Some((idx, l)) = best else {
            return false;
        };
        // A job for several cards only takes the LLM's cards once the rest of what it needs
        // is free: no point unloading the LLM while it would still wait for another card.
        if !self.ready_elsewhere(idx, &set, readings) {
            return false;
        }
        if self.inflight > 0 {
            let text = format!("waiting for {} in-flight LLM request(s)", self.inflight);
            self.note(idx, l, text);
            return false;
        }
        let need = self.required_idle(&self.waiters[idx].class, self.waiters[idx].prio);
        let now = Instant::now();
        let idle = now.saturating_duration_since(self.last_llm);
        if idle < need {
            let text = format!(
                "LLM ({}) used {:.0}s ago; card is handed over after {}s idle",
                self.last_llm_class,
                idle.as_secs_f64(),
                need.as_secs_f64()
            );
            self.note(idx, l, text);
            self.want(self.last_llm + need);
            return false;
        }
        if let State::Llm {
            retry_at: Some(at), ..
        } = self.cards[l].state
        {
            if now < at {
                self.want(at); // backend trouble: retry calmly
                return false;
            }
        }
        self.start_unload(set);
        true
    }

    /// Unload the backend from all of its cards.
    fn start_unload(&mut self, set: Vec<usize>) {
        let kind = self.backend.kind();
        if set.len() > 1 {
            let names = self.names(&set).join(",");
            self.event(format!("unloading {kind} from {names}"));
        } else {
            self.event(format!("unloading {kind}"));
        }
        let fx: Fx<Unloaded> = Box::pin(unload(self.env(&set)));
        for &c in &set[1..] {
            self.cards[c].state = State::Unloading { fx: None };
        }
        self.cards[set[0]].state = State::Unloading { fx: Some(fx) };
    }

    /// Whether a job that is first in line on an LLM card has the rest of its cards ready,
    /// so the LLM can be unloaded for it.
    fn ready_elsewhere(&self, idx: usize, llm: &[usize], readings: &[Option<VramReading>]) -> bool {
        let w = &self.waiters[idx];
        let Some(vram) = w.vram() else {
            return true;
        };
        let ready = |c: usize| {
            self.cards[c].state.is_idle_jobs()
                && self.top_for(c) == Some(idx)
                && readings[c].is_some_and(|r| self.fits_now(c, vram, r))
        };
        match &w.target {
            Target::Set(s) => s.iter().filter(|c| !llm.contains(c)).all(|&c| ready(c)),
            Target::Count(n) => {
                let inside = llm.iter().filter(|&&c| self.steer[c]).count();
                let outside = (0..self.cards.len())
                    .filter(|&c| self.steer[c] && !llm.contains(&c) && ready(c))
                    .count();
                inside + outside >= *n
            }
            Target::Card(_) | Target::Any => true,
        }
    }

    /// Jobs phase of card `i`: grant leases, or give the card (back) to the LLM.
    /// True = go again.
    fn step_jobs(&mut self, i: usize, readings: &[Option<VramReading>]) -> bool {
        if !self.cards[i].state.is_idle_jobs() {
            return false; // the hook is running; its completion triggers the next pass
        }
        if !self.busy(i) {
            // Once the driver has had time to hand the memory back, measure what is left.
            if let Some(at) = self.cards[i].released_at {
                let ready = at + self.t.driver_settle;
                if Instant::now() < ready {
                    self.want(ready);
                    return false;
                }
            }
            if let Some(r) = readings[i] {
                self.cards[i].baseline = Some(r.used_mib);
            }
        }
        let Some(idx) = self.top_for(i) else {
            return self.maybe_reload_idle(i);
        };
        if self.waiters[idx].is_llm() {
            self.serve_llm_waiter(i, idx)
        } else {
            self.serve_job_waiter(i, idx, readings)
        }
    }

    /// The LLM's cards have been left alone long enough: bring the resident model back.
    fn maybe_reload_idle(&mut self, i: usize) -> bool {
        if self.llm_phase().is_some()
            || self.busy(i)
            || !(self.t.reload_when_idle || self.resident.is_none())
        {
            return false;
        }
        let cards = self.llm_return();
        if !cards.contains(&i) {
            return false;
        }
        let mut not_before = Instant::now();
        for &c in &cards {
            if !self.cards[c].state.is_idle_jobs() || self.busy(c) || self.top_for(c).is_some() {
                return false;
            }
            let card = &self.cards[c];
            let delay = self.t.reload_delay_after(&card.released_class);
            let at = match card.released_at {
                Some(at) => card.grace_until.max(at + delay),
                None => card.grace_until,
            };
            not_before = not_before.max(at);
        }
        if Instant::now() >= not_before {
            self.start_restore(None);
            true
        } else {
            self.want(not_before);
            false
        }
    }

    fn serve_llm_waiter(&mut self, i: usize, idx: usize) -> bool {
        // Loading this model would push out the one a more important class is using.
        if self.still_held(idx) {
            return false;
        }
        if self.busy(i) {
            if self.hold_for_batch(i, idx, Instant::now()) {
                return false;
            }
            self.request_yield(i, self.waiters[idx].prio);
            self.waiters[idx].note = format!("waiting for GPU job(s): {}", self.lease_names(i));
            return false;
        }
        match self.llm_phase() {
            Some(Phase::Llm) => {
                // The LLM is up on other cards and this model needs this one too.
                self.enter_llm_state(i, None);
                let name = self.cards[i].name.clone();
                self.event(format!("llm takes {name}"));
                self.place(&self.llm_set());
                true
            }
            // An unload or a reload is under way; its end triggers the next pass.
            Some(_) => false,
            None => {
                let cards = self.waiters[idx].target.fixed();
                let free = cards.iter().all(|&c| {
                    self.cards[c].state.is_idle_jobs()
                        && !self.busy(c)
                        && self.top_for(c) == Some(idx)
                });
                if !free {
                    return false;
                }
                let model = match &self.waiters[idx].want {
                    Want::Llm { model, .. } => model.clone(),
                    Want::Job { .. } => return false,
                };
                self.start_restore(Some(&model));
                true
            }
        }
    }

    fn serve_job_waiter(&mut self, i: usize, idx: usize, readings: &[Option<VramReading>]) -> bool {
        let Some(reading) = readings[i] else {
            let why = self.cards[i].probe_error.clone().unwrap_or_default();
            self.note(idx, i, format!("cannot read VRAM: {why}"));
            return false;
        };
        let Some(vram) = self.waiters[idx].vram() else {
            return false;
        };
        let prio = self.waiters[idx].prio;
        if let Some(cards) = self.pick(idx, readings) {
            self.grant(&cards, idx);
            return true;
        }
        // A pinned job works on its own card. A job that can go to several cards picks the
        // ones to push on, so the other cards are not all asked to yield for it.
        if self.push_cards(idx, readings).contains(&i) && !self.fits_now(i, vram, reading) {
            self.request_yield(i, prio);
            if !self.busy(i) {
                self.maybe_offload(i, idx);
            }
        }
        self.explain(idx, i, readings);
        false
    }

    /// The cards to grant a job now, all of them fitting, or None.
    fn pick(&self, idx: usize, readings: &[Option<VramReading>]) -> Option<Vec<usize>> {
        let w = &self.waiters[idx];
        let vram = w.vram()?;
        let ok = |c: usize| {
            self.cards[c].state.is_idle_jobs()
                && self.top_for(c) == Some(idx)
                && readings[c].is_some_and(|r| self.fits_now(c, vram, r))
        };
        match &w.target {
            Target::Card(c) => ok(*c).then(|| vec![*c]),
            Target::Set(s) => s.iter().all(|&c| ok(c)).then(|| s.clone()),
            Target::Any | Target::Count(_) => {
                let need = w.target.need();
                let mut picked: Vec<usize> = Vec::new();
                for c in 0..self.cards.len() {
                    if picked.len() == need {
                        break;
                    }
                    if !self.steer[c] || !ok(c) {
                        continue;
                    }
                    picked.push(c);
                    if !self.env_ok(&picked) {
                        picked.pop();
                    }
                }
                (picked.len() == need).then_some(picked)
            }
        }
    }

    /// The cards a job that cannot be granted yet pushes on: asks their holders to yield,
    /// runs the offload hook on them.
    fn push_cards(&self, idx: usize, readings: &[Option<VramReading>]) -> Vec<usize> {
        let w = &self.waiters[idx];
        let Some(vram) = w.vram() else {
            return Vec::new();
        };
        // Free now, or free once lower yieldable leases leave (or the hook ran).
        let fits = |c: usize| readings[c].is_some_and(|r| self.fits_now(c, vram, r));
        let room = |c: usize| readings[c].is_some_and(|r| self.could_make_room(c, idx, r));
        let first = |c: usize| self.cards[c].state.is_idle_jobs() && self.top_for(c) == Some(idx);
        match &w.target {
            Target::Card(c) => vec![*c],
            Target::Any => self.any_target(idx, readings).into_iter().collect(),
            Target::Set(s) => {
                // Only when every card could be had: emptying one card is no use while
                // another stays blocked.
                let could = |c: usize| match self.cards[c].state {
                    State::Jobs { offload: None } => first(c) && (fits(c) || room(c)),
                    // The hook is running there, or the LLM holds it and is unloaded once
                    // the rest is ready.
                    _ => true,
                };
                if s.iter().all(|&c| could(c)) {
                    s.clone()
                } else {
                    Vec::new()
                }
            }
            Target::Count(n) => {
                let cands = (0..self.cards.len()).filter(|&c| self.steer[c]);
                let ready = cands.clone().filter(|&c| first(c) && fits(c)).count();
                let llm = cands
                    .clone()
                    .filter(|&c| self.cards[c].state.is_llm())
                    .count();
                let pushable: Vec<usize> =
                    cands.filter(|&c| first(c) && !fits(c) && room(c)).collect();
                let short = n.saturating_sub(ready + llm);
                if ready + llm + pushable.len() >= *n {
                    pushable.into_iter().take(short).collect()
                } else {
                    Vec::new()
                }
            }
        }
    }

    /// Fill in a job's `why`.
    fn explain(&mut self, idx: usize, i: usize, readings: &[Option<VramReading>]) {
        let Some(vram) = self.waiters[idx].vram() else {
            return;
        };
        match self.waiters[idx].target.clone() {
            Target::Card(_) => {
                if let Some(r) = readings[i] {
                    let text = self.why_not_fit(i, vram, r);
                    self.note(idx, i, text);
                }
            }
            Target::Any => {
                let j = self.any_target(idx, readings).unwrap_or(i);
                if let Some(r) = readings[j] {
                    let text = self.why_not_fit(j, vram, r);
                    self.note(idx, j, text);
                }
            }
            target => {
                let cards: Vec<usize> = match &target {
                    Target::Set(s) => s.clone(),
                    _ => (0..self.cards.len()).filter(|&c| self.steer[c]).collect(),
                };
                let mut ready = 0;
                let mut first_reason = None;
                for &c in &cards {
                    let reason = if self.cards[c].state.is_llm() {
                        Some(format!(
                            "held by the LLM ({})",
                            self.cards[c].state.phase().name()
                        ))
                    } else if !self.cards[c].state.is_idle_jobs() {
                        Some("offload hook running".to_owned())
                    } else if self.top_for(c) != Some(idx) {
                        Some("a lease ahead in the queue waits for it".to_owned())
                    } else {
                        match readings[c] {
                            None => Some("cannot read VRAM".to_owned()),
                            Some(r) if !self.fits_now(c, vram, r) => {
                                Some(self.why_not_fit(c, vram, r))
                            }
                            Some(_) => None,
                        }
                    };
                    match reason {
                        None => ready += 1,
                        Some(r) if first_reason.is_none() => {
                            first_reason = Some(format!("{}: {r}", self.cards[c].name));
                        }
                        Some(_) => {}
                    }
                }
                let need = target.need();
                let reason = first_reason.unwrap_or_default();
                self.waiters[idx].note = format!("{ready} of {need} cards ready; {reason}");
            }
        }
    }

    fn fits_now(&self, card: usize, vram: Vram, reading: VramReading) -> bool {
        let c = &self.cards[card];
        fits(vram, &self.held_vram(card), c.baseline, reading, c.limits)
    }

    fn why_not_fit(&self, card: usize, vram: Vram, reading: VramReading) -> String {
        let c = &self.cards[card];
        match vram {
            Vram::WholeCard if self.busy(card) => {
                "needs the whole card: other leases active".into()
            }
            Vram::WholeCard => match c.baseline {
                Some(b) => {
                    format!("needs the whole card: {b} MiB still in use by other processes")
                }
                None => "needs the whole card: VRAM not measured yet".into(),
            },
            Vram::Mib(m) => {
                let free = free_mib(
                    reading,
                    c.baseline,
                    declared_mib(&self.held_vram(card)),
                    c.limits.margin_mib,
                );
                format!("needs {m} MiB, {free} MiB free")
            }
        }
    }

    /// The one card an unplaced `Any` job pushes on (yield, offload): the first steerable job
    /// card, in config order, where it is the top waiter and asking could make room.
    fn any_target(&self, idx: usize, readings: &[Option<VramReading>]) -> Option<usize> {
        (0..self.cards.len()).find(|&j| {
            self.steer[j]
                && self.cards[j].state.is_idle_jobs()
                && self.top_for(j) == Some(idx)
                && readings[j].is_some_and(|r| self.could_make_room(j, idx, r))
        })
    }

    /// Whether yielding (or, on an empty card, the offload hook) could make the job fit on `card`.
    fn could_make_room(&self, card: usize, idx: usize, reading: VramReading) -> bool {
        let w = &self.waiters[idx];
        let Some(vram) = w.vram() else {
            return false;
        };
        let c = &self.cards[card];
        let held: Vec<&LeaseRec> = self.held(card).collect();
        if held.is_empty() {
            return self.hook.is_some() && !c.offload_tried.contains(&w.seq);
        }
        let stays: Vec<Vram> = held
            .iter()
            .filter(|l| !(l.yieldable && l.prio > w.prio))
            .map(|l| l.vram)
            .collect();
        if stays.len() == held.len() {
            return false;
        }
        // Once the yielders are gone, use falls back to about the baseline.
        let after = VramReading {
            used_mib: c.baseline.unwrap_or(reading.used_mib),
            ..reading
        };
        fits(vram, &stays, c.baseline, after, c.limits)
    }

    fn lease_names(&self, card: usize) -> String {
        let names: Vec<_> = self
            .held(card)
            .map(|l| format!("{}/{}", l.class, l.name))
            .collect();
        names.join(", ")
    }

    /// True while an LLM request should keep waiting behind yieldable batch leases on `card`.
    fn hold_for_batch(&mut self, card: usize, idx: usize, now: Instant) -> bool {
        let hold = self.t.batch_hold;
        if hold.is_zero() || self.waiters[idx].prio != self.classes.llm() {
            return false;
        }
        let batch = self.classes.batch();
        if self
            .held(card)
            .any(|l| Some(l.prio) != batch || !l.yieldable)
        {
            return false;
        }
        if self.held(card).any(|l| *l.yield_tx.borrow()) {
            return false; // hand-over already under way
        }
        let held_at = match self.waiters[idx].held_at {
            Some(t) => t,
            None => {
                self.waiters[idx].held_at = Some(now);
                let msg = format!(
                    "hold {} request up to {}s behind batch job(s)",
                    self.waiters[idx].class,
                    hold.as_secs_f64()
                );
                self.event(msg);
                now
            }
        };
        let until = held_at + hold;
        if now >= until {
            return false;
        }
        let queued = self.waiters.iter().filter(|w| w.is_llm()).count();
        if self.t.batch_hold_count > 0 && queued >= self.t.batch_hold_count {
            self.event(format!("{queued} LLM requests queued, ending batch hold"));
            return false;
        }
        self.waiters[idx].note = format!(
            "batched behind {}; {queued} queued, job asked to yield in {:.0}s",
            self.lease_names(card),
            until.saturating_duration_since(now).as_secs_f64()
        );
        self.want(until);
        true
    }

    /// Ask every yieldable lease on `card` that ranks below `prio` to stop at its next safe
    /// point. A lease on several cards is asked as soon as one of its cards is wanted.
    fn request_yield(&mut self, card: usize, prio: u32) {
        let on = self.on(card);
        let mut asked = Vec::new();
        for l in self.held(card) {
            if l.yieldable && l.prio > prio && !*l.yield_tx.borrow() {
                l.yield_tx.send_replace(true);
                asked.push(format!(
                    "ask {}/{} ({}){on} to yield",
                    l.class, l.name, l.id
                ));
            }
        }
        for msg in asked {
            self.event(msg);
        }
    }

    fn grant(&mut self, cards: &[usize], idx: usize) {
        let w = self.waiters.remove(idx);
        let Want::Job {
            vram,
            yieldable,
            pid,
            reply,
        } = w.want
        else {
            return;
        };
        let id = format!("L{:x}-{}", self.boot, self.next_lease);
        self.next_lease += 1;
        let (yield_tx, yield_rx) = watch::channel(false);
        let lease = LeaseRec {
            id: id.clone(),
            handle: Some(w.id),
            class: w.class.clone(),
            prio: w.prio,
            name: w.name.clone(),
            vram,
            yieldable,
            yield_tx,
            since: SystemTime::now(),
            pid,
            cards: cards.to_vec(),
        };
        let on = self.on_set(cards);
        self.event(format!(
            "grant {id} {}/{}{on} vram={vram} after {:.0}s",
            w.class,
            w.name,
            age_s(w.since)
        ));
        self.install((lease, yield_rx), reply);
    }

    // -------------------------------------------------------------- card hand-over

    /// Back to the LLM state, reloading the resident model if that is what comes next.
    fn start_restore(&mut self, model: Option<&str>) {
        let (target, cards) = match model {
            Some(m) => (Some(self.backend.resolve(m)), self.placement(m)),
            None => (self.resident.clone(), self.llm_return()),
        };
        match (self.resident.clone(), target) {
            (Some(resident), Some(target)) if resident == target => {
                self.start_reload(resident, RestoreKind::Card, cards);
            }
            _ => {
                // Any other model is loaded by the backend itself when the held request goes
                // through.
                for &c in &cards {
                    self.enter_llm_state(c, None);
                }
                self.place(&cards);
                self.last_llm = Instant::now();
                self.event("llm state (0.0s)");
            }
        }
    }

    /// Reload the resident model onto `cards`, which become the LLM's.
    fn start_reload(&mut self, resident: String, kind: RestoreKind, cards: Vec<usize>) {
        let had_saved = self.saved.contains(&resident);
        let floors = if kind == RestoreKind::Card {
            cards.iter().map(|&c| self.cards[c].floor).collect()
        } else {
            vec![None; cards.len()]
        };
        if kind == RestoreKind::Card {
            self.place(&cards);
        }
        let fx: Fx<Reloaded> =
            Box::pin(reload(self.env(&cards), resident, had_saved, floors, kind));
        for &c in &cards[1..] {
            self.cards[c].state = State::Restoring { fx: None };
        }
        self.cards[cards[0]].state = State::Restoring { fx: Some(fx) };
    }

    /// Run the offload hook once per waiting job and card, then re-measure the card.
    fn maybe_offload(&mut self, card: usize, idx: usize) {
        let Some(hook) = self.hook.clone() else {
            return;
        };
        let w = &self.waiters[idx];
        if self.cards[card].offload_tried.contains(&w.seq) {
            return;
        }
        let (seq, msg) = (w.seq, format!("offload hook for {}/{}", w.class, w.name));
        self.cards[card].offload_tried.insert(seq);
        let on = self.on(card);
        self.event(format!("{msg}{on}"));
        self.cards[card].state = State::Jobs {
            offload: Some(Box::pin(offload(self.env(&[card]), hook))),
        };
    }

    // -------------------------------------------------------------- reporting

    async fn status(&mut self) -> Status {
        let reads = join_all(self.cards.iter().map(|c| timed_read(c.probe.clone()))).await;
        let mut queue: Vec<&Waiter> = self.waiters.iter().collect();
        queue.sort_by_key(|w| w.key());
        let llm_set = self.llm_set();
        let llm_cards = if llm_set.is_empty() {
            self.llm_return()
        } else {
            llm_set.clone()
        };
        let cards: Vec<CardStatus> = self
            .cards
            .iter()
            .zip(reads)
            .enumerate()
            .map(|(i, (c, read))| {
                let (vram, vram_error) = match read {
                    Ok(r) => (Some(r), None),
                    Err(e) => (None, Some(e)),
                };
                CardStatus {
                    name: c.name.clone(),
                    device: c.probe.device(),
                    state: c.state.phase(),
                    llm: llm_cards.contains(&i),
                    baseline_mib: c.baseline,
                    floor_mib: c.floor,
                    vram,
                    vram_error,
                }
            })
            .collect();
        let primary = cards[0].clone();
        Status {
            state: primary.state,
            device: primary.device,
            inflight_llm: self.inflight,
            llm_idle_s: Instant::now()
                .saturating_duration_since(self.last_llm)
                .as_secs_f64()
                .round() as u64,
            idle_s: self.t.idle.as_secs(),
            baseline_mib: primary.baseline_mib,
            floor_mib: primary.floor_mib,
            vram: primary.vram,
            vram_error: primary.vram_error,
            leases: self
                .leases
                .iter()
                .map(|l| {
                    let names = self.names(&l.cards);
                    LeaseStatus {
                        id: l.id.clone(),
                        card: names.join(","),
                        cards: names,
                        class: l.class.clone(),
                        name: l.name.clone(),
                        vram: l.vram,
                        yieldable: l.yieldable,
                        yield_requested: *l.yield_tx.borrow(),
                        age_s: age_s(l.since).round() as u64,
                        pid: l.pid,
                    }
                })
                .collect(),
            queue: queue
                .into_iter()
                .map(|w| {
                    let (kind, vram) = match &w.want {
                        Want::Job { vram, .. } => (WaiterKind::Job, Some(*vram)),
                        Want::Llm { .. } => (WaiterKind::Llm, None),
                    };
                    QueueStatus {
                        kind,
                        card: self.target_name(&w.target),
                        class: w.class.clone(),
                        name: w.name.clone(),
                        vram,
                        wait_s: age_s(w.since).round() as u64,
                        why: w.note.clone(),
                    }
                })
                .collect(),
            history: self
                .history
                .iter()
                .map(|(t, msg)| HistoryEntry {
                    t: unix_secs(*t),
                    msg: msg.clone(),
                })
                .collect(),
            cards,
            llm_cards: self.names(&llm_set),
            model_hold: self.model_hold_status(),
        }
    }

    fn model_hold_status(&self) -> Option<ModelHold> {
        let h = self.hold_log.as_ref()?;
        let held: Vec<&Waiter> = self.waiters.iter().filter(|w| w.model_held).collect();
        if held.is_empty() {
            return None;
        }
        let mut models: Vec<String> = held.iter().map(|w| w.name.clone()).collect();
        models.sort();
        models.dedup();
        Some(ModelHold {
            behind_class: h.behind_class.clone(),
            behind_model: h.behind_model.clone(),
            held: held.len(),
            models,
            since_s: h.since.elapsed().as_secs(),
        })
    }
}

fn hold_note(b: &HeldBehind) -> String {
    match b.until {
        None => format!("held while {} uses {}", b.class, b.model),
        Some(t) => format!(
            "held while {} uses {} (quiet for {:.0}s more)",
            b.class,
            b.model,
            t.saturating_duration_since(Instant::now()).as_secs_f64()
        ),
    }
}

impl LeaseRec {
    /// A lease adopted without queueing; the receiver goes to the owner.
    fn new(
        id: String,
        handle: Option<u64>,
        class: &str,
        prio: u32,
        req: &LeaseRequest,
        cards: Vec<usize>,
    ) -> (Self, watch::Receiver<bool>) {
        let (yield_tx, yield_rx) = watch::channel(false);
        let lease = LeaseRec {
            id,
            handle,
            class: class.to_owned(),
            prio,
            name: req.name.clone(),
            vram: req.vram,
            yieldable: req.yieldable,
            yield_tx,
            since: SystemTime::now(),
            pid: req.pid,
            cards,
        };
        (lease, yield_rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIM: Limits = Limits {
        margin_mib: 256,
        whole_card_baseline_mib: 512,
    };

    fn reading(used: Mib, total: Mib) -> VramReading {
        VramReading {
            used_mib: used,
            total_mib: total,
            gtt_used_mib: None,
        }
    }

    #[test]
    fn free_counts_declared_leases_on_top_of_baseline() {
        // 500 baseline + 6000 declared = 6500 committed, used is lower.
        let free = free_mib(reading(1000, 24_560), Some(500), 6000, 256);
        assert_eq!(free, 24_560 - 6500 - 256);
    }

    #[test]
    fn free_trusts_actual_use_when_it_is_higher() {
        let free = free_mib(reading(10_000, 24_560), Some(500), 1000, 256);
        assert_eq!(free, 24_560 - 10_000 - 256);
    }

    #[test]
    fn free_saturates_instead_of_wrapping() {
        assert_eq!(free_mib(reading(30_000, 24_560), None, u64::MAX, 256), 0);
        assert_eq!(free_mib(reading(0, 100), Some(u64::MAX), u64::MAX, 0), 0);
    }

    #[test]
    fn whole_card_needs_no_leases_and_a_clean_baseline() {
        let r = reading(100, 24_560);
        assert!(fits(Vram::WholeCard, &[], Some(100), r, LIM));
        assert!(!fits(Vram::WholeCard, &[], Some(2000), r, LIM));
        assert!(!fits(Vram::WholeCard, &[], None, r, LIM));
        assert!(!fits(Vram::WholeCard, &[Vram::Mib(1)], Some(100), r, LIM));
    }

    #[test]
    fn whole_card_lease_blocks_everyone() {
        let r = reading(100, 24_560);
        assert!(!fits(Vram::Mib(1), &[Vram::WholeCard], Some(100), r, LIM));
        assert!(fits(Vram::Mib(1), &[], Some(100), r, LIM));
    }

    #[test]
    fn concurrent_jobs_share_the_card_by_declared_vram() {
        let r = reading(500, 24_560);
        let held = [Vram::Mib(6000), Vram::Mib(8000)];
        assert!(fits(Vram::Mib(9000), &held, Some(500), r, LIM));
        assert!(!fits(Vram::Mib(15_000), &held, Some(500), r, LIM));
    }

    #[test]
    fn each_card_fits_by_its_own_margin_and_baseline() {
        let r = reading(500, 8_000);
        let tight = Limits {
            margin_mib: 2_000,
            whole_card_baseline_mib: 100,
        };
        // 8000 - 500 = 7500 free before the margin: 5600 fits with a 256 margin, not with 2000.
        assert!(fits(Vram::Mib(5600), &[], Some(500), r, LIM));
        assert!(!fits(Vram::Mib(5600), &[], Some(500), r, tight));
        // 400 MiB in use by others is fine for one card's whole-card rule and not for the other.
        assert!(!fits(Vram::WholeCard, &[], Some(400), r, tight));
        assert!(!fits(Vram::WholeCard, &[], Some(600), r, LIM));
        assert!(fits(Vram::WholeCard, &[], Some(400), r, LIM));
    }

    fn waiter(seq: u64, prio: u32, job: bool, target: Target) -> Waiter {
        let want = if job {
            Want::Job {
                vram: Vram::Mib(1),
                yieldable: false,
                pid: None,
                reply: oneshot::channel().0,
            }
        } else {
            Want::Llm {
                model: "m".into(),
                reply: oneshot::channel().0,
            }
        };
        Waiter {
            id: 100 - seq, // handle ids must not matter
            seq,
            class: "c".into(),
            prio,
            name: String::new(),
            since: SystemTime::now(),
            note: String::new(),
            held_at: None,
            model_held: false,
            target,
            want,
        }
    }

    #[test]
    fn waiters_order_by_priority_then_arrival() {
        let t = Target::Card(0);
        let mut ws = [
            waiter(3, 1, true, t.clone()),
            waiter(1, 2, true, t.clone()),
            waiter(2, 1, false, t),
        ];
        ws.sort_by_key(Waiter::key);
        let seqs: Vec<_> = ws.iter().map(|w| w.seq).collect();
        assert_eq!(seqs, [2, 3, 1]);
    }

    #[test]
    fn only_better_waiting_jobs_block_an_llm_request() {
        let t = Target::Card(0);
        let steer = [true];
        let ws = [waiter(1, 5, true, t.clone()), waiter(2, 1, false, t)];
        assert!(blocked(&ws, (10, 3), &[0], &steer)); // job outranks it
        assert!(blocked(&ws, (5, 2), &[0], &steer)); // same priority, arrived first
        assert!(!blocked(&ws, (5, 1), &[0], &steer)); // same priority, arrived earlier than the job
        assert!(!blocked(&ws, (1, 9), &[0], &steer)); // the LLM waiter itself does not count
    }

    #[test]
    fn a_job_for_another_card_does_not_block_the_llm() {
        let steer = [true, true];
        let ws = [waiter(1, 1, true, Target::Card(1))];
        assert!(!blocked(&ws, (5, 2), &[0], &steer));
        assert!(blocked(&ws, (5, 2), &[1], &steer));
        let ws = [waiter(1, 1, true, Target::Card(0))];
        assert!(blocked(&ws, (5, 2), &[0], &steer));
    }

    #[test]
    fn any_jobs_only_count_on_steerable_cards() {
        // card 0 has no env: an Any job cannot go there, so it cannot block card 0's LLM.
        let steer = [false, true];
        let ws = [waiter(1, 1, true, Target::Any)];
        assert!(!blocked(&ws, (5, 2), &[0], &steer));
        assert!(Target::Any.can_use(1, &steer));
        assert!(!Target::Any.can_use(0, &steer));
        assert!(!Target::Any.can_use(7, &steer));
    }

    #[test]
    fn an_llm_on_two_cards_is_blocked_by_a_job_for_either() {
        let steer = [true, true, true];
        let ws = [waiter(1, 1, true, Target::Card(1))];
        assert!(blocked(&ws, (5, 2), &[0, 1], &steer));
        assert!(!blocked(&ws, (5, 2), &[0, 2], &steer));
    }

    #[test]
    fn set_and_count_targets() {
        let steer = [true, false, true];
        let set = Target::Set(vec![0, 1]);
        assert!(set.can_use(0, &steer) && set.can_use(1, &steer));
        assert!(!set.can_use(2, &steer));
        assert_eq!(set.need(), 2);
        assert_eq!(set.fixed(), vec![0, 1]);
        let two = Target::Count(2);
        assert!(two.can_use(0, &steer) && two.can_use(2, &steer));
        assert!(!two.can_use(1, &steer)); // no env: cannot be picked
        assert_eq!(two.need(), 2);
        assert!(two.fixed().is_empty());
    }

    #[test]
    fn pinned_target_fits_only_its_card() {
        let steer = [true, true, true];
        assert!(Target::Card(1).can_use(1, &steer));
        assert!(!Target::Card(1).can_use(0, &steer));
        assert!(!Target::Card(1).can_use(2, &steer));
    }
}
