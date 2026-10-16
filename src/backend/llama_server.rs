//! Plain llama.cpp `llama-server`: one model per process, so unloading means stopping it.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::stopstart::Managed;
use super::{Backend, BackendError, Capabilities};
use crate::workload::service::{http_client, ServiceControl};

#[derive(Debug)]
pub struct LlamaServer {
    base_url: String,
    model: String,
    slots: bool,
    inner: Managed,
    client: reqwest::Client,
    /// Slot file written by `save_slot`, waiting for `restore_slot`.
    saved: Mutex<Option<String>>,
}

impl LlamaServer {
    /// `slot_save`: the server was started with `--slot-save-path`.
    pub fn new(
        base_url: &str,
        model_name: impl Into<String>,
        control: ServiceControl,
        slot_save: bool,
    ) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model_name.into(),
            slots: slot_save,
            inner: Managed::new(base_url, "/health", control),
            client: http_client(),
            saved: Mutex::new(None),
        }
    }

    pub fn with_start_timeout(mut self, t: Duration) -> Self {
        self.inner.start_timeout = t;
        self
    }

    pub fn with_stop_timeout(mut self, t: Duration) -> Self {
        self.inner.stop_timeout = t;
        self
    }

    fn slot_file(&self) -> String {
        let safe: String = self
            .model
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        format!("{safe}.slot0.bin")
    }

    /// llama-server's slot API; the filename is relative to its `--slot-save-path`.
    async fn slot_call(&self, action: &str, filename: &str) -> Option<Value> {
        let url = format!("{}/slots/0?action={action}", self.base_url);
        let resp = match self
            .client
            .post(&url)
            .json(&json!({ "filename": filename }))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("slot {action} {}: {e}", self.model);
                return None;
            }
        };
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            tracing::warn!("slot {action} {}: HTTP {status} {body}", self.model);
            return None;
        }
        Some(body)
    }
}

#[async_trait]
impl Backend for LlamaServer {
    fn kind(&self) -> &'static str {
        "llama-server"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            unload: false,
            slots: self.slots,
            health: true,
        }
    }

    fn resolve(&self, _model: &str) -> String {
        self.model.clone()
    }

    fn uses_gpu(&self, _model: &str) -> bool {
        true
    }

    async fn loaded_gpu_models(&self) -> Result<Vec<String>, BackendError> {
        Ok(if self.inner.healthy().await {
            vec![self.model.clone()]
        } else {
            Vec::new()
        })
    }

    async fn save_slot(&self, _model: &str) -> Result<bool, BackendError> {
        if !self.slots {
            return Ok(false);
        }
        let name = self.slot_file();
        let Some(body) = self.slot_call("save", &name).await else {
            return Ok(false);
        };
        let n_saved = body.get("n_saved").and_then(Value::as_u64).unwrap_or(0);
        let mut saved = self.saved.lock().await;
        if n_saved == 0 {
            *saved = None;
            return Ok(false);
        }
        *saved = Some(name);
        Ok(true)
    }

    async fn restore_slot(&self, _model: &str) -> Result<bool, BackendError> {
        let Some(name) = self.saved.lock().await.take() else {
            return Ok(false);
        };
        Ok(self.slot_call("restore", &name).await.is_some())
    }

    async fn unload_all(&self) -> Result<(), BackendError> {
        self.inner.stop().await
    }

    async fn load(&self, _model: &str) -> bool {
        match self.inner.start().await {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!("llama-server: start failed: {e}");
                false
            }
        }
    }
}
