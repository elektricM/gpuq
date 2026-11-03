// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! `[[service]]` entries and the `ServiceSpec` that says how to stop and start a server.

use std::time::Duration;

use serde::{Deserialize, Deserializer};

use super::{is_http_url, secs};
use crate::arbiter::Vram;
use crate::config::{parse_vram, CardList, VramParseError};

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
    /// A server that stays up and frees its VRAM through its own API: "stop" is a POST to
    /// `unload_url`, "start" a POST to `load_url` (or nothing, for servers that load on the
    /// next request). Bodies are JSON text.
    Http {
        unload_url: String,
        unload_body: Option<String>,
        load_url: Option<String>,
        load_body: Option<String>,
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
    unload_url: Option<String>,
    unload_body: Option<String>,
    load_url: Option<String>,
    load_body: Option<String>,
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
            ("unload_url", raw.unload_url.is_some()),
            ("unload_body", raw.unload_body.is_some()),
            ("load_url", raw.load_url.is_some()),
            ("load_body", raw.load_body.is_some()),
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
            "http" => (
                &["unload_url", "unload_body", "load_url", "load_body"],
                ServiceKind::Http {
                    unload_url: raw
                        .unload_url
                        .clone()
                        .ok_or_else(|| Error::missing_field("unload_url"))?,
                    unload_body: raw.unload_body.clone(),
                    load_url: raw.load_url.clone(),
                    load_body: raw.load_body.clone(),
                },
            ),
            "" => return Err(Error::missing_field("kind")),
            other => {
                return Err(Error::custom(format!(
                    "unknown service kind {other:?}; use systemd, docker, podman, command or http"
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
            ServiceKind::Http {
                unload_url,
                unload_body,
                load_url,
                load_body,
            } => {
                for (name, url) in [
                    ("unload_url", Some(unload_url)),
                    ("load_url", load_url.as_ref()),
                ] {
                    if let Some(u) = url.filter(|u| !is_http_url(u)) {
                        p.push(format!("{at}: {name} must be an http(s) URL, not {u:?}"));
                    }
                }
                if load_body.is_some() && load_url.is_none() {
                    p.push(format!("{at}: load_body without load_url"));
                }
                for (name, body) in [("unload_body", unload_body), ("load_body", load_body)] {
                    if let Some(Err(e)) = body
                        .as_deref()
                        .map(serde_json::from_str::<serde_json::Value>)
                    {
                        p.push(format!("{at}: {name} is not valid JSON: {e}"));
                    }
                }
            }
            _ => {}
        }
        if let Some(u) = &self.health_url {
            if !is_http_url(u) {
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
            ServiceKind::Http { unload_url, .. } => format!("http unload {unload_url}"),
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
    /// `host:port` gpuq listens on.
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
    /// Card or cards the service runs on; default the primary card. "any" is not allowed.
    pub card: Option<CardList>,
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
            card: None,
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
        if let Some(c) = &self.card {
            s.push_str(&format!(", card {c}"));
        }
        s.push(')');
        s
    }
}
