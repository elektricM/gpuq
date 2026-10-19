// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! Everything the desktop controller asks of the system, behind [`System`] so the tests can
//! replace it. The real one runs `systemctl`, `loginctl` and `busctl`, each with a timeout,
//! and reads `/proc`.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::process::Command;

use super::procs::{self, Nodes, Scan};
use crate::probe::VramProbe;
use crate::Mib;

/// Which systemd manager a unit lives in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    System,
    User,
}

/// What systemd says about one unit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnitInfo {
    /// The name as configured (an alias such as `display-manager.service` stays as given).
    pub name: String,
    /// `Id`: the real name behind an alias.
    pub id: String,
    /// `loaded`, `not-found`, ...
    pub load: String,
    /// `active`, `inactive`, `failed`, `activating`, `deactivating`, `reloading`, ...
    pub active: String,
    /// cgroup path while it runs, empty otherwise.
    pub cgroup: String,
}

impl UnitInfo {
    pub fn is_down(&self) -> bool {
        matches!(self.active.as_str(), "inactive" | "failed" | "")
    }

    pub fn is_up(&self) -> bool {
        matches!(self.active.as_str(), "active" | "reloading" | "refreshing")
    }
}

/// One logind session on the seat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub user: String,
    /// `user`, `greeter`, `lock-screen`, ...
    pub class: String,
    pub remote: bool,
}

impl Session {
    /// A person logged in at the seat (a greeter or lock screen alone is not one).
    pub fn is_user(&self) -> bool {
        self.class.starts_with("user") && !self.remote
    }
}

/// What the journal shows since a moment: new trouble after a change.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Health {
    /// Messages at priority `err` or worse, system and user journal.
    pub errors: Vec<String>,
    /// Units in the `failed` state (system, and the user's with a `user:` prefix).
    pub failed_units: Vec<String>,
    /// Core dumps recorded.
    pub coredumps: Vec<String>,
    /// What could not be checked.
    pub unchecked: Vec<String>,
}

#[async_trait]
pub trait System: Send + Sync + 'static {
    async fn units(&self, scope: Scope, units: &[String]) -> Result<Vec<UnitInfo>, String>;
    async fn stop(&self, scope: Scope, units: &[String]) -> Result<(), String>;
    async fn start(&self, scope: Scope, units: &[String]) -> Result<(), String>;
    async fn restart(&self, scope: Scope, units: &[String]) -> Result<(), String>;
    /// Set (non-empty value) or remove (empty value) variables in a systemd manager.
    async fn manager_env(&self, scope: Scope, vars: &[(String, String)]) -> Result<(), String>;
    /// Run a configured command (argv, no shell).
    async fn command(&self, argv: &[String]) -> Result<(), String>;
    /// Connected display connectors per card PCI address, e.g. `0000:15:00.0 -> [DP-3]`.
    fn monitors(&self) -> std::collections::BTreeMap<String, Vec<String>>;
    /// Errors, failed units and core dumps since `since` (Unix seconds).
    async fn health(&self, since: u64) -> Health;
    async fn sessions(&self, seat: &str) -> Result<Vec<Session>, String>;
    /// Set (non-empty value) or remove (empty value) variables in the user manager and the
    /// D-Bus activation environment, then clear failed units there so they can start again.
    async fn session_env(&self, vars: &[(String, String)]) -> Result<(), String>;
    /// Whether a socket of this name exists in the user's runtime directory.
    fn runtime_socket(&self, name: &str) -> bool;
    async fn scan(&self) -> Scan;
    async fn vram_used(&self) -> Option<Mib>;
}

/// The real system.
pub struct Real {
    nodes: Nodes,
    probe: Option<Arc<dyn VramProbe>>,
    /// `["--user"]`, plus `--machine=<user>@.host` for another user.
    user_args: Vec<String>,
    runtime_dir: Option<PathBuf>,
    elevate: Vec<String>,
    command_timeout: Duration,
    stop_timeout: Duration,
}

impl Real {
    /// `user`: whose user manager to talk to; empty or the current user means our own.
    pub fn new(
        nodes: Nodes,
        probe: Option<Arc<dyn VramProbe>>,
        user: &str,
        elevate: Vec<String>,
        command_timeout: Duration,
        stop_timeout: Duration,
    ) -> Real {
        let me = current_user();
        let other = !user.is_empty() && me.as_deref() != Some(user);
        let mut user_args = vec!["--user".to_owned()];
        if other {
            user_args.push(format!("--machine={user}@.host"));
        }
        let runtime_dir = if other {
            uid_of(user).map(|u| PathBuf::from(format!("/run/user/{u}")))
        } else {
            std::env::var_os("XDG_RUNTIME_DIR")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .or_else(|| {
                    Some(PathBuf::from(format!(
                        "/run/user/{}",
                        nix::unistd::getuid()
                    )))
                })
        };
        Real {
            nodes,
            probe,
            user_args,
            runtime_dir,
            elevate,
            command_timeout,
            stop_timeout,
        }
    }

    fn systemctl(&self, scope: Scope, change: bool) -> Vec<String> {
        let mut v = Vec::new();
        if change && scope == Scope::System {
            v.extend(self.elevate.iter().cloned());
        }
        v.push("systemctl".into());
        if scope == Scope::User {
            v.extend(self.user_args.iter().cloned());
        }
        v
    }

    async fn change(&self, scope: Scope, verb: &str, units: &[String]) -> Result<(), String> {
        let mut argv = self.systemctl(scope, true);
        argv.push(verb.into());
        argv.extend(units.iter().cloned());
        run(&argv, self.stop_timeout).await.map(|_| ())
    }
}

fn current_user() -> Option<String> {
    nix::unistd::User::from_uid(nix::unistd::getuid())
        .ok()
        .flatten()
        .map(|u| u.name)
}

fn uid_of(user: &str) -> Option<u32> {
    nix::unistd::User::from_name(user)
        .ok()
        .flatten()
        .map(|u| u.uid.as_raw())
}

#[async_trait]
impl System for Real {
    async fn units(&self, scope: Scope, units: &[String]) -> Result<Vec<UnitInfo>, String> {
        if units.is_empty() {
            return Ok(Vec::new());
        }
        let mut argv = self.systemctl(scope, false);
        argv.extend(["show", "--property=Id,LoadState,ActiveState,ControlGroup"].map(String::from));
        argv.extend(units.iter().cloned());
        let out = run(&argv, self.command_timeout).await?;
        parse_show(&out, units)
    }

    async fn stop(&self, scope: Scope, units: &[String]) -> Result<(), String> {
        self.change(scope, "stop", units).await
    }

    async fn start(&self, scope: Scope, units: &[String]) -> Result<(), String> {
        self.change(scope, "start", units).await
    }

    async fn restart(&self, scope: Scope, units: &[String]) -> Result<(), String> {
        self.change(scope, "restart", units).await
    }

    async fn manager_env(&self, scope: Scope, vars: &[(String, String)]) -> Result<(), String> {
        let (set, unset) = split_env(vars);
        for (verb, list) in [("set-environment", set), ("unset-environment", unset)] {
            if list.is_empty() {
                continue;
            }
            let mut argv = self.systemctl(scope, true);
            argv.push(verb.into());
            argv.extend(list);
            run(&argv, self.command_timeout).await?;
        }
        Ok(())
    }

    async fn command(&self, argv: &[String]) -> Result<(), String> {
        run(argv, self.stop_timeout).await.map(|_| ())
    }

    fn monitors(&self) -> std::collections::BTreeMap<String, Vec<String>> {
        monitors_in(std::path::Path::new("/sys/class/drm"))
    }

    async fn health(&self, since: u64) -> Health {
        let t = self.command_timeout;
        let mut h = Health::default();
        let at = format!("--since=@{since}");
        let elevated = |argv: &[&str]| {
            let mut v = self.elevate.clone();
            v.extend(argv.iter().map(|a| (*a).to_owned()));
            v
        };
        let user = |argv: &[&str]| {
            let mut v = vec![argv[0].to_owned()];
            v.extend(self.user_args.iter().cloned());
            v.extend(argv[1..].iter().map(|a| (*a).to_owned()));
            v
        };
        let journal = [
            "journalctl",
            &at,
            "--priority=err",
            "--no-pager",
            "--quiet",
            "--output=short",
        ];
        for (label, argv) in [("system", elevated(&journal)), ("user", user(&journal))] {
            match run(&argv, t).await {
                Ok(out) => h
                    .errors
                    .extend(out.lines().filter(|l| !l.trim().is_empty()).map(|l| {
                        if label == "user" {
                            format!("user: {l}")
                        } else {
                            l.to_owned()
                        }
                    })),
                Err(e) => h.unchecked.push(format!("{label} journal: {e}")),
            }
        }
        let failed = ["systemctl", "--failed", "--no-legend", "--plain", "--full"];
        for (prefix, argv) in [
            ("", failed.map(String::from).to_vec()),
            ("user:", user(&failed)),
        ] {
            match run(&argv, t).await {
                Ok(out) => h.failed_units.extend(
                    out.lines()
                        .filter_map(|l| l.split_whitespace().next())
                        .map(|u| format!("{prefix}{u}")),
                ),
                Err(e) => h.unchecked.push(format!("failed units: {e}")),
            }
        }
        let dumps = elevated(&["coredumpctl", "list", &at, "--no-legend", "--no-pager"]);
        match run(&dumps, t).await {
            Ok(out) => h.coredumps.extend(
                out.lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(str::to_owned),
            ),
            // coredumpctl exits 1 when it finds nothing.
            Err(e) if e.contains("No coredumps") || e.contains("exit status: 1):") => {}
            Err(e) => h.unchecked.push(format!("core dumps: {e}")),
        }
        h
    }

    async fn sessions(&self, seat: &str) -> Result<Vec<Session>, String> {
        let t = self.command_timeout;
        let argv = [
            "loginctl",
            "show-seat",
            seat,
            "--property=Sessions",
            "--value",
        ]
        .map(String::from);
        let ids = run(&argv, t).await?;
        let mut out = Vec::new();
        for id in ids.split_whitespace() {
            let argv = [
                "loginctl",
                "show-session",
                id,
                "--property=Name,Class,Remote",
            ]
            .map(String::from);
            match run(&argv, t).await {
                Ok(text) => out.push(parse_session(id, &text)),
                // A session that ended between the two calls.
                Err(e) if e.contains("No session") => {}
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    async fn session_env(&self, vars: &[(String, String)]) -> Result<(), String> {
        let t = self.command_timeout;
        let mut errors = Vec::new();
        let base = self.systemctl(Scope::User, false);
        let (set, unset) = split_env(vars);
        for (verb, list) in [("set-environment", set), ("unset-environment", unset)] {
            if list.is_empty() {
                continue;
            }
            let mut argv = base.clone();
            argv.push(verb.into());
            argv.extend(list);
            if let Err(e) = run(&argv, t).await {
                errors.push(e);
            }
        }
        // D-Bus activation cannot remove a variable; an empty value is the closest.
        let mut argv = vec!["busctl".to_owned()];
        argv.extend(self.user_args.iter().cloned());
        argv.extend(
            [
                "call",
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "UpdateActivationEnvironment",
                "a{ss}",
            ]
            .map(String::from),
        );
        argv.push(vars.len().to_string());
        for (k, v) in vars {
            argv.push(k.clone());
            argv.push(v.clone());
        }
        if let Err(e) = run(&argv, t).await {
            errors.push(e);
        }
        let mut argv = base;
        argv.push("reset-failed".into());
        if let Err(e) = run(&argv, t).await {
            errors.push(e);
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    fn runtime_socket(&self, name: &str) -> bool {
        use std::os::unix::fs::FileTypeExt;
        let Some(dir) = &self.runtime_dir else {
            return false;
        };
        if name.contains('/') {
            return false;
        }
        std::fs::metadata(dir.join(name)).is_ok_and(|m| m.file_type().is_socket())
    }

    async fn scan(&self) -> Scan {
        let nodes = self.nodes.clone();
        tokio::task::spawn_blocking(move || procs::scan(std::path::Path::new("/proc"), &nodes))
            .await
            .unwrap_or_default()
    }

    async fn vram_used(&self) -> Option<Mib> {
        let probe = self.probe.as_ref()?;
        match tokio::time::timeout(Duration::from_secs(5), probe.read()).await {
            Ok(Ok(r)) => Some(r.used_mib),
            _ => None,
        }
    }
}

/// `K=V` for the variables to set, names for the ones to remove (empty value).
fn split_env(vars: &[(String, String)]) -> (Vec<String>, Vec<String>) {
    let set = vars
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    let unset = vars
        .iter()
        .filter(|(_, v)| v.is_empty())
        .map(|(k, _)| k.clone())
        .collect();
    (set, unset)
}

/// Connected connectors under a `/sys/class/drm`-like directory, by the card's PCI address.
pub fn monitors_in(drm: &std::path::Path) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut out: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    let Ok(entries) = std::fs::read_dir(drm) else {
        return out;
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    for name in names {
        // card1-DP-2: card1 is the card, DP-2 the connector.
        let Some((card, conn)) = name.split_once('-') else {
            continue;
        };
        if !card.starts_with("card") {
            continue;
        }
        let status = std::fs::read_to_string(drm.join(&name).join("status")).unwrap_or_default();
        if status.trim() != "connected" {
            continue;
        }
        let pci = std::fs::canonicalize(drm.join(card).join("device"))
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| card.to_owned());
        out.entry(pci).or_default().push(conn.to_owned());
    }
    out
}

/// `systemctl show` output for several units: one block each, in the order asked.
pub fn parse_show(out: &str, units: &[String]) -> Result<Vec<UnitInfo>, String> {
    let blocks: Vec<&str> = out
        .split("\n\n")
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .collect();
    if blocks.len() != units.len() {
        return Err(format!(
            "systemctl show: {} blocks for {} units",
            blocks.len(),
            units.len()
        ));
    }
    Ok(blocks
        .iter()
        .zip(units)
        .map(|(b, name)| {
            let mut u = UnitInfo {
                name: name.clone(),
                ..UnitInfo::default()
            };
            for line in b.lines() {
                let Some((k, v)) = line.split_once('=') else {
                    continue;
                };
                let v = v.trim().to_owned();
                match k {
                    "Id" => u.id = v,
                    "LoadState" => u.load = v,
                    "ActiveState" => u.active = v,
                    "ControlGroup" => u.cgroup = v,
                    _ => {}
                }
            }
            u
        })
        .collect())
}

pub fn parse_session(id: &str, text: &str) -> Session {
    let mut s = Session {
        id: id.to_owned(),
        user: String::new(),
        class: String::new(),
        remote: false,
    };
    for line in text.lines() {
        match line.split_once('=') {
            Some(("Name", v)) => s.user = v.to_owned(),
            Some(("Class", v)) => s.class = v.to_owned(),
            Some(("Remote", v)) => s.remote = v == "yes",
            _ => {}
        }
    }
    s
}

/// Run `argv` (no shell) within `timeout`; stdout on success. The error names the command
/// and carries the first line of its stderr.
pub async fn run(argv: &[String], timeout: Duration) -> Result<String, String> {
    let (bin, args) = argv.split_first().ok_or("empty command")?;
    let line = argv.join(" ");
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = async {
        crate::workload::spawn(&mut cmd)
            .await?
            .wait_with_output()
            .await
    };
    let out = match tokio::time::timeout(timeout, child).await {
        Err(_) => {
            return Err(format!(
                "`{line}` timed out after {}s",
                timeout.as_secs_f32()
            ))
        }
        Ok(Err(e)) => return Err(format!("`{line}`: {e}")),
        Ok(Ok(o)) => o,
    };
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let first = err
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    Err(format!("`{line}` failed ({}): {first}", out.status))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_systemctl_show() {
        let out = "Id=sddm.service\nLoadState=loaded\nActiveState=active\n\
ControlGroup=/system.slice/sddm.service\n\nId=plasma-kwin_wayland.service\nLoadState=loaded\n\
ActiveState=inactive\nControlGroup=\n";
        let units = vec![
            "display-manager.service".to_owned(),
            "plasma-kwin_wayland.service".to_owned(),
        ];
        let u = parse_show(out, &units).unwrap();
        assert_eq!(u[0].name, "display-manager.service");
        assert_eq!(u[0].id, "sddm.service");
        assert!(u[0].is_up());
        assert_eq!(u[0].cgroup, "/system.slice/sddm.service");
        assert!(u[1].is_down());
        assert!(parse_show("Id=a\n", &units).is_err());
    }

    #[test]
    fn greeters_are_not_users() {
        let s = parse_session("c1", "Name=sddm\nClass=greeter\nRemote=no\n");
        assert!(!s.is_user());
        let s = parse_session("2", "Name=martin\nClass=user\nRemote=no\n");
        assert!(s.is_user());
        assert_eq!(s.user, "martin");
        let s = parse_session("9", "Name=martin\nClass=user\nRemote=yes\n");
        assert!(!s.is_user());
    }

    #[test]
    fn finds_connected_monitors_per_card() {
        let d = tempfile::tempdir().unwrap();
        let drm = d.path().join("drm");
        let dev = d.path().join("devices");
        for (card, pci) in [("card1", "0000:03:00.0"), ("card2", "0000:15:00.0")] {
            std::fs::create_dir_all(dev.join(pci)).unwrap();
            std::fs::create_dir_all(drm.join(card)).unwrap();
            std::os::unix::fs::symlink(dev.join(pci), drm.join(card).join("device")).unwrap();
        }
        for (conn, status) in [
            ("card1-DP-1", "disconnected"),
            ("card1-HDMI-A-1", "disconnected"),
            ("card2-DP-3", "connected"),
            ("card2-Writeback-2", "unknown"),
        ] {
            std::fs::create_dir_all(drm.join(conn)).unwrap();
            std::fs::write(drm.join(conn).join("status"), format!("{status}\n")).unwrap();
        }
        let m = monitors_in(&drm);
        assert_eq!(m.len(), 1);
        assert_eq!(m["0000:15:00.0"], vec!["DP-3".to_owned()]);
    }

    #[tokio::test]
    async fn commands_time_out() {
        let argv = ["sleep", "5"].map(String::from);
        let e = run(&argv, Duration::from_millis(100)).await.unwrap_err();
        assert!(e.contains("timed out"), "{e}");
        let argv = ["sh", "-c", "echo boom >&2; exit 3"].map(String::from);
        let e = run(&argv, Duration::from_secs(5)).await.unwrap_err();
        assert!(e.contains("boom"), "{e}");
        let argv = ["echo", "hi"].map(String::from);
        assert_eq!(run(&argv, Duration::from_secs(5)).await.unwrap(), "hi\n");
    }
}
