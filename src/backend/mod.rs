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

use crate::Mib;

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

/// The cards the next load goes to, for servers that gpuq starts itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Placement {
    /// Card names, in device order.
    pub cards: Vec<String>,
    /// The cards' env merged, e.g. `CUDA_VISIBLE_DEVICES=0,1`.
    pub env: Vec<(String, String)>,
    /// VRAM each card offers, same order as `cards`.
    pub free_mib: Vec<Mib>,
}

impl Placement {
    /// `--tensor-split` proportions for llama.cpp: free VRAM per card in GiB, at least 1.
    /// None for a single card.
    pub fn tensor_split(&self) -> Option<String> {
        if self.cards.len() < 2 || self.free_mib.len() != self.cards.len() {
            return None;
        }
        let parts: Vec<String> = self
            .free_mib
            .iter()
            .map(|m| (m / 1024).max(1).to_string())
            .collect();
        Some(parts.join(","))
    }
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

    /// VRAM `model` needs over all its cards, if the backend can tell.
    fn vram_estimate(&self, _model: &str) -> Option<Mib> {
        None
    }

    /// The cards the next load goes to. Called before every reload and whenever the LLM
    /// takes another card. Only servers that gpuq starts itself can act on it.
    fn place(&self, _placement: &Placement) {}
}
