//! Immich ML on the GPU, only while gpud grants a lease.
//!
//! Immich is configured with two ML URLs, the GPU container first and a CPU one second. The
//! GPU container exists only while this task holds a lease: a reachable container without
//! VRAM would fail requests instead of letting Immich fall back to the CPU one.
//!
//! The loop, once per `poll_s`:
//!
//! * Backlog in the ML queues at `min_backlog` or more: pause the queues (so the CPU container
//!   does not chew through a reindex) and queue for a yieldable lease.
//! * Granted: start the container, wait until it is healthy, wait `settle_s` so Immich has
//!   noticed it, raise the queue concurrency, resume the queues.
//! * Asked to yield, or the backlog empty for two polls: pause, let active jobs finish, stop the
//!   container, put the concurrency back, release. A leftover backlog of `cpu_ok_backlog` or
//!   less is resumed for the CPU container; a larger one stays paused for the next lease.
//!
//! This is a plain task next to the arbiter, not a gated service: nothing proxies through it.

use std::collections::BTreeMap;
use std::future::pending;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context};
use reqwest::Method;
use serde_json::{json, Value};
use tokio::process::Command;
use tokio::sync::watch;
use tokio::time::{sleep, timeout, Instant};

use crate::arbiter::{CardSel, Handle, Lease, LeaseRequest};
use crate::config::{parse_vram, Immich};

/// Lease name, as shown in `gpud status`.
pub const LEASE_NAME: &str = "immich-ml";

/// Per-request limit for the Immich API.
const API_TIMEOUT: Duration = Duration::from_secs(30);
/// Checks for active jobs before the container is stopped anyway.
const DRAIN_CHECKS: usize = 60;
/// Time the release path gets when gpud is shutting down.
const SHUTDOWN_RELEASE: Duration = Duration::from_secs(30);
/// Longest wait for one docker call.
const DOCKER_TIMEOUT: Duration = Duration::from_secs(300);
/// Failed polls in a row while serving before the card is given back.
const MAX_POLL_ERRORS: u32 = 3;
const HEALTH_POLL: Duration = Duration::from_secs(3);
/// How many times a failing `compose stop` is tried.
const STOP_TRIES: u32 = 3;

/// What the adapter is doing, for `/status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    /// Queues paused, queued for a lease.
    Waiting,
    /// Lease held, container coming up.
    Starting,
    Serving,
    /// Winding down: waiting for jobs, stopping the container.
    Draining,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Waiting => "waiting",
            State::Starting => "starting",
            State::Serving => "serving",
            State::Draining => "draining",
        }
    }
}

#[derive(Clone, Debug)]
struct Snapshot {
    state: State,
    backlog: Option<u64>,
    paused: bool,
    error: Option<String>,
}

/// What the adapter shows the outside: the `immich` object in `/status`.
pub struct ImmichView(Mutex<Snapshot>);

impl ImmichView {
    pub fn new() -> Arc<Self> {
        Arc::new(ImmichView(Mutex::new(Snapshot {
            state: State::Idle,
            backlog: None,
            paused: false,
            error: None,
        })))
    }

    fn update(&self, f: impl FnOnce(&mut Snapshot)) {
        if let Ok(mut s) = self.0.lock() {
            f(&mut s);
        }
    }

    fn get(&self) -> Snapshot {
        match self.0.lock() {
            Ok(s) => s.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }

    /// `{"state", "backlog", "paused_by_gpud", "error"}`.
    pub fn json(&self) -> Value {
        let s = self.get();
        json!({
            "state": s.state.as_str(),
            "backlog": s.backlog,
            "paused_by_gpud": s.paused,
            "error": s.error,
        })
    }
}

/// Why the Immich API did not give an answer.
#[derive(Debug)]
enum ApiError {
    NoKey,
    /// 401: the key is wrong.
    Unauthorized,
    Other(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::NoKey => f.write_str("no usable API key"),
            ApiError::Unauthorized => f.write_str("Immich rejects the API key"),
            ApiError::Other(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for ApiError {}

#[derive(Default)]
struct KeyWatch {
    /// The key last read, if it was usable.
    last: Option<String>,
    /// A problem with the current key (or no key) was already logged.
    warned: bool,
    /// A key Immich answered 401 to; no calls are made while the file still holds it.
    rejected: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wake {
    Timer,
    Yield,
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stat {
    waiting: u64,
    active: u64,
    delayed: u64,
    paused: u64,
}

type Stats = BTreeMap<String, Stat>;

/// Jobs the ML queues still hold: `waiting + active + delayed + paused`.
fn backlog(stats: &Stats) -> u64 {
    stats
        .values()
        .map(|s| s.waiting + s.active + s.delayed + s.paused)
        .sum()
}

/// What `serve` has done so far, for the release path.
#[derive(Default)]
struct Progress {
    /// The queues were resumed on the GPU, so they have to be paused and drained again.
    served: bool,
    /// Concurrency before it was raised.
    old_concurrency: BTreeMap<String, u64>,
}

struct Adapter {
    c: Immich,
    arbiter: Handle,
    view: Arc<ImmichView>,
    http: reqwest::Client,
    base: String,
    state_path: PathBuf,
    request: LeaseRequest,
    shutdown: watch::Receiver<bool>,
    key: Mutex<KeyWatch>,
    health_poll: Duration,
}

/// Run the adapter until `shutdown` turns true. If it holds the card then, the release path
/// runs first (bounded to 30 s).
pub async fn run(
    cfg: Immich,
    arbiter: Handle,
    shutdown: watch::Receiver<bool>,
    view: Arc<ImmichView>,
) -> anyhow::Result<()> {
    let a = Adapter::new(cfg, arbiter, shutdown, view)?;
    a.stop_orphan().await;
    while !a.stopping() {
        match a.tick().await {
            Ok(note) => a.view.update(|s| s.error = note),
            Err(e) => {
                tracing::warn!("immich: {e:#}");
                a.view.update(|s| {
                    s.error = Some(format!("{e:#}"));
                    s.state = State::Idle;
                });
            }
        }
        if a.nap(a.c.poll, None).await == Wake::Shutdown {
            break;
        }
    }
    Ok(())
}

impl Adapter {
    fn new(
        c: Immich,
        arbiter: Handle,
        shutdown: watch::Receiver<bool>,
        view: Arc<ImmichView>,
    ) -> anyhow::Result<Adapter> {
        let vram = parse_vram(&c.vram).context("immich.vram")?;
        let state_path = c
            .state_path()
            .ok_or_else(|| anyhow!("immich: no state_file and no home directory"))?;
        let request = LeaseRequest {
            class: c.class.clone(),
            vram,
            yieldable: true,
            name: LEASE_NAME.into(),
            pid: None,
            resume_id: None,
            card: c.card.clone().map_or(CardSel::Primary, CardSel::Named),
        };
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(API_TIMEOUT)
            .build()
            .context("immich: http client")?;
        let paused = load_state(&state_path);
        view.update(|s| s.paused = paused);
        Ok(Adapter {
            base: format!("{}/api", c.url.trim_end_matches('/')),
            health_poll: HEALTH_POLL.min(c.poll),
            c,
            arbiter,
            view,
            http,
            state_path,
            request,
            shutdown,
            key: Mutex::new(KeyWatch::default()),
        })
    }

    fn stopping(&self) -> bool {
        *self.shutdown.borrow()
    }

    fn paused(&self) -> bool {
        self.view.get().paused
    }

    fn set_state(&self, state: State) {
        self.view.update(|s| s.state = state);
    }

    /// Sleep `d`, or less if gpud shuts down or `lease` is asked to yield.
    async fn nap(&self, d: Duration, lease: Option<&Lease>) -> Wake {
        let mut sd = self.shutdown.clone();
        tokio::select! {
            _ = sleep(d) => Wake::Timer,
            _ = sd.wait_for(|v| *v) => Wake::Shutdown,
            _ = async {
                match lease {
                    Some(l) => l.yield_wanted().await,
                    None => pending::<()>().await,
                }
            } => Wake::Yield,
        }
    }

    // ------------------------------------------------------------------ key and API

    /// The API key. Read on every call: the file gets rotated and gpud must follow.
    fn key(&self) -> Option<String> {
        let read = std::fs::read_to_string(&self.c.key_file)
            .map_err(|e| e.to_string())
            .map(|t| t.trim().to_owned());
        let mut w = self.key.lock().unwrap_or_else(|p| p.into_inner());
        match read {
            Ok(key) if !key.is_empty() => {
                if w.last.as_deref() != Some(key.as_str()) {
                    w.warned = false;
                    w.last = Some(key.clone());
                }
                Some(key)
            }
            other => {
                if !w.warned {
                    let why = other.err().unwrap_or_else(|| "the file is empty".into());
                    tracing::warn!(
                        "immich: no usable API key at {} ({why}); adapter idle",
                        self.c.key_file.display()
                    );
                    w.warned = true;
                }
                w.last = None;
                None
            }
        }
    }

    fn reject_key(&self, key: &str) {
        let mut w = self.key.lock().unwrap_or_else(|p| p.into_inner());
        if !w.warned {
            tracing::warn!(
                "immich: Immich rejects the API key in {}; waiting for a new one",
                self.c.key_file.display()
            );
            w.warned = true;
        }
        w.rejected = Some(key.to_owned());
    }

    fn is_rejected(&self, key: &str) -> bool {
        let w = self.key.lock().unwrap_or_else(|p| p.into_inner());
        w.rejected.as_deref() == Some(key)
    }

    async fn api(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, ApiError> {
        let key = self.key().ok_or(ApiError::NoKey)?;
        let mut req = self
            .http
            .request(method.clone(), format!("{}{path}", self.base))
            .header("x-api-key", &key);
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| ApiError::Other(format!("Immich {method} {path}: {e}")))?;
        let code = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| ApiError::Other(format!("Immich {method} {path}: {e}")))?;
        if code.as_u16() == 401 {
            self.reject_key(&key);
            return Err(ApiError::Unauthorized);
        }
        if code.as_u16() >= 400 {
            let head: String = text.chars().take(300).collect();
            return Err(ApiError::Other(format!(
                "Immich {method} {path}: {} {head}",
                code.as_u16()
            )));
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text)
            .map_err(|e| ApiError::Other(format!("Immich {method} {path}: bad JSON: {e}")))
    }

    async fn stats(&self) -> Result<Stats, ApiError> {
        let list = self.api(Method::GET, "/queues", None).await?;
        let mut out = Stats::new();
        for q in list.as_array().map_or(&[][..], Vec::as_slice) {
            let Some(name) = q["name"]
                .as_str()
                .filter(|n| self.c.queues.iter().any(|w| w == n))
            else {
                continue;
            };
            let n = |k: &str| q["statistics"][k].as_u64().unwrap_or(0);
            out.insert(
                name.to_owned(),
                Stat {
                    waiting: n("waiting"),
                    active: n("active"),
                    delayed: n("delayed"),
                    paused: n("paused"),
                },
            );
        }
        Ok(out)
    }

    /// Pause or resume every ML queue and remember that gpud did it. A failure on one queue does
    /// not stop the others; the first error is returned.
    async fn set_paused(&self, paused: bool) -> Result<(), ApiError> {
        let mut first = None;
        let mut done = 0;
        for q in &self.c.queues {
            match self
                .api(
                    Method::PUT,
                    &format!("/queues/{q}"),
                    Some(&json!({"isPaused": paused})),
                )
                .await
            {
                Ok(_) => done += 1,
                Err(e) => {
                    first.get_or_insert(e);
                }
            }
        }
        // Pausing even one queue means gpud has something to undo; resuming is only done
        // once every queue is.
        if (paused && done > 0) || (!paused && first.is_none()) {
            self.save_state(paused);
        }
        match first {
            None => {
                tracing::info!(
                    "immich: ML queues {}",
                    if paused { "paused" } else { "resumed" }
                );
                Ok(())
            }
            Some(e) => Err(e),
        }
    }

    /// Set the job concurrency per queue; returns the previous values.
    async fn set_concurrency(
        &self,
        values: &BTreeMap<String, u64>,
    ) -> Result<BTreeMap<String, u64>, ApiError> {
        let mut cfg = self.api(Method::GET, "/admin/config", None).await?;
        let mut old = BTreeMap::new();
        for (q, n) in values {
            if let Some(slot) = cfg.pointer_mut(&format!("/job/{q}/concurrency")) {
                if let Some(prev) = slot.as_u64() {
                    old.insert(q.clone(), prev);
                }
                *slot = json!(n);
            }
        }
        self.api(Method::PUT, "/admin/config", Some(&cfg)).await?;
        Ok(old)
    }

    // ------------------------------------------------------------------ state file

    fn save_state(&self, paused: bool) {
        self.view.update(|s| s.paused = paused);
        if let Err(e) = write_state(&self.state_path, paused) {
            tracing::warn!("immich: cannot write {}: {e}", self.state_path.display());
        }
    }

    // ------------------------------------------------------------------ docker

    async fn docker(&self, args: &[&str], cwd: Option<&Path>) -> Result<(bool, String), String> {
        let (prog, prefix) = self.c.docker.split_first().ok_or("empty docker command")?;
        let mut cmd = Command::new(prog);
        cmd.args(prefix)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(d) = cwd {
            cmd.current_dir(d);
        }
        let child = super::spawn(&mut cmd)
            .await
            .map_err(|e| format!("cannot run {prog}: {e}"))?;
        let out = timeout(DOCKER_TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| format!("{prog} {} timed out", args.join(" ")))?
            .map_err(|e| format!("{prog}: {e}"))?;
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        if !out.status.success() {
            text.push_str(&String::from_utf8_lossy(&out.stderr));
        }
        Ok((out.status.success(), text))
    }

    /// `docker compose --profile <profile> <args>` in the compose directory. True on success.
    async fn compose(&self, args: &[&str]) -> bool {
        let mut full = vec!["compose", "--profile", self.c.compose_profile.as_str()];
        full.extend_from_slice(args);
        match self.docker(&full, Some(&self.c.compose_dir)).await {
            Ok((true, _)) => true,
            Ok((false, out)) => {
                let tail: String = out
                    .chars()
                    .rev()
                    .take(500)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                tracing::warn!(
                    "immich: docker compose {} failed: {}",
                    args.join(" "),
                    tail.trim()
                );
                false
            }
            Err(e) => {
                tracing::warn!("immich: docker compose {}: {e}", args.join(" "));
                false
            }
        }
    }

    /// The container's health status; "absent" when there is none.
    async fn health(&self) -> String {
        let args = [
            "inspect",
            "-f",
            "{{.State.Health.Status}}",
            &self.c.container,
        ];
        match self.docker(&args, None).await {
            Ok((true, out)) if !out.trim().is_empty() => out.trim().to_owned(),
            _ => "absent".into(),
        }
    }

    async fn container_running(&self) -> bool {
        let args = ["inspect", "-f", "{{.State.Running}}", &self.c.container];
        matches!(self.docker(&args, None).await, Ok((true, out)) if out.trim() == "true")
    }

    /// After a crash or restart the container may still be running with no lease behind it.
    async fn stop_orphan(&self) {
        if self.container_running().await {
            tracing::warn!(
                "immich: GPU ML container {} is running without a lease; stopping it",
                self.c.container
            );
            self.compose(&["stop", &self.c.service]).await;
        }
    }

    // ------------------------------------------------------------------ the loop

    /// One pass. `Ok(Some(why))` means the adapter is idle for a reason worth showing (a key
    /// problem); it stays in `/status` until a pass gets through.
    async fn tick(&self) -> anyhow::Result<Option<String>> {
        let Some(key) = self.key() else {
            return Ok(Some(ApiError::NoKey.to_string()));
        };
        let rejected = || Ok(Some(ApiError::Unauthorized.to_string()));
        if self.is_rejected(&key) {
            return rejected();
        }
        let stats = match self.stats().await {
            Ok(s) => s,
            Err(ApiError::Unauthorized) => return rejected(),
            Err(e) => return Err(e.into()),
        };
        let n = backlog(&stats);
        self.view.update(|s| s.backlog = Some(n));
        let paused = self.paused();
        if n < self.c.min_backlog && !paused {
            return Ok(None);
        }
        if n == 0 {
            if paused {
                self.set_paused(false).await?;
            }
            return Ok(None);
        }
        if !paused {
            self.set_paused(true).await?;
        }
        tracing::info!("immich: ML backlog {n}; queueing for the GPU");
        self.set_state(State::Waiting);
        let mut sd = self.shutdown.clone();
        let lease = tokio::select! {
            l = self.arbiter.acquire(self.request.clone()) => l.context("immich lease")?,
            _ = sd.wait_for(|v| *v) => {
                self.set_state(State::Idle);
                return Ok(None);
            }
        };
        self.serve(lease).await?;
        Ok(None)
    }

    /// Run the container for as long as the lease lasts, then wind down.
    async fn serve(&self, lease: Lease) -> anyhow::Result<()> {
        self.set_state(State::Starting);
        let mut progress = Progress::default();
        let outcome = self.serve_inner(&lease, &mut progress).await;
        let bounded = self.stopping();
        let release = self.release(lease, progress);
        if bounded {
            if timeout(SHUTDOWN_RELEASE, release).await.is_err() {
                tracing::warn!("immich: release did not finish within {SHUTDOWN_RELEASE:?}");
            }
        } else {
            release.await;
        }
        self.set_state(State::Idle);
        outcome
    }

    async fn serve_inner(&self, lease: &Lease, progress: &mut Progress) -> anyhow::Result<()> {
        if !self.compose(&["up", "-d", &self.c.service]).await {
            return Err(anyhow!("docker compose up failed"));
        }
        let deadline = Instant::now() + self.c.health_timeout;
        loop {
            let h = self.health().await;
            if h == "healthy" {
                break;
            }
            if Instant::now() > deadline {
                tracing::warn!("immich: GPU ML container not healthy ({h}); giving the card back");
                return Ok(());
            }
            if self.nap(self.health_poll, Some(lease)).await != Wake::Timer {
                tracing::warn!(
                    "immich: asked to stop while waiting for health ({h}); giving the card back"
                );
                return Ok(());
            }
        }
        // Immich re-checks ML availability every 30 s; resuming before it has seen the GPU URL
        // would send the first batch to the CPU container.
        if self.nap(self.c.settle, Some(lease)).await != Wake::Timer {
            tracing::info!("immich: asked to stop while settling; giving the card back");
            return Ok(());
        }
        let gpu: BTreeMap<String, u64> = self
            .c
            .gpu_concurrency
            .iter()
            .map(|(k, v)| (k.clone(), u64::from(*v)))
            .collect();
        progress.old_concurrency = self.set_concurrency(&gpu).await?;
        self.set_paused(false).await?;
        progress.served = true;
        self.view.update(|s| {
            s.state = State::Serving;
            s.error = None;
        });

        let (mut empty_polls, mut errors) = (0u32, 0u32);
        loop {
            if self.nap(self.c.poll, Some(lease)).await != Wake::Timer {
                return Ok(());
            }
            match self.stats().await {
                Ok(stats) => {
                    errors = 0;
                    let n = backlog(&stats);
                    self.view.update(|s| s.backlog = Some(n));
                    empty_polls = if n == 0 { empty_polls + 1 } else { 0 };
                    if empty_polls >= 2 {
                        tracing::info!("immich: ML backlog done on the GPU");
                        return Ok(());
                    }
                }
                Err(e) => {
                    errors += 1;
                    tracing::warn!(
                        "immich: cannot read the queues ({errors}/{MAX_POLL_ERRORS}): {e}"
                    );
                    if errors >= MAX_POLL_ERRORS {
                        return Err(e.into());
                    }
                }
            }
        }
    }

    /// Pause, drain, stop the container, restore the concurrency, release, maybe resume.
    /// Every step is best effort: the container must go whatever else fails.
    async fn release(&self, lease: Lease, progress: Progress) {
        self.set_state(State::Draining);
        let shutting_down = self.stopping();
        if progress.served {
            if let Err(e) = self.set_paused(true).await {
                tracing::warn!("immich: cannot pause the queues: {e}");
            }
            for _ in 0..DRAIN_CHECKS {
                match self.stats().await {
                    Ok(s) if s.values().all(|s| s.active == 0) => break,
                    Ok(_) => {}
                    Err(e) => tracing::warn!("immich: cannot read the queues: {e}"),
                }
                if shutting_down {
                    sleep(self.c.drain_poll).await;
                } else if self.nap(self.c.drain_poll, None).await == Wake::Shutdown {
                    break;
                }
            }
        }
        for attempt in 1..=STOP_TRIES {
            if self.compose(&["stop", &self.c.service]).await {
                break;
            }
            if attempt < STOP_TRIES {
                sleep(self.c.drain_poll).await;
            }
        }
        if !progress.old_concurrency.is_empty() {
            if let Err(e) = self.set_concurrency(&progress.old_concurrency).await {
                tracing::warn!("immich: cannot restore the queue concurrency: {e}");
            }
        }
        drop(lease);
        match self.stats().await {
            Ok(stats) => {
                let n = backlog(&stats);
                self.view.update(|s| s.backlog = Some(n));
                if n <= self.c.cpu_ok_backlog {
                    // Small leftovers: the CPU container finishes them.
                    if let Err(e) = self.set_paused(false).await {
                        tracing::warn!("immich: cannot resume the queues: {e}");
                    }
                } else {
                    tracing::info!(
                        "immich: {n} ML jobs left; kept paused until the next GPU lease"
                    );
                }
            }
            Err(e) => {
                tracing::warn!("immich: cannot read the queues, leaving them as they are: {e}")
            }
        }
    }
}

fn load_state(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v["paused_by_gpud"].as_bool())
        .unwrap_or(false)
}

fn write_state(path: &Path, paused: bool) -> std::io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json!({"paused_by_gpud": paused}).to_string())?;
    std::fs::rename(&tmp, path)
}
