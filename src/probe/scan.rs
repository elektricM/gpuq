// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! Find every GPU on the machine, and the device string that points a program at each one.
//!
//! AMD cards come from sysfs: every PCI device with amdgpu's `mem_info_vram_total`. NVIDIA
//! cards come from `nvidia-smi --query-gpu`. Each card is identified by its PCI address, never
//! by an assumed order: HIP, CUDA and Vulkan number devices their own way, and the order can
//! change after a driver update (an integrated GPU can become device 0).
//!
//! The device string:
//!
//! * NVIDIA: the GPU UUID (`GPU-...`) for `CUDA_VISIBLE_DEVICES`, which CUDA accepts in place
//!   of an index and which does not depend on `CUDA_DEVICE_ORDER`.
//! * AMD: `GPU-<unique_id>` for `ROCR_VISIBLE_DEVICES` when sysfs has a `unique_id` (ROCm
//!   accepts the same UUIDs that `rocminfo` prints). Cards without one (integrated GPUs) get
//!   their ROCm index, which is the position of the card among the GPU nodes of the KFD
//!   topology, matched by PCI address. If the topology cannot be read, there is none.
//!
//! An AMD GPU counts as integrated when sysfs has no `mem_info_vram_vendor`: amdgpu only
//! creates that file for cards with their own memory chips, which an APU's GPU does not have.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::Mib;

/// The KFD topology, where ROCm numbers its devices.
pub const KFD_NODES: &str = "/sys/class/kfd/kfd/topology/nodes";

/// How long `nvidia-smi` may take during detection.
const SMI_TIMEOUT: Duration = Duration::from_secs(10);

const MIB: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FoundVendor {
    Amd,
    Nvidia,
}

impl FoundVendor {
    pub fn as_str(self) -> &'static str {
        match self {
            FoundVendor::Amd => "amd",
            FoundVendor::Nvidia => "nvidia",
        }
    }
}

/// One GPU found on the machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub vendor: FoundVendor,
    /// PCI address, `0000:03:00.0`.
    pub pci: String,
    pub integrated: bool,
    pub total_mib: Mib,
    /// NVIDIA: the index nvidia-smi knows it by.
    pub nvidia_index: Option<u32>,
    /// The variable and value that point a program at this card, if known.
    pub visible: Option<(String, String)>,
}

/// `0000:03:00.0` from `0000:03:00.0`, `03:00.0` or nvidia-smi's `00000000:03:00.0`, in
/// lower case. None for anything else.
pub fn normalize_pci(s: &str) -> Option<String> {
    let s = s.trim().to_ascii_lowercase();
    let parts: Vec<&str> = s.split(':').collect();
    let (domain, bus, devfn) = match parts[..] {
        [bus, devfn] => ("0000", bus, devfn),
        [domain, bus, devfn] => (domain, bus, devfn),
        _ => return None,
    };
    let (dev, func) = devfn.split_once('.')?;
    let hex = |t: &str, max: usize| {
        !t.is_empty() && t.len() <= max && t.bytes().all(|b| b.is_ascii_hexdigit())
    };
    if !hex(domain, 8) || !hex(bus, 2) || !hex(dev, 2) || !hex(func, 1) {
        return None;
    }
    let domain = u32::from_str_radix(domain, 16).ok()?;
    let bus = u8::from_str_radix(bus, 16).ok()?;
    let dev = u8::from_str_radix(dev, 16).ok()?;
    let func = u8::from_str_radix(func, 16).ok()?;
    if domain > 0xffff || dev > 0x1f || func > 7 {
        return None;
    }
    Some(format!("{domain:04x}:{bus:02x}:{dev:02x}.{func:x}"))
}

fn read_trim(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_owned())
}

/// Every amdgpu device under `sysfs_root`, in PCI order. `kfd` is the KFD topology directory
/// used to number cards that have no `unique_id`; None skips it.
pub fn scan_amd(sysfs_root: &Path, kfd: Option<&Path>) -> Vec<Found> {
    let Ok(dir) = std::fs::read_dir(sysfs_root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = dir
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .collect();
    names.sort();
    let indices = kfd.map(kfd_indices).unwrap_or_default();
    let mut out = Vec::new();
    for name in names {
        let dev = sysfs_root.join(&name);
        let total = read_trim(&dev.join("mem_info_vram_total")).and_then(|t| t.parse::<u64>().ok());
        let Some(total) = total else {
            continue;
        };
        let pci = normalize_pci(&name).unwrap_or(name);
        let visible = match read_trim(&dev.join("unique_id")).filter(|u| !u.is_empty()) {
            Some(uid) => Some(format!("GPU-{uid}")),
            None => indices
                .iter()
                .position(|p| *p == pci)
                .map(|i| i.to_string()),
        };
        out.push(Found {
            vendor: FoundVendor::Amd,
            integrated: !dev.join("mem_info_vram_vendor").exists(),
            total_mib: total / MIB,
            nvidia_index: None,
            visible: visible.map(|v| ("ROCR_VISIBLE_DEVICES".to_owned(), v)),
            pci,
        });
    }
    out
}

/// PCI addresses of the GPU nodes of the KFD topology, in ROCm's device order. Empty when
/// any node cannot be read, because a gap would shift every index after it.
pub fn kfd_indices(nodes: &Path) -> Vec<String> {
    let Ok(dir) = std::fs::read_dir(nodes) else {
        return Vec::new();
    };
    let mut ids: Vec<(u64, PathBuf)> = dir
        .filter_map(|e| {
            let e = e.ok()?;
            let n = e.file_name().into_string().ok()?.parse().ok()?;
            Some((n, e.path()))
        })
        .collect();
    ids.sort();
    let mut out = Vec::new();
    for (_, node) in ids {
        let Some(props) = read_trim(&node.join("properties")) else {
            return Vec::new();
        };
        let field = |key: &str| {
            props.lines().find_map(|l| {
                let (k, v) = l.split_once(' ')?;
                (k == key).then(|| v.trim().parse::<u64>().ok()).flatten()
            })
        };
        // CPU nodes have no SIMDs.
        if field("simd_count").unwrap_or(0) == 0 {
            continue;
        }
        let (Some(domain), Some(loc)) = (field("domain"), field("location_id")) else {
            return Vec::new();
        };
        out.push(format!(
            "{domain:04x}:{:02x}:{:02x}.{:x}",
            loc >> 8,
            (loc >> 3) & 0x1f,
            loc & 7
        ));
    }
    out
}

/// Every GPU `nvidia-smi` lists. Nothing when the tool is missing or fails.
pub fn scan_nvidia(nvidia_smi: &str) -> Vec<Found> {
    let mut child = match Command::new(nvidia_smi)
        .args([
            "--query-gpu=index,pci.bus_id,uuid,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < SMI_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Vec::new();
            }
        }
    }
    match child.wait_with_output() {
        Ok(out) if out.status.success() => parse_nvidia(&String::from_utf8_lossy(&out.stdout)),
        _ => Vec::new(),
    }
}

/// Lines of `index, pci.bus_id, uuid, memory.total` (MiB).
pub fn parse_nvidia(out: &str) -> Vec<Found> {
    out.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(',').map(str::trim).collect();
            let [index, bus, uuid, total] = f[..] else {
                return None;
            };
            let uuid = (!uuid.is_empty() && !uuid.contains(' ')).then(|| uuid.to_owned());
            Some(Found {
                vendor: FoundVendor::Nvidia,
                pci: normalize_pci(bus)?,
                integrated: false,
                total_mib: total.parse().ok()?,
                nvidia_index: Some(index.parse().ok()?),
                visible: uuid.map(|u| ("CUDA_VISIBLE_DEVICES".to_owned(), u)),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pci_addresses_normalize() {
        assert_eq!(
            normalize_pci("0000:03:00.0").as_deref(),
            Some("0000:03:00.0")
        );
        assert_eq!(normalize_pci("03:00.0").as_deref(), Some("0000:03:00.0"));
        assert_eq!(
            normalize_pci("00000000:0A:00.1").as_deref(),
            Some("0000:0a:00.1")
        );
        for bad in [
            "",
            "gpu0",
            "0000:03:00",
            "0000:03:20.0",
            "0000:03:00.8",
            "x:y:z.0",
        ] {
            assert_eq!(normalize_pci(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn nvidia_lines_parse() {
        let out = "0, 00000000:01:00.0, GPU-aaaa, 24564\n1, 00000000:02:00.0, GPU-bbbb, 16376\n";
        let found = parse_nvidia(out);
        assert_eq!(found.len(), 2);
        assert_eq!(found[1].pci, "0000:02:00.0");
        assert_eq!(found[1].nvidia_index, Some(1));
        assert_eq!(found[1].total_mib, 16376);
        assert_eq!(
            found[0].visible,
            Some(("CUDA_VISIBLE_DEVICES".into(), "GPU-aaaa".into()))
        );
        assert!(parse_nvidia("garbage\n").is_empty());
    }
}
