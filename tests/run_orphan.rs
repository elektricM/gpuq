// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! `gpud run` killed with SIGKILL takes its job down with it (Linux only: PR_SET_PDEATHSIG).
#![cfg(target_os = "linux")]

mod support_api;

use std::process::Stdio;
use std::time::Duration;

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use support_api::*;
use tokio::process::Command;

fn alive(pid: i32) -> bool {
    // A zombie still answers kill(0); count it as gone.
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => !s
            .rsplit(')')
            .next()
            .unwrap_or("")
            .trim_start()
            .starts_with('Z'),
        Err(_) => false,
    }
}

#[tokio::test]
async fn job_dies_when_gpud_run_is_killed() {
    let api = TestApi::start(Duration::from_millis(200)).await;
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let script = format!("echo $$ > {}; exec sleep 60", pidfile.display());
    let mut run = Command::new(env!("CARGO_BIN_EXE_gpud"))
        .args(["run", "--vram", "1G", "--", "sh", "-c", &script])
        .env("GPUD_API", api.addr.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    let mut job = None;
    for _ in 0..200 {
        if let Some(pid) = std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
        {
            job = Some(pid);
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let job = job.expect("the job never started");
    assert!(alive(job));

    kill(Pid::from_raw(run.id().unwrap() as i32), Signal::SIGKILL).unwrap();
    run.wait().await.unwrap();

    let mut gone = false;
    for _ in 0..200 {
        if !alive(job) {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    if !gone {
        let _ = kill(Pid::from_raw(job), Signal::SIGKILL);
    }
    assert!(gone, "the job outlived gpud run");
    assert!(status_within(&api.rig, Duration::from_secs(5), |s| s.leases.is_empty()).await);
}
