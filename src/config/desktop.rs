// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! `[desktop]`: free the VRAM the graphical desktop holds on a card, by moving the desktop
//! to another card or stopping it, and undo that later.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

use super::secs;
use crate::Mib;

/// What gpuq does with the desktop when its VRAM is needed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Policy {
    /// Never touch it.
    #[default]
    Off,
    /// Stop the display manager and the session units.
    Stop,
    /// Restart the desktop on another card, see `[desktop.offload]`.
    Offload,
}

impl Policy {
    pub fn as_str(self) -> &'static str {
        match self {
            Policy::Off => "off",
            Policy::Stop => "stop",
            Policy::Offload => "offload",
        }
    }
}

/// `[desktop.offload]`: how to restart the desktop on another card.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Offload {
    /// Set in the user manager before the restart, e.g.
    /// `KWIN_DRM_DEVICES = "/dev/dri/by-path/pci-0000:15:00.0-card"`. Removed on restore.
    pub user_env: BTreeMap<String, String>,
    /// Set in the system manager (with `elevate`) before the restart. Removed on restore.
    pub system_env: BTreeMap<String, String>,
    /// Commands (argv, no shell) run before the restart, e.g. to write a drop-in.
    pub commands: Vec<Vec<String>>,
    /// Commands run on restore, before the restart that moves the desktop back.
    pub undo_commands: Vec<Vec<String>>,
    /// Move the desktop back on its own once nothing needs the VRAM (it restarts again).
    pub move_back: bool,
    /// Wait this long after the restart for the session to come up before checking the card.
    #[serde(rename = "settle_s", deserialize_with = "secs::deserialize")]
    pub settle: Duration,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Desktop {
    /// `off` (default), `stop` or `offload`.
    pub policy: Policy,
    /// What `offload` does when the desktop cannot be moved: `stop` or `off`.
    pub fallback: Policy,
    pub offload: Offload,
    /// The card the desktop is moved off. Empty: the LLM's first card, else the primary card.
    pub card: String,
    /// logind seat whose sessions block a release.
    pub seat: String,
    /// System units stopped on release and started on restore, e.g. the display manager.
    pub system_units: Vec<String>,
    /// Units of the user manager stopped on release (after the session, they would keep
    /// the compositor alive). Not started on restore: a login starts them.
    pub user_units: Vec<String>,
    /// Whose user manager `user_units` and `session_env` refer to. Empty: the user gpuq runs as.
    pub user: String,
    /// Prefix for commands that change system units, e.g. `["sudo", "-n"]`.
    pub elevate: Vec<String>,
    /// Processes never counted as the desktop: an exact process name, or a piece of the
    /// command line (e.g. `"kwin_wayland --virtual"` for a headless compositor).
    pub ignore: Vec<String>,
    /// LLM models that want the desktop off the card while loaded; `*` matches any text.
    pub models: Vec<String>,
    /// Release when an LLM model is loaded on the card and less VRAM than this is free.
    /// 0 turns the check off.
    pub min_free_mib: Mib,
    /// A job that waits for the card and does not fit releases the desktop.
    pub leases: bool,
    #[serde(rename = "release_delay_s", deserialize_with = "secs::deserialize")]
    pub release_delay: Duration,
    #[serde(rename = "restore_delay_s", deserialize_with = "secs::deserialize")]
    pub restore_delay: Duration,
    /// After a restore asked for by hand or by input, no automatic release for this long.
    #[serde(rename = "hold_s", deserialize_with = "secs::deserialize")]
    pub hold: Duration,
    #[serde(rename = "retry_s", deserialize_with = "secs::deserialize")]
    pub retry: Duration,
    #[serde(rename = "poll_s", deserialize_with = "secs::deserialize")]
    pub poll: Duration,
    #[serde(rename = "command_timeout_s", deserialize_with = "secs::deserialize")]
    pub command_timeout: Duration,
    /// How long stopped units and their processes get to go away (and started units to come up).
    #[serde(rename = "stop_timeout_s", deserialize_with = "secs::deserialize")]
    pub stop_timeout: Duration,
    /// Set in the user manager and the D-Bus activation environment after a release, so
    /// services started later do not connect to the display that is gone. An empty value
    /// removes the variable. A `WAYLAND_DISPLAY` socket that does not exist is removed too.
    pub session_env: BTreeMap<String, String>,
    /// Restore when a key is pressed at the seat, and hold off a release while there was
    /// input in the last `input_idle_s`. Needs read access to the input devices.
    pub wake_on_input: bool,
    /// Device paths; `*` in the file name matches any text.
    pub input_devices: Vec<String>,
    #[serde(rename = "input_idle_s", deserialize_with = "secs::deserialize")]
    pub input_idle: Duration,
    /// Where gpuq remembers that it stopped the desktop, across restarts.
    pub state_file: Option<PathBuf>,
    /// After each change, look for new errors, failed units and core dumps in the journal.
    pub check_journal: bool,
    /// How long after a change the journal is checked.
    #[serde(rename = "check_delay_s", deserialize_with = "secs::deserialize")]
    pub check_delay: Duration,
}

impl Default for Desktop {
    fn default() -> Self {
        Desktop {
            policy: Policy::Off,
            fallback: Policy::Off,
            offload: Offload {
                settle: Duration::from_secs(15),
                ..Offload::default()
            },
            card: String::new(),
            seat: "seat0".into(),
            system_units: vec!["display-manager.service".into()],
            user_units: Vec::new(),
            user: String::new(),
            elevate: Vec::new(),
            ignore: Vec::new(),
            models: Vec::new(),
            min_free_mib: 0,
            leases: true,
            release_delay: Duration::from_secs(30),
            restore_delay: Duration::from_secs(600),
            hold: Duration::from_secs(1800),
            retry: Duration::from_secs(900),
            poll: Duration::from_secs(5),
            command_timeout: Duration::from_secs(30),
            stop_timeout: Duration::from_secs(60),
            session_env: [("DISPLAY", ""), ("WAYLAND_DISPLAY", "")]
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .into(),
            wake_on_input: false,
            input_devices: vec![
                "/dev/input/by-path/*-event-kbd".into(),
                "/dev/input/by-id/*-event-kbd".into(),
            ],
            input_idle: Duration::from_secs(300),
            state_file: None,
            check_journal: true,
            check_delay: Duration::from_secs(20),
        }
    }
}

impl Desktop {
    /// `state_file`, else `$XDG_STATE_HOME/gpuq/desktop.json`, else
    /// `~/.local/state/gpuq/desktop.json`. None when no home can be found.
    pub fn state_path(&self) -> Option<PathBuf> {
        if let Some(p) = &self.state_file {
            return Some(p.clone());
        }
        let env = |k: &str| {
            std::env::var_os(k)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let dir = env("XDG_STATE_HOME").or_else(|| env("HOME").map(|h| h.join(".local/state")))?;
        Some(dir.join("gpuq/desktop.json"))
    }

    /// Problems with this section, except the card name (see [`Desktop::card_problem`]).
    pub fn problems(&self) -> Vec<String> {
        let mut p = Vec::new();
        if self.fallback == Policy::Offload {
            p.push("desktop.fallback must be \"stop\" or \"off\"".into());
        }
        if !self.enabled() {
            return p;
        }
        if self.policy == Policy::Offload {
            let o = &self.offload;
            if o.user_env.is_empty() && o.system_env.is_empty() && o.commands.is_empty() {
                p.push(
                    "desktop.policy = \"offload\" needs offload.user_env, offload.system_env or \
                     offload.commands: something that points the desktop at the other card"
                        .into(),
                );
            }
            if self.system_units.is_empty() {
                p.push("desktop.policy = \"offload\" restarts system_units; it is empty".into());
            }
            for (what, list) in [
                ("commands", &o.commands),
                ("undo_commands", &o.undo_commands),
            ] {
                if list
                    .iter()
                    .any(|c| !matches!(c.first(), Some(b) if !b.is_empty()))
                {
                    p.push(format!("desktop.offload.{what}: a command with no program"));
                }
            }
            for k in o.user_env.keys().chain(o.system_env.keys()) {
                if !is_var(k) {
                    p.push(format!("desktop.offload: {k:?} is not a variable name"));
                }
            }
        }
        if self.system_units.is_empty() && self.user_units.is_empty() {
            p.push("desktop: system_units and user_units are both empty; nothing to stop".into());
        }
        for u in self.system_units.iter().chain(&self.user_units) {
            if u.is_empty() || u.starts_with('-') || u.contains(char::is_whitespace) {
                p.push(format!("desktop: {u:?} is not a unit name"));
            }
        }
        if self.elevate.first().is_some_and(|c| c.is_empty()) {
            p.push("desktop.elevate: the program name is empty".into());
        }
        if self.ignore.iter().any(|i| i.trim().is_empty()) {
            p.push("desktop.ignore: empty entries would match every process".into());
        }
        if self.poll.is_zero() {
            p.push("desktop.poll_s must be above 0".into());
        }
        if self.command_timeout.is_zero() || self.stop_timeout.is_zero() {
            p.push("desktop: command_timeout_s and stop_timeout_s must be above 0".into());
        }
        for k in self.session_env.keys() {
            if !is_var(k) {
                p.push(format!("desktop.session_env: {k:?} is not a variable name"));
            }
        }
        if self.wake_on_input && self.input_devices.is_empty() {
            p.push("desktop.wake_on_input needs input_devices".into());
        }
        p
    }

    /// The policy is not `off`.
    pub fn enabled(&self) -> bool {
        self.policy != Policy::Off
    }

    /// `card` names a card that is not in `cards`, the cards in use.
    pub fn card_problem(&self, cards: &[String]) -> Option<String> {
        (self.enabled() && !self.card.is_empty() && !cards.contains(&self.card)).then(|| {
            format!(
                "desktop.card {:?} is not a configured card; known: {}",
                self.card,
                cards.join(", ")
            )
        })
    }

    /// One line for `gpuq check-config`.
    pub fn summary(&self, card: &str) -> String {
        if !self.enabled() {
            return "desktop: off".into();
        }
        let mut units: Vec<String> = self.system_units.clone();
        units.extend(self.user_units.iter().map(|u| format!("{u} (user)")));
        let mut why = Vec::new();
        if !self.models.is_empty() {
            why.push(format!("models {}", self.models.join(", ")));
        }
        if self.min_free_mib > 0 {
            why.push(format!("LLM with < {} MiB free", self.min_free_mib));
        }
        if self.leases {
            why.push("jobs that do not fit".into());
        }
        let why = if why.is_empty() {
            "only by hand".to_owned()
        } else {
            why.join(", ")
        };
        let policy = match self.policy {
            Policy::Offload => format!("offload, else {}", self.fallback.as_str()),
            other => other.as_str().to_owned(),
        };
        format!(
            "desktop: {policy} on {card}, seat {}, units {}; released for {why}",
            self.seat,
            units.join(", ")
        )
    }
}

fn is_var(k: &str) -> bool {
    !k.is_empty()
        && !k.starts_with(|c: char| c.is_ascii_digit())
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}
