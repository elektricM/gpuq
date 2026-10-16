// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! The arbiter task: owns all scheduling state and changes it only in response to messages.
//!
//! The loop waits on three things: the message channel, a timer at the next decision deadline
//! (never more than [`TICK`] away, because drain and baseline decisions depend on polled VRAM)
//! and the one effect in flight, if any. After each of them it runs a bounded scheduling pass.
//!
//! Slow backend work (unload, reload, the offload hook, waiting for VRAM to drain) runs as an
//! effect: a boxed future that lives inside the phase it belongs to and is polled by the loop.
//! So `Status` and every other message is answered while an unload is under way.

use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{sleep, sleep_until, Instant};

use super::{
    Classes, Granted, HistoryEntry, LeaseRequest, LeaseStatus, Msg, OffloadHook, QueueStatus,
    Settings, Status, Tuning, Vram, WaiterKind,
};
use crate::backend::{Backend, BackendError};
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

/// The phase, with the data that only exists in it.
enum State {
    /// `retry_at`: no new unload before then, the last one failed.
    Llm {
        retry_at: Option<Instant>,
    },
    Unloading {
        fx: Fx<Unloaded>,
    },
    /// `offload`: the hook (and its settle pause) is running.
    Jobs {
        offload: Option<Fx<Option<Mib>>>,
    },
    Restoring {
        fx: Fx<Reloaded>,
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
}

struct Unloaded {
    /// Models whose slot was saved, even when the unload failed afterwards.
    saved: Vec<String>,
    /// VRAM in use afterwards (None: the probe failed).
    result: Result<Option<Mib>, String>,
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

/// Waits for the effect of the current phase; never resolves when there is none.
async fn fx_wait(state: &mut State) -> Done {
    match state {
        State::Unloading { fx } => Done::Unloaded(fx.await),
        State::Restoring { fx } => Done::Reloaded(fx.await),
        State::Jobs { offload: Some(fx) } => Done::Offloaded(fx.await),
        _ => std::future::pending().await,
    }
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

/// What effects need from the outside world.
#[derive(Clone)]
struct Env {
    backend: Arc<dyn Backend>,
    probe: Arc<dyn VramProbe>,
    t: Arc<Tuning>,
    log: Log,
}

impl Env {
    /// VRAM in use now; None (and a history line) when the probe fails.
    async fn used(&self) -> Option<Mib> {
        match self.probe.read().await {
            Ok(r) => Some(r.used_mib),
            Err(e) => {
                self.log.say(format!("cannot read VRAM ({e})"));
                None
            }
        }
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
            Ok(env.used().await)
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
/// into system RAM and run several times slower.
async fn wait_for_floor(env: &Env, floor: Mib) {
    let limit = floor.saturating_add(env.t.reload_slack_mib);
    let deadline = Instant::now() + env.t.reload_wait;
    let Some(mut used) = env.used().await else {
        return;
    };
    if used <= limit {
        return;
    }
    env.log.say(format!(
        "waiting for VRAM to drain before reload ({used} MiB used, floor {floor})"
    ));
    while used > limit && Instant::now() < deadline {
        sleep(env.t.driver_settle.max(MIN_POLL)).await;
        match env.used().await {
            Some(u) => used = u,
            None => return,
        }
    }
    if used > limit {
        env.log.say(format!(
            "VRAM still {used} MiB after {}s; reloading anyway",
            env.t.reload_wait.as_secs_f64()
        ));
    }
}

/// Load the resident model, then give it its saved slot back.
async fn reload(
    env: Env,
    resident: String,
    had_saved: bool,
    floor: Option<Mib>,
    kind: RestoreKind,
) -> Reloaded {
    let started = Instant::now();
    if let Some(floor) = floor {
        wait_for_floor(&env, floor).await;
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
    if !hook.run().await {
        env.log.say("offload hook failed");
    }
    sleep(env.t.offload_settle).await;
    env.used().await
}

// ------------------------------------------------------------------------ records

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
    want: Want,
}

impl Waiter {
    fn key(&self) -> (u32, u64) {
        (self.prio, self.seq)
    }

    fn is_llm(&self) -> bool {
        matches!(self.want, Want::Llm { .. })
    }
}

struct LeaseRec {
    id: String,
    /// Handle-side id of the request that got it (None for a resumed lease).
    handle: Option<u64>,
    class: String,
    prio: u32,
    name: String,
    vram: Vram,
    yieldable: bool,
    yield_tx: watch::Sender<bool>,
    since: SystemTime,
    pid: Option<u32>,
}

/// An LLM request is held back by any waiting job that outranks it.
fn blocked(waiters: &[Waiter], key: (u32, u64)) -> bool {
    waiters
        .iter()
        .any(|w| matches!(w.want, Want::Job { .. }) && w.key() < key)
}

/// `free = total - max(used, baseline_or_used + declared) - margin`, never below zero.
fn free_mib(r: VramReading, baseline: Option<Mib>, declared: Mib, margin: Mib) -> Mib {
    let base = baseline.unwrap_or(r.used_mib);
    let committed = r.used_mib.max(base.saturating_add(declared));
    r.total_mib.saturating_sub(committed).saturating_sub(margin)
}

/// Whether a request of `want` VRAM fits next to the leases holding `held`.
fn fits(want: Vram, held: &[Vram], baseline: Option<Mib>, r: VramReading, t: &Tuning) -> bool {
    match want {
        Vram::WholeCard => {
            held.is_empty() && baseline.is_some_and(|b| b < t.whole_card_baseline_mib)
        }
        Vram::Mib(m) => {
            if held.contains(&Vram::WholeCard) {
                return false;
            }
            let declared = held
                .iter()
                .map(|v| match v {
                    Vram::Mib(m) => *m,
                    Vram::WholeCard => 0,
                })
                .fold(0, Mib::saturating_add);
            free_mib(r, baseline, declared, t.margin_mib) >= m
        }
    }
}

fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn age_s(t: SystemTime) -> f64 {
    t.elapsed().map_or(0.0, |d| d.as_secs_f64())
}

// ------------------------------------------------------------------------ the actor

struct Actor {
    classes: Classes,
    resident: Option<String>,
    t: Arc<Tuning>,
    backend: Arc<dyn Backend>,
    probe: Arc<dyn VramProbe>,
    hook: Option<Arc<dyn OffloadHook>>,
    weak: mpsc::WeakUnboundedSender<Msg>,

    state: State,
    inflight: usize,
    last_llm: Instant,
    last_llm_class: String,
    leases: Vec<LeaseRec>,
    waiters: Vec<Waiter>,
    history: VecDeque<(SystemTime, String)>,

    next_seq: u64,
    next_lease: u64,
    boot: u64,
    /// VRAM in use with no lease held (driver, desktop).
    baseline: Option<Mib>,
    /// VRAM in use right after the last unload.
    floor: Option<Mib>,
    grace_until: Instant,
    /// None: no lease has ended yet.
    released_at: Option<Instant>,
    released_class: String,
    /// Next time a waiting decision can change; collected during a pass.
    deadline: Option<Instant>,
    offload_tried: HashSet<u64>,
    saved: HashSet<String>,
    recent_admits: VecDeque<u64>,
    probe_error: Option<String>,
}

pub(super) async fn run(
    settings: Settings,
    backend: Arc<dyn Backend>,
    probe: Arc<dyn VramProbe>,
    offload: Option<Arc<dyn OffloadHook>>,
    tx: mpsc::UnboundedSender<Msg>,
    mut rx: mpsc::UnboundedReceiver<Msg>,
) {
    // Keep only a weak sender: the loop ends once every handle is gone.
    let weak = tx.downgrade();
    drop(tx);
    let mut actor = Actor::new(settings, backend, probe, offload, weak);
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
            done = fx_wait(&mut actor.state) => actor.finish(done),
        }
        actor.pass().await;
    }
}

impl Actor {
    fn new(
        settings: Settings,
        backend: Arc<dyn Backend>,
        probe: Arc<dyn VramProbe>,
        hook: Option<Arc<dyn OffloadHook>>,
        weak: mpsc::WeakUnboundedSender<Msg>,
    ) -> Self {
        let now = Instant::now();
        Actor {
            classes: settings.classes,
            resident: settings.resident.filter(|r| !r.is_empty()),
            t: Arc::new(settings.tuning),
            backend,
            probe,
            hook,
            weak,
            state: State::Llm { retry_at: None },
            inflight: 0,
            last_llm: now,
            last_llm_class: "llm".into(),
            leases: Vec::new(),
            waiters: Vec::new(),
            history: VecDeque::new(),
            next_seq: 0,
            next_lease: 1,
            // Keeps lease ids unique across restarts.
            boot: unix_secs(SystemTime::now()) & 0xFFFFF,
            baseline: None,
            floor: None,
            grace_until: now,
            released_at: None,
            released_class: String::new(),
            deadline: None,
            offload_tried: HashSet::new(),
            saved: HashSet::new(),
            recent_admits: VecDeque::new(),
            probe_error: None,
        }
    }

    fn env(&self) -> Env {
        Env {
            backend: self.backend.clone(),
            probe: self.probe.clone(),
            t: self.t.clone(),
            log: Log(self.weak.clone()),
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

    /// One reading of the card. A failure is logged once per distinct error and means
    /// "cannot decide now": the next tick tries again.
    async fn read_probe(&mut self) -> Option<VramReading> {
        match self.probe.read().await {
            Ok(r) => {
                self.probe_error = None;
                Some(r)
            }
            Err(e) => {
                let e = e.to_string();
                if self.probe_error.as_deref() != Some(e.as_str()) {
                    self.event(format!("cannot read VRAM ({e}); will retry"));
                }
                self.probe_error = Some(e);
                None
            }
        }
    }

    /// Decide where to begin from what the backend has loaded.
    async fn start(&mut self) {
        match self.backend.loaded_gpu_models().await {
            Err(e) => {
                // Backend down: assume it owns the card, it loads on demand.
                let kind = self.backend.kind();
                self.warn(format!(
                    "cannot read {kind} state ({e}); starting in llm state"
                ));
            }
            Ok(loaded) if !loaded.is_empty() => {
                self.event(format!("start: llm state, loaded {loaded:?}"));
            }
            Ok(_) => {
                // Nothing loaded: maybe gpud restarted under a running job. Give `gpu run`
                // clients time to re-adopt their leases before reloading the LLM on top.
                let grace = self.t.resume_grace;
                self.grace_until = Instant::now() + grace;
                self.state = State::Jobs { offload: None };
                let used = self.read_probe().await.map(|r| r.used_mib);
                self.baseline = used;
                self.floor = used;
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
            Msg::ExitLlm => {
                self.inflight = self.inflight.saturating_sub(1);
                self.last_llm = Instant::now();
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
        let in_llm = matches!(self.state, State::Llm { .. });
        if in_llm && !blocked(&self.waiters, (prio, seq)) && !self.needs_slot_restore(&model) {
            if reply.send(()).is_ok() {
                self.admit(id, &class);
            }
            return;
        }
        let phase = self.state.phase().name();
        self.event(format!("hold {class} request for {model} (state {phase})"));
        self.waiters.push(Waiter {
            id,
            seq,
            class,
            prio,
            name: model.clone(),
            since: SystemTime::now(),
            note: String::new(),
            held_at: None,
            want: Want::Llm { model, reply },
        });
    }

    fn acquire(&mut self, id: u64, req: LeaseRequest, reply: oneshot::Sender<Granted>) {
        let prio = self.classes.job_prio(&req.class);
        if let Some(resume) = req.resume_id.clone().filter(|r| !r.is_empty()) {
            let lease = LeaseRec::new(resume.clone(), None, &req.class, prio, &req);
            if matches!(self.state, State::Llm { .. }) {
                let kind = self.backend.kind();
                self.warn(format!(
                    "lease {resume} resumed while {kind} holds the card"
                ));
            }
            self.event(format!("resume lease {resume} {}/{}", req.class, req.name));
            self.install(lease, reply);
            return;
        }
        let seq = self.take_seq();
        self.event(format!(
            "queue {}/{} vram={}",
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

    /// Record a lease and tell its owner. If the owner is already gone, take it back.
    fn install(
        &mut self,
        (lease, yield_rx): (LeaseRec, watch::Receiver<bool>),
        reply: oneshot::Sender<Granted>,
    ) {
        let id = lease.id.clone();
        self.leases.push(lease);
        if reply
            .send(Granted {
                id: id.clone(),
                yield_rx,
            })
            .is_err()
        {
            if let Some(i) = self.leases.iter().position(|l| l.id == id) {
                let lease = self.leases.remove(i);
                self.log_release(&lease);
            }
        }
    }

    fn cancel(&mut self, id: u64) {
        if let Some(i) = self.waiters.iter().position(|w| w.id == id) {
            self.waiters.remove(i);
        } else if let Some(i) = self.leases.iter().position(|l| l.handle == Some(id)) {
            // Granted in the same breath as the client went away.
            let lease = self.leases.remove(i);
            self.after_release(lease);
        } else if let Some(i) = self.recent_admits.iter().position(|&a| a == id) {
            self.recent_admits.remove(i);
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
        self.event(format!(
            "release lease {} {}/{} after {:.0}s",
            lease.id,
            lease.class,
            lease.name,
            age_s(lease.since)
        ));
    }

    fn after_release(&mut self, lease: LeaseRec) {
        self.log_release(&lease);
        if self.leases.is_empty() {
            self.baseline = None; // re-measured once the job's memory is gone
            self.released_at = Some(Instant::now());
            self.released_class = lease.class;
        }
    }

    fn admit(&mut self, id: u64, class: &str) {
        self.inflight += 1;
        self.last_llm = Instant::now();
        self.last_llm_class = class.to_owned();
        if self.recent_admits.len() == RECENT_ADMITS {
            self.recent_admits.pop_front();
        }
        self.recent_admits.push_back(id);
    }

    // -------------------------------------------------------------- scheduling

    async fn pass(&mut self) {
        self.deadline = None;
        for _ in 0..MAX_PASSES {
            let again = match self.state {
                State::Llm { .. } => self.step_llm(),
                State::Jobs { .. } => self.step_jobs().await,
                // The effect in flight drives these.
                State::Unloading { .. } | State::Restoring { .. } => false,
            };
            if !again {
                return;
            }
        }
    }

    /// An effect finished: move to the phase it was heading for.
    fn finish(&mut self, done: Done) {
        match done {
            Done::Unloaded(u) => self.finish_unload(u),
            Done::Reloaded(r) => self.finish_reload(r),
            Done::Offloaded(measured) => {
                if measured.is_some() {
                    self.baseline = measured;
                }
                self.state = State::Jobs { offload: None };
            }
        }
    }

    fn finish_unload(&mut self, u: Unloaded) {
        self.saved.extend(u.saved);
        match u.result {
            Err(e) => {
                self.event(format!("unload failed ({e}); staying in llm state"));
                self.enter_llm_state(Some(Instant::now() + UNLOAD_BACKOFF));
            }
            Ok(used) => {
                self.baseline = used;
                self.floor = used;
                self.state = State::Jobs { offload: None };
                let used = used.map_or_else(|| "?".to_owned(), |m| m.to_string());
                self.event(format!(
                    "card free in {:.1}s (VRAM {used} MiB still in use)",
                    u.elapsed.as_secs_f64()
                ));
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
        self.enter_llm_state(None);
        if r.kind == RestoreKind::Card {
            self.last_llm = Instant::now();
            self.event(format!("llm state ({:.1}s)", r.elapsed.as_secs_f64()));
        }
    }

    fn enter_llm_state(&mut self, retry_at: Option<Instant>) {
        self.state = State::Llm { retry_at };
        self.offload_tried.clear();
    }

    fn top_idx(&self) -> Option<usize> {
        self.waiters
            .iter()
            .enumerate()
            .min_by_key(|(_, w)| w.key())
            .map(|(i, _)| i)
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
            if blocked(&self.waiters, key) {
                continue;
            }
            let Some(idx) = self.waiters.iter().position(|w| w.key() == key) else {
                continue;
            };
            let restore = match &self.waiters[idx].want {
                Want::Llm { model, .. } => self.needs_slot_restore(model),
                Want::Job { .. } => continue,
            };
            if restore {
                if let Some(resident) = self.resident.clone() {
                    self.event(format!("restoring {resident} slot before serving it"));
                    self.start_reload(resident, RestoreKind::Slot);
                    return true;
                }
            }
            let w = self.waiters.remove(idx);
            let Want::Llm { model, reply } = w.want else {
                continue;
            };
            self.event(format!(
                "release held {} request for {model} after {:.0}s",
                w.class,
                age_s(w.since)
            ));
            if reply.send(()).is_ok() {
                self.admit(w.id, &w.class);
            }
        }
        false
    }

    /// Llm phase: hand the card to the top job once it may have it. True = phase changed.
    fn step_llm(&mut self) -> bool {
        if self.admit_llm() {
            return true;
        }
        let Some(idx) = self.top_idx() else {
            return false;
        };
        if self.waiters[idx].is_llm() {
            return false;
        }
        if self.inflight > 0 {
            self.waiters[idx].note =
                format!("waiting for {} in-flight LLM request(s)", self.inflight);
            return false;
        }
        let need = self.required_idle(&self.waiters[idx].class, self.waiters[idx].prio);
        let now = Instant::now();
        let idle = now.saturating_duration_since(self.last_llm);
        if idle < need {
            self.waiters[idx].note = format!(
                "LLM ({}) used {:.0}s ago; card is handed over after {}s idle",
                self.last_llm_class,
                idle.as_secs_f64(),
                need.as_secs_f64()
            );
            self.want(self.last_llm + need);
            return false;
        }
        if let State::Llm {
            retry_at: Some(at), ..
        } = self.state
        {
            if now < at {
                self.want(at); // backend trouble: retry calmly
                return false;
            }
        }
        let kind = self.backend.kind();
        self.event(format!("unloading {kind}"));
        self.state = State::Unloading {
            fx: Box::pin(unload(self.env())),
        };
        true
    }

    /// Jobs phase: grant leases, or give the card back to the LLM. True = go again.
    async fn step_jobs(&mut self) -> bool {
        if matches!(self.state, State::Jobs { offload: Some(_) }) {
            return false; // the hook is running; its completion triggers the next pass
        }
        let mut reading = None;
        if self.leases.is_empty() {
            // Once the driver has had time to hand the memory back, measure what is left.
            if let Some(at) = self.released_at {
                let ready = at + self.t.driver_settle;
                if Instant::now() < ready {
                    self.want(ready);
                    return false;
                }
            }
            reading = self.read_probe().await;
            if let Some(r) = reading {
                self.baseline = Some(r.used_mib);
            }
        }
        let Some(idx) = self.top_idx() else {
            return self.maybe_reload_idle();
        };
        if self.waiters[idx].is_llm() {
            self.serve_llm_waiter(idx)
        } else {
            self.serve_job_waiter(idx, reading).await
        }
    }

    fn maybe_reload_idle(&mut self) -> bool {
        if !self.leases.is_empty() || !(self.t.reload_when_idle || self.resident.is_none()) {
            return false;
        }
        let delay = self.t.reload_delay_after(&self.released_class);
        let not_before = match self.released_at {
            Some(at) => self.grace_until.max(at + delay),
            None => self.grace_until,
        };
        if Instant::now() >= not_before {
            self.start_restore(None);
            true
        } else {
            self.want(not_before);
            false
        }
    }

    fn serve_llm_waiter(&mut self, idx: usize) -> bool {
        if self.leases.is_empty() {
            let model = match &self.waiters[idx].want {
                Want::Llm { model, .. } => model.clone(),
                Want::Job { .. } => return false,
            };
            self.start_restore(Some(&model));
            return true;
        }
        if self.hold_for_batch(idx, Instant::now()) {
            return false;
        }
        self.request_yield(self.waiters[idx].prio);
        self.waiters[idx].note = format!("waiting for GPU job(s): {}", self.lease_names());
        false
    }

    async fn serve_job_waiter(&mut self, idx: usize, reading: Option<VramReading>) -> bool {
        let reading = match reading {
            Some(r) => r,
            None => match self.read_probe().await {
                Some(r) => r,
                None => {
                    let why = self.probe_error.clone().unwrap_or_default();
                    self.waiters[idx].note = format!("cannot read VRAM: {why}");
                    return false;
                }
            },
        };
        let (vram, prio) = match &self.waiters[idx].want {
            Want::Job { vram, .. } => (*vram, self.waiters[idx].prio),
            Want::Llm { .. } => return false,
        };
        if self.fits_now(vram, reading) {
            self.grant(idx);
            return true;
        }
        self.request_yield(prio);
        if self.leases.is_empty() {
            self.maybe_offload(idx);
        }
        self.waiters[idx].note = self.why_not_fit(vram, reading);
        false
    }

    fn fits_now(&self, vram: Vram, reading: VramReading) -> bool {
        let held: Vec<Vram> = self.leases.iter().map(|l| l.vram).collect();
        fits(vram, &held, self.baseline, reading, &self.t)
    }

    fn why_not_fit(&self, vram: Vram, reading: VramReading) -> String {
        match vram {
            Vram::WholeCard if !self.leases.is_empty() => {
                "needs the whole card: other leases active".into()
            }
            Vram::WholeCard => match self.baseline {
                Some(b) => {
                    format!("needs the whole card: {b} MiB still in use by other processes")
                }
                None => "needs the whole card: VRAM not measured yet".into(),
            },
            Vram::Mib(m) => {
                let declared = self
                    .leases
                    .iter()
                    .map(|l| match l.vram {
                        Vram::Mib(m) => m,
                        Vram::WholeCard => 0,
                    })
                    .fold(0, Mib::saturating_add);
                let free = free_mib(reading, self.baseline, declared, self.t.margin_mib);
                format!("needs {m} MiB, {free} MiB free")
            }
        }
    }

    fn lease_names(&self) -> String {
        let names: Vec<_> = self
            .leases
            .iter()
            .map(|l| format!("{}/{}", l.class, l.name))
            .collect();
        names.join(", ")
    }

    /// True while an LLM request should keep waiting behind yieldable batch leases.
    fn hold_for_batch(&mut self, idx: usize, now: Instant) -> bool {
        let hold = self.t.batch_hold;
        if hold.is_zero() || self.waiters[idx].prio != self.classes.llm() {
            return false;
        }
        let batch = self.classes.batch();
        if self
            .leases
            .iter()
            .any(|l| Some(l.prio) != batch || !l.yieldable)
        {
            return false;
        }
        if self.leases.iter().any(|l| *l.yield_tx.borrow()) {
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
            self.lease_names(),
            until.saturating_duration_since(now).as_secs_f64()
        );
        self.want(until);
        true
    }

    /// Ask every yieldable lease that ranks below `prio` to stop at its next safe point.
    fn request_yield(&mut self, prio: u32) {
        let mut asked = Vec::new();
        for l in &self.leases {
            if l.yieldable && l.prio > prio && !*l.yield_tx.borrow() {
                l.yield_tx.send_replace(true);
                asked.push(format!("ask {}/{} ({}) to yield", l.class, l.name, l.id));
            }
        }
        for msg in asked {
            self.event(msg);
        }
    }

    fn grant(&mut self, idx: usize) {
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
        };
        self.event(format!(
            "grant {id} {}/{} vram={vram} after {:.0}s",
            w.class,
            w.name,
            age_s(w.since)
        ));
        self.install((lease, yield_rx), reply);
    }

    // -------------------------------------------------------------- card hand-over

    /// Back to the LLM state, reloading the resident model if that is what comes next.
    fn start_restore(&mut self, model: Option<&str>) {
        let target = match model {
            Some(m) => Some(self.backend.resolve(m)),
            None => self.resident.clone(),
        };
        match (self.resident.clone(), target) {
            (Some(resident), Some(target)) if resident == target => {
                self.start_reload(resident, RestoreKind::Card);
            }
            _ => {
                // Any other model is loaded by the backend itself when the held request goes
                // through.
                self.enter_llm_state(None);
                self.last_llm = Instant::now();
                self.event("llm state (0.0s)");
            }
        }
    }

    fn start_reload(&mut self, resident: String, kind: RestoreKind) {
        let had_saved = self.saved.contains(&resident);
        let floor = if kind == RestoreKind::Card {
            self.floor
        } else {
            None
        };
        self.state = State::Restoring {
            fx: Box::pin(reload(self.env(), resident, had_saved, floor, kind)),
        };
    }

    /// Run the offload hook once per waiting job, then re-measure the card.
    fn maybe_offload(&mut self, idx: usize) {
        let Some(hook) = self.hook.clone() else {
            return;
        };
        let w = &self.waiters[idx];
        if self.offload_tried.contains(&w.seq) {
            return;
        }
        let (seq, msg) = (w.seq, format!("offload hook for {}/{}", w.class, w.name));
        self.offload_tried.insert(seq);
        self.event(msg);
        self.state = State::Jobs {
            offload: Some(Box::pin(offload(self.env(), hook))),
        };
    }

    // -------------------------------------------------------------- reporting

    async fn status(&mut self) -> Status {
        let (vram, vram_error) = match self.probe.read().await {
            Ok(r) => (Some(r), None),
            Err(e) => (None, Some(e.to_string())),
        };
        let mut queue: Vec<&Waiter> = self.waiters.iter().collect();
        queue.sort_by_key(|w| w.key());
        Status {
            state: self.state.phase(),
            device: self.probe.device(),
            inflight_llm: self.inflight,
            llm_idle_s: Instant::now()
                .saturating_duration_since(self.last_llm)
                .as_secs_f64()
                .round() as u64,
            idle_s: self.t.idle.as_secs(),
            baseline_mib: self.baseline,
            floor_mib: self.floor,
            vram,
            vram_error,
            leases: self
                .leases
                .iter()
                .map(|l| LeaseStatus {
                    id: l.id.clone(),
                    class: l.class.clone(),
                    name: l.name.clone(),
                    vram: l.vram,
                    yieldable: l.yieldable,
                    yield_requested: *l.yield_tx.borrow(),
                    age_s: age_s(l.since).round() as u64,
                    pid: l.pid,
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
        }
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
        };
        (lease, yield_rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let t = Tuning::default();
        let r = reading(100, 24_560);
        assert!(fits(Vram::WholeCard, &[], Some(100), r, &t));
        assert!(!fits(Vram::WholeCard, &[], Some(2000), r, &t));
        assert!(!fits(Vram::WholeCard, &[], None, r, &t));
        assert!(!fits(Vram::WholeCard, &[Vram::Mib(1)], Some(100), r, &t));
    }

    #[test]
    fn whole_card_lease_blocks_everyone() {
        let t = Tuning::default();
        let r = reading(100, 24_560);
        assert!(!fits(Vram::Mib(1), &[Vram::WholeCard], Some(100), r, &t));
        assert!(fits(Vram::Mib(1), &[], Some(100), r, &t));
    }

    #[test]
    fn concurrent_jobs_share_the_card_by_declared_vram() {
        let t = Tuning::default();
        let r = reading(500, 24_560);
        let held = [Vram::Mib(6000), Vram::Mib(8000)];
        assert!(fits(Vram::Mib(9000), &held, Some(500), r, &t));
        assert!(!fits(Vram::Mib(15_000), &held, Some(500), r, &t));
    }

    fn waiter(seq: u64, prio: u32, job: bool) -> Waiter {
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
            want,
        }
    }

    #[test]
    fn waiters_order_by_priority_then_arrival() {
        let mut ws = [waiter(3, 1, true), waiter(1, 2, true), waiter(2, 1, false)];
        ws.sort_by_key(Waiter::key);
        let seqs: Vec<_> = ws.iter().map(|w| w.seq).collect();
        assert_eq!(seqs, [2, 3, 1]);
    }

    #[test]
    fn only_better_waiting_jobs_block_an_llm_request() {
        let ws = [waiter(1, 5, true), waiter(2, 1, false)];
        assert!(blocked(&ws, (10, 3))); // job outranks it
        assert!(blocked(&ws, (5, 2))); // same priority, arrived first
        assert!(!blocked(&ws, (5, 1))); // same priority, arrived earlier than the job
        assert!(!blocked(&ws, (1, 9))); // the LLM waiter itself does not count
    }
}
