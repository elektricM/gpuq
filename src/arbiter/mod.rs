// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! The arbiter: one owner of the card at a time, a priority queue for everyone else.
//!
//! ```text
//!   llm ──(top job may run)──> unloading ──(card free)──> jobs
//!    ^                             │                        │
//!    │                       (unload failed)     (no lease, LLM wanted or idle)
//!    │                             v                        v
//!    └──────────(model ready)─── llm <────────────────── restoring
//! ```
//!
//! * `llm`: the LLM backend owns the card; GPU-model requests pass straight through the gate.
//! * `unloading`: slots of loaded models are saved and the backend unloads.
//! * `jobs`: leases are granted against free VRAM.
//! * `restoring`: the resident model is reloaded and its slot restored.
//!
//! Nothing is killed. LLM requests wait for running jobs; a yieldable job is asked to stop at
//! its next safe point when something more important waits. Priority comes from the class
//! table (lower number = more important).
//!
//! One task owns all state ([`actor`]); everyone else talks to it through a [`Handle`].
//! Leases and in-flight LLM requests are guards: dropping one tells the arbiter, so a client
//! that goes away can never leak the card.

mod actor;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use serde::{Serialize, Serializer};
use tokio::sync::{mpsc, oneshot, watch};

use crate::backend::Backend;
use crate::probe::{VramProbe, VramReading};
use crate::Mib;

pub use actor::Phase;

/// How much VRAM a job declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vram {
    Mib(Mib),
    /// The entire card: granted only with no other lease and an otherwise empty card.
    WholeCard,
}

impl fmt::Display for Vram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Vram::Mib(m) => write!(f, "{m}MiB"),
            Vram::WholeCard => f.write_str("all"),
        }
    }
}

impl Serialize for Vram {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

/// Priority classes. Lower number = more important. `llm` always exists.
#[derive(Clone, Debug)]
pub struct Classes {
    prio: BTreeMap<String, u32>,
}

impl Classes {
    /// Fails when `llm` is missing.
    pub fn new(prio: BTreeMap<String, u32>) -> Result<Self, String> {
        if !prio.contains_key("llm") {
            return Err("classes must define \"llm\"".into());
        }
        Ok(Classes { prio })
    }

    pub fn contains(&self, class: &str) -> bool {
        self.prio.contains_key(class)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.prio.keys().map(String::as_str)
    }

    pub fn llm(&self) -> u32 {
        self.prio["llm"]
    }

    /// Priority of `batch`, if defined.
    pub fn batch(&self) -> Option<u32> {
        self.prio.get("batch").copied()
    }

    /// Priority of a job class; unknown classes rank as `batch` (or last).
    pub fn job_prio(&self, class: &str) -> u32 {
        self.prio
            .get(class)
            .copied()
            .unwrap_or_else(|| self.batch().unwrap_or(u32::MAX))
    }

    /// Priority of an LLM request class; unknown classes rank as `llm`.
    pub fn llm_prio(&self, class: &str) -> u32 {
        self.prio.get(class).copied().unwrap_or_else(|| self.llm())
    }
}

/// Per-class overrides for the quiet time after an LLM request and the reload delay.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClassTuning {
    pub gap: Option<Duration>,
    pub reload_delay: Option<Duration>,
}

/// Scheduling knobs, from the `[llm]`, `[gpu]` and `[hooks]` config sections.
#[derive(Clone, Debug)]
pub struct Tuning {
    /// Silence required before classes in `idle_wait_classes` take the card.
    pub idle: Duration,
    pub idle_wait_classes: HashSet<String>,
    /// Silence required before other classes below the LLM take the card.
    pub gap: Duration,
    pub class_tuning: HashMap<String, ClassTuning>,
    pub reload_when_idle: bool,
    /// Pause after the last lease before the resident model is reloaded.
    pub reload_delay: Duration,
    /// Reload once VRAM is within this of the post-unload floor...
    pub reload_slack_mib: Mib,
    /// ...or after waiting this long.
    pub reload_wait: Duration,
    /// After a gpuq restart with an empty card, time for clients to re-adopt their leases.
    pub resume_grace: Duration,
    /// Hold LLM requests this long behind yieldable batch leases (zero = off).
    pub batch_hold: Duration,
    /// End the batch hold once this many LLM requests are queued (0 = timer only).
    pub batch_hold_count: usize,
    /// VRAM kept free on top of what leases declare.
    pub margin_mib: Mib,
    /// How long to wait for the backend to report its models gone after unloading.
    pub settle_timeout: Duration,
    /// Pause after an unload (and between VRAM re-checks) for the driver to free memory.
    pub driver_settle: Duration,
    /// A whole-card lease needs at most this much VRAM used by other processes.
    pub whole_card_baseline_mib: Mib,
    /// Pause after the offload hook before VRAM is measured again.
    pub offload_settle: Duration,
}

impl Tuning {
    pub fn gap_after(&self, class: &str) -> Duration {
        self.class_tuning
            .get(class)
            .and_then(|t| t.gap)
            .unwrap_or(self.gap)
    }

    pub fn reload_delay_after(&self, class: &str) -> Duration {
        self.class_tuning
            .get(class)
            .and_then(|t| t.reload_delay)
            .unwrap_or(self.reload_delay)
    }
}

impl Default for Tuning {
    /// The documented config defaults.
    fn default() -> Self {
        Tuning {
            idle: Duration::from_secs(300),
            idle_wait_classes: ["background", "batch"].map(String::from).into(),
            gap: Duration::from_secs(10),
            class_tuning: HashMap::new(),
            reload_when_idle: true,
            reload_delay: Duration::from_secs(20),
            reload_slack_mib: 1024,
            reload_wait: Duration::from_secs(120),
            resume_grace: Duration::from_secs(30),
            batch_hold: Duration::ZERO,
            batch_hold_count: 0,
            margin_mib: 256,
            settle_timeout: Duration::from_secs(60),
            driver_settle: Duration::from_secs(2),
            whole_card_baseline_mib: 512,
            offload_settle: Duration::from_secs(3),
        }
    }
}

/// Everything the arbiter needs to know at start.
#[derive(Clone, Debug)]
pub struct Settings {
    pub classes: Classes,
    /// Model reloaded when the card goes idle; None = load models on demand only.
    pub resident: Option<String>,
    pub tuning: Tuning,
}

/// Runs once per waiting job that does not fit while no lease is active on `card`, e.g. to
/// move a desktop session off it. Returns whether the hook ran successfully.
#[async_trait::async_trait]
pub trait OffloadHook: Send + Sync + 'static {
    async fn run(&self, card: &str) -> bool;
}

/// One GPU as the arbiter sees it.
#[derive(Clone)]
pub struct Card {
    /// Unique name used in leases and status, e.g. "gpu0" or "big".
    pub name: String,
    pub probe: Arc<dyn VramProbe>,
    /// VRAM kept free on this card on top of what leases declare.
    pub margin_mib: Mib,
    /// A whole-card lease needs at most this much VRAM used by other processes.
    pub whole_card_baseline_mib: Mib,
    /// Environment that points a program at this card, e.g. `CUDA_VISIBLE_DEVICES=1`.
    /// Handed to lease holders; a card without it cannot be picked for `CardSel::Any`
    /// when there is more than one card.
    pub env: Vec<(String, String)>,
}

impl fmt::Debug for Card {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Card")
            .field("name", &self.name)
            .field("device", &self.probe.device())
            .field("env", &self.env)
            .finish()
    }
}

/// Which card a job wants.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum CardSel {
    /// The first configured card. Clients that know nothing about cards get this.
    #[default]
    Primary,
    Named(String),
    /// Whichever card can take the job first. Only cards with `env` qualify, so the job can
    /// be pointed at the card it got.
    Any,
}

impl fmt::Display for CardSel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CardSel::Primary => f.write_str("primary"),
            CardSel::Named(n) => f.write_str(n),
            CardSel::Any => f.write_str("any"),
        }
    }
}

/// A job asking for the card.
#[derive(Clone, Debug)]
pub struct LeaseRequest {
    pub class: String,
    pub vram: Vram,
    pub yieldable: bool,
    pub name: String,
    pub pid: Option<u32>,
    /// Re-adopt a lease granted before a gpuq restart instead of queueing.
    pub resume_id: Option<String>,
    pub card: CardSel,
}

#[derive(Debug, thiserror::Error)]
pub enum ArbiterError {
    #[error("arbiter is not running")]
    Closed,
    #[error("unknown card '{0}'; known: {1}")]
    UnknownCard(String, String),
    #[error("no card can be picked for \"any\": give the cards a visible_devices setting")]
    NoSteerableCard,
}

// ------------------------------------------------------------------------ status

#[derive(Clone, Debug, Serialize)]
pub struct LeaseStatus {
    pub id: String,
    pub card: String,
    pub class: String,
    pub name: String,
    pub vram: Vram,
    pub yieldable: bool,
    pub yield_requested: bool,
    pub age_s: u64,
    pub pid: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WaiterKind {
    Job,
    Llm,
}

#[derive(Clone, Debug, Serialize)]
pub struct QueueStatus {
    pub kind: WaiterKind,
    /// Card name, or "any".
    pub card: String,
    pub class: String,
    pub name: String,
    /// Declared VRAM for jobs; None for LLM requests.
    pub vram: Option<Vram>,
    pub wait_s: u64,
    /// Why it is still waiting.
    pub why: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct HistoryEntry {
    /// Seconds since the Unix epoch.
    pub t: u64,
    pub msg: String,
}

/// One card's view. The top-level `Status` fields repeat the primary card's.
#[derive(Clone, Debug, Serialize)]
pub struct CardStatus {
    pub name: String,
    pub device: String,
    pub state: Phase,
    /// The LLM backend lives on this card.
    pub llm: bool,
    pub baseline_mib: Option<Mib>,
    pub floor_mib: Option<Mib>,
    pub vram: Option<VramReading>,
    pub vram_error: Option<String>,
}

/// Top-level fields describe the primary (first) card, so single-card clients keep working;
/// `cards` has every card.
#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub state: Phase,
    pub device: String,
    pub inflight_llm: usize,
    pub llm_idle_s: u64,
    pub idle_s: u64,
    /// VRAM in use with no lease held (driver, desktop), as last measured.
    pub baseline_mib: Option<Mib>,
    /// VRAM in use right after the last unload.
    pub floor_mib: Option<Mib>,
    /// None when the probe failed; see `vram_error`.
    pub vram: Option<VramReading>,
    pub vram_error: Option<String>,
    pub leases: Vec<LeaseStatus>,
    /// Waiters in the order they will be served.
    pub queue: Vec<QueueStatus>,
    /// Most recent last; at most 40 entries.
    pub history: Vec<HistoryEntry>,
    pub cards: Vec<CardStatus>,
}

// ------------------------------------------------------------------------ handle

/// Messages to the arbiter task. Sent on an unbounded channel so guards can send from Drop.
#[derive(Debug)]
enum Msg {
    EnterLlm {
        id: u64,
        model: String,
        class: String,
        reply: oneshot::Sender<()>,
    },
    ExitLlm,
    Acquire {
        id: u64,
        req: LeaseRequest,
        reply: oneshot::Sender<Granted>,
    },
    /// A waiter (LLM request or job) went away before it was served.
    Cancel(u64),
    Release(String),
    Status(oneshot::Sender<Status>),
    Wake,
    /// A line for the history, sent by an effect while it runs.
    Event(String),
}

/// What the arbiter sends back on a grant.
#[derive(Debug)]
struct Granted {
    id: String,
    card: String,
    env: Vec<(String, String)>,
    yield_rx: watch::Receiver<bool>,
}

/// Cheap to clone; every clone talks to the same arbiter task.
#[derive(Clone)]
pub struct Handle {
    tx: mpsc::UnboundedSender<Msg>,
    classes: Arc<Classes>,
    /// Card names in config order, and whether each can be picked for `CardSel::Any`.
    cards: Arc<Vec<(String, bool)>>,
    ids: Arc<std::sync::atomic::AtomicU64>,
}

/// Start the arbiter for one card named "gpu0", using the margin and whole-card baseline from
/// `settings.tuning`. It first asks the backend what is loaded, then serves messages.
pub fn spawn(
    settings: Settings,
    backend: Arc<dyn Backend>,
    probe: Arc<dyn VramProbe>,
    offload: Option<Arc<dyn OffloadHook>>,
) -> Handle {
    let card = Card {
        name: "gpu0".into(),
        probe,
        margin_mib: settings.tuning.margin_mib,
        whole_card_baseline_mib: settings.tuning.whole_card_baseline_mib,
        env: Vec::new(),
    };
    spawn_cards(settings, vec![card], 0, backend, offload)
}

/// Start the arbiter for several cards. `cards[0]` is the primary card; the LLM backend lives
/// on `cards[llm_card]`. Panics if `cards` is empty or `llm_card` is out of range.
pub fn spawn_cards(
    settings: Settings,
    cards: Vec<Card>,
    llm_card: usize,
    backend: Arc<dyn Backend>,
    offload: Option<Arc<dyn OffloadHook>>,
) -> Handle {
    assert!(llm_card < cards.len(), "llm_card out of range");
    let many = cards.len() > 1;
    let (tx, rx) = mpsc::unbounded_channel();
    let handle = Handle {
        tx: tx.clone(),
        classes: Arc::new(settings.classes.clone()),
        cards: Arc::new(
            cards
                .iter()
                .map(|c| (c.name.clone(), !many || !c.env.is_empty()))
                .collect(),
        ),
        ids: Arc::new(std::sync::atomic::AtomicU64::new(1)),
    };
    tokio::spawn(actor::run(
        settings, cards, llm_card, backend, offload, tx, rx,
    ));
    handle
}

impl Handle {
    pub fn classes(&self) -> &Classes {
        &self.classes
    }

    /// Card names in config order; the first is the primary card.
    pub fn card_names(&self) -> impl Iterator<Item = &str> {
        self.cards.iter().map(|(n, _)| n.as_str())
    }

    /// Check a card selection before queueing.
    pub fn check_card(&self, sel: &CardSel) -> Result<(), ArbiterError> {
        match sel {
            CardSel::Primary => Ok(()),
            CardSel::Named(n) if self.cards.iter().any(|(c, _)| c == n) => Ok(()),
            CardSel::Named(n) => Err(ArbiterError::UnknownCard(
                n.clone(),
                self.card_names().collect::<Vec<_>>().join(", "),
            )),
            CardSel::Any if self.cards.iter().any(|(_, steer)| *steer) => Ok(()),
            CardSel::Any => Err(ArbiterError::NoSteerableCard),
        }
    }

    fn next_id(&self) -> u64 {
        self.ids.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Wait until an LLM request for `model` may go to the backend. Keep the guard for the
    /// whole request, response streaming included.
    pub async fn enter_llm(&self, model: &str, class: &str) -> Result<LlmGuard, ArbiterError> {
        let id = self.next_id();
        let (reply, rx) = oneshot::channel();
        self.send(Msg::EnterLlm {
            id,
            model: model.to_owned(),
            class: class.to_owned(),
            reply,
        })?;
        let cancel = CancelOnDrop {
            tx: self.tx.clone(),
            id,
        };
        rx.await.map_err(|_| ArbiterError::Closed)?;
        cancel.disarm();
        Ok(LlmGuard {
            tx: self.tx.clone(),
        })
    }

    /// Wait for a lease. The lease lasts until it is dropped. Fails at once for an unknown
    /// card, or for `CardSel::Any` when no card can be picked.
    pub async fn acquire(&self, req: LeaseRequest) -> Result<Lease, ArbiterError> {
        self.check_card(&req.card)?;
        let id = self.next_id();
        let (reply, rx) = oneshot::channel();
        let (class, vram, yieldable) = (req.class.clone(), req.vram, req.yieldable);
        self.send(Msg::Acquire { id, req, reply })?;
        let cancel = CancelOnDrop {
            tx: self.tx.clone(),
            id,
        };
        let granted = rx.await.map_err(|_| ArbiterError::Closed)?;
        cancel.disarm();
        Ok(Lease {
            id: granted.id,
            card: granted.card,
            env: granted.env,
            class,
            vram,
            yieldable,
            yield_rx: granted.yield_rx,
            tx: self.tx.clone(),
        })
    }

    pub async fn status(&self) -> Result<Status, ArbiterError> {
        let (reply, rx) = oneshot::channel();
        self.send(Msg::Status(reply))?;
        rx.await.map_err(|_| ArbiterError::Closed)
    }

    /// Ask for a scheduling pass now (e.g. after something outside changed VRAM use).
    pub fn wake(&self) {
        let _ = self.tx.send(Msg::Wake);
    }

    fn send(&self, msg: Msg) -> Result<(), ArbiterError> {
        self.tx.send(msg).map_err(|_| ArbiterError::Closed)
    }
}

/// Sends `Cancel` if the waiting future is dropped before it was served.
struct CancelOnDrop {
    tx: mpsc::UnboundedSender<Msg>,
    id: u64,
}

impl CancelOnDrop {
    fn disarm(self) {
        std::mem::forget(self);
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Cancel(self.id));
    }
}

/// An admitted LLM request. Dropping it marks the request finished.
#[must_use = "the request counts as in flight until the guard is dropped"]
pub struct LlmGuard {
    tx: mpsc::UnboundedSender<Msg>,
}

impl Drop for LlmGuard {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::ExitLlm);
    }
}

/// A granted lease. Dropping it releases the card.
#[must_use = "the lease is released as soon as it is dropped"]
pub struct Lease {
    id: String,
    card: String,
    env: Vec<(String, String)>,
    class: String,
    vram: Vram,
    yieldable: bool,
    yield_rx: watch::Receiver<bool>,
    tx: mpsc::UnboundedSender<Msg>,
}

impl Lease {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn class(&self) -> &str {
        &self.class
    }

    /// Name of the card this lease holds.
    pub fn card(&self) -> &str {
        &self.card
    }

    /// Environment that points a program at the card (empty when the card has none).
    pub fn env(&self) -> &[(String, String)] {
        &self.env
    }

    pub fn vram(&self) -> Vram {
        self.vram
    }

    pub fn yieldable(&self) -> bool {
        self.yieldable
    }

    /// Whether the arbiter asked this lease to yield.
    pub fn yield_requested(&self) -> bool {
        *self.yield_rx.borrow()
    }

    /// Resolves once the arbiter asks this lease to yield (never, if the arbiter stops).
    pub async fn yield_wanted(&self) {
        let mut rx = self.yield_rx.clone();
        if rx.wait_for(|y| *y).await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Release(std::mem::take(&mut self.id)));
    }
}

impl fmt::Debug for Lease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lease")
            .field("id", &self.id)
            .field("card", &self.card)
            .field("class", &self.class)
            .field("vram", &self.vram)
            .finish()
    }
}
