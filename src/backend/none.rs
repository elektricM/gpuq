// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! The "no LLM" backend: gpuq only arbitrates leases and services.

use async_trait::async_trait;

use super::{Backend, BackendError, Capabilities};

/// Reports nothing loaded and does nothing when asked to unload or load.
pub struct NoBackend;

#[async_trait]
impl Backend for NoBackend {
    fn kind(&self) -> &'static str {
        "none"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    fn resolve(&self, model: &str) -> String {
        model.to_owned()
    }

    fn uses_gpu(&self, _model: &str) -> bool {
        false
    }

    async fn loaded_gpu_models(&self) -> Result<Vec<String>, BackendError> {
        Ok(Vec::new())
    }

    async fn save_slot(&self, _model: &str) -> Result<bool, BackendError> {
        Ok(false)
    }

    async fn restore_slot(&self, _model: &str) -> Result<bool, BackendError> {
        Ok(false)
    }

    async fn unload_all(&self) -> Result<(), BackendError> {
        Ok(())
    }

    async fn load(&self, _model: &str) -> bool {
        true
    }
}
