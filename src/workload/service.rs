// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! Start, stop and check a GPU service: a systemd unit, a container, or plain commands.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use crate::config::{self as cfg, ServiceSpec};
use tokio::time::{sleep, Instant};

const DEFAULT_CMD_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_POLL: Duration = Duration::from_millis(500);

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("could not run `{cmd}`: {source}")]
    Spawn { cmd: String, source: std::io::Error },
    #[error("`{cmd}` failed ({status}): {output}")]
    Failed {
        cmd: String,
        status: String,
        output: String,
    },
    #[error("`{cmd}` timed out after {after:?}")]
    CommandTimeout { cmd: String, after: Duration },
    #[error("service not ready after {0:?}")]
    NotReady(Duration),
    #[error("service still up after {0:?}")]
    StillUp(Duration),
    #[error("no status command configured")]
    NoStatus,
    #[error("empty command")]
    EmptyCommand,
}

pub type Result<T> = std::result::Result<T, ServiceError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    Docker,
    Podman,
}

impl Engine {
    fn binary(self) -> &'static str {
        match self {
            Engine::Docker => "docker",
            Engine::Podman => "podman",
        }
    }
}

/// What kind of service this is and how to name it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceKind {
    Systemd {
        unit: String,
        user: bool,
    },
    Container {
        engine: Engine,
        name: String,
    },
    Commands {
        start: Vec<String>,
        stop: Vec<String>,
        status: Option<Vec<String>>,
    },
}

#[derive(Clone, Copy)]
enum Op {
    Start,
    Stop,
    Status,
}

/// Controls one service. Binaries (`systemctl`, `docker`, `podman`) can be overridden so tests
/// can point them at fake scripts.
#[derive(Clone, Debug)]
pub struct ServiceControl {
    kind: ServiceKind,
    bin: Option<PathBuf>,
    cmd_timeout: Duration,
    poll: Duration,
}

impl ServiceControl {
    pub fn new(kind: ServiceKind) -> Self {
        Self {
            kind,
            bin: None,
            cmd_timeout: DEFAULT_CMD_TIMEOUT,
            poll: DEFAULT_POLL,
        }
    }

    pub fn systemd(unit: impl Into<String>, user: bool) -> Self {
        Self::new(ServiceKind::Systemd {
            unit: unit.into(),
            user,
        })
    }

    pub fn container(engine: Engine, name: impl Into<String>) -> Self {
        Self::new(ServiceKind::Container {
            engine,
            name: name.into(),
        })
    }

    /// The control a `[llm.service]` or `[service.control]` table describes.
    pub fn from_spec(spec: &ServiceSpec) -> Self {
        match &spec.kind {
            cfg::ServiceKind::Systemd { unit, user } => Self::systemd(unit.clone(), *user),
            cfg::ServiceKind::Docker { container } => {
                Self::container(Engine::Docker, container.clone())
            }
            cfg::ServiceKind::Podman { container } => {
                Self::container(Engine::Podman, container.clone())
            }
            cfg::ServiceKind::Command {
                start,
                stop,
                status,
            } => Self::commands(start.clone(), stop.clone(), status.clone()),
        }
    }

    pub fn commands(start: Vec<String>, stop: Vec<String>, status: Option<Vec<String>>) -> Self {
        Self::new(ServiceKind::Commands {
            start,
            stop,
            status,
        })
    }

    /// Use this binary instead of `systemctl` / `docker` / `podman`. No effect on commands.
    pub fn with_binary(mut self, bin: impl Into<PathBuf>) -> Self {
        self.bin = Some(bin.into());
        self
    }

    /// Limit for a single start, stop or status command.
    pub fn with_command_timeout(mut self, t: Duration) -> Self {
        self.cmd_timeout = t;
        self
    }

    /// How often `wait_ready` and the backends' waits poll.
    pub fn with_poll_interval(mut self, t: Duration) -> Self {
        self.poll = t;
        self
    }

    pub fn poll_interval(&self) -> Duration {
        self.poll
    }

    pub fn kind(&self) -> &ServiceKind {
        &self.kind
    }

    fn argv(&self, op: Op) -> Option<Vec<String>> {
        let bin = |default: &str| {
            self.bin
                .as_deref()
                .unwrap_or_else(|| Path::new(default))
                .to_string_lossy()
                .into_owned()
        };
        match &self.kind {
            ServiceKind::Systemd { unit, user } => {
                let verb = match op {
                    Op::Start => "start",
                    Op::Stop => "stop",
                    Op::Status => "is-active",
                };
                let mut v = vec![bin("systemctl")];
                if *user {
                    v.push("--user".into());
                }
                v.extend([verb.into(), "--quiet".into(), unit.clone()]);
                Some(v)
            }
            ServiceKind::Container { engine, name } => {
                let mut v = vec![bin(engine.binary())];
                match op {
                    Op::Start => v.extend(["start".into(), name.clone()]),
                    Op::Stop => v.extend(["stop".into(), name.clone()]),
                    Op::Status => v.extend([
                        "inspect".into(),
                        "-f".into(),
                        "{{.State.Running}}".into(),
                        name.clone(),
                    ]),
                }
                Some(v)
            }
            ServiceKind::Commands {
                start,
                stop,
                status,
            } => match op {
                Op::Start => Some(start.clone()),
                Op::Stop => Some(stop.clone()),
                Op::Status => status.clone(),
            },
        }
    }

    async fn run(&self, argv: &[String]) -> Result<Finished> {
        let (prog, args) = argv.split_first().ok_or(ServiceError::EmptyCommand)?;
        let cmd = argv.join(" ");
        let mut command = Command::new(prog);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = async { super::spawn(&mut command).await?.wait_with_output().await };
        let out = match tokio::time::timeout(self.cmd_timeout, child).await {
            Err(_) => {
                return Err(ServiceError::CommandTimeout {
                    cmd,
                    after: self.cmd_timeout,
                })
            }
            Ok(Err(source)) => return Err(ServiceError::Spawn { cmd, source }),
            Ok(Ok(o)) => o,
        };
        Ok(Finished {
            cmd,
            success: out.status.success(),
            status: out.status.to_string(),
            stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }

    async fn run_checked(&self, op: Op) -> Result<()> {
        let argv = self.argv(op).ok_or(ServiceError::NoStatus)?;
        let f = self.run(&argv).await?;
        if f.success {
            return Ok(());
        }
        let output = match (f.stderr.is_empty(), f.stdout.is_empty()) {
            (false, false) => format!("{}; {}", f.stderr, f.stdout),
            (false, true) => f.stderr,
            (true, _) => f.stdout,
        };
        Err(ServiceError::Failed {
            cmd: f.cmd,
            status: f.status,
            output,
        })
    }

    pub async fn start(&self) -> Result<()> {
        self.run_checked(Op::Start).await
    }

    pub async fn stop(&self) -> Result<()> {
        self.run_checked(Op::Stop).await
    }

    /// A status command that exits non-zero means "not running", not an error.
    pub async fn is_running(&self) -> Result<bool> {
        let argv = self.argv(Op::Status).ok_or(ServiceError::NoStatus)?;
        let f = self.run(&argv).await?;
        Ok(match self.kind {
            ServiceKind::Container { .. } => f.success && f.stdout == "true",
            _ => f.success,
        })
    }

    /// Wait until the service serves. Polls `health_url` for a 2xx, or `is_running` without
    /// one. A command service with neither is taken as ready once `start` returned.
    pub async fn wait_ready(&self, health_url: Option<&str>, timeout: Duration) -> Result<()> {
        let client = http_client();
        let deadline = Instant::now() + timeout;
        loop {
            let ok = match health_url {
                Some(url) => http_ok(&client, url).await,
                None => match self.is_running().await {
                    Ok(up) => up,
                    Err(ServiceError::NoStatus) => return Ok(()),
                    Err(e) => return Err(e),
                },
            };
            if ok {
                return Ok(());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(ServiceError::NotReady(timeout));
            }
            sleep(self.poll.min(left)).await;
        }
    }
}

struct Finished {
    cmd: String,
    success: bool,
    status: String,
    stdout: String,
    stderr: String,
}

pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap_or_default()
}

/// True when `url` answers a GET with a 2xx.
pub(crate) async fn http_ok(client: &reqwest::Client, url: &str) -> bool {
    match client.get(url).send().await {
        Ok(r) => r.status().is_success(),
        Err(_) => false,
    }
}

/// Wait until `url` stops answering 2xx.
pub(crate) async fn wait_down(url: &str, timeout: Duration, poll: Duration) -> Result<()> {
    let client = http_client();
    let deadline = Instant::now() + timeout;
    loop {
        if !http_ok(&client, url).await {
            return Ok(());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(ServiceError::StillUp(timeout));
        }
        sleep(poll.min(left)).await;
    }
}
