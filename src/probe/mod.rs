//! VRAM probes: how much of the card is in use, whoever uses it.

use async_trait::async_trait;
use serde::Serialize;

use crate::Mib;

#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Parse(String),
    #[error("{0}")]
    NotFound(String),
}

/// One reading of the card.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct VramReading {
    pub used_mib: Mib,
    pub total_mib: Mib,
    /// AMD GTT (system memory mapped for the GPU); None where the vendor has no such counter.
    pub gtt_used_mib: Option<Mib>,
}

#[async_trait]
pub trait VramProbe: Send + Sync + 'static {
    /// Device this probe reads, e.g. "amd:0000:03:00.0" or "nvidia:0".
    fn device(&self) -> String;

    async fn read(&self) -> Result<VramReading, ProbeError>;
}
