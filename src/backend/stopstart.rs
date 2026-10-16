// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The gpud authors

//! Servers that cannot unload a model at runtime (vLLM, SGLang): unloading stops the service,
//! loading starts it again.

use std::time::Duration;

use async_trait::async_trait;

use super::{Backend, BackendError, Capabilities};
use crate::workload::service::{http_client, http_ok, wait_down, ServiceControl};

const DEFAULT_START_TIMEOUT: Duration = Duration::from_secs(300);
const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(60);

/// A server process that is started and stopped through a [`ServiceControl`] and answers a
/// health URL. Shared by [`StopStart`] and the llama-server backend.
#[derive(Debug)]
pub(crate) struct Managed {
    health_url: String,
    control: ServiceControl,
    client: reqwest::Client,
    pub start_timeout: Duration,
    pub stop_timeout: Duration,
}

impl Managed {
    pub fn new(base_url: &str, health_path: &str, control: ServiceControl) -> Self {
        let health_url = format!(
            "{}/{}",
            base_url.trim_end_matches('/'),
            health_path.trim_start_matches('/')
        );
        Self {
            health_url,
            control,
            client: http_client(),
            start_timeout: DEFAULT_START_TIMEOUT,
            stop_timeout: DEFAULT_STOP_TIMEOUT,
        }
    }

    pub async fn healthy(&self) -> bool {
        http_ok(&self.client, &self.health_url).await
    }

    pub async fn stop(&self) -> Result<(), BackendError> {
        self.control
            .stop()
            .await
            .map_err(|e| BackendError::Failed(e.to_string()))?;
        wait_down(
            &self.health_url,
            self.stop_timeout,
            self.control.poll_interval(),
        )
        .await
        .map_err(|e| BackendError::Failed(e.to_string()))
    }

    pub async fn start(&self) -> Result<(), BackendError> {
        self.control
            .start()
            .await
            .map_err(|e| BackendError::Failed(e.to_string()))?;
        self.control
            .wait_ready(Some(&self.health_url), self.start_timeout)
            .await
            .map_err(|e| BackendError::Failed(e.to_string()))
    }
}

/// Backend for a server that serves one model and has no unload call.
#[derive(Debug)]
pub struct StopStart {
    kind: &'static str,
    model: String,
    inner: Managed,
}

impl StopStart {
    pub fn new(
        kind_name: &'static str,
        base_url: &str,
        model_name: impl Into<String>,
        control: ServiceControl,
        health_path: &str,
    ) -> Self {
        Self {
            kind: kind_name,
            model: model_name.into(),
            inner: Managed::new(base_url, health_path, control),
        }
    }

    pub fn vllm(base_url: &str, model_name: impl Into<String>, control: ServiceControl) -> Self {
        Self::new("vllm", base_url, model_name, control, "/health")
    }

    pub fn sglang(base_url: &str, model_name: impl Into<String>, control: ServiceControl) -> Self {
        Self::new("sglang", base_url, model_name, control, "/health")
    }

    pub fn with_start_timeout(mut self, t: Duration) -> Self {
        self.inner.start_timeout = t;
        self
    }

    pub fn with_stop_timeout(mut self, t: Duration) -> Self {
        self.inner.stop_timeout = t;
        self
    }
}

#[async_trait]
impl Backend for StopStart {
    fn kind(&self) -> &'static str {
        self.kind
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            unload: false,
            slots: false,
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
        Ok(false)
    }

    async fn restore_slot(&self, _model: &str) -> Result<bool, BackendError> {
        Ok(false)
    }

    async fn unload_all(&self) -> Result<(), BackendError> {
        self.inner.stop().await
    }

    async fn load(&self, _model: &str) -> bool {
        match self.inner.start().await {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!("{}: start failed: {e}", self.kind);
                false
            }
        }
    }
}
