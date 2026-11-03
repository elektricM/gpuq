// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! Rig for several fake cards. Each card has its own `FakeCard` (VRAM model) and its own probe
//! with a failure switch; the fake llama-swap is attached to the LLM card only.
#![allow(dead_code)]

#[path = "../common/mod.rs"]
mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use gpuq::arbiter::{
    self, ArbiterError, Card, CardSel, Handle, Lease, LeaseRequest, OffloadHook, Settings, Status,
    Vram,
};
use gpuq::backend::Backend;
use gpuq::probe::{ProbeError, VramProbe, VramReading};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout, Instant};

#[allow(unused_imports)]
pub use common::{base_settings, done, ms, secs, FakeBackend, FakeCard};

/// A fake probe that can be told to fail.
pub struct SwitchProbe {
    inner: common::FakeProbe,
    device: String,
    fail: Arc<AtomicBool>,
}

#[async_trait]
impl VramProbe for SwitchProbe {
    fn device(&self) -> String {
        self.device.clone()
    }

    async fn read(&self) -> Result<VramReading, ProbeError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(ProbeError::NotFound("probe switched off".into()));
        }
        self.inner.read().await
    }
}

/// An offload hook that frees the card it is run for and remembers the names.
pub struct CardHook {
    cards: Vec<FakeCard>,
    pub runs: Mutex<Vec<String>>,
}

#[async_trait]
impl OffloadHook for CardHook {
    async fn run(&self, card: &str) -> bool {
        self.runs.lock().unwrap().push(card.to_owned());
        if let Some(i) = card
            .strip_prefix("gpu")
            .and_then(|n| n.parse::<usize>().ok())
        {
            self.cards[i].set_job_vram(0);
        }
        true
    }
}

/// A card's env and vendor.
pub type EnvVendor = (Vec<(String, String)>, String);

/// How to lay out the cards.
#[derive(Clone)]
pub struct Opts {
    pub cards: usize,
    pub llm_card: usize,
    /// Whether card `i` gets `CUDA_VISIBLE_DEVICES=i`.
    pub env: Vec<bool>,
    /// Card `i`'s env and vendor, instead of the default above.
    pub custom: Vec<Option<EnvVendor>>,
    pub hook: bool,
}

impl Opts {
    pub fn new(cards: usize) -> Opts {
        Opts {
            cards,
            llm_card: 0,
            env: vec![true; cards],
            custom: vec![None; cards],
            hook: false,
        }
    }

    /// Give card `i` this env (`K=V` pairs) and vendor.
    pub fn card(mut self, i: usize, env: &[(&str, &str)], vendor: &str) -> Opts {
        let env = env
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        self.custom[i] = Some((env, vendor.to_owned()));
        self
    }

    /// Run an offload hook that frees the card it is called for.
    pub fn hook(mut self) -> Opts {
        self.hook = true;
        self
    }

    pub fn llm(mut self, card: usize) -> Opts {
        self.llm_card = card;
        self
    }

    pub fn no_env(mut self) -> Opts {
        self.env = vec![false; self.cards];
        self
    }

    pub fn no_env_on(mut self, card: usize) -> Opts {
        self.env[card] = false;
        self
    }
}

#[derive(Clone)]
pub struct MultiRig {
    pub handle: Handle,
    /// One fake card per configured card, in config order.
    pub cards: Vec<FakeCard>,
    pub fail: Vec<Arc<AtomicBool>>,
    pub llm_card: usize,
    pub backend: Arc<FakeBackend>,
    pub hook: Option<Arc<CardHook>>,
}

pub fn card_name(i: usize) -> String {
    format!("gpu{i}")
}

pub fn named(i: usize) -> CardSel {
    CardSel::Named(card_name(i))
}

/// These cards at once.
pub fn set(cards: &[usize]) -> CardSel {
    CardSel::Set(cards.iter().map(|&i| card_name(i)).collect())
}

pub fn request_on(
    card: CardSel,
    class: &str,
    vram: Vram,
    yieldable: bool,
    name: &str,
) -> LeaseRequest {
    let mut req = common::request(class, vram, yieldable, name);
    req.card = card;
    req
}

impl MultiRig {
    /// `n` cards, LLM on gpu0 with "big" loaded, env on every card.
    pub async fn start(n: usize) -> MultiRig {
        MultiRig::start_with(Opts::new(n), |_| {}).await
    }

    /// "big" is loaded on the LLM card.
    pub async fn start_with(opts: Opts, tweak: impl FnOnce(&mut Settings)) -> MultiRig {
        let fakes: Vec<FakeCard> = (0..opts.cards).map(|_| FakeCard::default()).collect();
        fakes[opts.llm_card].preload("big");
        let mut settings = base_settings();
        tweak(&mut settings);
        let mut fail = Vec::new();
        let cards = fakes
            .iter()
            .enumerate()
            .map(|(i, fake)| {
                let switch = Arc::new(AtomicBool::new(false));
                fail.push(switch.clone());
                Card {
                    name: card_name(i),
                    probe: Arc::new(SwitchProbe {
                        inner: common::FakeProbe::new(fake.clone()),
                        device: format!("fake:{i}"),
                        fail: switch,
                    }),
                    margin_mib: settings.tuning.margin_mib,
                    whole_card_baseline_mib: settings.tuning.whole_card_baseline_mib,
                    env: match &opts.custom[i] {
                        Some((env, _)) => env.clone(),
                        None if opts.env[i] => {
                            vec![("CUDA_VISIBLE_DEVICES".into(), i.to_string())]
                        }
                        None => Vec::new(),
                    },
                    pin_env: Vec::new(),
                    vendor: match &opts.custom[i] {
                        Some((_, vendor)) => vendor.clone(),
                        None => "nvidia".into(),
                    },
                }
            })
            .collect();
        let backend = Arc::new(FakeBackend::new(fakes[opts.llm_card].clone()));
        let hook = opts.hook.then(|| {
            Arc::new(CardHook {
                cards: fakes.clone(),
                runs: Mutex::new(Vec::new()),
            })
        });
        let handle = arbiter::spawn_cards(
            settings,
            cards,
            opts.llm_card,
            backend.clone(),
            hook.clone().map(|h| h as Arc<dyn OffloadHook>),
        );
        MultiRig {
            handle,
            cards: fakes,
            fail,
            llm_card: opts.llm_card,
            backend,
            hook,
        }
    }

    /// The fake card the LLM backend lives on.
    pub fn llm(&self) -> &FakeCard {
        &self.cards[self.llm_card]
    }

    pub async fn status(&self) -> Status {
        self.handle.status().await.expect("arbiter stopped")
    }

    pub fn set_probe_failing(&self, card: usize, on: bool) {
        self.fail[card].store(on, Ordering::SeqCst);
    }

    /// What the gate does: wait to be admitted, serve on the LLM card, release.
    pub async fn chat_with(&self, model: &str, n: u32, delay: Duration) -> u16 {
        timeout(secs(10.0), self.serve(model, n, delay))
            .await
            .unwrap_or_else(|_| panic!("chat {model} still held after 10s"))
    }

    async fn serve(&self, model: &str, n: u32, delay: Duration) -> u16 {
        let Ok(guard) = self.handle.enter_llm(model, "llm").await else {
            return 503;
        };
        if !self.backend.load(model).await {
            return 502;
        }
        {
            let mut s = self.llm().lock();
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

    /// Wait up to 5 s for a lease on `card`.
    pub async fn lease_on(
        &self,
        card: CardSel,
        class: &str,
        mib: u64,
        yieldable: bool,
        name: &str,
    ) -> Lease {
        let req = request_on(card, class, Vram::Mib(mib), yieldable, name);
        timeout(secs(5.0), self.handle.acquire(req))
            .await
            .expect("timed out waiting for a lease")
            .expect("acquire failed")
    }

    /// Wait up to 5 s for a lease of `vram` on each of `cards`.
    pub async fn lease_cards(
        &self,
        card: CardSel,
        class: &str,
        vram: Vram,
        yieldable: bool,
        name: &str,
    ) -> Lease {
        let req = request_on(card, class, vram, yieldable, name);
        timeout(secs(5.0), self.handle.acquire(req))
            .await
            .expect("timed out waiting for a lease")
            .expect("acquire failed")
    }

    pub fn spawn_acquire_on(
        &self,
        card: CardSel,
        class: &str,
        vram: Vram,
        yieldable: bool,
        name: &str,
    ) -> JoinHandle<Result<Lease, ArbiterError>> {
        let handle = self.handle.clone();
        let req = request_on(card, class, vram, yieldable, name);
        tokio::spawn(async move { handle.acquire(req).await })
    }

    /// Poll every 20 ms for up to 5 s; panic with the card states and events on timeout.
    pub async fn wait_for(&self, mut pred: impl FnMut(&Status, &[FakeCard]) -> bool) {
        let end = Instant::now() + secs(5.0);
        loop {
            let st = self.status().await;
            if pred(&st, &self.cards) {
                return;
            }
            if Instant::now() >= end {
                let events: Vec<_> = self.cards.iter().map(|c| c.events()).collect();
                panic!(
                    "timed out; cards={:?} queue={:?} events={events:?}",
                    st.cards, st.queue
                );
            }
            sleep(ms(20)).await;
        }
    }
}

/// Await a spawned acquire with a 5 s cap.
pub async fn granted(task: JoinHandle<Result<Lease, ArbiterError>>) -> Lease {
    timeout(secs(5.0), task)
        .await
        .expect("acquire timed out")
        .expect("acquire task failed")
        .expect("acquire failed")
}
