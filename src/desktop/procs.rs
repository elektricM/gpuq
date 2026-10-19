// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! Who holds a card: processes with one of its DRM nodes open, found through `/proc/*/fd`,
//! with the VRAM each one has resident from `/proc/*/fdinfo`.
//!
//! The nodes of a card come from its PCI address (`/sys/bus/pci/devices/<addr>/drm`), so a
//! card is never confused with another one. Only processes whose `fd` directory can be read
//! are seen: all of them as root, the own user's otherwise.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

/// The DRM device nodes of one card.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Nodes {
    /// Primary (KMS) nodes, e.g. `/dev/dri/card1`. Display servers open these.
    pub primary: Vec<PathBuf>,
    /// Render nodes, e.g. `/dev/dri/renderD128`. Compute and rendering use these.
    pub render: Vec<PathBuf>,
}

impl Nodes {
    /// The nodes listed under `<sysfs_pci>/<addr>/drm`, as `/dev/dri/<name>`.
    pub fn for_pci(sysfs_pci: &Path, addr: &str) -> Nodes {
        let mut n = Nodes::default();
        let Ok(entries) = fs::read_dir(sysfs_pci.join(addr).join("drm")) else {
            return n;
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        names.sort();
        for name in names {
            let dev = Path::new("/dev/dri").join(&name);
            if name.starts_with("renderD") {
                n.render.push(dev);
            } else if name.strip_prefix("card").is_some_and(is_digits) {
                n.primary.push(dev);
            }
        }
        n
    }

    pub fn is_empty(&self) -> bool {
        self.primary.is_empty() && self.render.is_empty()
    }
}

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// One process with the card open.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Holder {
    pub pid: u32,
    pub comm: String,
    /// Command line, spaces between the arguments, cut at 200 characters.
    pub cmdline: String,
    /// cgroup v2 path, e.g. `/user.slice/.../plasma-kwin_wayland.service`.
    pub cgroup: String,
    /// Has a primary (KMS) node open.
    pub primary: bool,
    /// VRAM resident for its DRM clients on this card, if the driver reports it.
    pub vram_mib: Option<u64>,
}

/// The result of one scan.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Scan {
    pub holders: Vec<Holder>,
    /// Processes whose open files could not be read (other users, without root).
    pub unreadable: usize,
}

/// Every process under `proc_root` with one of `nodes` open.
pub fn scan(proc_root: &Path, nodes: &Nodes) -> Scan {
    let mut out = Scan::default();
    if nodes.is_empty() {
        return out;
    }
    let Ok(entries) = fs::read_dir(proc_root) else {
        return out;
    };
    for e in entries.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        match holder(&e.path(), pid, nodes) {
            Ok(Some(h)) => out.holders.push(h),
            Ok(None) => {}
            Err(()) => out.unreadable += 1,
        }
    }
    out.holders.sort_by_key(|h| h.pid);
    out
}

/// `Err` when the fd directory cannot be read; a process that exits meanwhile is `Ok(None)`.
fn holder(dir: &Path, pid: u32, nodes: &Nodes) -> Result<Option<Holder>, ()> {
    let fds = match fs::read_dir(dir.join("fd")) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return Err(()),
        Err(_) => return Ok(None),
    };
    let mut primary = false;
    let mut matched: Vec<String> = Vec::new();
    for fd in fds.flatten() {
        let Ok(target) = fs::read_link(fd.path()) else {
            continue;
        };
        let is_primary = nodes.primary.contains(&target);
        if is_primary || nodes.render.contains(&target) {
            primary |= is_primary;
            matched.push(fd.file_name().to_string_lossy().into_owned());
        }
    }
    if matched.is_empty() {
        return Ok(None);
    }
    // One DRM client can sit behind several fds (dup, fork); count each client once.
    let mut clients: HashMap<String, u64> = HashMap::new();
    let mut anonymous: u64 = 0;
    let mut reported = false;
    for fd in &matched {
        let Ok(info) = fs::read_to_string(dir.join("fdinfo").join(fd)) else {
            continue;
        };
        let f = parse_fdinfo(&info);
        if let Some(kib) = f.vram_kib {
            reported = true;
            match f.client_id {
                Some(id) => {
                    clients.insert(id, kib);
                }
                None => anonymous += kib,
            }
        }
    }
    let read = |name: &str| fs::read(dir.join(name)).unwrap_or_default();
    let comm = String::from_utf8_lossy(&read("comm")).trim().to_owned();
    let cmdline = cmdline_text(&read("cmdline"));
    let cgroup = cgroup_path(&String::from_utf8_lossy(&read("cgroup")));
    let kib: u64 = clients.values().sum::<u64>() + anonymous;
    Ok(Some(Holder {
        pid,
        comm,
        cmdline,
        cgroup,
        primary,
        vram_mib: reported.then_some(kib.div_ceil(1024)),
    }))
}

/// What an fdinfo file says about one DRM client.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FdInfo {
    pub client_id: Option<String>,
    pub pdev: Option<String>,
    /// `drm-resident-vram`, else `drm-memory-vram` (older kernels), in KiB.
    pub vram_kib: Option<u64>,
}

pub fn parse_fdinfo(text: &str) -> FdInfo {
    let mut f = FdInfo::default();
    let mut memory = None;
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        match k.trim() {
            "drm-client-id" => f.client_id = Some(v.to_owned()),
            "drm-pdev" => f.pdev = Some(v.to_owned()),
            "drm-resident-vram" => f.vram_kib = parse_kib(v),
            "drm-memory-vram" => memory = parse_kib(v),
            _ => {}
        }
    }
    if f.vram_kib.is_none() {
        f.vram_kib = memory;
    }
    f
}

/// `8484 KiB`, `12 MiB` or a bare number of bytes, in KiB.
fn parse_kib(v: &str) -> Option<u64> {
    let mut parts = v.split_whitespace();
    let n: u64 = parts.next()?.parse().ok()?;
    Some(match parts.next() {
        Some("KiB") => n,
        Some("MiB") => n * 1024,
        Some("GiB") => n * 1024 * 1024,
        None => n / 1024,
        Some(_) => return None,
    })
}

fn cmdline_text(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let joined = text
        .split('\0')
        .filter(|a| !a.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    joined.chars().take(200).collect()
}

/// The cgroup v2 path (`0::/path`), or the first path for v1-only systems.
fn cgroup_path(text: &str) -> String {
    let mut first = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("0::") {
            return p.to_owned();
        }
        if first.is_none() {
            first = line.splitn(3, ':').nth(2).map(str::to_owned);
        }
    }
    first.unwrap_or_default()
}

/// Whether a process matches one of the `ignore` entries: its exact name, or a piece of its
/// command line.
pub fn ignored(h: &Holder, ignore: &[String]) -> bool {
    ignore
        .iter()
        .any(|i| h.comm == *i || (!i.is_empty() && h.cmdline.contains(i.as_str())))
}

/// Whether `cgroup` is `unit` or below it.
pub fn in_cgroup(cgroup: &str, unit: &str) -> bool {
    !unit.is_empty()
        && cgroup
            .strip_prefix(unit)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// The holders that belong to the desktop: not ignored, and either inside one of the units'
/// cgroups or holding a primary node (driving a display).
pub fn desktop_holders<'a>(
    scan: &'a Scan,
    unit_cgroups: &[String],
    ignore: &[String],
) -> Vec<&'a Holder> {
    let mine: HashSet<&str> = unit_cgroups
        .iter()
        .map(String::as_str)
        .filter(|c| !c.is_empty())
        .collect();
    scan.holders
        .iter()
        .filter(|h| !ignored(h, ignore))
        .filter(|h| h.primary || mine.iter().any(|u| in_cgroup(&h.cgroup, u)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    const KWIN_FDINFO: &str = "pos:\t0\nflags:\t02100002\ndrm-driver:\tamdgpu\n\
drm-client-id:\t18\ndrm-pdev:\t0000:03:00.0\ndrm-total-vram:\t8488 KiB\n\
drm-resident-vram:\t8484 KiB\ndrm-memory-vram:\t8484 KiB\n";

    #[test]
    fn reads_resident_vram() {
        let f = parse_fdinfo(KWIN_FDINFO);
        assert_eq!(f.client_id.as_deref(), Some("18"));
        assert_eq!(f.pdev.as_deref(), Some("0000:03:00.0"));
        assert_eq!(f.vram_kib, Some(8484));
        let old = parse_fdinfo("drm-client-id: 3\ndrm-memory-vram: 2 MiB\n");
        assert_eq!(old.vram_kib, Some(2048));
        assert_eq!(parse_fdinfo("pos: 0\n").vram_kib, None);
    }

    #[test]
    fn nodes_come_from_the_pci_address() {
        let d = tempfile::tempdir().unwrap();
        let drm = d.path().join("0000:03:00.0/drm");
        for n in ["card1", "renderD128", "controlD65"] {
            fs::create_dir_all(drm.join(n)).unwrap();
        }
        let n = Nodes::for_pci(d.path(), "0000:03:00.0");
        assert_eq!(n.primary, vec![PathBuf::from("/dev/dri/card1")]);
        assert_eq!(n.render, vec![PathBuf::from("/dev/dri/renderD128")]);
        assert!(Nodes::for_pci(d.path(), "0000:15:00.0").is_empty());
    }

    /// A fake process: fds pointing at `targets` (paths need not exist).
    fn proc_entry(root: &Path, pid: u32, comm: &str, cmd: &[&str], cg: &str, fds: &[(&str, &str)]) {
        let p = root.join(pid.to_string());
        fs::create_dir_all(p.join("fd")).unwrap();
        fs::create_dir_all(p.join("fdinfo")).unwrap();
        fs::write(p.join("comm"), format!("{comm}\n")).unwrap();
        fs::write(p.join("cmdline"), cmd.join("\0") + "\0").unwrap();
        fs::write(p.join("cgroup"), format!("0::{cg}\n")).unwrap();
        for (i, (target, info)) in fds.iter().enumerate() {
            let fd = (i + 3).to_string();
            symlink(target, p.join("fd").join(&fd)).unwrap();
            fs::write(p.join("fdinfo").join(&fd), info).unwrap();
        }
    }

    fn nodes() -> Nodes {
        Nodes {
            primary: vec!["/dev/dri/card1".into()],
            render: vec!["/dev/dri/renderD128".into()],
        }
    }

    const KWIN_UNIT: &str =
        "/user.slice/user-1000.slice/user@1000.service/session.slice/plasma-kwin_wayland.service";

    fn fake_proc() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        let info =
            |id: u32, kib: u32| format!("drm-client-id:\t{id}\ndrm-resident-vram:\t{kib} KiB\n");
        // The real compositor: primary and render node, two fds of one client.
        proc_entry(
            r,
            900,
            "kwin_wayland",
            &["/usr/bin/kwin_wayland", "--xwayland"],
            KWIN_UNIT,
            &[
                ("/dev/dri/card1", &info(1, 500_000)),
                ("/dev/dri/renderD128", &info(2, 100_000)),
                ("/dev/dri/renderD128", &info(2, 100_000)),
            ],
        );
        // The headless compositor of a browser robot: render node only, not a desktop unit.
        proc_entry(
            r,
            1140,
            "kwin_wayland",
            &[
                "/usr/bin/kwin_wayland",
                "--virtual",
                "--socket",
                "wayland-chell",
            ],
            "/user.slice/user-1000.slice/user@1000.service/app.slice/chell-compositor.service",
            &[("/dev/dri/renderD128", &info(18, 8484))],
        );
        // An LLM server on the render node.
        proc_entry(
            r,
            580,
            "llama-server",
            &["llama-server", "-m", "big.gguf"],
            "/system.slice/llama-swap.service",
            &[("/dev/dri/renderD128", &info(40, 20_000_000))],
        );
        // Something on another card.
        proc_entry(
            r,
            3697,
            "brave",
            &["/opt/brave-bin/brave"],
            "/user.slice/user-1000.slice/user@1000.service/app.slice/app-brave.scope",
            &[("/dev/dri/renderD129", &info(7, 1000))],
        );
        // Not a process.
        fs::create_dir_all(r.join("self-not-a-pid")).unwrap();
        d
    }

    #[test]
    fn finds_the_processes_on_the_card() {
        let d = fake_proc();
        let s = scan(d.path(), &nodes());
        let pids: Vec<u32> = s.holders.iter().map(|h| h.pid).collect();
        assert_eq!(pids, vec![580, 900, 1140]);
        let kwin = &s.holders[1];
        assert!(kwin.primary);
        // Client 2 is counted once although two fds point at it.
        assert_eq!(kwin.vram_mib, Some(600_000u64.div_ceil(1024)));
        assert_eq!(kwin.cmdline, "/usr/bin/kwin_wayland --xwayland");
        assert_eq!(kwin.cgroup, KWIN_UNIT);
        assert!(!s.holders[2].primary);
    }

    #[test]
    fn the_headless_compositor_is_not_the_desktop() {
        let d = fake_proc();
        let s = scan(d.path(), &nodes());
        let units = vec![KWIN_UNIT.to_owned()];
        let desk: Vec<u32> = desktop_holders(&s, &units, &[])
            .iter()
            .map(|h| h.pid)
            .collect();
        assert_eq!(desk, vec![900]);
        // Without the units' cgroups, the primary node alone marks the desktop.
        let none: Vec<u32> = desktop_holders(&s, &[], &[])
            .iter()
            .map(|h| h.pid)
            .collect();
        assert_eq!(none, vec![900], "the primary node alone marks the desktop");
        let ignore = vec!["kwin_wayland --xwayland".to_owned()];
        assert!(desktop_holders(&s, &units, &ignore).is_empty());
    }

    #[test]
    fn ignore_matches_a_name_or_part_of_the_command_line() {
        let h = Holder {
            pid: 1,
            comm: "kwin_wayland".into(),
            cmdline: "/usr/bin/kwin_wayland --virtual --socket wayland-chell".into(),
            cgroup: String::new(),
            primary: true,
            vram_mib: None,
        };
        assert!(ignored(&h, &["kwin_wayland --virtual".into()]));
        assert!(ignored(&h, &["kwin_wayland".into()]));
        assert!(!ignored(&h, &["Xwayland".into(), "--xwayland".into()]));
    }

    #[test]
    fn cgroup_prefixes_stop_at_a_slash() {
        assert!(in_cgroup("/a/sddm.service", "/a/sddm.service"));
        assert!(in_cgroup("/a/sddm.service/x", "/a/sddm.service"));
        assert!(!in_cgroup("/a/sddm.service-helper", "/a/sddm.service"));
        assert!(!in_cgroup("/a", ""));
        assert_eq!(
            cgroup_path("1:net_cls:/\n0::/system.slice/x.service\n"),
            "/system.slice/x.service"
        );
    }
}
