// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The gpud authors

//! VRAM probes: how much of the card is in use, whoever uses it.

pub mod amd;
pub mod command;
pub mod detect;
pub mod intel;
pub mod nvidia;

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

/// How long an external probe command may run before it counts as failed.
pub const TOOL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Run `argv` (no shell) and return its stdout. Spawn failure is `Io`, a timeout kills the
/// child and is `Io(TimedOut)`, a non-zero exit is `Io(Other)` with the first stderr line.
pub(crate) async fn run_tool(
    argv: &[String],
    timeout: std::time::Duration,
) -> Result<String, ProbeError> {
    use std::io::{Error, ErrorKind};
    use std::process::Stdio;

    let Some((bin, args)) = argv.split_first() else {
        return Err(ProbeError::NotFound("empty command".into()));
    };
    let mut command = tokio::process::Command::new(bin);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = crate::workload::spawn(&mut command)
        .await
        .map_err(|e| Error::new(e.kind(), format!("{bin}: {e}")))?;
    // On timeout the future (and the child inside it) is dropped, which kills the process.
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(r) => r.map_err(|e| Error::new(e.kind(), format!("{bin}: {e}")))?,
        Err(_) => {
            let msg = format!("{bin}: timed out after {}s", timeout.as_secs_f32());
            return Err(Error::new(ErrorKind::TimedOut, msg).into());
        }
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let line = err.lines().next().unwrap_or("").trim();
        return Err(Error::other(format!("{bin}: {}: {line}", out.status)).into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
