// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The gpud authors

//! amdgpu's `mem_info_*` counters under `/sys/bus/pci/devices/<pci>`.
//!
//! Files read: `mem_info_vram_used`, `mem_info_vram_total`, `mem_info_gtt_used`; each holds
//! a byte count as ASCII. Sysfs reads are memory-backed and take microseconds, so plain
//! `std::fs` inside the async fn is fine; a `spawn_blocking` hop would cost more than it saves.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use super::{ProbeError, VramProbe, VramReading};
use crate::Mib;

pub const DEFAULT_SYSFS_ROOT: &str = "/sys/bus/pci/devices";
const MIB: u64 = 1024 * 1024;

#[derive(Debug)]
pub struct AmdSysfsProbe {
    pci: String,
    dev: PathBuf,
}

impl AmdSysfsProbe {
    pub fn new(pci: impl Into<String>, sysfs_root: impl AsRef<Path>) -> Self {
        let pci = pci.into();
        let dev = sysfs_root.as_ref().join(&pci);
        Self { pci, dev }
    }

    fn read_mib(&self, name: &str) -> Result<Mib, ProbeError> {
        let path = self.dev.join(name);
        let text = std::fs::read_to_string(&path)
            .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
        let bytes: u64 = text
            .trim()
            .parse()
            .map_err(|e| ProbeError::Parse(format!("{}: {e}", path.display())))?;
        Ok(bytes / MIB)
    }
}

#[async_trait]
impl VramProbe for AmdSysfsProbe {
    fn device(&self) -> String {
        format!("amd:{}", self.pci)
    }

    async fn read(&self) -> Result<VramReading, ProbeError> {
        Ok(VramReading {
            used_mib: self.read_mib("mem_info_vram_used")?,
            total_mib: self.read_mib("mem_info_vram_total")?,
            // Older kernels lack the counter; any failure just means "unknown".
            gtt_used_mib: self.read_mib("mem_info_gtt_used").ok(),
        })
    }
}

/// PCI address of the amdgpu device with the most VRAM (skips integrated GPUs).
pub fn find_amd_device(sysfs_root: impl AsRef<Path>) -> Option<String> {
    let root = sysfs_root.as_ref();
    let mut names: Vec<String> = std::fs::read_dir(root)
        .ok()?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .collect();
    names.sort(); // deterministic ties: first by name wins
    let mut best: Option<(u64, String)> = None;
    for name in names {
        let file = root.join(&name).join("mem_info_vram_total");
        let Some(total) = std::fs::read_to_string(file)
            .ok()
            .and_then(|t| t.trim().parse::<u64>().ok())
        else {
            continue;
        };
        if best.as_ref().map_or(true, |(b, _)| total > *b) {
            best = Some((total, name));
        }
    }
    best.map(|(_, n)| n)
}
