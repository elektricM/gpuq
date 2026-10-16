//! Shared test doubles: an in-memory fake llama-swap plus amdgpu VRAM counter, and a rig that
//! starts the arbiter in front of them. No sockets, no processes; all waiting is tokio time.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use gpud::arbiter::{
    self, ArbiterError, ClassTuning, Classes, Handle, Lease, LeaseRequest, OffloadHook, Settings,
    Status, Tuning, Vram,
};
use gpud::backend::{Backend, BackendError, Capabilities};
use gpud::probe::{ProbeError, VramProbe, VramReading};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout, Instant};

pub const TOTAL_MIB: u64 = 24560;
pub const FLOOR: u64 = 50;

pub fn size_of(model: &str) -> u64 {
    match model {
        "big" => 22000,
        "other" => 20000,
        "rec" => 21000,
        "embed" => 0,
        other => panic!("fake card does not know model {other:?}"),
    }
}

pub fn secs(s: f64) -> Duration {
    Duration::from_secs_f64(s)
}

pub fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

// ------------------------------------------------------------------------------ the card

pub struct CardState {
    pub loaded: BTreeSet<String>,
    /// VRAM a "job" (or a desktop session) is using, MiB.
    pub extra: u64,
    pub slot_tokens: u64,
    pub saved: BTreeMap<String, u64>,
    pub events: Vec<String>,
    pub load_delay: Duration,
    pub unload_delay: Duration,
    pub unload_fails: bool,
    pub load_fails: bool,
}

/// Shared state of the fake card; clones talk to the same card.
#[derive(Clone)]
pub struct FakeCard(Arc<Mutex<CardState>>);

impl Default for FakeCard {
    fn default() -> Self {
        FakeCard(Arc::new(Mutex::new(CardState {
            loaded: BTreeSet::new(),
            extra: 0,
            slot_tokens: 0,
            saved: BTreeMap::new(),
            events: Vec::new(),
            load_delay: ms(50),
            unload_delay: Duration::ZERO,
            unload_fails: false,
            load_fails: false,
        })))
    }
}

impl FakeCard {
    pub fn lock(&self) -> MutexGuard<'_, CardState> {
        self.0.lock().unwrap()
    }

    pub fn loaded(&self) -> BTreeSet<String> {
        self.lock().loaded.clone()
    }

    pub fn is_loaded(&self, model: &str) -> bool {
        self.lock().loaded.contains(model)
    }

    pub fn events(&self) -> Vec<String> {
        self.lock().events.clone()
    }

    pub fn has_event(&self, ev: &str) -> bool {
        self.lock().events.iter().any(|e| e == ev)
    }

    pub fn count(&self, ev: &str) -> usize {
        self.lock().events.iter().filter(|e| *e == ev).count()
    }

    pub fn slot_tokens(&self) -> u64 {
        self.lock().slot_tokens
    }

    pub fn saved(&self) -> BTreeMap<String, u64> {
        self.lock().saved.clone()
    }

    pub fn set_job_vram(&self, mib: u64) {
        self.lock().extra = mib;
    }

    pub fn set_unload_fails(&self, on: bool) {
        self.lock().unload_fails = on;
    }

    pub fn set_load_fails(&self, on: bool) {
        self.lock().load_fails = on;
    }

    pub fn set_unload_delay(&self, d: Duration) {
        self.lock().unload_delay = d;
    }

    pub fn set_load_delay(&self, d: Duration) {
        self.lock().load_delay = d;
    }

    pub fn used_mib(&self) -> u64 {
        let s = self.lock();
        FLOOR + s.loaded.iter().map(|m| size_of(m)).sum::<u64>() + s.extra
    }

    /// Bring a model up instantly (like base.py's `fake._load("big")` before the arbiter starts).
    pub fn preload(&self, model: &str) {
        let mut s = self.lock();
        load_now(&mut s, model);
    }

    pub fn clear_loaded(&self) {
        self.lock().loaded.clear();
    }
}

fn load_now(s: &mut CardState, model: &str) {
    // llama-swap's exclusive group: loading one GPU model evicts the other GPU models.
    if size_of(model) > 0 {
        s.loaded.retain(|m| size_of(m) == 0);
    }
    s.loaded.insert(model.to_owned());
    s.events.push(format!("load {model}"));
    if model == "big" {
        s.slot_tokens = 0;
    }
}

// ------------------------------------------------------------------------------ the backend

pub struct FakeBackend {
    card: FakeCard,
}

impl FakeBackend {
    pub fn new(card: FakeCard) -> Self {
        FakeBackend { card }
    }
}

#[async_trait]
impl Backend for FakeBackend {
    fn kind(&self) -> &'static str {
        "llama-swap"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            unload: true,
            slots: true,
            health: true,
        }
    }

    fn resolve(&self, model: &str) -> String {
        match model {
            "other-alias" => "other".to_owned(),
            m => m.to_owned(),
        }
    }

    fn uses_gpu(&self, model: &str) -> bool {
        self.resolve(model) != "embed"
    }

    async fn loaded_gpu_models(&self) -> Result<Vec<String>, BackendError> {
        let s = self.card.lock();
        Ok(s.loaded
            .iter()
            .filter(|m| size_of(m) > 0)
            .cloned()
            .collect())
    }

    async fn save_slot(&self, model: &str) -> Result<bool, BackendError> {
        if self.resolve(model) != "big" {
            return Ok(false);
        }
        let mut s = self.card.lock();
        let n = s.slot_tokens;
        s.saved.insert("big".into(), n);
        s.events.push(format!("save {n}"));
        Ok(true)
    }

    async fn restore_slot(&self, model: &str) -> Result<bool, BackendError> {
        if self.resolve(model) != "big" {
            return Ok(false);
        }
        let mut s = self.card.lock();
        let n = s.saved.get("big").copied().unwrap_or(0);
        s.slot_tokens = n;
        s.events.push(format!("restore {n}"));
        Ok(n > 0)
    }

    async fn unload_all(&self) -> Result<(), BackendError> {
        let (delay, fails) = {
            let s = self.card.lock();
            (s.unload_delay, s.unload_fails)
        };
        if fails {
            return Err(BackendError::Unreachable("llama-swap down".into()));
        }
        if !delay.is_zero() {
            sleep(delay).await;
        }
        let mut s = self.card.lock();
        s.events.push("unload".into());
        s.loaded.clear();
        Ok(())
    }

    async fn load(&self, model: &str) -> bool {
        let model = self.resolve(model);
        let (already, fails, delay) = {
            let s = self.card.lock();
            (s.loaded.contains(&model), s.load_fails, s.load_delay)
        };
        if already {
            return true;
        }
        if fails {
            return false;
        }
        sleep(delay).await;
        load_now(&mut self.card.lock(), &model);
        true
    }
}

// ------------------------------------------------------------------------------ probe, hook

pub struct FakeProbe {
    card: FakeCard,
}

impl FakeProbe {
    pub fn new(card: FakeCard) -> Self {
        FakeProbe { card }
    }
}

#[async_trait]
impl VramProbe for FakeProbe {
    fn device(&self) -> String {
        "fake:0".into()
    }

    async fn read(&self) -> Result<VramReading, ProbeError> {
        Ok(VramReading {
            used_mib: self.card.used_mib(),
            total_mib: TOTAL_MIB,
            gtt_used_mib: Some(0),
        })
    }
}

/// Stands in for the shell offload hook.
pub struct FakeHook {
    card: FakeCard,
    runs: AtomicUsize,
    frees: bool,
    ok: bool,
}

impl FakeHook {
    /// A hook that succeeds and sets the job VRAM back to zero.
    pub fn freeing(card: &FakeCard) -> Arc<Self> {
        Arc::new(FakeHook {
            card: card.clone(),
            runs: AtomicUsize::new(0),
            frees: true,
            ok: true,
        })
    }

    /// A hook that fails and frees nothing.
    pub fn failing(card: &FakeCard) -> Arc<Self> {
        Arc::new(FakeHook {
            card: card.clone(),
            runs: AtomicUsize::new(0),
            frees: false,
            ok: false,
        })
    }

    pub fn runs(&self) -> usize {
        self.runs.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl OffloadHook for FakeHook {
    async fn run(&self) -> bool {
        self.runs.fetch_add(1, Ordering::SeqCst);
        if self.frees {
            self.card.set_job_vram(0);
        }
        self.ok
    }
}

// ------------------------------------------------------------------------------ the rig

/// The arbiter in front of the fakes, configured like tests/base.py.
#[derive(Clone)]
pub struct Rig {
    pub handle: Handle,
    pub card: FakeCard,
    pub backend: Arc<FakeBackend>,
}

pub fn base_settings() -> Settings {
    let prio: BTreeMap<String, u32> = [
        ("realtime", 0),
        ("llm", 1),
        ("interactive", 2),
        ("background", 3),
        ("batch", 4),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    let mut class_tuning = HashMap::new();
    class_tuning.insert(
        "realtime".to_owned(),
        ClassTuning {
            gap: Some(secs(1.0)),
            reload_delay: Some(secs(60.0)),
        },
    );
    Settings {
        classes: Classes::new(prio).unwrap(),
        resident: Some("big".into()),
        tuning: Tuning {
            idle: secs(0.3),
            resume_grace: Duration::ZERO,
            reload_delay: secs(0.2),
            reload_wait: secs(5.0),
            gap: secs(0.1),
            class_tuning,
            settle_timeout: secs(5.0),
            driver_settle: secs(0.05),
            ..Tuning::default()
        },
    }
}

pub fn request(class: &str, vram: Vram, yieldable: bool, name: &str) -> LeaseRequest {
    LeaseRequest {
        class: class.into(),
        vram,
        yieldable,
        name: name.into(),
        pid: None,
        resume_id: None,
    }
}

impl Rig {
    /// Big is loaded, like the real card; no offload hook.
    pub async fn start(tweak: impl FnOnce(&mut Settings)) -> Rig {
        Rig::start_with(true, None, tweak).await
    }

    pub async fn start_with(
        preload: bool,
        hook: Option<Arc<FakeHook>>,
        tweak: impl FnOnce(&mut Settings),
    ) -> Rig {
        Rig::start_on(FakeCard::default(), preload, hook, tweak).await
    }

    /// Like `start_with`, on a card made earlier (e.g. so a hook can share it).
    pub async fn start_on(
        card: FakeCard,
        preload: bool,
        hook: Option<Arc<FakeHook>>,
        tweak: impl FnOnce(&mut Settings),
    ) -> Rig {
        if preload {
            card.preload("big");
        }
        let mut settings = base_settings();
        tweak(&mut settings);
        let backend = Arc::new(FakeBackend::new(card.clone()));
        let probe = Arc::new(FakeProbe::new(card.clone()));
        let hook = hook.map(|h| h as Arc<dyn OffloadHook>);
        let handle = arbiter::spawn(settings, backend.clone(), probe, hook);
        Rig {
            handle,
            card,
            backend,
        }
    }

    pub async fn status(&self) -> Status {
        self.handle.status().await.expect("arbiter stopped")
    }

    /// What the gate does: wait to be admitted, serve (load on demand, stream), release.
    pub async fn chat_with(&self, model: &str, n: u32, delay: Duration) -> u16 {
        // A held request that is never let through fails the test instead of hanging it.
        timeout(secs(10.0), self.serve(model, n, delay))
            .await
            .unwrap_or_else(|_| panic!("chat {model} still held after 10s"))
    }

    async fn serve(&self, model: &str, n: u32, delay: Duration) -> u16 {
        let class = if model == "rec" { "realtime" } else { "llm" };
        let Ok(guard) = self.handle.enter_llm(model, class).await else {
            return 503;
        };
        if !self.backend.load(model).await {
            return 502;
        }
        {
            let mut s = self.card.lock();
            if model == "big" {
                s.slot_tokens += 100;
            }
            s.events.push(format!("chat {model}"));
        }
        for _ in 0..n {
            sleep(delay).await;
        }
        drop(guard);
        200
    }

    pub async fn chat(&self, model: &str) -> u16 {
        self.chat_with(model, 3, ms(10)).await
    }

    pub fn spawn_chat(&self, model: &str) -> JoinHandle<u16> {
        let rig = self.clone();
        let model = model.to_owned();
        tokio::spawn(async move { rig.chat(&model).await })
    }

    pub fn spawn_chat_with(&self, model: &str, n: u32, delay: Duration) -> JoinHandle<u16> {
        let rig = self.clone();
        let model = model.to_owned();
        tokio::spawn(async move { rig.chat_with(&model, n, delay).await })
    }

    /// Wait up to 5 s for a lease.
    pub async fn lease(&self, class: &str, mib: u64, yieldable: bool, name: &str) -> Lease {
        let req = request(class, Vram::Mib(mib), yieldable, name);
        timeout(secs(5.0), self.handle.acquire(req))
            .await
            .expect("timed out waiting for a lease")
            .expect("arbiter stopped")
    }

    pub fn spawn_acquire(
        &self,
        class: &str,
        vram: Vram,
        yieldable: bool,
        name: &str,
    ) -> JoinHandle<Result<Lease, ArbiterError>> {
        let handle = self.handle.clone();
        let req = request(class, vram, yieldable, name);
        tokio::spawn(async move { handle.acquire(req).await })
    }

    /// Poll every 20 ms for up to 5 s; panic with the state, events and history on timeout.
    pub async fn wait_for(&self, mut pred: impl FnMut(&Status, &FakeCard) -> bool) {
        let end = Instant::now() + secs(5.0);
        loop {
            let st = self.status().await;
            if pred(&st, &self.card) {
                return;
            }
            if Instant::now() >= end {
                let history: Vec<_> = st.history.iter().map(|h| h.msg.as_str()).collect();
                panic!(
                    "timed out; state={:?} events={:?} history={:?}",
                    st.state,
                    self.card.events(),
                    history
                );
            }
            sleep(ms(20)).await;
        }
    }
}

/// Await a spawned chat with the 5 s cap the Python tests use.
pub async fn done(task: JoinHandle<u16>) -> u16 {
    timeout(secs(5.0), task)
        .await
        .expect("chat timed out")
        .expect("chat task failed")
}

/// Await a spawned acquire with a 5 s cap.
pub async fn granted(task: JoinHandle<Result<Lease, ArbiterError>>) -> Lease {
    timeout(secs(5.0), task)
        .await
        .expect("acquire timed out")
        .expect("acquire task failed")
        .expect("arbiter stopped")
}
