// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! `gpud status`.
use std::process::ExitCode;

use serde_json::Value;

use super::client::{self, ClientError};
use super::{say, EX_UNAVAILABLE};

pub async fn status(addr: &str, json: bool) -> ExitCode {
    let st = match client::get_json(addr, "/status").await {
        Ok(st) => st,
        Err(ClientError::Down(_)) => {
            say(&format!("gpud is not running ({addr})"));
            return ExitCode::from(EX_UNAVAILABLE);
        }
        Err(e) => {
            say(&e.to_string());
            return ExitCode::FAILURE;
        }
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&st).unwrap_or_default());
    } else {
        print!("{}", render(&st));
    }
    ExitCode::SUCCESS
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("?")
}

fn n(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// The text form of a status object.
fn render(st: &Value) -> String {
    let mut out = String::new();
    let mut put = |label: &str, text: String| {
        out.push_str(&format!("{label:<10} {text}\n"));
    };
    put(
        "state",
        format!(
            "{}   in-flight LLM {}   LLM idle {}s",
            s(st, "state"),
            n(st, "inflight_llm"),
            n(st, "llm_idle_s")
        ),
    );
    if let Some(dev) = st.get("device").and_then(Value::as_str) {
        put("device", dev.to_owned());
    }
    if let Some(v) = st.get("vram").filter(|v| v.is_object()) {
        let gtt = match v.get("gtt_mib").and_then(Value::as_u64) {
            Some(g) if g > 0 => format!("   gtt {g} MiB"),
            _ => String::new(),
        };
        put(
            "vram",
            format!("{}/{} MiB{gtt}", n(v, "used_mib"), n(v, "total_mib")),
        );
    }
    if let Some(e) = st.get("vram_error").and_then(Value::as_str) {
        put("vram", format!("unreadable: {e}"));
    }
    match st.get("backend") {
        Some(Value::String(e)) => put("backend", e.clone()),
        Some(b) => {
            if let Some(models) = b.get("models").and_then(Value::as_array) {
                let list: Vec<String> = models.iter().map(model_line).collect();
                let text = if list.is_empty() {
                    "empty".to_owned()
                } else {
                    list.join(", ")
                };
                put("backend", text);
            }
            if let Some(e) = b.get("error").and_then(Value::as_str) {
                put("backend", e.to_owned());
            }
        }
        None => {}
    }
    for l in arr(st, "leases") {
        let y = if l.get("yield_requested").and_then(Value::as_bool) == Some(true) {
            " YIELD REQUESTED"
        } else {
            ""
        };
        let pid = l
            .get("pid")
            .and_then(Value::as_u64)
            .map_or("-".to_owned(), |p| p.to_string());
        put(
            "lease",
            format!(
                "{} {}/{} vram={} {}s pid={pid}{y}",
                s(l, "id"),
                s(l, "class"),
                s(l, "name"),
                s(l, "vram"),
                n(l, "age_s")
            ),
        );
    }
    for w in arr(st, "queue") {
        let vram = w
            .get("vram")
            .and_then(Value::as_str)
            .map_or(String::new(), |v| format!("vram={v} "));
        put(
            "queued",
            format!(
                "{} {}/{} {vram}{}s  {}",
                s(w, "kind"),
                s(w, "class"),
                s(w, "name"),
                n(w, "wait_s"),
                s(w, "why")
            ),
        );
    }
    let history = arr(st, "history");
    if !history.is_empty() {
        out.push_str("recent\n");
        for h in &history[history.len().saturating_sub(8)..] {
            let t = h
                .get("t")
                .and_then(Value::as_u64)
                .map_or_else(String::new, clock);
            out.push_str(&format!("  {t} {}\n", s(h, "msg")));
        }
    }
    out
}

/// Local wall-clock time of a Unix timestamp, as HH:MM:SS.
fn clock(t: u64) -> String {
    #[cfg(unix)]
    {
        use nix::libc;
        let secs = t as libc::time_t;
        // SAFETY: localtime_r only writes into the struct we pass.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        if !unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
            return format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec);
        }
    }
    let d = t % 86_400;
    format!("{:02}:{:02}:{:02}", d / 3600, d / 60 % 60, d % 60)
}

fn arr<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// `model (state)` for one entry of the backend's model list.
fn model_line(m: &Value) -> String {
    match m {
        Value::String(name) => name.clone(),
        _ => {
            let name = ["model", "name", "id"]
                .iter()
                .find_map(|k| m.get(*k).and_then(Value::as_str))
                .unwrap_or("?");
            match m.get("state").and_then(Value::as_str) {
                Some(state) => format!("{name} ({state})"),
                None => name.to_owned(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_models_and_vram() {
        let st = json!({
            "state": "llm", "inflight_llm": 1, "llm_idle_s": 3, "device": "fake:0",
            "vram": {"used_mib": 100, "total_mib": 200, "gtt_mib": 0},
            "backend": {"models": [{"model": "big", "state": "ready"}]},
            "leases": [], "queue": [], "history": []
        });
        let t = render(&st);
        assert!(t.contains("big (ready)"), "{t}");
        assert!(t.contains("100/200 MiB\n"), "{t}");
        assert!(t.contains("device     fake:0"), "{t}");
    }

    #[test]
    fn history_shows_clock_times() {
        let st = json!({"state": "llm", "history": [{"t": 1_792_177_510u64, "msg": "grant x"}]});
        let t = render(&st);
        let line = t.lines().find(|l| l.contains("grant x")).unwrap();
        let clock = line.split_whitespace().next().unwrap();
        assert!(!t.contains("1792177510"), "{t}");
        assert_eq!(clock.len(), 8, "{line}");
        assert_eq!(clock.matches(':').count(), 2, "{line}");
    }

    #[test]
    fn renders_vram_error() {
        let st = json!({"state": "llm", "vram": null, "vram_error": "no card"});
        assert!(render(&st).contains("unreadable: no card"));
    }
}
