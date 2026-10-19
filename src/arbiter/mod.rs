// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! The arbiter: one owner of each card at a time, a priority queue for everyone else.
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
//! In the `llm` phase, a class with `hold_other_models` keeps the backend on its model: while
//! it has requests in flight, or had one less than that long ago, requests of less important
//! classes for another model wait, so the server does not swap models back and forth.
//!
//! A lease can hold several cards. It is granted all at once or not at all, so nothing ever
//! holds one card while it waits for another. The LLM can also span several cards; it then
//! gives all of them back together.
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

/// Per-class overrides for the quiet time after an LLM request and the reload delay, and
/// how long the class keeps other models off the backend.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClassTuning {
    pub gap: Option<Duration>,
    pub reload_delay: Option<Duration>,
    /// While requests of this class are in flight, and this long after the last one,
    /// requests of less important classes for another model wait.
    pub hold_other_models: Option<Duration>,
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

    /// Classes that hold other models off the backend, with their hold time.
    pub fn model_holders(&self) -> impl Iterator<Item = (&str, Duration)> {
        self.class_tuning
            .iter()
            .filter_map(|(c, t)| Some((c.as_str(), t.hold_other_models?)))
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

/// Which cards the LLM uses. Names refer to [`Card::name`]; unknown names are ignored.
#[derive(Clone, Debug, Default)]
pub struct LlmPlan {
    /// Cards a model uses when nothing more specific is known. Empty: the `llm_card` given to
    /// [`spawn_cards`].
    pub cards: Vec<String>,
    /// Canonical model id to the cards it is split over.
    pub model_cards: HashMap<String, Vec<String>>,
    /// Canonical model id to the VRAM it needs over all its cards. A model that needs more
    /// than `cards` offer gets the fewest extra cards that fit it.
    pub model_vram: HashMap<String, Mib>,
}

/// Everything the arbiter needs to know at start.
#[derive(Clone, Debug)]
pub struct Settings {
    pub classes: Classes,
    /// Model reloaded when the card goes idle; None = load models on demand only.
    pub resident: Option<String>,
    pub tuning: Tuning,
    pub llm: LlmPlan,
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
    /// Vendor, e.g. "amd" or "nvidia". Two cards of different vendors may share a lease only
    /// when their env uses different variables, see [`merge_env`].
    pub vendor: String,
}

impl fmt::Debug for Card {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Card")
            .field("name", &self.name)
            .field("device", &self.probe.device())
            .field("env", &self.env)
            .field("vendor", &self.vendor)
            .finish()
    }
}

/// Which card or cards a job wants.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum CardSel {
    /// Nothing said: any card that can be steered, or the primary card when none can.
    #[default]
    Default,
    /// The first configured card.
    Primary,
    Named(String),
    /// Whichever card can take the job first. Only cards with `env` qualify, so the job can
    /// be pointed at the card it got.
    Any,
    /// All of these cards at once.
    Set(Vec<String>),
    /// Every configured card at once.
    All,
    /// Any `n` cards at once, picked like `Any`.
    Count(usize),
}

impl CardSel {
    /// `gpu1`, `any`, `all`, `primary` or a comma list such as `gpu0,gpu1`.
    pub fn parse(s: &str) -> Result<CardSel, String> {
        let s = s.trim();
        if s.contains(',') {
            let names: Vec<String> = s.split(',').map(|n| n.trim().to_owned()).collect();
            if names.iter().any(|n| n.is_empty()) {
                return Err(format!("bad card list {s:?}"));
            }
            if let Some(n) = names
                .iter()
                .find(|n| matches!(n.as_str(), "any" | "all" | "primary"))
            {
                return Err(format!("\"{n}\" cannot be part of a card list"));
            }
            return Ok(CardSel::Set(names));
        }
        Ok(match s {
            "" => return Err("empty card name".into()),
            "any" => CardSel::Any,
            "all" => CardSel::All,
            "primary" => CardSel::Primary,
            name => CardSel::Named(name.to_owned()),
        })
    }
}

impl fmt::Display for CardSel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CardSel::Default => f.write_str("default"),
            CardSel::Primary => f.write_str("primary"),
            CardSel::Named(n) => f.write_str(n),
            CardSel::Any => f.write_str("any"),
            CardSel::Set(names) => f.write_str(&names.join(",")),
            CardSel::All => f.write_str("all"),
            CardSel::Count(n) => write!(f, "any {n}"),
        }
    }
}

/// The env of several cards as one list, in the order given. A variable set by several
/// cards gets their values joined with commas (`CUDA_VISIBLE_DEVICES=0,1`). Fails when two
/// cards of different vendors set the same variable: their device numbers do not mean the
/// same thing.
pub fn merge_env<'a>(
    cards: impl IntoIterator<Item = (&'a str, &'a str, &'a [(String, String)])>,
) -> Result<Vec<(String, String)>, String> {
    // variable -> (value, first card, its vendor)
    let mut out: Vec<(String, String, &str, &str)> = Vec::new();
    for (name, vendor, env) in cards {
        for (k, v) in env {
            match out.iter_mut().find(|e| e.0 == *k) {
                Some(e) if e.3 != vendor => {
                    return Err(format!(
                        "cards {} ({}) and {name} ({vendor}) both set {k}; cards of different \
                         vendors can share a lease only when they use different variables",
                        e.2, e.3
                    ));
                }
                Some(e) => {
                    e.1.push(',');
                    e.1.push_str(v);
                }
                None => out.push((k.clone(), v.clone(), name, vendor)),
            }
        }
    }
    Ok(out.into_iter().map(|(k, v, _, _)| (k, v)).collect())
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
    #[error("card '{0}' is listed twice")]
    DuplicateCard(String),
    #[error("asked for {0} cards, but only {1} can be picked (cards need visible_devices)")]
    NotEnoughCards(usize, usize),
    #[error("card '{0}' has no visible_devices; a lease on several cards needs it on every card")]
    NoEnv(String),
    #[error("{0}")]
    MixedVendors(String),
}

// ------------------------------------------------------------------------ status

#[derive(Clone, Debug, Serialize)]
pub struct LeaseStatus {
    pub id: String,
    /// Card names joined with commas.
    pub card: String,
    /// Every card the lease holds, in grant order.
    pub cards: Vec<String>,
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
    /// Card name, a comma list of names, "any", or "any N" for N cards.
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
    /// The LLM backend lives on this card (or comes back to it).
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
    /// The cards the LLM backend holds now; empty while it is unloaded.
    pub llm_cards: Vec<String>,
    /// LLM requests waiting because a more important class is using another model.
    pub model_hold: Option<ModelHold>,
}

/// Requests held so the backend stays on the model a more important class uses.
#[derive(Clone, Debug, Serialize)]
pub struct ModelHold {
    /// The class being served, and its model.
    pub behind_class: String,
    pub behind_model: String,
    /// How many requests wait, and for which models.
    pub held: usize,
    pub models: Vec<String>,
    /// Since the hold began.
    pub since_s: u64,
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
    ExitLlm {
        class: String,
        model: String,
    },
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
    cards: Vec<String>,
    env: Vec<(String, String)>,
    yield_rx: watch::Receiver<bool>,
}

/// What the handle knows about a card, to check requests before they queue.
struct CardInfo {
    name: String,
    /// Can be picked for `CardSel::Any` and `CardSel::Count`.
    steer: bool,
    vendor: String,
    env: Vec<(String, String)>,
}

/// Cheap to clone; every clone talks to the same arbiter task.
#[derive(Clone)]
pub struct Handle {
    tx: mpsc::UnboundedSender<Msg>,
    classes: Arc<Classes>,
    /// Cards in config order.
    cards: Arc<Vec<CardInfo>>,
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
        vendor: String::new(),
    };
    spawn_cards(settings, vec![card], 0, backend, offload)
}

/// Start the arbiter for several cards. `cards[0]` is the primary card; the LLM backend lives
/// on `cards[llm_card]` unless `settings.llm` names its cards. Panics if `cards` is empty or
/// `llm_card` is out of range.
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
                .map(|c| CardInfo {
                    name: c.name.clone(),
                    steer: !many || !c.env.is_empty(),
                    vendor: c.vendor.clone(),
                    env: c.env.clone(),
                })
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
        self.cards.iter().map(|c| c.name.as_str())
    }

    fn known(&self) -> String {
        self.card_names().collect::<Vec<_>>().join(", ")
    }

    /// Check a card selection before queueing.
    pub fn check_card(&self, sel: &CardSel) -> Result<(), ArbiterError> {
        let find = |n: &str| {
            self.cards
                .iter()
                .find(|c| c.name == n)
                .ok_or_else(|| ArbiterError::UnknownCard(n.to_owned(), self.known()))
        };
        let steerable = self.cards.iter().filter(|c| c.steer).count();
        match sel {
            CardSel::Default | CardSel::Primary => Ok(()),
            CardSel::Named(n) => find(n).map(|_| ()),
            CardSel::Any if steerable > 0 => Ok(()),
            CardSel::Any => Err(ArbiterError::NoSteerableCard),
            CardSel::Count(0) => Err(ArbiterError::NotEnoughCards(0, steerable)),
            CardSel::Count(1) if steerable == 0 => Err(ArbiterError::NoSteerableCard),
            CardSel::Count(n) if *n > steerable => Err(ArbiterError::NotEnoughCards(*n, steerable)),
            CardSel::Count(_) => Ok(()),
            CardSel::Set(names) => {
                let mut picked: Vec<&CardInfo> = Vec::new();
                for n in names {
                    let c = find(n)?;
                    if picked.iter().any(|p| p.name == c.name) {
                        return Err(ArbiterError::DuplicateCard(n.clone()));
                    }
                    picked.push(c);
                }
                self.check_set(&picked)
            }
            CardSel::All => self.check_set(&self.cards.iter().collect::<Vec<_>>()),
        }
    }

    /// Several cards in one lease: each needs env, and the env must merge.
    fn check_set(&self, cards: &[&CardInfo]) -> Result<(), ArbiterError> {
        if cards.len() < 2 {
            return Ok(());
        }
        if let Some(c) = cards.iter().find(|c| c.env.is_empty()) {
            return Err(ArbiterError::NoEnv(c.name.clone()));
        }
        merge_env(
            cards
                .iter()
                .map(|c| (c.name.as_str(), c.vendor.as_str(), c.env.as_slice())),
        )
        .map(|_| ())
        .map_err(ArbiterError::MixedVendors)
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
            class: class.to_owned(),
            model: model.to_owned(),
        })
    }

    /// Wait for a lease. The lease lasts until it is dropped. Fails at once when
    /// [`Handle::check_card`] refuses the card selection.
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
            card: granted.cards.join(","),
            cards: granted.cards,
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
    class: String,
    model: String,
}

impl Drop for LlmGuard {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::ExitLlm {
            class: std::mem::take(&mut self.class),
            model: std::mem::take(&mut self.model),
        });
    }
}

/// A granted lease. Dropping it releases the card.
#[must_use = "the lease is released as soon as it is dropped"]
pub struct Lease {
    id: String,
    card: String,
    cards: Vec<String>,
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

    /// Name of the card this lease holds; several names joined with commas.
    pub fn card(&self) -> &str {
        &self.card
    }

    /// Every card this lease holds, in grant order.
    pub fn cards(&self) -> &[String] {
        &self.cards
    }

    /// Environment that points a program at its cards (empty when the cards have none).
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
