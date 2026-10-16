//! Client for the llama-swap instance behind the gate.
//!
//! Unload everything, save and restore a llama-server KV slot, preload a model by polling its
//! health endpoint. Slot calls are never fatal: a failed save just means a cold start later.

use std::collections::HashSet;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::time::Instant;

use super::models::ModelTable;
use super::{Backend, BackendError, Capabilities};

const RUNNING_TIMEOUT: Duration = Duration::from_secs(10);
const UNLOAD_TIMEOUT: Duration = Duration::from_secs(60);
const SLOT_TIMEOUT: Duration = Duration::from_secs(120);

/// One entry of llama-swap's `/running`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct RunningModel {
    pub model: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub ttl: Value,
}

#[derive(Deserialize)]
struct RunningDoc {
    running: Option<Vec<RunningModel>>,
}

pub struct LlamaSwap {
    base: Url,
    client: reqwest::Client,
    models: ModelTable,
    load_timeout: Duration,
    poll_interval: Duration,
    /// Models whose slot this process saved, so a restore never loads a stale file.
    saved: Mutex<HashSet<String>>,
}

fn unreachable_or_failed(e: reqwest::Error) -> BackendError {
    if e.is_connect() || e.is_timeout() || e.is_request() {
        BackendError::Unreachable(e.to_string())
    } else {
        BackendError::Failed(e.to_string())
    }
}

impl LlamaSwap {
    /// `base_url` is llama-swap's root, e.g. `http://127.0.0.1:8080`.
    pub fn new(
        base_url: &str,
        models: ModelTable,
        load_timeout: Duration,
    ) -> Result<Self, BackendError> {
        let base = Url::parse(base_url)
            .map_err(|e| BackendError::Failed(format!("bad llama-swap url {base_url:?}: {e}")))?;
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| BackendError::Failed(e.to_string()))?;
        Ok(LlamaSwap {
            base,
            client,
            models,
            load_timeout,
            poll_interval: Duration::from_secs(2),
            saved: Mutex::new(HashSet::new()),
        })
    }

    /// Time between health checks while waiting for a model to load (default 2 s).
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    pub fn models(&self) -> &ModelTable {
        &self.models
    }

    fn saved(&self) -> MutexGuard<'_, HashSet<String>> {
        self.saved.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn url(&self, segments: &[&str]) -> Url {
        let mut url = self.base.clone();
        if let Ok(mut path) = url.path_segments_mut() {
            path.pop_if_empty().extend(segments);
        }
        url
    }

    /// Everything llama-swap has running, loaded or not yet ready.
    pub async fn running(&self) -> Result<Vec<RunningModel>, BackendError> {
        let resp = self
            .client
            .get(self.url(&["running"]))
            .timeout(RUNNING_TIMEOUT)
            .send()
            .await
            .map_err(unreachable_or_failed)?
            .error_for_status()
            .map_err(unreachable_or_failed)?;
        let doc: RunningDoc = resp
            .json()
            .await
            .map_err(|e| BackendError::Failed(format!("bad /running reply: {e}")))?;
        Ok(doc.running.unwrap_or_default())
    }

    /// llama-server's slot API; `filename` is relative to its `--slot-save-path`.
    async fn slot_call(&self, model: &str, action: &str, filename: &str) -> Option<Value> {
        let mut url = self.url(&["upstream", model, "slots", "0"]);
        url.query_pairs_mut().append_pair("action", action);
        let result = async {
            let resp = self
                .client
                .post(url)
                .timeout(SLOT_TIMEOUT)
                .json(&json!({ "filename": filename }))
                .send()
                .await?;
            let status = resp.status();
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            Ok::<_, reqwest::Error>((status, body))
        }
        .await;
        match result {
            Ok((status, body)) if status.is_success() => Some(body),
            Ok((status, body)) => {
                tracing::warn!("slot {action} {model} failed: HTTP {status} {body}");
                None
            }
            Err(e) => {
                tracing::warn!("slot {action} {model} failed: {e}");
                None
            }
        }
    }
}

#[async_trait]
impl Backend for LlamaSwap {
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
        self.models.resolve(model)
    }

    fn uses_gpu(&self, model: &str) -> bool {
        self.models.uses_gpu(model)
    }

    async fn loaded_gpu_models(&self) -> Result<Vec<String>, BackendError> {
        Ok(self
            .running()
            .await?
            .into_iter()
            .filter(|m| (m.state == "ready" || m.state == "starting") && self.uses_gpu(&m.model))
            .map(|m| m.model)
            .collect())
    }

    async fn unload_all(&self) -> Result<(), BackendError> {
        self.client
            .post(self.url(&["api", "models", "unload"]))
            .timeout(UNLOAD_TIMEOUT)
            .send()
            .await
            .map_err(unreachable_or_failed)?
            .error_for_status()
            .map_err(unreachable_or_failed)?;
        Ok(())
    }

    async fn save_slot(&self, model: &str) -> Result<bool, BackendError> {
        if !self.models.slot_save(model) {
            return Ok(false);
        }
        let started = Instant::now();
        let name = format!("{model}.slot0.bin");
        let Some(body) = self.slot_call(model, "save", &name).await else {
            return Ok(false);
        };
        let n_saved = body.get("n_saved").and_then(Value::as_u64).unwrap_or(0);
        if n_saved == 0 {
            tracing::info!("slot save {model}: slot empty, nothing to restore later");
            self.saved().remove(model);
            return Ok(false);
        }
        self.saved().insert(model.to_owned());
        let written = body.get("n_written").and_then(Value::as_u64).unwrap_or(0);
        tracing::info!(
            "slot save {model}: {n_saved} tokens, {:.1} MiB in {:.1}s",
            written as f64 / 1_048_576.0,
            started.elapsed().as_secs_f64()
        );
        Ok(true)
    }

    async fn restore_slot(&self, model: &str) -> Result<bool, BackendError> {
        if !self.saved().remove(model) {
            return Ok(false);
        }
        let started = Instant::now();
        let name = format!("{model}.slot0.bin");
        let Some(body) = self.slot_call(model, "restore", &name).await else {
            return Ok(false);
        };
        let n = body.get("n_restored").and_then(Value::as_u64).unwrap_or(0);
        tracing::info!(
            "slot restore {model}: {n} tokens in {:.1}s",
            started.elapsed().as_secs_f64()
        );
        Ok(true)
    }

    /// Touch the model's upstream health endpoint; llama-swap starts it on demand. Every
    /// request is bounded by the time left, so the deadline holds even if one attempt hangs.
    async fn load(&self, model: &str) -> bool {
        let started = Instant::now();
        let deadline = started + self.load_timeout;
        let url = self.url(&["upstream", model, "health"]);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let attempt = self.client.get(url.clone()).timeout(left).send().await;
            if matches!(attempt, Ok(ref r) if r.status().as_u16() == 200) {
                tracing::info!(
                    "preload {model}: ready in {:.1}s",
                    started.elapsed().as_secs_f64()
                );
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            tokio::time::sleep(self.poll_interval.min(left)).await;
        }
        tracing::warn!(
            "preload {model}: not ready after {}s",
            self.load_timeout.as_secs_f64()
        );
        false
    }
}
