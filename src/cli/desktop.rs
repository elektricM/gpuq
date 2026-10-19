// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! `gpuq desktop`.
use std::process::ExitCode;

use clap::Subcommand;
use serde_json::{json, Value};

use super::client::{self, ClientError};
use super::{say, EX_UNAVAILABLE};

#[derive(Subcommand)]
pub enum Action {
    /// Show the state, why, and the processes on the card (the default).
    Status {
        /// Print the state as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Free the card now: move or stop the desktop as the policy says.
    Release {
        /// Also when someone is logged in at the seat.
        #[arg(long)]
        force: bool,
    },
    /// Give the desktop back now, and hold off automatic releases.
    Restore {
        /// Hold off automatic releases this many seconds (default: hold_s from the config).
        #[arg(long, value_name = "SECS")]
        hold: Option<f64>,
    },
}

pub async fn desktop(addr: &str, action: Option<Action>) -> ExitCode {
    let (path, body) = match action.unwrap_or(Action::Status { json: false }) {
        Action::Status { json } => return status(addr, json).await,
        Action::Release { force } => ("/desktop/release", json!({ "force": force })),
        Action::Restore { hold } => ("/desktop/restore", json!({ "hold_s": hold })),
    };
    match client::post_json(addr, path, &body).await {
        Ok((_, v)) if v.get("ok").and_then(Value::as_bool) == Some(true) => {
            println!("{}", s(&v, "message"));
            ExitCode::SUCCESS
        }
        Ok((_, v)) => {
            say(v.get("error").and_then(Value::as_str).unwrap_or("refused"));
            ExitCode::FAILURE
        }
        Err(e) => fail(addr, e),
    }
}

fn fail(addr: &str, e: ClientError) -> ExitCode {
    match e {
        ClientError::Down(_) => {
            say(&format!("gpuq is not running ({addr})"));
            ExitCode::from(EX_UNAVAILABLE)
        }
        e => {
            say(&e.to_string());
            ExitCode::FAILURE
        }
    }
}

async fn status(addr: &str, json: bool) -> ExitCode {
    match client::get_json(addr, "/desktop").await {
        Ok(v) if json => {
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            ExitCode::SUCCESS
        }
        Ok(v) => {
            print!("{}", render(&v));
            ExitCode::SUCCESS
        }
        Err(e) => fail(addr, e),
    }
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn list(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The text form of `GET /desktop`.
pub fn render(v: &Value) -> String {
    let mut out = String::new();
    let mut put = |label: &str, text: String| out.push_str(&format!("{label:<10} {text}\n"));
    put("desktop", super::status::desktop_line(v));
    put(
        "policy",
        format!(
            "{} on {}, seat {}",
            s(v, "policy"),
            s(v, "card"),
            s(v, "seat")
        ),
    );
    let users = list(v, "seat_users");
    if !users.is_empty() {
        put("seat", format!("{} logged in", users.join(", ")));
    }
    for u in v
        .get("units")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let user = if u.get("user").and_then(Value::as_bool) == Some(true) {
            " (user)"
        } else {
            ""
        };
        put("unit", format!("{}{user} {}", s(u, "name"), s(u, "active")));
    }
    for w in list(v, "wants") {
        put("wants", w);
    }
    for k in list(v, "keeps") {
        put("keeps", k);
    }
    if let Some(e) = v.get("offload_impossible").and_then(Value::as_str) {
        put("offload", format!("not possible here: {e}"));
    }
    if let Some(e) = v.get("last_error").and_then(Value::as_str) {
        put("error", e.to_owned());
    }
    if let Some(r) = v.get("last_release").filter(|r| r.is_object()) {
        let n = |k: &str| r.get(k).and_then(Value::as_u64);
        let vram = match (n("vram_before_mib"), n("vram_after_mib")) {
            (Some(b), Some(a)) => format!(", VRAM {b} -> {a} MiB"),
            _ => String::new(),
        };
        let desk =
            n("desktop_vram_mib").map_or(String::new(), |m| format!(", desktop held {m} MiB"));
        put(
            "released",
            format!(
                "{} in {:.1}s{vram}{desk}",
                s(r, "how"),
                r.get("took_s").and_then(Value::as_f64).unwrap_or(0.0)
            ),
        );
    }
    if let Some(c) = v.get("last_check").filter(|c| c.is_object()) {
        put("journal", super::status::check_line(c));
        for l in list(c, "errors")
            .iter()
            .chain(&list(c, "coredumps"))
            .take(10)
        {
            put("", l.clone());
        }
    }
    for h in v
        .get("holders")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let mib = h
            .get("vram_mib")
            .and_then(Value::as_u64)
            .map_or("? MiB".to_owned(), |m| format!("{m} MiB"));
        let node = if h.get("primary").and_then(Value::as_bool) == Some(true) {
            "display"
        } else {
            "render"
        };
        put(
            "process",
            format!(
                "{:<8} {} {:>7} {:<8} {}",
                h.get("pid").and_then(Value::as_u64).unwrap_or(0),
                node,
                mib,
                s(h, "role"),
                s(h, "cmdline").chars().take(70).collect::<String>()
            ),
        );
    }
    if let Some(n) = v
        .get("unreadable_processes")
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
    {
        put(
            "",
            format!("{n} processes of other users not readable (run gpuq as root to see them)"),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_state_processes_and_journal() {
        let v = json!({
            "policy": "stop", "card": "gpu0", "seat": "seat0", "state": "released",
            "how": "stopped", "since_s": 125, "why": "kept released: model big loaded",
            "wants": ["model big loaded"], "keeps": [], "seat_users": [],
            "units": [{"name": "display-manager.service", "user": false, "active": "inactive"}],
            "last_release": {"how": "stopped", "took_s": 3.2, "vram_before_mib": 23935,
                              "vram_after_mib": 23906, "desktop_vram_mib": 30},
            "last_check": {"after": "release", "clean": false, "errors": ["kernel: boom"],
                           "coredumps": [], "new_failed_units": []},
            "holders": [{"pid": 1140, "comm": "kwin_wayland", "primary": false, "vram_mib": 9,
                         "cmdline": "/usr/bin/kwin_wayland --virtual", "role": "ignored"}],
            "unreadable_processes": 0
        });
        let t = render(&v);
        assert!(
            t.contains("desktop    released (stopped) 2m5s: kept released"),
            "{t}"
        );
        assert!(
            t.contains("unit       display-manager.service inactive"),
            "{t}"
        );
        assert!(
            t.contains("VRAM 23935 -> 23906 MiB, desktop held 30 MiB"),
            "{t}"
        );
        assert!(t.contains("journal    1 error"), "{t}");
        assert!(t.contains("kernel: boom"), "{t}");
        assert!(t.contains("ignored"), "{t}");
    }
}
