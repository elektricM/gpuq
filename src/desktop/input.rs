// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! `wake_on_input`: notice key presses at the seat. One thread per input device reads its
//! events and keeps the time of the last key press; the keys themselves are dropped. Devices
//! are looked for again every 30 s, so a keyboard plugged in later is picked up.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const RESCAN: Duration = Duration::from_secs(30);
const EV_KEY: u16 = 1;

#[derive(Clone, Default)]
pub struct InputWatch {
    last: Arc<Mutex<Option<Instant>>>,
}

impl InputWatch {
    /// Start watching the devices matching `patterns` (`*` in the file name only).
    pub fn start(patterns: Vec<String>) -> InputWatch {
        let w = InputWatch::default();
        let last = w.last.clone();
        let open: Arc<Mutex<HashSet<PathBuf>>> = Arc::default();
        let spawned = std::thread::Builder::new()
            .name("gpuq-input".into())
            .spawn(move || {
                let mut warned = HashSet::new();
                loop {
                    for dev in devices(&patterns) {
                        if !open
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .insert(dev.clone())
                        {
                            continue;
                        }
                        match std::fs::File::open(&dev) {
                            Ok(f) => {
                                tracing::info!(
                                    "desktop: watching {} for key presses",
                                    dev.display()
                                );
                                let (last, open) = (last.clone(), open.clone());
                                let _ = std::thread::Builder::new()
                                    .name("gpuq-input-dev".into())
                                    .spawn(move || {
                                        read_keys(f, &last);
                                        open.lock().unwrap_or_else(|p| p.into_inner()).remove(&dev);
                                    });
                            }
                            Err(e) => {
                                if warned.insert(dev.clone()) {
                                    tracing::warn!("desktop: cannot read {}: {e}", dev.display());
                                }
                                open.lock().unwrap_or_else(|p| p.into_inner()).remove(&dev);
                            }
                        }
                    }
                    std::thread::sleep(RESCAN);
                }
            });
        if let Err(e) = spawned {
            tracing::warn!("desktop: cannot watch input: {e}");
        }
        w
    }

    /// When a key was last pressed.
    pub fn last(&self) -> Option<Instant> {
        *self.last.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// `struct input_event`: a timeval, then type (u16), code (u16), value (i32).
const EVENT: usize = std::mem::size_of::<nix::libc::input_event>();

fn read_keys(mut f: std::fs::File, last: &Mutex<Option<Instant>>) {
    let mut buf = [0u8; EVENT * 32];
    loop {
        let n = match f.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        if buf[..n].chunks_exact(EVENT).any(is_key_press) {
            *last.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
        }
    }
}

/// A key (or button) going down.
pub fn is_key_press(ev: &[u8]) -> bool {
    let at = ev.len().saturating_sub(8);
    let Some(tail) = ev.get(at..) else {
        return false;
    };
    let kind = u16::from_ne_bytes([tail[0], tail[1]]);
    let value = i32::from_ne_bytes([tail[4], tail[5], tail[6], tail[7]]);
    kind == EV_KEY && value == 1
}

/// The real devices behind the patterns, each once.
pub fn devices(patterns: &[String]) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for p in patterns {
        let path = Path::new(p);
        let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str()))
        else {
            continue;
        };
        let candidates: Vec<PathBuf> = if name.contains('*') {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            let mut v: Vec<PathBuf> = entries
                .flatten()
                .filter(|e| e.file_name().to_str().is_some_and(|n| super::glob(name, n)))
                .map(|e| e.path())
                .collect();
            v.sort();
            v
        } else {
            vec![path.to_path_buf()]
        };
        for c in candidates {
            if let Ok(real) = std::fs::canonicalize(&c) {
                if seen.insert(real.clone()) {
                    out.push(real);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: u16, code: u16, value: i32) -> Vec<u8> {
        let mut v = vec![0u8; EVENT - 8];
        v.extend(kind.to_ne_bytes());
        v.extend(code.to_ne_bytes());
        v.extend(value.to_ne_bytes());
        v
    }

    #[test]
    fn only_key_downs_count() {
        assert!(is_key_press(&event(EV_KEY, 30, 1)));
        assert!(!is_key_press(&event(EV_KEY, 30, 0)), "release");
        assert!(!is_key_press(&event(EV_KEY, 30, 2)), "repeat");
        assert!(!is_key_press(&event(0, 0, 0)), "sync");
        assert!(!is_key_press(&event(4, 4, 1)), "scan code");
    }

    #[test]
    fn devices_are_found_once() {
        let d = tempfile::tempdir().unwrap();
        let ev = d.path().join("event3");
        std::fs::write(&ev, b"").unwrap();
        let by_path = d.path().join("by-path");
        let by_id = d.path().join("by-id");
        std::fs::create_dir_all(&by_path).unwrap();
        std::fs::create_dir_all(&by_id).unwrap();
        std::os::unix::fs::symlink(&ev, by_path.join("pci-0000:00:14.0-usb-0:1:1.0-event-kbd"))
            .unwrap();
        std::os::unix::fs::symlink(&ev, by_id.join("usb-Keyboard-event-kbd")).unwrap();
        std::os::unix::fs::symlink(&ev, by_id.join("usb-Mouse-event-mouse")).unwrap();
        let pats = vec![
            format!("{}/*-event-kbd", by_path.display()),
            format!("{}/*-event-kbd", by_id.display()),
        ];
        assert_eq!(devices(&pats), vec![std::fs::canonicalize(&ev).unwrap()]);
    }

    #[test]
    fn a_key_press_is_noticed() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("event0");
        let mut data = event(EV_KEY, 30, 0);
        data.extend(event(EV_KEY, 30, 1));
        std::fs::write(&path, data).unwrap();
        let last = Mutex::new(None);
        read_keys(std::fs::File::open(&path).unwrap(), &last);
        assert!(last.lock().unwrap().is_some());
    }
}
