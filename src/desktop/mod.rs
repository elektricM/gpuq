// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! The desktop: free the VRAM the graphical session holds on a card while something needs it,
//! and give the desktop back afterwards. `[desktop] policy` picks how:
//!
//! * `off`: never touch it (default).
//! * `stop`: stop the configured user units (the session's compositor and shell) and system
//!   units (the display manager), then start the display manager again later.
//! * `offload`: restart the desktop pointed at another card (environment, commands), and
//!   check that nothing of it still holds this card. When that cannot work (monitors only on
//!   this card) or does not (something still holds it), gpuq says so once and uses
//!   `fallback` from then on.
//!
//! One task owns the state ([`machine::Machine`]) and polls every `poll_s`: units, the seat,
//! what gpuq runs on the card. It acts only on transitions and logs only when what it waits
//! for changes. Every step has a timeout and is checked; a release that does not complete is
//! rolled back. Nothing is ever killed: gpuq only stops and starts the units it is given.
//! After each change the journal is checked for new errors, failed units and core dumps.

pub mod input;
pub mod machine;
pub mod procs;
pub mod system;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{sleep, Instant};

use crate::arbiter::{self, OffloadHook, Phase, Vram, WaiterKind};
use crate::backend::Backend;
use crate::config::{Desktop as Cfg, DesktopPolicy as Policy};
use crate::probe::VramReading;
use crate::Mib;
pub use machine::{How, State};
use machine::{Knobs, Machine, Seen, Step, Units};
use procs::{desktop_holders, Holder, Scan};
use system::{Health, Scope, System, UnitInfo};

/// Pause between checks while units stop or start.
const VERIFY_POLL: Duration = Duration::from_secs(1);
/// Pause after the desktop is gone before VRAM is measured.
const DRIVER_SETTLE: Duration = Duration::from_secs(2);
/// Journal lines kept per check.
const HEALTH_LINES: usize = 20;

// ------------------------------------------------------------------------ what gpuq runs

/// What gpuq itself has on the card, as far as the desktop cares.
#[derive(Clone, Debug, Default)]
pub struct Load {
    /// GPU models loaded, when the LLM holds the card.
    pub models: Vec<String>,
    pub vram: Option<VramReading>,
    /// Jobs waiting for the card: `class/name` and the VRAM they declared.
    pub waiting: Vec<(String, Vram)>,
    /// Leases holding the card, `class/name`.
    pub leases: Vec<String>,
    /// The arbiter is moving the LLM on or off the card.
    pub moving: Option<String>,
    pub error: Option<String>,
}

#[async_trait]
pub trait Workload: Send + Sync + 'static {
    async fn load(&self, card: &str) -> Load;
}

/// The arbiter and the LLM backend.
pub struct Gpuq {
    pub arbiter: arbiter::Handle,
    pub backend: Arc<dyn Backend>,
}

#[async_trait]
impl Workload for Gpuq {
    async fn load(&self, card: &str) -> Load {
        let mut l = Load::default();
        let st = match self.arbiter.status().await {
            Ok(st) => st,
            Err(e) => {
                l.error = Some(e.to_string());
                return l;
            }
        };
        let Some(c) = st.cards.iter().find(|c| c.name == card) else {
            l.error = Some(format!("no card {card}"));
            return l;
        };
        l.vram = c.vram;
        match c.state {
            Phase::Unloading => l.moving = Some("gpuq is unloading the LLM".into()),
            Phase::Restoring => l.moving = Some("gpuq is reloading the LLM".into()),
            Phase::Jobs => {
                for w in st.queue.iter().filter(|w| w.kind == WaiterKind::Job) {
                    let mine = w.card.split(',').any(|n| n == card) || w.card.starts_with("any");
                    if let (true, Some(v)) = (mine, w.vram) {
                        l.waiting.push((format!("{}/{}", w.class, w.name), v));
                    }
                }
            }
            Phase::Llm => {}
        }
        if st.llm_cards.iter().any(|n| n == card) && c.state == Phase::Llm {
            match self.backend.loaded_gpu_models().await {
                Ok(m) => l.models = m,
                Err(e) => l.error = Some(e.to_string()),
            }
        }
        l.leases = st
            .leases
            .iter()
            .filter(|x| x.cards.iter().any(|n| n == card))
            .map(|x| format!("{}/{}", x.class, x.name))
            .collect();
        l
    }
}

// ------------------------------------------------------------------------ status

/// What one release did.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Report {
    /// Unix seconds.
    pub at: u64,
    pub how: String,
    pub took_s: f64,
    pub vram_before_mib: Option<Mib>,
    pub vram_after_mib: Option<Mib>,
    /// VRAM the desktop's processes had resident on the card before.
    pub desktop_vram_mib: Option<Mib>,
    /// The desktop's processes on the card before, `pid comm`.
    pub desktop_processes: Vec<String>,
}

/// The journal after the last change.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Check {
    /// Which change, e.g. `release`.
    pub after: String,
    /// Unix seconds: the window checked.
    pub from: u64,
    pub to: u64,
    pub clean: bool,
    /// Failed units that were not failed before the change.
    pub new_failed_units: Vec<String>,
    #[serde(flatten)]
    pub health: Health,
}

/// `GET /desktop` and the `desktop` object in `GET /status`.
#[derive(Clone, Debug, Serialize)]
pub struct DesktopStatus {
    pub policy: String,
    pub card: String,
    pub state: State,
    pub how: Option<How>,
    pub since_s: u64,
    /// What it waits for, or why it is in its state.
    pub why: String,
    pub wants: Vec<String>,
    pub keeps: Vec<String>,
    pub seat: String,
    pub seat_users: Vec<String>,
    pub units: Vec<UnitView>,
    pub hold_s: Option<u64>,
    pub retry_s: Option<u64>,
    pub last_error: Option<String>,
    /// Why offload does not work here, once it was found out.
    pub offload_impossible: Option<String>,
    pub last_release: Option<Report>,
    pub last_check: Option<Check>,
    /// The processes on the card, with `GET /desktop` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub holders: Option<Vec<HolderView>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unreadable_processes: Option<usize>,
}

#[derive(Clone, Debug, Serialize)]
pub struct UnitView {
    pub name: String,
    pub user: bool,
    pub active: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct HolderView {
    #[serde(flatten)]
    pub holder: Holder,
    /// `desktop`, `ignored` or `other`.
    pub role: &'static str,
}

/// The status, plus the instant `since_s` counts from.
#[derive(Clone)]
struct View {
    st: DesktopStatus,
    since: Instant,
    /// The units' cgroups as last seen while they ran.
    cgroups: Vec<String>,
}

// ------------------------------------------------------------------------ handle

enum Cmd {
    Release {
        force: bool,
        reply: oneshot::Sender<Result<String, String>>,
    },
    Restore {
        hold: Option<Duration>,
        reply: oneshot::Sender<Result<String, String>>,
    },
    Offload {
        reply: oneshot::Sender<bool>,
    },
}

/// Talks to the desktop task. Cheap to clone.
#[derive(Clone)]
pub struct Handle {
    tx: mpsc::UnboundedSender<Cmd>,
    view: watch::Receiver<View>,
    sys: Arc<dyn System>,
    ignore: Arc<Vec<String>>,
    card: String,
}

impl Handle {
    pub fn status(&self) -> DesktopStatus {
        let v = self.view.borrow();
        let mut st = v.st.clone();
        st.since_s = v.since.elapsed().as_secs();
        st
    }

    /// The status with the processes that hold the card right now.
    pub async fn status_with_holders(&self) -> DesktopStatus {
        let mut st = self.status();
        let cgroups = self.view.borrow().cgroups.clone();
        let scan = self.sys.scan().await;
        st.holders = Some(roles(&scan, &cgroups, &self.ignore));
        st.unreadable_processes = Some(scan.unreadable);
        st
    }

    /// Release now, without waiting for the delays. Waits until done.
    pub async fn release(&self, force: bool) -> Result<String, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Cmd::Release { force, reply })
            .map_err(|_| "the desktop task is not running".to_owned())?;
        rx.await
            .map_err(|_| "the desktop task stopped".to_owned())?
    }

    /// Restore now and hold off releases for `hold` (default `hold_s`). Waits until done.
    pub async fn restore(&self, hold: Option<Duration>) -> Result<String, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Cmd::Restore { hold, reply })
            .map_err(|_| "the desktop task is not running".to_owned())?;
        rx.await
            .map_err(|_| "the desktop task stopped".to_owned())?
    }
}

/// The arbiter's offload hook: a job does not fit on an idle card.
#[async_trait]
impl OffloadHook for Handle {
    async fn run(&self, card: &str) -> bool {
        if card != self.card {
            return false;
        }
        let (reply, rx) = oneshot::channel();
        if self.tx.send(Cmd::Offload { reply }).is_err() {
            return false;
        }
        rx.await.unwrap_or(false)
    }
}

/// Runs the desktop hook and then another one (the `[hooks] offload` command).
pub struct Chain(pub Vec<Arc<dyn OffloadHook>>);

#[async_trait]
impl OffloadHook for Chain {
    async fn run(&self, card: &str) -> bool {
        let mut any = false;
        for h in &self.0 {
            any |= h.run(card).await;
        }
        any
    }
}

fn roles(scan: &Scan, cgroups: &[String], ignore: &[String]) -> Vec<HolderView> {
    let desk: Vec<u32> = desktop_holders(scan, cgroups, ignore)
        .iter()
        .map(|h| h.pid)
        .collect();
    scan.holders
        .iter()
        .map(|h| HolderView {
            holder: h.clone(),
            role: if desk.contains(&h.pid) {
                "desktop"
            } else if procs::ignored(h, ignore) {
                "ignored"
            } else {
                "other"
            },
        })
        .collect()
}

// ------------------------------------------------------------------------ persisted state

#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    /// gpuq stopped the desktop.
    stopped: bool,
    /// gpuq moved the desktop to another card; whether it moves it back on its own.
    moved: Option<bool>,
    /// The offload environment that is set, to remove it on restore after a restart.
    #[serde(default)]
    env_set: bool,
}

fn load_saved(path: Option<&PathBuf>) -> Saved {
    path.and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save(path: Option<&PathBuf>, s: &Saved) {
    let Some(p) = path else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text = serde_json::to_string(s).unwrap_or_default();
    if let Err(e) = std::fs::write(p, text) {
        tracing::warn!("desktop: cannot write {}: {e}", p.display());
    }
}

// ------------------------------------------------------------------------ the task

/// Everything the task needs besides the config.
pub struct Parts {
    pub card: String,
    /// The card's PCI address, to tell which card the monitors are on.
    pub pci: Option<String>,
    pub sys: Arc<dyn System>,
    pub work: Arc<dyn Workload>,
    pub input: Option<input::InputWatch>,
    /// Asked for a scheduling pass after each change.
    pub arbiter: Option<arbiter::Handle>,
}

/// Start the desktop task. Returns once the first observation is done.
pub async fn spawn(cfg: Cfg, parts: Parts, shutdown: watch::Receiver<bool>) -> Handle {
    let cfg = Arc::new(cfg);
    let (tx, rx) = mpsc::unbounded_channel();
    let mut c = Controller::new(cfg.clone(), parts).await;
    let handle = Handle {
        tx,
        view: c.view_rx.clone(),
        sys: c.sys.clone(),
        ignore: Arc::new(cfg.ignore.clone()),
        card: c.card.clone(),
    };
    tokio::spawn(async move { c.run(rx, shutdown).await });
    handle
}

struct Controller {
    cfg: Arc<Cfg>,
    card: String,
    pci: Option<String>,
    sys: Arc<dyn System>,
    work: Arc<dyn Workload>,
    input: Option<input::InputWatch>,
    arbiter: Option<arbiter::Handle>,
    m: Machine,
    state_path: Option<PathBuf>,
    view_tx: Arc<watch::Sender<View>>,
    view_rx: watch::Receiver<View>,
    /// Models loaded when VRAM ran short; the pressure reason lasts while one of them stays.
    pressure: Option<Vec<String>>,
    offload_impossible: Option<String>,
    /// The offload environment and commands are applied.
    env_set: bool,
    last_logged: String,
    last_seen: Option<Seen>,
    last_release: Option<Report>,
    cgroups: Vec<String>,
    warned_units: bool,
}

impl Controller {
    async fn new(cfg: Arc<Cfg>, p: Parts) -> Controller {
        let state_path = cfg.state_path();
        let saved = load_saved(state_path.as_ref());
        let units = p.sys_units(&cfg).await;
        let observed = match &units {
            Ok(u) => units_state(u),
            Err(_) => Units::Changing,
        };
        for u in units.iter().flatten() {
            if u.load == "not-found" {
                tracing::warn!("desktop: unit {} not found; it is ignored", u.name);
            }
        }
        let moved = saved.moved.map(|auto_restore| How::Moved { auto_restore });
        let knobs = Knobs {
            release_delay: cfg.release_delay,
            restore_delay: cfg.restore_delay,
            hold: cfg.hold,
            retry: cfg.retry,
            debounce: (cfg.poll * 2).max(Duration::from_secs(10)),
            input_idle: cfg.input_idle,
        };
        let m = Machine::new(knobs, Instant::now(), observed, saved.stopped, moved);
        let cgroups = units.as_ref().map(|u| cgroups_of(u)).unwrap_or_default();
        let st = DesktopStatus {
            policy: cfg.policy.as_str().to_owned(),
            card: p.card.clone(),
            state: m.state(),
            how: (m.state() == State::Released).then(|| m.how()),
            since_s: 0,
            why: m.why().to_owned(),
            wants: Vec::new(),
            keeps: Vec::new(),
            seat: cfg.seat.clone(),
            seat_users: Vec::new(),
            units: units
                .as_ref()
                .map(|u| unit_views(&cfg, u))
                .unwrap_or_default(),
            hold_s: None,
            retry_s: None,
            last_error: None,
            offload_impossible: None,
            last_release: None,
            last_check: None,
            holders: None,
            unreadable_processes: None,
        };
        let (view_tx, view_rx) = watch::channel(View {
            st,
            since: m.since(),
            cgroups: cgroups.clone(),
        });
        tracing::info!(
            "desktop: policy {} on {}, {} ({})",
            cfg.policy.as_str(),
            p.card,
            m.state().name(),
            m.why()
        );
        Controller {
            card: p.card,
            pci: p.pci,
            sys: p.sys,
            work: p.work,
            input: p.input,
            arbiter: p.arbiter,
            m,
            state_path,
            view_tx: Arc::new(view_tx),
            view_rx,
            pressure: None,
            offload_impossible: None,
            env_set: saved.env_set,
            last_logged: String::new(),
            last_seen: None,
            last_release: None,
            cgroups,
            warned_units: false,
            cfg,
        }
    }

    async fn run(
        &mut self,
        mut rx: mpsc::UnboundedReceiver<Cmd>,
        mut shutdown: watch::Receiver<bool>,
    ) {
        loop {
            let seen = self.observe().await;
            let now = Instant::now();
            let step = self.m.step(now, &seen);
            self.last_seen = Some(seen);
            let _ = self.act(step).await;
            self.publish();
            tokio::select! {
                _ = stopped(&mut shutdown) => return,
                cmd = rx.recv() => match cmd {
                    Some(cmd) => self.command(cmd).await,
                    None => return,
                },
                _ = sleep(self.cfg.poll) => {}
            }
        }
    }

    async fn command(&mut self, cmd: Cmd) {
        let seen = self.observe().await;
        let now = Instant::now();
        match cmd {
            Cmd::Release { force, reply } => {
                let r = match self.m.ask_release(now, &seen, force) {
                    Ok(step) => self.act(step).await,
                    Err(e) => Err(e),
                };
                let _ = reply.send(r);
            }
            Cmd::Restore { hold, reply } => {
                let r = match self.m.ask_restore(now, hold) {
                    Ok(step) => self.act(step).await,
                    Err(e) => Err(e),
                };
                let _ = reply.send(r);
            }
            Cmd::Offload { reply } => {
                let ok = if !self.cfg.leases {
                    false
                } else {
                    match self.m.offload(now, &seen) {
                        Ok(Step::Wait) => true,
                        Ok(step) => self.act(step).await.is_ok(),
                        Err(e) => {
                            tracing::info!("desktop: a job does not fit, but not released: {e}");
                            false
                        }
                    }
                };
                let _ = reply.send(ok);
            }
        }
        self.last_seen = Some(seen);
        self.publish();
    }

    /// Carry out a step; Ok with what was done.
    async fn act(&mut self, step: Step) -> Result<String, String> {
        let (what, why) = match &step {
            Step::Wait => return Ok(String::new()),
            Step::Release(w) => ("release", w.clone()),
            Step::Restore(w) => ("restore", w.clone()),
        };
        let started = unix_now();
        let failed_before = self.failed_units().await;
        self.m.begin(Instant::now(), &step);
        self.publish();
        let result = if what == "release" {
            tracing::info!("desktop: releasing {} ({why})", self.card);
            let r = self.release().await;
            let done = r
                .as_ref()
                .map(|(note, how)| (note.clone(), *how))
                .map_err(Clone::clone);
            self.m.released(Instant::now(), done);
            r.map(|(note, _)| note)
        } else {
            tracing::info!("desktop: restoring ({why})");
            let r = self.restore().await;
            self.m.restored(Instant::now(), r.clone());
            r
        };
        match &result {
            Ok(note) => tracing::info!("desktop: {note}"),
            Err(e) => tracing::warn!("desktop: {what} failed: {e}"),
        }
        self.persist();
        self.last_logged = norm(self.m.why());
        if let Some(a) = &self.arbiter {
            a.wake();
        }
        self.publish();
        if self.cfg.check_journal {
            self.check_later(what, started, failed_before);
        }
        result
    }

    fn persist(&self) {
        let saved = Saved {
            stopped: self.m.state() == State::Released && self.m.how() == How::Stopped,
            moved: match (self.m.state(), self.m.how()) {
                (State::Released, How::Moved { auto_restore }) => Some(auto_restore),
                _ => None,
            },
            env_set: self.env_set,
        };
        save(self.state_path.as_ref(), &saved);
    }

    // -------------------------------------------------------------- observing

    async fn units(&self) -> Result<Vec<UnitInfo>, String> {
        let mut all = self
            .sys
            .units(Scope::System, &self.cfg.system_units)
            .await?;
        all.extend(self.sys.units(Scope::User, &self.cfg.user_units).await?);
        Ok(all)
    }

    async fn observe(&mut self) -> Seen {
        let units = match self.units().await {
            Ok(u) => {
                self.warned_units = false;
                let c = cgroups_of(&u);
                if !c.is_empty() {
                    self.cgroups = c;
                }
                self.view_tx
                    .send_modify(|v| v.st.units = unit_views(&self.cfg, &u));
                units_state(&u)
            }
            Err(e) => {
                if !self.warned_units {
                    tracing::warn!("desktop: cannot read the units: {e}");
                    self.warned_units = true;
                }
                Units::Changing
            }
        };
        let (seat_users, seat_error) = match self.sys.sessions(&self.cfg.seat).await {
            Ok(s) => {
                let mut users: Vec<String> = s
                    .iter()
                    .filter(|s| s.is_user())
                    .map(|s| s.user.clone())
                    .collect();
                users.dedup();
                (users, None)
            }
            Err(e) => (Vec::new(), Some(e)),
        };
        let load = self.work.load(&self.card).await;
        let (wants, keeps) = self.reasons(&load);
        Seen {
            units,
            wants,
            keeps,
            seat_users,
            seat_error,
            last_input: self
                .input
                .as_ref()
                .and_then(|i| i.last())
                .map(Instant::from_std),
        }
    }

    /// Reasons to release, and reasons not to restore yet.
    fn reasons(&mut self, l: &Load) -> (Vec<String>, Vec<String>) {
        let mut wants = Vec::new();
        let mut keeps: Vec<String> = l.leases.iter().map(|x| format!("lease {x}")).collect();
        keeps.extend(l.moving.clone());
        if self.policy() == Policy::Off {
            // Offload turned out impossible and the fallback is off: nothing to release for.
            return (wants, keeps);
        }
        for m in &l.models {
            if self.cfg.models.iter().any(|p| glob(p, m)) {
                wants.push(format!("model {m} loaded"));
            }
        }
        let free = l.vram.map(|v| v.total_mib.saturating_sub(v.used_mib));
        if self.cfg.min_free_mib > 0 && !l.models.is_empty() {
            if let Some(f) = free.filter(|f| *f < self.cfg.min_free_mib) {
                if self.pressure.is_none() {
                    tracing::info!(
                        "desktop: {f} MiB free on {} with {} loaded",
                        self.card,
                        l.models.join(", ")
                    );
                }
                self.pressure = Some(l.models.clone());
            }
        }
        if let Some(p) = &self.pressure {
            let still: Vec<&String> = l.models.iter().filter(|m| p.contains(m)).collect();
            if still.is_empty() {
                self.pressure = None;
            } else {
                let names: Vec<&str> = still.iter().map(|s| s.as_str()).collect();
                wants.push(format!(
                    "{} loaded with less than {} MiB free",
                    names.join(", "),
                    self.cfg.min_free_mib
                ));
            }
        }
        if self.cfg.leases {
            for (who, v) in &l.waiting {
                match (v, free) {
                    (Vram::WholeCard, _) => wants.push(format!("job {who} wants the whole card")),
                    (Vram::Mib(m), Some(f)) if *m > f => {
                        wants.push(format!("job {who} needs {m} MiB, {f} MiB free"))
                    }
                    _ => {}
                }
            }
        }
        (wants, keeps)
    }

    async fn failed_units(&self) -> Vec<String> {
        if !self.cfg.check_journal {
            return Vec::new();
        }
        self.sys.health(unix_now()).await.failed_units
    }

    fn publish(&mut self) {
        let now = Instant::now();
        let seen = self.last_seen.clone();
        let m = &self.m;
        let key = norm(m.why());
        if key != self.last_logged {
            tracing::info!("desktop: {} ({})", m.state().name(), m.why());
            self.last_logged = key;
        }
        let impossible = self.offload_impossible.clone();
        let release = self.last_release.clone();
        let cgroups = self.cgroups.clone();
        self.view_tx.send_modify(|v| {
            v.st.state = m.state();
            v.st.how = (m.state() == State::Released).then(|| m.how());
            v.st.why = m.why().to_owned();
            v.st.hold_s = m.hold_left(now).map(|d| d.as_secs());
            v.st.retry_s = m.retry_left(now).map(|d| d.as_secs());
            v.st.last_error = m.last_error().map(str::to_owned);
            v.st.offload_impossible = impossible;
            v.st.last_release = release;
            if let Some(s) = seen {
                v.st.wants = s.wants;
                v.st.keeps = s.keeps;
                v.st.seat_users = s.seat_users;
            }
            v.since = m.since();
            v.cgroups = cgroups;
        });
    }

    // -------------------------------------------------------------- release

    /// The policy this release uses.
    fn policy(&self) -> Policy {
        match (self.cfg.policy, &self.offload_impossible) {
            (Policy::Offload, Some(_)) => self.cfg.fallback,
            (p, _) => p,
        }
    }

    async fn release(&mut self) -> Result<(String, How), String> {
        if self.policy() == Policy::Off {
            let why = self.offload_impossible.clone().unwrap_or_default();
            return Err(format!(
                "offload does not work here ({why}) and fallback is off"
            ));
        }
        let t0 = Instant::now();
        let units = self.units().await?;
        let cgroups = cgroups_of(&units);
        if !cgroups.is_empty() {
            self.cgroups = cgroups;
        }
        let scan = self.sys.scan().await;
        let desk: Vec<Holder> = desktop_holders(&scan, &self.cgroups, &self.cfg.ignore)
            .into_iter()
            .cloned()
            .collect();
        let mut report = Report {
            at: unix_now(),
            vram_before_mib: self.sys.vram_used().await,
            desktop_vram_mib: desk.iter().map(|h| h.vram_mib).sum(),
            desktop_processes: desk
                .iter()
                .map(|h| format!("{} {}", h.pid, h.comm))
                .collect(),
            ..Report::default()
        };
        if !desk.is_empty() {
            tracing::info!(
                "desktop: on {}: {} ({} MiB)",
                self.card,
                report.desktop_processes.join(", "),
                report.desktop_vram_mib.unwrap_or(0)
            );
        }
        let mut how = None;
        if self.policy() == Policy::Offload {
            match self.offload(&units).await {
                Ok(note) => {
                    how = Some((
                        note,
                        How::Moved {
                            auto_restore: self.cfg.offload.move_back,
                        },
                    ))
                }
                Err(Offload::Failed(e)) => return Err(e),
                Err(Offload::Impossible(why)) => {
                    tracing::warn!(
                        "desktop: offload does not work here: {why}; using fallback \"{}\" from now on",
                        self.cfg.fallback.as_str()
                    );
                    self.offload_impossible = Some(why.clone());
                    if self.cfg.fallback != Policy::Stop {
                        return Err(format!("offload impossible ({why}); fallback is off"));
                    }
                }
            }
        }
        let (note, how) = match how {
            Some(h) => h,
            None => {
                // The units as they are now: an offload attempt may have restarted them.
                let units = self.units().await?;
                (self.stop(&units).await?, How::Stopped)
            }
        };
        sleep(DRIVER_SETTLE).await;
        report.vram_after_mib = self.sys.vram_used().await;
        report.took_s = t0.elapsed().as_secs_f64();
        report.how = match how {
            How::Stopped => "stopped".into(),
            How::Moved { .. } => "moved".into(),
        };
        let vram = match (report.vram_before_mib, report.vram_after_mib) {
            (Some(b), Some(a)) => format!("; VRAM {b} -> {a} MiB"),
            _ => String::new(),
        };
        self.last_release = Some(report);
        Ok((format!("{note}{vram}"), how))
    }

    /// Stop the units, check they and their processes are gone, fix the session env.
    async fn stop(&mut self, units: &[UnitInfo]) -> Result<String, String> {
        let user_up = active(units, &self.cfg.user_units);
        let sys_up = active(units, &self.cfg.system_units);
        if !user_up.is_empty() {
            if let Err(e) = self.sys.stop(Scope::User, &user_up).await {
                return Err(self
                    .rollback(&sys_up, format!("stopping {}: {e}", user_up.join(" ")))
                    .await);
            }
        }
        if !sys_up.is_empty() {
            if let Err(e) = self.sys.stop(Scope::System, &sys_up).await {
                return Err(self
                    .rollback(&sys_up, format!("stopping {}: {e}", sys_up.join(" ")))
                    .await);
            }
        }
        if let Err(e) = self.wait_gone().await {
            return Err(self.rollback(&sys_up, e).await);
        }
        self.fix_session_env().await;
        let mut stopped = user_up;
        stopped.extend(sys_up);
        Ok(if stopped.is_empty() {
            "released: nothing was running".into()
        } else {
            format!("released: stopped {}", stopped.join(", "))
        })
    }

    /// Wait until every unit is down and no desktop process holds the card.
    async fn wait_gone(&self) -> Result<(), String> {
        let deadline = Instant::now() + self.cfg.stop_timeout;
        loop {
            let units = self.units().await?;
            let scan = self.sys.scan().await;
            let left: Vec<String> = desktop_holders(&scan, &self.cgroups, &self.cfg.ignore)
                .iter()
                .map(|h| format!("{} {}", h.pid, h.comm))
                .collect();
            let up: Vec<&str> = units
                .iter()
                .filter(|u| !u.is_down() && u.load != "not-found")
                .map(|u| u.name.as_str())
                .collect();
            if up.is_empty() && left.is_empty() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                let mut what = Vec::new();
                if !up.is_empty() {
                    what.push(format!("units still up: {}", up.join(", ")));
                }
                if !left.is_empty() {
                    what.push(format!("still on {}: {}", self.card, left.join(", ")));
                }
                return Err(format!(
                    "after {}s {}",
                    self.cfg.stop_timeout.as_secs(),
                    what.join("; ")
                ));
            }
            sleep(VERIFY_POLL).await;
        }
    }

    /// Start again the system units that were running. Returns the error to report.
    async fn rollback(&self, sys_up: &[String], why: String) -> String {
        if sys_up.is_empty() {
            return why;
        }
        match self.start_and_wait(sys_up).await {
            Ok(()) => format!("{why}; started {} again", sys_up.join(", ")),
            Err(e) => format!("{why}; starting it again failed too: {e}"),
        }
    }

    async fn start_and_wait(&self, units: &[String]) -> Result<(), String> {
        self.sys.start(Scope::System, units).await?;
        self.wait_up(Scope::System, units).await
    }

    async fn wait_up(&self, scope: Scope, units: &[String]) -> Result<(), String> {
        let deadline = Instant::now() + self.cfg.stop_timeout;
        loop {
            let infos = self.sys.units(scope, units).await?;
            if infos.iter().all(UnitInfo::is_up) {
                return Ok(());
            }
            if infos.iter().any(|u| u.active == "failed") || Instant::now() >= deadline {
                let state: Vec<String> = infos
                    .iter()
                    .map(|u| format!("{} {}", u.name, u.active))
                    .collect();
                return Err(format!("not up: {}", state.join(", ")));
            }
            sleep(VERIFY_POLL).await;
        }
    }

    /// Point the user manager away from the display that is gone. A `WAYLAND_DISPLAY` whose
    /// socket does not exist is removed instead.
    async fn fix_session_env(&self) {
        if self.cfg.session_env.is_empty() {
            return;
        }
        let vars: Vec<(String, String)> = self
            .cfg
            .session_env
            .iter()
            .map(|(k, v)| {
                let gone = k == "WAYLAND_DISPLAY" && !v.is_empty() && !self.sys.runtime_socket(v);
                if gone {
                    tracing::info!("desktop: no {v} socket; removing WAYLAND_DISPLAY instead");
                }
                (k.clone(), if gone { String::new() } else { v.clone() })
            })
            .collect();
        let shown: Vec<String> = vars
            .iter()
            .map(|(k, v)| {
                if v.is_empty() {
                    format!("-{k}")
                } else {
                    format!("{k}={v}")
                }
            })
            .collect();
        match self.sys.session_env(&vars).await {
            Ok(()) => tracing::info!("desktop: session env {}", shown.join(" ")),
            Err(e) => tracing::warn!("desktop: session env: {e}"),
        }
    }

    // -------------------------------------------------------------- offload

    async fn offload(&mut self, units: &[UnitInfo]) -> Result<String, Offload> {
        let monitors = self.sys.monitors();
        if let Some(why) = offload_impossible(&monitors, self.pci.as_deref()) {
            return Err(Offload::Impossible(why));
        }
        let cfg = self.cfg.clone();
        let o = &cfg.offload;
        self.apply_offload_env(true)
            .await
            .map_err(|e| Offload::Failed(format!("offload environment: {e}")))?;
        for argv in &o.commands {
            if let Err(e) = self.sys.command(argv).await {
                self.apply_offload_env(false).await.ok();
                return Err(Offload::Failed(format!("offload command: {e}")));
            }
        }
        // Old session units outlive the display manager: stop them so the session starts
        // fresh, pointed at the other card.
        let user_up = active(units, &self.cfg.user_units);
        if !user_up.is_empty() {
            if let Err(e) = self.sys.stop(Scope::User, &user_up).await {
                self.undo_offload().await;
                return Err(Offload::Failed(format!(
                    "stopping {}: {e}",
                    user_up.join(" ")
                )));
            }
        }
        let sys = self.cfg.system_units.clone();
        if let Err(e) = self.sys.restart(Scope::System, &sys).await {
            let e = format!("restarting {}: {e}", sys.join(" "));
            self.undo_offload().await;
            let _ = self.start_and_wait(&sys).await;
            return Err(Offload::Failed(e));
        }
        if let Err(e) = self.wait_up(Scope::System, &sys).await {
            self.undo_offload().await;
            let _ = self.sys.restart(Scope::System, &sys).await;
            return Err(Offload::Failed(format!("after the restart: {e}")));
        }
        sleep(o.settle).await;
        if let Ok(u) = self.units().await {
            let c = cgroups_of(&u);
            if !c.is_empty() {
                self.cgroups = c;
            }
        }
        let scan = self.sys.scan().await;
        let left: Vec<String> = desktop_holders(&scan, &self.cgroups, &self.cfg.ignore)
            .iter()
            .map(|h| {
                let node = if h.primary {
                    "display node"
                } else {
                    "render node"
                };
                format!("{} ({}, {node})", h.comm, h.pid)
            })
            .collect();
        if !left.is_empty() {
            // Moved back by the fallback (stop) or by the restart below.
            self.undo_offload().await;
            if self.cfg.fallback != Policy::Stop {
                let _ = self.sys.restart(Scope::System, &sys).await;
            }
            return Err(Offload::Impossible(format!(
                "after restarting the desktop pointed at the other card, still on {}: {}",
                self.card,
                left.join(", ")
            )));
        }
        Ok(format!(
            "released: moved the desktop off {} ({})",
            self.card,
            sys.join(", ")
        ))
    }

    /// Set (or remove) the offload environment.
    async fn apply_offload_env(&mut self, on: bool) -> Result<(), String> {
        let o = &self.cfg.offload;
        for (scope, env) in [(Scope::User, &o.user_env), (Scope::System, &o.system_env)] {
            if env.is_empty() {
                continue;
            }
            let vars: Vec<(String, String)> = env
                .iter()
                .map(|(k, v)| (k.clone(), if on { v.clone() } else { String::new() }))
                .collect();
            self.sys.manager_env(scope, &vars).await?;
        }
        self.env_set = on;
        Ok(())
    }

    /// Remove the offload environment and run the undo commands. Errors are logged.
    async fn undo_offload(&mut self) {
        if let Err(e) = self.apply_offload_env(false).await {
            tracing::warn!("desktop: removing the offload environment: {e}");
        }
        for argv in &self.cfg.offload.undo_commands {
            if let Err(e) = self.sys.command(argv).await {
                tracing::warn!("desktop: offload undo command: {e}");
            }
        }
    }

    // -------------------------------------------------------------- restore

    async fn restore(&mut self) -> Result<String, String> {
        let sys = self.cfg.system_units.clone();
        let moved = matches!(self.m.how(), How::Moved { .. });
        if moved || self.env_set {
            self.undo_offload().await;
        }
        if moved {
            let units = self.units().await?;
            let user_up = active(&units, &self.cfg.user_units);
            if !user_up.is_empty() {
                self.sys.stop(Scope::User, &user_up).await?;
            }
            self.sys.restart(Scope::System, &sys).await?;
            self.wait_up(Scope::System, &sys).await?;
            return Ok(format!(
                "moved the desktop back to {} ({})",
                self.card,
                sys.join(", ")
            ));
        }
        // A desktop made of user units only is started there.
        let (scope, list) = if sys.is_empty() {
            (Scope::User, self.cfg.user_units.clone())
        } else {
            (Scope::System, sys)
        };
        let t0 = Instant::now();
        self.sys.start(scope, &list).await?;
        self.wait_up(scope, &list).await?;
        Ok(format!(
            "restored: started {} in {:.1}s",
            list.join(", "),
            t0.elapsed().as_secs_f64()
        ))
    }

    // -------------------------------------------------------------- journal

    /// Check the journal `check_delay` after a change, in the background.
    fn check_later(&self, what: &str, from: u64, failed_before: Vec<String>) {
        let (sys, view, delay, what) = (
            self.sys.clone(),
            self.view_tx.clone(),
            self.cfg.check_delay,
            what.to_owned(),
        );
        tokio::spawn(async move {
            sleep(delay).await;
            let mut health = sys.health(from).await;
            let new_failed: Vec<String> = health
                .failed_units
                .iter()
                .filter(|u| !failed_before.contains(u))
                .cloned()
                .collect();
            health.errors.truncate(HEALTH_LINES);
            health.coredumps.truncate(HEALTH_LINES);
            let clean =
                health.errors.is_empty() && health.coredumps.is_empty() && new_failed.is_empty();
            if clean {
                tracing::info!("desktop: journal clean since the {what}");
            } else {
                tracing::warn!(
                    "desktop: since the {what}: {} errors, {} core dumps, new failed units: {}",
                    health.errors.len(),
                    health.coredumps.len(),
                    if new_failed.is_empty() {
                        "none".to_owned()
                    } else {
                        new_failed.join(", ")
                    }
                );
                for l in health.errors.iter().chain(&health.coredumps).take(5) {
                    tracing::warn!("desktop:   {l}");
                }
            }
            for u in &health.unchecked {
                tracing::info!("desktop: not checked: {u}");
            }
            let check = Check {
                after: what,
                from,
                to: unix_now(),
                clean,
                new_failed_units: new_failed,
                health,
            };
            view.send_modify(|v| v.st.last_check = Some(check));
        });
    }
}

impl Parts {
    async fn sys_units(&self, cfg: &Cfg) -> Result<Vec<UnitInfo>, String> {
        let mut all = self.sys.units(Scope::System, &cfg.system_units).await?;
        all.extend(self.sys.units(Scope::User, &cfg.user_units).await?);
        Ok(all)
    }
}

async fn stopped(rx: &mut watch::Receiver<bool>) {
    let _ = rx.wait_for(|s| *s).await;
}

enum Offload {
    /// It went wrong; the desktop was put back as it was.
    Failed(String),
    /// It cannot work here; use the fallback.
    Impossible(String),
}

/// Why the desktop cannot be moved off the card at `pci`: its monitors are on it, or no
/// other card has one.
pub fn offload_impossible(
    monitors: &BTreeMap<String, Vec<String>>,
    pci: Option<&str>,
) -> Option<String> {
    let pci = pci?;
    let here = monitors.get(pci).filter(|m| !m.is_empty());
    let elsewhere = monitors.iter().any(|(p, m)| p != pci && !m.is_empty());
    match (here, elsewhere) {
        (Some(m), _) => Some(format!(
            "monitors are connected to this card ({}); the desktop cannot leave it",
            m.join(", ")
        )),
        (None, false) => Some("no other card has a monitor connected".into()),
        (None, true) => None,
    }
}

fn units_state(units: &[UnitInfo]) -> Units {
    let known: Vec<&UnitInfo> = units.iter().filter(|u| u.load != "not-found").collect();
    if known.iter().any(|u| !u.is_up() && !u.is_down()) {
        Units::Changing
    } else if known.iter().any(|u| u.is_up()) {
        Units::Up
    } else {
        Units::Down
    }
}

fn cgroups_of(units: &[UnitInfo]) -> Vec<String> {
    units
        .iter()
        .map(|u| u.cgroup.clone())
        .filter(|c| !c.is_empty())
        .collect()
}

/// The configured units among `units` that are not down.
fn active(units: &[UnitInfo], names: &[String]) -> Vec<String> {
    names
        .iter()
        .filter(|n| {
            units
                .iter()
                .any(|u| &u.name == *n && !u.is_down() && u.load != "not-found")
        })
        .cloned()
        .collect()
}

fn unit_views(cfg: &Cfg, units: &[UnitInfo]) -> Vec<UnitView> {
    units
        .iter()
        .map(|u| UnitView {
            name: u.name.clone(),
            user: cfg.user_units.contains(&u.name) && !cfg.system_units.contains(&u.name),
            active: if u.load == "not-found" {
                "not-found".into()
            } else {
                u.active.clone()
            },
        })
        .collect()
}

/// The text with digits dropped, so a countdown does not count as a change worth logging.
fn norm(s: &str) -> String {
    s.chars().filter(|c| !c.is_ascii_digit()).collect()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `*` matches any run of characters; everything else matches itself.
pub fn glob(pattern: &str, s: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == s;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !s.starts_with(first) || s.len() < first.len() + last.len() || !s.ends_with(last) {
        return false;
    }
    let mut rest = &s[first.len()..s.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests;
