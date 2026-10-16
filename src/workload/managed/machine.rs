//! The per-service state machine. One task owns it; the HTTP gate talks to it over a channel
//! and reads what it needs (is the service up, which prompts are held) from a watch channel.
//!
//! ```text
//! Idle -> Acquiring -> Starting -> Running -> Releasing -> Idle
//!                          \-> Backoff -> Idle
//! ```

use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{sleep, Instant};

use super::comfy::{Comfy, HeldView};
use super::Cfg;
use crate::arbiter::{Handle, Lease, LeaseRequest};
use crate::workload::service::ServiceControl;

/// Pause after a failed start before the next acquire.
const BACKOFF: Duration = Duration::from_secs(10);

/// Poll interval while work is being drained or waited for.
const FAST_POLL: Duration = Duration::from_millis(100);

/// What the gate asks of the machine.
pub(super) enum Msg {
    /// A GPU request wants in; the reply is a permit to keep for the whole response.
    Admit(oneshot::Sender<Result<Permit, String>>),
    /// A permit was dropped.
    Done,
    /// ComfyUI flavor: a prompt to hold. Replies with its id and number.
    Prompt {
        body: Value,
        headers: Vec<(String, String)>,
        reply: oneshot::Sender<(String, u64)>,
    },
    /// ComfyUI flavor: cancel held prompts by id, or all.
    Cancel {
        ids: Vec<String>,
        all: bool,
        reply: oneshot::Sender<bool>,
    },
}

/// One admitted GPU request. Dropping it (end of the response body) frees the slot.
pub(super) struct Permit(mpsc::UnboundedSender<Msg>);

impl Drop for Permit {
    fn drop(&mut self) {
        let _ = self.0.send(Msg::Done);
    }
}

/// What the gate may read without asking the machine.
#[derive(Clone, Default)]
pub(super) struct View {
    /// The service answers requests (a stopped service gets a 503 for non-GPU paths).
    pub up: bool,
    pub held: Vec<HeldView>,
    pub failed: Vec<(String, String)>,
}

enum State {
    Idle,
    Acquiring,
    Starting(Lease),
    Running(Lease),
    Releasing(Lease, Why),
    Backoff(Instant, String),
    Stop,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Why {
    /// Nothing to do for `idle_release`.
    Idle,
    /// The arbiter wants the card for someone else.
    Yield,
}

/// How GPU requests are treated while a state runs.
enum Mode {
    Allow,
    Hold,
    Refuse(String),
}

struct Machine {
    cfg: Cfg,
    arbiter: Handle,
    rx: mpsc::UnboundedReceiver<Msg>,
    tx: mpsc::UnboundedSender<Msg>,
    view: watch::Sender<View>,
    http: reqwest::Client,
    comfy: Option<Comfy>,
    /// GPU requests waiting for a lease.
    waiting: Vec<oneshot::Sender<Result<Permit, String>>>,
    inflight: usize,
    up: bool,
}

pub(super) async fn run(
    cfg: Cfg,
    arbiter: Handle,
    rx: mpsc::UnboundedReceiver<Msg>,
    tx: mpsc::UnboundedSender<Msg>,
    view: watch::Sender<View>,
    mut shutdown: watch::Receiver<bool>,
) {
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default();
    let comfy = cfg.comfy.then(|| Comfy::new(&cfg.backend));
    let up = !cfg.stops_when_idle();
    let mut m = Machine {
        cfg,
        arbiter,
        rx,
        tx,
        view,
        http,
        comfy,
        waiting: Vec::new(),
        inflight: 0,
        up,
    };
    m.publish();
    tokio::select! {
        _ = shutdown.wait_for(|s| *s) => {}
        _ = m.drive() => {}
    }
}

impl Machine {
    async fn drive(&mut self) {
        // A gated service starts out stopped, like after a release.
        if self.cfg.gated && self.cfg.stops_when_idle() {
            if let Some(c) = &self.cfg.control {
                if let Err(e) = c.stop().await {
                    tracing::debug!("{}: initial stop: {e}", self.cfg.name);
                }
            }
        }
        let mut state = State::Idle;
        loop {
            state = match state {
                State::Idle => self.idle().await,
                State::Acquiring => self.acquiring().await,
                State::Starting(lease) => self.starting(lease).await,
                State::Running(lease) => self.running(lease).await,
                State::Releasing(lease, why) => self.releasing(lease, why).await,
                State::Backoff(until, why) => self.backoff(until, why).await,
                State::Stop => return,
            };
        }
    }

    // ---------------------------------------------------------------- helpers

    fn publish(&self) {
        let (held, failed) = match &self.comfy {
            Some(c) => (c.held_view(), c.failed.clone()),
            None => (Vec::new(), Vec::new()),
        };
        self.view.send_replace(View {
            up: self.up,
            held,
            failed,
        });
    }

    fn set_up(&mut self, up: bool) {
        if self.up != up {
            self.up = up;
            self.publish();
        }
    }

    fn held_prompts(&self) -> bool {
        self.comfy.as_ref().is_some_and(|c| !c.held.is_empty())
    }

    /// Whether something wants the card. A gate-less service always does.
    fn wants(&mut self) -> bool {
        if !self.cfg.gated {
            return true;
        }
        self.waiting.retain(|w| !w.is_closed());
        !self.waiting.is_empty() || self.held_prompts()
    }

    fn permit(&mut self) -> Permit {
        self.inflight += 1;
        Permit(self.tx.clone())
    }

    fn on_msg(&mut self, msg: Msg, mode: &Mode) {
        match msg {
            Msg::Admit(reply) => match mode {
                Mode::Allow => {
                    let p = self.permit();
                    // A failed send drops the permit, which counts it out again.
                    let _ = reply.send(Ok(p));
                }
                Mode::Hold => self.waiting.push(reply),
                Mode::Refuse(why) => {
                    let _ = reply.send(Err(why.clone()));
                }
            },
            Msg::Done => self.inflight = self.inflight.saturating_sub(1),
            Msg::Prompt {
                body,
                headers,
                reply,
            } => {
                if let Some(c) = &mut self.comfy {
                    let r = c.add(body, headers);
                    self.publish();
                    let _ = reply.send(r);
                }
            }
            Msg::Cancel { ids, all, reply } => {
                let dropped = self.comfy.as_mut().is_some_and(|c| c.cancel(&ids, all));
                if dropped {
                    self.publish();
                }
                let _ = reply.send(dropped);
            }
        }
    }

    fn admit_waiting(&mut self) {
        for w in std::mem::take(&mut self.waiting) {
            let p = self.permit();
            let _ = w.send(Ok(p));
        }
    }

    fn refuse_waiting(&mut self, why: &str) {
        for w in self.waiting.drain(..) {
            let _ = w.send(Err(why.to_owned()));
        }
        if let Some(c) = &mut self.comfy {
            c.fail_all(why);
        }
        self.publish();
    }

    async fn recv(&mut self, mode: &Mode) {
        if let Some(msg) = self.rx.recv().await {
            self.on_msg(msg, mode);
        }
    }

    /// Busy according to `busy_url`; a down or silent service counts as idle.
    async fn busy(&self) -> bool {
        let Some(url) = &self.cfg.busy_url else {
            return false;
        };
        match self.http.get(url).send().await {
            Ok(r) => {
                let status = r.status().as_u16();
                busy_from(status, &r.text().await.unwrap_or_default())
            }
            Err(_) => false,
        }
    }

    // ---------------------------------------------------------------- states

    async fn idle(&mut self) -> State {
        loop {
            if self.wants() {
                return State::Acquiring;
            }
            // Wake now and then to notice requests whose client has left.
            tokio::select! {
                _ = self.recv(&Mode::Hold) => {}
                _ = sleep(Duration::from_secs(1)) => {}
            }
        }
    }

    async fn acquiring(&mut self) -> State {
        let poll = self.cfg.poll;
        let arbiter = self.arbiter.clone();
        let acquire = arbiter.acquire(LeaseRequest {
            class: self.cfg.class.clone(),
            vram: self.cfg.vram,
            yieldable: self.cfg.yieldable,
            name: self.cfg.name.clone(),
            pid: None,
            resume_id: None,
        });
        tokio::pin!(acquire);
        loop {
            tokio::select! {
                r = &mut acquire => {
                    return match r {
                        Ok(lease) if self.cfg.control.is_some() => State::Starting(lease),
                        Ok(lease) => State::Running(lease),
                        Err(_) => State::Stop,
                    };
                }
                _ = self.recv(&Mode::Hold) => {}
                _ = sleep(poll) => {}
            }
            // Everyone gave up: leave the arbiter's queue (dropping `acquire` cancels).
            if !self.wants() {
                return State::Idle;
            }
        }
    }

    async fn starting(&mut self, lease: Lease) -> State {
        let Some(control) = self.cfg.control.clone() else {
            return State::Running(lease);
        };
        let (health, timeout) = (self.cfg.health_url.clone(), self.cfg.start_timeout);
        tracing::info!("{}: lease granted, starting", self.cfg.name);
        let start = async move {
            if !control.is_running().await.unwrap_or(false) {
                control.start().await?;
            }
            control.wait_ready(health.as_deref(), timeout).await
        };
        tokio::pin!(start);
        let result = loop {
            tokio::select! {
                r = &mut start => break r,
                _ = self.recv(&Mode::Hold) => {}
            }
        };
        match result {
            Ok(()) => {
                self.set_up(true);
                State::Running(lease)
            }
            Err(e) => {
                let why = format!("gpud: {} did not start: {e}", self.cfg.name);
                tracing::warn!("{why}");
                if let Some(c) = &self.cfg.control {
                    let _ = c.stop().await;
                }
                self.set_up(false);
                self.refuse_waiting(&why);
                drop(lease);
                State::Backoff(Instant::now() + BACKOFF, why)
            }
        }
    }

    async fn running(&mut self, lease: Lease) -> State {
        let poll = self.cfg.poll;
        let fast = poll.min(FAST_POLL);
        self.admit_waiting();
        let mut idle_since: Option<Instant> = None;
        loop {
            if lease.yield_requested() {
                return State::Releasing(lease, Why::Yield);
            }
            self.pump(&lease).await;
            let mut ticked = false;
            let tick = if self.held_prompts() { fast } else { poll };
            tokio::select! {
                _ = self.recv(&Mode::Allow) => {}
                _ = lease.yield_wanted() => {}
                _ = sleep(tick) => ticked = true,
            }
            // A gate-less service holds its lease until asked to yield.
            if !ticked || !self.cfg.gated {
                continue;
            }
            let busy = self.inflight > 0 || self.held_prompts() || self.busy().await;
            if busy {
                idle_since = None;
                continue;
            }
            let since = *idle_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= self.cfg.idle_release {
                return State::Releasing(lease, Why::Idle);
            }
        }
    }

    async fn releasing(&mut self, lease: Lease, why: Why) -> State {
        let poll = self.cfg.poll.min(FAST_POLL);
        loop {
            // New work: hold it. If we were only idle, keep the card instead.
            if why == Why::Idle && self.wants() {
                return State::Running(lease);
            }
            if why == Why::Yield {
                if let Some(mut c) = self.comfy.take() {
                    c.reclaim(&self.http).await;
                    self.comfy = Some(c);
                    self.publish();
                }
            }
            if self.inflight == 0 && !self.busy().await {
                break;
            }
            tokio::select! {
                _ = self.recv(&Mode::Hold) => {}
                _ = sleep(poll) => {}
            }
        }
        let stop = self.cfg.control.is_some() && (why == Why::Yield || self.cfg.stop_on_release);
        if stop {
            if let Some(c) = &self.cfg.control {
                if let Err(e) = c.stop().await {
                    tracing::warn!("{}: stop failed: {e}", self.cfg.name);
                }
            }
            self.set_up(false);
        } else if let Some(c) = &self.comfy {
            c.free(
                &self.http,
                json!({"unload_models": true, "free_memory": true}),
            )
            .await;
        }
        if let Some(c) = &mut self.comfy {
            c.reset();
        }
        tracing::info!("{}: lease released", self.cfg.name);
        drop(lease);
        State::Idle
    }

    async fn backoff(&mut self, until: Instant, why: String) -> State {
        let mode = Mode::Refuse(why);
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(until) => return State::Idle,
                _ = self.recv(&mode) => {}
            }
        }
    }

    // ---------------------------------------------------------------- ComfyUI

    /// Submit held prompts in order while the card is ours.
    async fn pump(&mut self, lease: &Lease) {
        while !lease.yield_requested() {
            let Some(front) = self.comfy.as_ref().and_then(|c| c.held.front().cloned()) else {
                return;
            };
            let switch = self.comfy.as_ref().is_some_and(|c| c.switches(&front));
            if switch {
                // Other weights next: let ComfyUI finish, then start from an empty card.
                if self.busy().await {
                    return;
                }
                self.fresh_comfyui().await;
            }
            let Some(c) = &mut self.comfy else { return };
            c.held.pop_front();
            c.note_models(&front);
            c.submit(&self.http, front).await;
            self.publish();
        }
    }

    async fn fresh_comfyui(&mut self) {
        if let Some(control) = self.cfg.control.clone() {
            tracing::info!(
                "{}: restarting, the next prompt uses other models",
                self.cfg.name
            );
            let r = restart(
                &control,
                self.cfg.health_url.as_deref(),
                self.cfg.start_timeout,
            )
            .await;
            if let Err(e) = r {
                tracing::warn!("{}: restart failed: {e}", self.cfg.name);
            }
        } else if let Some(c) = &self.comfy {
            c.free(&self.http, json!({"unload_models": true})).await;
        }
        if let Some(c) = &mut self.comfy {
            // Whatever was loaded is gone now.
            c.reset_models();
        }
    }
}

async fn restart(
    control: &ServiceControl,
    health: Option<&str>,
    timeout: Duration,
) -> crate::workload::service::Result<()> {
    control.stop().await?;
    control.start().await?;
    control.wait_ready(health, timeout).await
}

/// Decide from a `busy_url` answer whether the service is working.
///
/// A JSON body with a non-empty array anywhere at its top level (an object member, or the
/// body itself) means busy; this fits ComfyUI's `/queue` (`queue_running`, `queue_pending`)
/// and a custom `{"jobs": [...]}`. JSON without such an array means idle. A body that is not
/// JSON counts as busy on any 2xx except 204, and as idle otherwise (404 included).
pub(super) fn busy_from(status: u16, body: &str) -> bool {
    match serde_json::from_str::<Value>(body) {
        Ok(Value::Object(m)) => m
            .values()
            .any(|v| v.as_array().is_some_and(|a| !a.is_empty())),
        Ok(Value::Array(a)) => !a.is_empty(),
        Ok(_) => false,
        Err(_) => (200..300).contains(&status) && status != 204,
    }
}

#[cfg(test)]
mod tests {
    use super::busy_from;

    #[test]
    fn busy_answers() {
        assert!(busy_from(
            200,
            r#"{"queue_running":[[0,"a"]],"queue_pending":[]}"#
        ));
        assert!(!busy_from(
            200,
            r#"{"queue_running":[],"queue_pending":[]}"#
        ));
        assert!(!busy_from(200, r#"{"status":"ok","count":3}"#));
        assert!(busy_from(200, "[1]"));
        assert!(!busy_from(200, "[]"));
        assert!(busy_from(200, "working"));
        assert!(busy_from(202, ""));
        assert!(!busy_from(204, ""));
        assert!(!busy_from(404, "not found"));
        assert!(!busy_from(500, "oops"));
    }
}
