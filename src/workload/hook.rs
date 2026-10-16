// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The gpud authors

//! Offload hook: a command run when a job does not fit.

use std::time::Duration;

use async_trait::async_trait;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tracing::{info, warn};

use crate::arbiter::OffloadHook;

pub struct CommandHook {
    argv: Vec<String>,
    timeout: Duration,
}

impl CommandHook {
    pub fn new(argv: Vec<String>, timeout: Duration) -> Self {
        Self { argv, timeout }
    }
}

#[async_trait]
impl OffloadHook for CommandHook {
    async fn run(&self) -> bool {
        run_hook(&self.argv, self.timeout).await == Some(0)
    }
}

async fn read_all<R: tokio::io::AsyncRead + Unpin>(r: Option<R>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(mut r) = r {
        let _ = r.read_to_end(&mut buf).await;
    }
    buf
}

/// Run `argv` (no shell) and return its exit code, or None if it could not run, was killed
/// by a signal, or timed out. Stdout and stderr are logged together (last 300 chars); they
/// are captured on separate pipes, so stderr follows stdout rather than interleaving.
pub async fn run_hook(argv: &[String], timeout: Duration) -> Option<i32> {
    let Some((bin, args)) = argv.split_first() else {
        warn!("offload hook: empty command");
        return None;
    };
    let mut command = Command::new(bin);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = match super::spawn(&mut command).await {
        Ok(c) => c,
        Err(e) => {
            warn!("hook {bin}: {e}");
            return None;
        }
    };
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let run = async { tokio::join!(child.wait(), read_all(stdout), read_all(stderr)) };
    let (status, mut out, err) = match tokio::time::timeout(timeout, run).await {
        Ok((Ok(status), out, err)) => (status, out, err),
        Ok((Err(e), ..)) => {
            warn!("hook {bin}: {e}");
            return None;
        }
        Err(_) => {
            let _ = child.kill().await; // kills and reaps
            warn!("hook {bin} timed out after {}s", timeout.as_secs_f32());
            return None;
        }
    };
    out.extend_from_slice(&err);
    let text = String::from_utf8_lossy(&out);
    let text = text.trim();
    let skip = text.chars().count().saturating_sub(300);
    let tail: String = text.chars().skip(skip).collect();
    let rc = status.code();
    info!("hook {bin} rc={rc:?} {tail}");
    rc
}
