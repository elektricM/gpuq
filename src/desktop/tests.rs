// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! The controller against a fake system, in paused time.

use std::collections::BTreeMap;
use std::sync::Mutex;

use super::system::Session;
use super::*;
use crate::config::Offload as OffloadCfg;

const DM: &str = "display-manager.service";
const KWIN: &str = "plasma-kwin_wayland.service";
const DM_CG: &str = "/system.slice/sddm.service";
const KWIN_CG: &str =
    "/user.slice/user-1000.slice/user@1000.service/session.slice/plasma-kwin_wayland.service";

/// A desktop: a display manager (system unit) running Xorg on the card's display node, and a
/// compositor (user unit) on its render node. A headless compositor and an LLM server hold
/// the render node too and must never be touched.
#[derive(Default)]
struct World {
    units: BTreeMap<String, String>,
    users: Vec<String>,
    calls: Vec<String>,
    /// Stopping this unit fails.
    stop_fails: Option<String>,
    /// This desktop process does not go away when its unit stops.
    lingers: bool,
    /// After an offload restart, the display manager still opens this card.
    dm_sticks: bool,
    /// The offload environment is set in the system manager.
    env: BTreeMap<String, String>,
    monitors: BTreeMap<String, Vec<String>>,
    vram: Mib,
    errors: Vec<String>,
}

struct Fake(Mutex<World>);

impl Fake {
    fn new() -> Arc<Fake> {
        let mut w = World {
            vram: 20_000,
            ..World::default()
        };
        w.units.insert(DM.into(), "active".into());
        w.units.insert(KWIN.into(), "active".into());
        w.monitors
            .insert("0000:15:00.0".into(), vec!["DP-3".into()]);
        Arc::new(Fake(Mutex::new(w)))
    }

    fn w(&self) -> std::sync::MutexGuard<'_, World> {
        self.0.lock().unwrap()
    }

    fn calls(&self, prefix: &str) -> usize {
        self.w()
            .calls
            .iter()
            .filter(|c| c.starts_with(prefix))
            .count()
    }
}

fn holder(pid: u32, comm: &str, cmd: &str, cg: &str, primary: bool, mib: u64) -> Holder {
    Holder {
        pid,
        comm: comm.into(),
        cmdline: cmd.into(),
        cgroup: cg.into(),
        primary,
        vram_mib: Some(mib),
    }
}

#[async_trait]
impl System for Fake {
    async fn units(&self, _scope: Scope, units: &[String]) -> Result<Vec<UnitInfo>, String> {
        let w = self.w();
        Ok(units
            .iter()
            .map(|u| {
                let active = w.units.get(u).cloned().unwrap_or_else(|| "inactive".into());
                let cgroup = match (u.as_str(), active.as_str()) {
                    (DM, "active") => DM_CG.into(),
                    (KWIN, "active") => KWIN_CG.into(),
                    _ => String::new(),
                };
                UnitInfo {
                    name: u.clone(),
                    id: u.clone(),
                    load: "loaded".into(),
                    active,
                    cgroup,
                }
            })
            .collect())
    }

    async fn stop(&self, scope: Scope, units: &[String]) -> Result<(), String> {
        let mut w = self.w();
        w.calls.push(format!("stop {scope:?} {}", units.join(" ")));
        if let Some(u) = w.stop_fails.clone().filter(|u| units.contains(u)) {
            return Err(format!("{u}: access denied"));
        }
        for u in units {
            w.units.insert(u.clone(), "inactive".into());
        }
        if units.iter().any(|u| u == DM) {
            w.vram -= 30;
        }
        Ok(())
    }

    async fn start(&self, scope: Scope, units: &[String]) -> Result<(), String> {
        let mut w = self.w();
        w.calls.push(format!("start {scope:?} {}", units.join(" ")));
        for u in units {
            w.units.insert(u.clone(), "active".into());
        }
        if units.iter().any(|u| u == DM) {
            // The display manager logs the user in again, which starts the compositor.
            w.units.insert(KWIN.into(), "active".into());
            w.vram += 30;
        }
        Ok(())
    }

    async fn restart(&self, scope: Scope, units: &[String]) -> Result<(), String> {
        self.w()
            .calls
            .push(format!("restart {scope:?} {}", units.join(" ")));
        self.start(scope, units).await
    }

    async fn manager_env(&self, scope: Scope, vars: &[(String, String)]) -> Result<(), String> {
        let mut w = self.w();
        for (k, v) in vars {
            w.calls.push(format!("env {scope:?} {k}={v}"));
            if v.is_empty() {
                w.env.remove(k);
            } else {
                w.env.insert(k.clone(), v.clone());
            }
        }
        Ok(())
    }

    async fn command(&self, argv: &[String]) -> Result<(), String> {
        self.w().calls.push(format!("command {}", argv.join(" ")));
        Ok(())
    }

    fn monitors(&self) -> BTreeMap<String, Vec<String>> {
        self.w().monitors.clone()
    }

    async fn health(&self, _since: u64) -> Health {
        Health {
            errors: self.w().errors.clone(),
            ..Health::default()
        }
    }

    async fn sessions(&self, _seat: &str) -> Result<Vec<Session>, String> {
        Ok(self
            .w()
            .users
            .iter()
            .map(|u| Session {
                id: "2".into(),
                user: u.clone(),
                class: "user".into(),
                remote: false,
            })
            .collect())
    }

    async fn session_env(&self, vars: &[(String, String)]) -> Result<(), String> {
        let shown: Vec<String> = vars.iter().map(|(k, v)| format!("{k}={v}")).collect();
        self.w()
            .calls
            .push(format!("session_env {}", shown.join(" ")));
        Ok(())
    }

    fn runtime_socket(&self, name: &str) -> bool {
        name == "wayland-chell"
    }

    async fn scan(&self) -> Scan {
        let w = self.w();
        let moved = w.env.contains_key("KWIN_DRM_DEVICES");
        let mut holders = vec![
            holder(
                1140,
                "kwin_wayland",
                "/usr/bin/kwin_wayland --virtual --socket wayland-chell",
                "/user.slice/app.slice/chell-compositor.service",
                false,
                8,
            ),
            holder(
                580,
                "llama-server",
                "llama-server -m big.gguf",
                "/system.slice/llama-swap.service",
                false,
                19_000,
            ),
        ];
        if w.units[DM] == "active" && (!moved || w.dm_sticks) {
            holders.push(holder(700, "Xorg", "/usr/bin/Xorg vt2", DM_CG, true, 21));
        }
        if (w.units[KWIN] == "active" && !moved) || w.lingers {
            holders.push(holder(
                900,
                "kwin_wayland",
                "/usr/bin/kwin_wayland --xwayland",
                KWIN_CG,
                false,
                8,
            ));
        }
        Scan {
            holders,
            unreadable: 0,
        }
    }

    async fn vram_used(&self) -> Option<Mib> {
        Some(self.w().vram)
    }
}

/// What gpuq runs: a model, changeable by the test.
#[derive(Default)]
struct Work(Mutex<Load>);

#[async_trait]
impl Workload for Work {
    async fn load(&self, _card: &str) -> Load {
        self.0.lock().unwrap().clone()
    }
}

impl Work {
    fn models(&self, m: &[&str]) {
        self.0.lock().unwrap().models = m.iter().map(|s| (*s).to_owned()).collect();
    }
}

fn cfg(policy: Policy) -> Cfg {
    Cfg {
        policy,
        fallback: Policy::Stop,
        system_units: vec![DM.into()],
        user_units: vec![KWIN.into()],
        ignore: vec!["kwin_wayland --virtual".into()],
        models: vec!["big*".into()],
        release_delay: Duration::from_secs(30),
        restore_delay: Duration::from_secs(600),
        stop_timeout: Duration::from_secs(10),
        check_delay: Duration::from_secs(5),
        session_env: [("DISPLAY", ""), ("WAYLAND_DISPLAY", "wayland-chell")]
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .into(),
        offload: OffloadCfg {
            user_env: [("KWIN_DRM_DEVICES".to_owned(), "/dev/dri/card2".to_owned())].into(),
            system_env: [("KWIN_DRM_DEVICES".to_owned(), "/dev/dri/card2".to_owned())].into(),
            settle: Duration::from_secs(5),
            ..OffloadCfg::default()
        },
        state_file: Some(std::env::temp_dir().join(format!(
            "gpuq-desktop-test-{}-{}.json",
            std::process::id(),
            uuid::Uuid::new_v4()
        ))),
        ..Cfg::default()
    }
}

struct Rig {
    fake: Arc<Fake>,
    work: Arc<Work>,
    h: Handle,
    _stop: watch::Sender<bool>,
}

async fn rig(c: Cfg) -> Rig {
    rig_with(c, Fake::new()).await
}

async fn rig_with(c: Cfg, fake: Arc<Fake>) -> Rig {
    let work = Arc::new(Work::default());
    let (stop, rx) = watch::channel(false);
    let parts = Parts {
        card: "gpu0".into(),
        pci: Some("0000:03:00.0".into()),
        sys: fake.clone(),
        work: work.clone(),
        input: None,
        arbiter: None,
    };
    let h = spawn(c, parts, rx).await;
    Rig {
        fake,
        work,
        h,
        _stop: stop,
    }
}

/// Let `s` seconds of paused time pass.
async fn pass(s: u64) {
    for _ in 0..s {
        sleep(Duration::from_secs(1)).await;
    }
}

#[tokio::test(start_paused = true)]
async fn stops_the_desktop_once_and_leaves_the_rest_alone() {
    let r = rig(cfg(Policy::Stop)).await;
    assert_eq!(r.h.status().state, State::Active);
    r.work.models(&["big-model"]);
    pass(20).await;
    assert_eq!(r.h.status().state, State::Active, "release_delay not over");
    pass(20).await;
    let st = r.h.status();
    assert_eq!(st.state, State::Released, "{st:?}");
    assert_eq!(st.how, Some(How::Stopped));
    let calls = r.fake.w().calls.clone();
    let stop_user = calls
        .iter()
        .position(|c| c == &format!("stop User {KWIN}"))
        .unwrap();
    let stop_sys = calls
        .iter()
        .position(|c| c == &format!("stop System {DM}"))
        .unwrap();
    assert!(stop_user < stop_sys, "{calls:?}");
    assert!(
        calls.contains(&"session_env DISPLAY= WAYLAND_DISPLAY=wayland-chell".to_owned()),
        "{calls:?}"
    );
    let rep = st.last_release.unwrap();
    assert_eq!(rep.desktop_vram_mib, Some(29));
    assert_eq!(rep.desktop_processes, vec!["700 Xorg", "900 kwin_wayland"]);
    assert_eq!(
        (rep.vram_before_mib, rep.vram_after_mib),
        (Some(20_000), Some(19_970))
    );
    // An hour of polls with the model still loaded: nothing is stopped again.
    pass(3600).await;
    assert_eq!(r.fake.calls("stop"), 2, "{:?}", r.fake.w().calls);
    assert_eq!(r.fake.calls("session_env"), 1);
    // The headless compositor and the LLM server are still there.
    let pids: Vec<u32> = r.fake.scan().await.holders.iter().map(|h| h.pid).collect();
    assert_eq!(pids, vec![1140, 580]);
}

#[tokio::test(start_paused = true)]
async fn the_headless_compositor_does_not_count_as_the_desktop() {
    let r = rig(cfg(Policy::Stop)).await;
    let st = r.h.status_with_holders().await;
    let roles: Vec<(u32, &str)> = st
        .holders
        .unwrap()
        .iter()
        .map(|h| (h.holder.pid, h.role))
        .collect();
    assert_eq!(
        roles,
        vec![
            (1140, "ignored"),
            (580, "other"),
            (700, "desktop"),
            (900, "desktop")
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn restores_once_nothing_needs_the_vram() {
    let r = rig(cfg(Policy::Stop)).await;
    r.work.models(&["big-model"]);
    pass(40).await;
    assert_eq!(r.h.status().state, State::Released);
    r.work.models(&["small"]);
    pass(590).await;
    assert_eq!(r.h.status().state, State::Released);
    pass(20).await;
    let st = r.h.status();
    assert_eq!(st.state, State::Active, "{st:?}");
    assert_eq!(r.fake.calls(&format!("start System {DM}")), 1);
    // Back up and nothing loaded: stays up.
    pass(3600).await;
    assert_eq!(r.fake.calls("stop"), 2);
}

#[tokio::test(start_paused = true)]
async fn a_failed_stop_starts_the_display_manager_again() {
    let fake = Fake::new();
    fake.w().stop_fails = Some(DM.into());
    let r = rig_with(cfg(Policy::Stop), fake).await;
    r.work.models(&["big-model"]);
    pass(40).await;
    let st = r.h.status();
    assert_eq!(st.state, State::Active, "{st:?}");
    let err = st.last_error.unwrap();
    assert!(err.contains("access denied"), "{err}");
    assert!(st.retry_s.is_some());
    // The display manager was never down, so there is nothing to start: no rollback start.
    assert_eq!(r.fake.calls("start"), 1, "{:?}", r.fake.w().calls);
    // No new attempt before retry_s.
    pass(600).await;
    assert_eq!(r.fake.calls(&format!("stop System {DM}")), 1);
}

#[tokio::test(start_paused = true)]
async fn a_process_that_stays_rolls_the_release_back() {
    let fake = Fake::new();
    fake.w().lingers = true;
    let r = rig_with(cfg(Policy::Stop), fake).await;
    r.work.models(&["big-model"]);
    pass(60).await;
    let st = r.h.status();
    assert_eq!(st.state, State::Active, "{st:?}");
    let err = st.last_error.unwrap();
    assert!(err.contains("still on gpu0: 900 kwin_wayland"), "{err}");
    assert!(
        err.contains("started display-manager.service again"),
        "{err}"
    );
    assert_eq!(r.fake.w().units[DM], "active");
}

#[tokio::test(start_paused = true)]
async fn never_releases_under_a_logged_in_user() {
    let fake = Fake::new();
    fake.w().users = vec!["martin".into()];
    let r = rig_with(cfg(Policy::Stop), fake).await;
    r.work.models(&["big-model"]);
    pass(3600).await;
    let st = r.h.status();
    assert_eq!(st.state, State::Active);
    assert!(st.why.contains("martin logged in"), "{}", st.why);
    assert_eq!(r.fake.calls("stop"), 0);
    assert!(r.h.release(false).await.is_err());
    assert!(!r.h.run("gpu0").await, "the offload hook refuses too");
    assert!(r.h.release(true).await.is_ok(), "--force");
    assert_eq!(r.h.status().state, State::Released);
}

#[tokio::test(start_paused = true)]
async fn a_job_that_does_not_fit_releases_at_once() {
    let r = rig(cfg(Policy::Stop)).await;
    assert!(!r.h.run("gpu1").await, "another card");
    assert!(r.h.run("gpu0").await);
    assert_eq!(r.h.status().state, State::Released);
    assert!(r.h.run("gpu0").await, "already released");
    assert_eq!(r.fake.calls("stop"), 2);
}

#[tokio::test(start_paused = true)]
async fn offload_moves_the_desktop_and_back() {
    let r = rig(cfg(Policy::Offload)).await;
    r.work.models(&["big-model"]);
    pass(40).await;
    let st = r.h.status();
    assert_eq!(st.state, State::Released, "{st:?}");
    assert_eq!(
        st.how,
        Some(How::Moved {
            auto_restore: false
        })
    );
    assert_eq!(
        r.fake.w().units[DM],
        "active",
        "still up, on the other card"
    );
    assert_eq!(r.fake.calls(&format!("restart System {DM}")), 1);
    assert_eq!(
        r.fake.calls("env System KWIN_DRM_DEVICES=/dev/dri/card2"),
        1
    );
    assert_eq!(r.fake.calls("env User KWIN_DRM_DEVICES=/dev/dri/card2"), 1);
    // move_back is off: it stays moved, even with nothing loaded.
    r.work.models(&[]);
    pass(3600).await;
    assert_eq!(r.h.status().state, State::Released);
    assert_eq!(r.fake.calls("restart"), 1);
    let msg = r.h.restore(None).await.unwrap();
    assert!(msg.contains("moved the desktop back"), "{msg}");
    assert!(r.fake.w().env.is_empty());
    assert_eq!(r.fake.calls(&format!("restart System {DM}")), 2);
    assert_eq!(r.h.status().state, State::Active);
}

#[tokio::test(start_paused = true)]
async fn offload_with_monitors_on_the_card_falls_back_to_stop_and_says_so_once() {
    let fake = Fake::new();
    fake.w().monitors = [("0000:03:00.0".to_owned(), vec!["DP-1".to_owned()])].into();
    let r = rig_with(cfg(Policy::Offload), fake).await;
    r.work.models(&["big-model"]);
    pass(40).await;
    let st = r.h.status();
    assert_eq!(st.state, State::Released);
    assert_eq!(st.how, Some(How::Stopped));
    let why = st.offload_impossible.unwrap();
    assert!(
        why.contains("monitors are connected to this card (DP-1)"),
        "{why}"
    );
    assert_eq!(
        r.fake.calls("restart"),
        0,
        "nothing restarted for an impossible offload"
    );
    assert_eq!(r.fake.calls("env"), 0);
    // The next release goes straight to the fallback.
    r.h.restore(Some(Duration::ZERO)).await.unwrap();
    pass(40).await;
    assert_eq!(r.h.status().state, State::Released);
    assert_eq!(r.fake.calls("restart"), 0);
}

#[tokio::test(start_paused = true)]
async fn offload_that_leaves_something_on_the_card_falls_back() {
    let fake = Fake::new();
    fake.w().dm_sticks = true;
    let r = rig_with(cfg(Policy::Offload), fake).await;
    r.work.models(&["big-model"]);
    pass(60).await;
    let st = r.h.status();
    assert_eq!(st.state, State::Released, "{st:?}");
    assert_eq!(st.how, Some(How::Stopped));
    let why = st.offload_impossible.unwrap();
    assert!(
        why.contains("still on gpu0: Xorg (700, display node)"),
        "{why}"
    );
    assert!(
        r.fake.w().env.is_empty(),
        "the offload environment was removed"
    );
    assert_eq!(r.fake.w().units[DM], "inactive");
}

#[tokio::test(start_paused = true)]
async fn offload_with_fallback_off_leaves_the_desktop_running() {
    let fake = Fake::new();
    fake.w().monitors.clear();
    let mut c = cfg(Policy::Offload);
    c.fallback = Policy::Off;
    let r = rig_with(c, fake).await;
    r.work.models(&["big-model"]);
    pass(40).await;
    let st = r.h.status();
    assert_eq!(st.state, State::Active);
    assert!(st.offload_impossible.unwrap().contains("no other card"));
    assert_eq!(r.fake.calls("stop"), 0);
}

#[tokio::test(start_paused = true)]
async fn the_journal_is_checked_after_each_change() {
    let r = rig(cfg(Policy::Stop)).await;
    r.fake.w().errors = vec!["kernel: amdgpu: Failed to upload overdrive table!".into()];
    r.h.release(false).await.unwrap();
    pass(10).await;
    let c = r.h.status().last_check.unwrap();
    assert_eq!(c.after, "release");
    assert!(!c.clean);
    assert_eq!(c.health.errors.len(), 1);
    r.fake.w().errors.clear();
    r.h.restore(None).await.unwrap();
    pass(10).await;
    let c = r.h.status().last_check.unwrap();
    assert_eq!(c.after, "restore");
    assert!(c.clean, "{c:?}");
}

#[tokio::test(start_paused = true)]
async fn short_vram_keeps_it_released_while_the_model_stays() {
    let mut c = cfg(Policy::Stop);
    c.models.clear();
    c.min_free_mib = 1024;
    let r = rig(c).await;
    {
        let mut l = r.work.0.lock().unwrap();
        l.models = vec!["huge".into()];
        l.vram = Some(VramReading {
            used_mib: 23_900,
            total_mib: 24_560,
            gtt_used_mib: None,
        });
    }
    pass(40).await;
    assert_eq!(r.h.status().state, State::Released);
    // The desktop's VRAM is free now; the model is the same, so it stays released.
    r.work.0.lock().unwrap().vram = Some(VramReading {
        used_mib: 20_000,
        total_mib: 24_560,
        gtt_used_mib: None,
    });
    pass(1200).await;
    assert_eq!(r.h.status().state, State::Released);
    r.work.models(&[]);
    pass(620).await;
    assert_eq!(r.h.status().state, State::Active);
}

#[tokio::test(start_paused = true)]
async fn a_restart_remembers_who_stopped_the_desktop() {
    let c = cfg(Policy::Stop);
    let fake = Fake::new();
    let r = rig_with(c.clone(), fake.clone()).await;
    r.h.release(false).await.unwrap();
    drop(r);
    // Same state file, units down: gpuq stopped it, so it may start it again.
    let r = rig_with(c.clone(), fake.clone()).await;
    let st = r.h.status();
    assert_eq!(st.state, State::Released);
    assert!(st.why.contains("stopped by gpuq"), "{}", st.why);
    pass(620).await;
    assert_eq!(r.h.status().state, State::Active);
    // Stopped by someone else: left alone.
    fake.w().units.insert(DM.into(), "inactive".into());
    fake.w().units.insert(KWIN.into(), "inactive".into());
    let _ = std::fs::remove_file(c.state_file.as_ref().unwrap());
    let r = rig_with(c, fake).await;
    pass(3600).await;
    let st = r.h.status();
    assert_eq!(st.state, State::Released);
    assert!(st.why.contains("outside gpuq"), "{}", st.why);
}

#[test]
fn globs() {
    assert!(glob("qwen3.8-27b*", "qwen3.8-27b-openclaw"));
    assert!(glob("*", "x"));
    assert!(glob("a*c*e", "abcde"));
    assert!(!glob("a*c*e", "abde"));
    assert!(glob("exact", "exact"));
    assert!(!glob("exact", "exact2"));
    assert!(glob("*-event-kbd", "usb-Kbd-event-kbd"));
    assert!(!glob("ab*ba", "aba"));
}

#[test]
fn offload_needs_a_monitor_elsewhere_and_none_here() {
    let m = |v: &[(&str, &str)]| -> BTreeMap<String, Vec<String>> {
        v.iter()
            .map(|(p, c)| ((*p).to_owned(), vec![(*c).to_owned()]))
            .collect()
    };
    let here = "0000:03:00.0";
    assert_eq!(
        offload_impossible(&m(&[("0000:15:00.0", "DP-3")]), Some(here)),
        None
    );
    assert!(offload_impossible(&m(&[(here, "DP-1")]), Some(here))
        .unwrap()
        .contains("DP-1"));
    assert!(offload_impossible(&m(&[]), Some(here))
        .unwrap()
        .contains("no other card"));
}
