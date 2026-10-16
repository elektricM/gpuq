// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The gpud authors

//! ServiceControl against fake systemctl/docker scripts and plain commands.
#![cfg(unix)]

#[path = "support_backends/mod.rs"]
mod support;

use std::time::Duration;

use gpud::workload::service::{Engine, ServiceControl, ServiceError};
use support::*;

const FAST: Duration = Duration::from_millis(20);

#[tokio::test]
async fn systemd_start_stop_is_active() {
    let dir = tempfile::tempdir().unwrap();
    let ctl =
        ServiceControl::systemd("llm.service", true).with_binary(fake_systemctl(dir.path(), false));

    assert!(!ctl.is_running().await.unwrap());
    ctl.start().await.unwrap();
    assert!(ctl.is_running().await.unwrap());
    ctl.stop().await.unwrap();
    assert!(!ctl.is_running().await.unwrap());

    assert_eq!(
        read_calls(dir.path()),
        [
            "--user is-active --quiet llm.service",
            "--user start --quiet llm.service",
            "--user is-active --quiet llm.service",
            "--user stop --quiet llm.service",
            "--user is-active --quiet llm.service",
        ]
    );
}

#[tokio::test]
async fn systemd_system_scope_has_no_user_flag() {
    let dir = tempfile::tempdir().unwrap();
    let ctl = ServiceControl::systemd("llm.service", false)
        .with_binary(fake_systemctl(dir.path(), false));
    ctl.start().await.unwrap();
    assert_eq!(read_calls(dir.path()), ["start --quiet llm.service"]);
}

#[tokio::test]
async fn container_engines_use_inspect_for_status() {
    for (engine, bin) in [(Engine::Docker, "docker"), (Engine::Podman, "podman")] {
        let dir = tempfile::tempdir().unwrap();
        let ctl = ServiceControl::container(engine, "vllm")
            .with_binary(fake_container_engine(dir.path(), bin));
        assert!(!ctl.is_running().await.unwrap());
        ctl.start().await.unwrap();
        assert!(ctl.is_running().await.unwrap());
        ctl.stop().await.unwrap();
        assert!(!ctl.is_running().await.unwrap());
        assert_eq!(
            read_calls(dir.path())[1],
            "start vllm",
            "{bin} start argument order"
        );
        assert!(read_calls(dir.path())[0].starts_with("inspect -f {{.State.Running}} vllm"));
    }
}

#[tokio::test]
async fn command_kind_runs_argv_and_status() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("up");
    let sh = |script: String| vec!["sh".to_string(), "-c".to_string(), script];
    let ctl = ServiceControl::commands(
        sh(format!("touch {}", state.display())),
        sh(format!("rm -f {}", state.display())),
        Some(sh(format!("test -e {}", state.display()))),
    );
    assert!(!ctl.is_running().await.unwrap());
    ctl.start().await.unwrap();
    assert!(state.exists());
    assert!(ctl.is_running().await.unwrap());
    ctl.stop().await.unwrap();
    assert!(!ctl.is_running().await.unwrap());
}

#[tokio::test]
async fn command_without_status_cannot_report_but_is_ready_after_start() {
    let ctl = ServiceControl::commands(vec!["true".into()], vec!["true".into()], None);
    assert!(matches!(
        ctl.is_running().await,
        Err(ServiceError::NoStatus)
    ));
    ctl.start().await.unwrap();
    ctl.wait_ready(None, Duration::from_secs(1)).await.unwrap();
}

#[tokio::test]
async fn failing_command_reports_stderr() {
    let ctl = ServiceControl::commands(
        vec!["sh".into(), "-c".into(), "echo boom >&2; exit 3".into()],
        vec!["true".into()],
        None,
    );
    let err = ctl.start().await.unwrap_err().to_string();
    assert!(err.contains("boom"), "{err}");
    assert!(err.contains("exit status: 3"), "{err}");
}

#[tokio::test]
async fn failing_systemctl_start_reports_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let ctl = ServiceControl::systemd("nope.service", false)
        .with_binary(fake_systemctl(dir.path(), true));
    let err = ctl.start().await.unwrap_err().to_string();
    assert!(err.contains("Unit not found"), "{err}");
}

#[tokio::test]
async fn missing_binary_is_a_spawn_error() {
    let ctl = ServiceControl::systemd("x", false).with_binary("/nonexistent/systemctl");
    assert!(matches!(ctl.start().await, Err(ServiceError::Spawn { .. })));
}

#[tokio::test]
async fn slow_command_times_out() {
    let ctl = ServiceControl::commands(vec!["sleep".into(), "5".into()], vec!["true".into()], None)
        .with_command_timeout(Duration::from_millis(100));
    assert!(matches!(
        ctl.start().await,
        Err(ServiceError::CommandTimeout { .. })
    ));
}

#[tokio::test]
async fn wait_ready_polls_health_url() {
    let dir = tempfile::tempdir().unwrap();
    write_state(dir.path(), "down");
    let state_dir = dir.path().to_path_buf();
    let server = FakeServer::start(move |_| {
        if read_state(&state_dir) == "up" {
            (200, "{}".into())
        } else {
            (503, "{}".into())
        }
    })
    .await;
    let ctl = ServiceControl::commands(vec!["true".into()], vec!["true".into()], None)
        .with_poll_interval(FAST);
    let flip = {
        let d = dir.path().to_path_buf();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            write_state(&d, "up");
        })
    };
    let url = format!("{}/health", server.url());
    ctl.wait_ready(Some(&url), Duration::from_secs(3))
        .await
        .unwrap();
    flip.await.unwrap();
    assert!(server.requests().len() >= 2);
}

#[tokio::test]
async fn wait_ready_falls_back_to_is_running() {
    let dir = tempfile::tempdir().unwrap();
    let ctl = ServiceControl::systemd("llm.service", false)
        .with_binary(fake_systemctl(dir.path(), false))
        .with_poll_interval(FAST);
    let d = dir.path().to_path_buf();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        write_state(&d, "active");
    });
    ctl.wait_ready(None, Duration::from_secs(3)).await.unwrap();
}

#[tokio::test]
async fn wait_ready_times_out() {
    let url = format!("{}/health", dead_url().await);
    let ctl = ServiceControl::commands(vec!["true".into()], vec!["true".into()], None)
        .with_poll_interval(FAST);
    let started = std::time::Instant::now();
    let err = ctl
        .wait_ready(Some(&url), Duration::from_millis(200))
        .await
        .unwrap_err();
    assert!(matches!(err, ServiceError::NotReady(_)), "{err}");
    assert!(started.elapsed() < Duration::from_secs(2));
}
