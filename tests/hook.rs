// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! Hook runner. Ported from tests/test_hooks.py. Unix only (uses sh, true, sleep).
#![cfg(unix)]

use std::time::{Duration, Instant};

use gpuq::arbiter::OffloadHook;
use gpuq::workload::hook::{run_hook, CommandHook};

fn argv(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

#[tokio::test]
async fn returns_exit_code() {
    let t = Duration::from_secs(5);
    assert_eq!(run_hook(&argv(&["true"]), t).await, Some(0));
    assert_eq!(run_hook(&argv(&["sh", "-c", "exit 3"]), t).await, Some(3));
}

#[tokio::test]
async fn missing_binary_returns_none() {
    let r = run_hook(&argv(&["/nonexistent/hook"]), Duration::from_secs(5)).await;
    assert_eq!(r, None);
}

#[tokio::test]
async fn empty_argv_returns_none() {
    assert_eq!(run_hook(&[], Duration::from_secs(5)).await, None);
}

#[tokio::test]
async fn timeout_kills_the_command() {
    let start = Instant::now();
    let r = run_hook(&argv(&["sleep", "30"]), Duration::from_millis(200)).await;
    assert_eq!(r, None);
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn offload_hook_is_true_only_on_rc_zero() {
    let t = Duration::from_secs(5);
    assert!(CommandHook::new(argv(&["true"]), t).run("gpu0").await);
    assert!(
        !CommandHook::new(argv(&["sh", "-c", "exit 1"]), t)
            .run("gpu0")
            .await
    );
    assert!(
        !CommandHook::new(argv(&["/nonexistent/hook"]), t)
            .run("gpu0")
            .await
    );
}
