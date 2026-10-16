//! `[[service]]` entries and the `ServiceSpec` that says how to stop and start a server.

use std::time::Duration;

use serde::{Deserialize, Deserializer};

use super::secs;
use crate::arbiter::Vram;
use crate::config::{parse_vram, VramParseError};

/// What a service is controlled with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceKind {
    Systemd {
        unit: String,
        user: bool,
    },
    Docker {
        container: String,
    },
    Podman {
        container: String,
    },
    Command {
        start: Vec<String>,
        stop: Vec<String>,
        status: Option<Vec<String>>,
    },
}

/// How to start and stop a server, plus how to tell it is up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceSpec {
    pub kind: ServiceKind,
    /// Polled after a start; the service counts as up when it answers 2xx.
    pub health_url: Option<String>,
    pub start_timeout: Duration,
}

const DEFAULT_START_TIMEOUT: Duration = Duration::from_secs(120);

/// Flat TOML shape of a spec; `kind` picks which fields apply.
#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawSpec {
    kind: String,
    unit: Option<String>,
    user: Option<bool>,
    container: Option<String>,
    start: Option<Vec<String>>,
    stop: Option<Vec<String>>,
    status: Option<Vec<String>>,
    health_url: Option<String>,
    #[serde(deserialize_with = "secs::option")]
    start_timeout_s: Option<Duration>,
}

impl<'de> Deserialize<'de> for ServiceSpec {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let raw = RawSpec::deserialize(d)?;
        // Fields that do not belong to the chosen kind are almost certainly a mistake.
        let present = [
            ("unit", raw.unit.is_some()),
            ("user", raw.user.is_some()),
            ("container", raw.container.is_some()),
            ("start", raw.start.is_some()),
            ("stop", raw.stop.is_some()),
            ("status", raw.status.is_some()),
        ];
        let (allowed, kind): (&[&str], ServiceKind) = match raw.kind.as_str() {
            "systemd" => (
                &["unit", "user"],
                ServiceKind::Systemd {
                    unit: raw
                        .unit
                        .clone()
                        .ok_or_else(|| Error::missing_field("unit"))?,
                    user: raw.user.unwrap_or(false),
                },
            ),
            "docker" => (
                &["container"],
                ServiceKind::Docker {
                    container: raw
                        .container
                        .clone()
                        .ok_or_else(|| Error::missing_field("container"))?,
                },
            ),
            "podman" => (
                &["container"],
                ServiceKind::Podman {
                    container: raw
                        .container
                        .clone()
                        .ok_or_else(|| Error::missing_field("container"))?,
                },
            ),
            "command" => (
                &["start", "stop", "status"],
                ServiceKind::Command {
                    start: raw
                        .start
                        .clone()
                        .ok_or_else(|| Error::missing_field("start"))?,
                    stop: raw
                        .stop
                        .clone()
                        .ok_or_else(|| Error::missing_field("stop"))?,
                    status: raw.status.clone(),
                },
            ),
            "" => return Err(Error::missing_field("kind")),
            other => {
                return Err(Error::custom(format!(
                    "unknown service kind {other:?}; use systemd, docker, podman or command"
                )))
            }
        };
        for (name, set) in present {
            if set && !allowed.contains(&name) {
                return Err(Error::custom(format!(
                    "`{name}` does not apply to kind \"{}\"",
                    raw.kind
                )));
            }
        }
        Ok(ServiceSpec {
            kind,
            health_url: raw.health_url,
            start_timeout: raw.start_timeout_s.unwrap_or(DEFAULT_START_TIMEOUT),
        })
    }
}

impl ServiceSpec {
    /// Completeness problems, each prefixed with `at`.
    pub fn problems(&self, at: &str) -> Vec<String> {
        let mut p = Vec::new();
        let blank = |s: &str| s.trim().is_empty();
        let bad_argv = |v: &[String]| v.first().map_or(true, |c| blank(c));
        match &self.kind {
            ServiceKind::Systemd { unit, .. } if blank(unit) => {
                p.push(format!("{at}: systemd needs a unit"))
            }
            ServiceKind::Docker { container } | ServiceKind::Podman { container }
                if blank(container) =>
            {
                p.push(format!("{at}: container name is empty"))
            }
            ServiceKind::Command {
                start,
                stop,
                status,
            } => {
                if bad_argv(start) {
                    p.push(format!("{at}: command needs a non-empty start argv"));
                }
                if bad_argv(stop) {
                    p.push(format!("{at}: command needs a non-empty stop argv"));
                }
                if status.as_deref().is_some_and(bad_argv) {
                    p.push(format!("{at}: status argv is empty (omit it instead)"));
                }
            }
            _ => {}
        }
        if let Some(u) = &self.health_url {
            if !(u.starts_with("http://") || u.starts_with("https://")) {
                p.push(format!(
                    "{at}: health_url must be an http(s) URL, not {u:?}"
                ));
            }
        }
        p
    }

    /// One-line description for `check-config`.
    pub fn summary(&self) -> String {
        match &self.kind {
            ServiceKind::Systemd { unit, user } => {
                format!("systemd {}unit {unit}", if *user { "user " } else { "" })
            }
            ServiceKind::Docker { container } => format!("docker container {container}"),
            ServiceKind::Podman { container } => format!("podman container {container}"),
            ServiceKind::Command { start, .. } => {
                format!("command {}", start.first().map_or("", String::as_str))
            }
        }
    }
}

/// Which request-level behavior a gated service gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Flavor {
    #[default]
    Generic,
    Comfyui,
}

/// A small HTTP gate in front of a service.
#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServiceGate {
    /// `host:port` gpud listens on.
    pub listen: String,
    /// Where requests are forwarded.
    pub backend: String,
    /// Path prefixes that need the card; empty means every request.
    pub gpu_paths: Vec<String>,
    /// While this URL reports work, the lease is kept.
    pub busy_url: Option<String>,
}

impl ServiceGate {
    /// Split `listen` into host and non-zero port.
    pub fn host_port(&self) -> Result<(String, u16), String> {
        let (host, port) = self
            .listen
            .rsplit_once(':')
            .ok_or_else(|| format!("{:?} is not host:port", self.listen))?;
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let port: u16 = port
            .parse()
            .map_err(|_| format!("bad port in {:?}", self.listen))?;
        if host.is_empty() || port == 0 {
            return Err(format!(
                "{:?} needs a host and a non-zero port",
                self.listen
            ));
        }
        Ok((host.to_string(), port))
    }
}

/// A managed GPU service (ComfyUI, an ML container, a TTS server...).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Service {
    pub name: String,
    pub class: String,
    /// "16G", "6144M", "512" or "all"; see [`parse_vram`].
    pub vram: String,
    pub yieldable: bool,
    /// None means always running and never stopped.
    pub control: Option<ServiceSpec>,
    /// Stop the service when its lease is released.
    pub stop_on_release: bool,
    #[serde(rename = "idle_release_s", deserialize_with = "secs::deserialize")]
    pub idle_release: Duration,
    pub gate: Option<ServiceGate>,
    pub flavor: Flavor,
}

impl Default for Service {
    fn default() -> Self {
        Service {
            name: String::new(),
            class: "interactive".into(),
            vram: String::new(),
            yieldable: true,
            control: None,
            stop_on_release: true,
            idle_release: Duration::from_secs(120),
            gate: None,
            flavor: Flavor::Generic,
        }
    }
}

impl Service {
    pub fn vram(&self) -> Result<Vram, VramParseError> {
        parse_vram(&self.vram)
    }

    pub fn summary(&self) -> String {
        let mut s = format!("{} ({}, vram {}", self.name, self.class, self.vram);
        if let Some(c) = &self.control {
            s.push_str(&format!(", {}", c.summary()));
        } else {
            s.push_str(", always running");
        }
        if let Some(g) = &self.gate {
            s.push_str(&format!(", gate {} -> {}", g.listen, g.backend));
        }
        s.push(')');
        s
    }
}
