// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! Ollama: models load on demand and unload with `keep_alive: 0`.

use std::collections::HashSet;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::time::{sleep, Instant};

use super::{Backend, BackendError, Capabilities};

const LOAD_TIMEOUT: Duration = Duration::from_secs(300);
const UNLOAD_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Deserialize)]
struct Ps {
    #[serde(default)]
    models: Vec<PsModel>,
}

#[derive(Debug, Deserialize)]
struct PsModel {
    #[serde(default)]
    name: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    size_vram: u64,
}

impl PsModel {
    fn id(&self) -> &str {
        if self.name.is_empty() {
            &self.model
        } else {
            &self.name
        }
    }
}

#[derive(Debug)]
pub struct Ollama {
    base_url: String,
    client: reqwest::Client,
    cpu_models: HashSet<String>,
    keep_alive: Option<Value>,
    load_timeout: Duration,
    unload_timeout: Duration,
    poll: Duration,
}

impl Ollama {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap_or_default(),
            cpu_models: HashSet::new(),
            keep_alive: None,
            load_timeout: LOAD_TIMEOUT,
            unload_timeout: UNLOAD_TIMEOUT,
            poll: Duration::from_millis(250),
        }
    }

    /// Models that run on the CPU and never need the card. Names without a tag mean `:latest`.
    pub fn with_cpu_models<I, S>(mut self, models: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.cpu_models = models.into_iter().map(|m| tag(m.as_ref())).collect();
        self
    }

    /// `keep_alive` sent with `load`, e.g. `json!(-1)` to keep the model resident. Unset by
    /// default, so Ollama applies its own idle timeout.
    pub fn with_keep_alive(mut self, keep_alive: Value) -> Self {
        self.keep_alive = Some(keep_alive);
        self
    }

    pub fn with_load_timeout(mut self, t: Duration) -> Self {
        self.load_timeout = t;
        self
    }

    pub fn with_unload_timeout(mut self, t: Duration) -> Self {
        self.unload_timeout = t;
        self
    }

    pub fn with_poll_interval(mut self, t: Duration) -> Self {
        self.poll = t;
        self
    }

    async fn ps(&self) -> Result<Vec<PsModel>, BackendError> {
        let resp = self
            .client
            .get(format!("{}/api/ps", self.base_url))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(err)?;
        if !resp.status().is_success() {
            return Err(BackendError::Failed(format!(
                "/api/ps: HTTP {}",
                resp.status()
            )));
        }
        let ps: Ps = resp.json().await.map_err(err)?;
        Ok(ps.models)
    }

    async fn generate(&self, body: Value, timeout: Duration) -> Result<(), BackendError> {
        let resp = self
            .client
            .post(format!("{}/api/generate", self.base_url))
            .timeout(timeout)
            .json(&body)
            .send()
            .await
            .map_err(err)?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(BackendError::Failed(format!(
                "/api/generate: HTTP {status} {text}"
            )));
        }
        Ok(())
    }
}

fn err(e: reqwest::Error) -> BackendError {
    if e.is_connect() || e.is_timeout() {
        BackendError::Unreachable(e.to_string())
    } else {
        BackendError::Failed(e.to_string())
    }
}

/// Append `:latest` when the name has no tag. Colons in a registry host do not count.
fn tag(model: &str) -> String {
    let last = model.rsplit('/').next().unwrap_or(model);
    if last.contains(':') || last.contains('@') {
        model.to_string()
    } else {
        format!("{model}:latest")
    }
}

#[async_trait]
impl Backend for Ollama {
    fn kind(&self) -> &'static str {
        "ollama"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            unload: true,
            slots: false,
            health: true,
        }
    }

    fn resolve(&self, model: &str) -> String {
        tag(model)
    }

    fn uses_gpu(&self, model: &str) -> bool {
        !self.cpu_models.contains(&tag(model))
    }

    async fn loaded_gpu_models(&self) -> Result<Vec<String>, BackendError> {
        Ok(self
            .ps()
            .await?
            .into_iter()
            .filter(|m| m.size_vram > 0 && self.uses_gpu(m.id()))
            .map(|m| m.id().to_string())
            .collect())
    }

    async fn save_slot(&self, _model: &str) -> Result<bool, BackendError> {
        Ok(false)
    }

    async fn restore_slot(&self, _model: &str) -> Result<bool, BackendError> {
        Ok(false)
    }

    async fn unload_all(&self) -> Result<(), BackendError> {
        for model in self.loaded_gpu_models().await? {
            self.generate(
                json!({ "model": model, "keep_alive": 0 }),
                Duration::from_secs(30),
            )
            .await?;
        }
        let deadline = Instant::now() + self.unload_timeout;
        loop {
            if self.loaded_gpu_models().await?.is_empty() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(BackendError::Failed(
                    "models still loaded after unload".into(),
                ));
            }
            sleep(self.poll).await;
        }
    }

    async fn load(&self, model: &str) -> bool {
        let name = tag(model);
        // A generate call without a prompt loads the model and returns.
        let mut body = json!({ "model": name, "stream": false });
        if let (Some(ka), Some(obj)) = (&self.keep_alive, body.as_object_mut()) {
            obj.insert("keep_alive".into(), ka.clone());
        }
        if let Err(e) = self.generate(body, self.load_timeout).await {
            tracing::warn!("ollama: load {name} failed: {e}");
            return false;
        }
        match self.ps().await {
            Ok(models) => models.iter().any(|m| tag(m.id()) == name),
            Err(e) => {
                tracing::warn!("ollama: load {name}: {e}");
                false
            }
        }
    }
}
