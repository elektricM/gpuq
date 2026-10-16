// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! LLM servers that sit behind the gate.
//!
//! The arbiter only talks to a backend through [`Backend`]. A server that cannot drop a model
//! at runtime (vLLM, SGLang, plain llama-server) implements `unload_all` by stopping its
//! service and `load` by starting it again.

pub mod llama_server;
pub mod llamaswap;
pub mod models;
pub mod none;
pub mod ollama;
pub mod stopstart;

use async_trait::async_trait;
use serde::Serialize;

/// What a backend can do. The arbiter skips steps a backend does not support.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Capabilities {
    /// Models can be unloaded and loaded while the server keeps running.
    pub unload: bool,
    /// KV cache slots can be saved to disk and restored.
    pub slots: bool,
    /// A health endpoint tells when a model is ready.
    pub health: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("backend unreachable: {0}")]
    Unreachable(String),
    #[error("backend error: {0}")]
    Failed(String),
}

#[async_trait]
pub trait Backend: Send + Sync + 'static {
    /// Short name for logs and status, e.g. "llama-swap".
    fn kind(&self) -> &'static str;

    fn capabilities(&self) -> Capabilities;

    /// Canonical model id for the name a client used (aliases resolve to their model).
    fn resolve(&self, model: &str) -> String;

    /// Whether requests for `model` need the GPU. Unknown names do.
    fn uses_gpu(&self, model: &str) -> bool;

    /// GPU models that are loaded or loading right now.
    async fn loaded_gpu_models(&self) -> Result<Vec<String>, BackendError>;

    /// Save the KV slot of a loaded model. `Ok(true)` when something was saved.
    async fn save_slot(&self, model: &str) -> Result<bool, BackendError>;

    /// Restore the slot saved by `save_slot`. `Ok(false)` when nothing was restored.
    async fn restore_slot(&self, model: &str) -> Result<bool, BackendError>;

    /// Free the card: unload every model, or stop the service.
    async fn unload_all(&self) -> Result<(), BackendError>;

    /// Load `model` (or start the service) and wait until it serves. False on failure.
    async fn load(&self, model: &str) -> bool;
}
