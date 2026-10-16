//! `[[gpu.card]]` entries and the `CardSpec` the daemon builds probes from.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::PathBuf;

use serde::Deserialize;

use super::{Config, GpuVendor};
use crate::Mib;

/// One `[[gpu.card]]` table. Unset probe paths fall back to the `[gpu]` values.
#[derive(Clone, Debug, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GpuCard {
    /// Unique, `[a-z0-9_-]`, not "any" or "primary".
    pub name: String,
    pub vendor: GpuVendor,
    pub pci: String,
    pub sysfs_root: Option<PathBuf>,
    pub nvidia_index: u32,
    pub nvidia_smi: Option<String>,
    pub command: Vec<String>,
    /// Device list that points a program at this card, e.g. "1" for `CUDA_VISIBLE_DEVICES`.
    pub visible_devices: Option<String>,
    /// Variables `visible_devices` is exported as; the default depends on the vendor.
    pub visible_devices_env: Option<Vec<String>>,
    /// Defaults to `gpu.margin_mib`.
    pub margin_mib: Option<Mib>,
    /// Defaults to `gpu.whole_card_baseline_mib`.
    pub whole_card_baseline_mib: Option<Mib>,
}

/// A card with every default resolved.
#[derive(Clone, Debug, PartialEq)]
pub struct CardSpec {
    pub name: String,
    pub vendor: GpuVendor,
    pub pci: String,
    pub sysfs_root: PathBuf,
    pub nvidia_index: u32,
    pub nvidia_smi: String,
    pub command: Vec<String>,
    pub visible_devices: Option<String>,
    /// Variable and value pairs handed to lease holders; empty without `visible_devices`.
    pub env: Vec<(String, String)>,
    pub margin_mib: Mib,
    pub whole_card_baseline_mib: Mib,
}

impl CardSpec {
    /// `K=V, K2=V2`, or None when the card has no env.
    pub fn env_summary(&self) -> Option<String> {
        (!self.env.is_empty()).then(|| {
            self.env
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(", ")
        })
    }
}

/// Variables `visible_devices` is exported as for a vendor.
pub fn default_env_vars(vendor: GpuVendor) -> &'static [&'static str] {
    match vendor {
        GpuVendor::Amd => &["HIP_VISIBLE_DEVICES", "ROCR_VISIBLE_DEVICES"],
        // NVIDIA, and the stacks that follow CUDA's convention (command probes, oneAPI).
        GpuVendor::Nvidia | GpuVendor::Intel | GpuVendor::Command | GpuVendor::Auto => {
            &["CUDA_VISIBLE_DEVICES"]
        }
    }
}

fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

fn valid_var(v: &str) -> bool {
    !v.is_empty()
        && !v.as_bytes()[0].is_ascii_digit()
        && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

impl Config {
    /// True when `[[gpu.card]]` is used instead of the single `[gpu]` card.
    pub fn has_card_list(&self) -> bool {
        !self.gpu.card.is_empty()
    }

    /// Every card in config order; the first is the primary card. Without `[[gpu.card]]`
    /// that is one card named `gpu.device`, else "gpu0", with no env.
    pub fn cards(&self) -> Vec<CardSpec> {
        let g = &self.gpu;
        if g.card.is_empty() {
            return vec![CardSpec {
                name: g.device.clone().unwrap_or_else(|| "gpu0".into()),
                vendor: g.vendor,
                pci: g.pci.clone(),
                sysfs_root: g.sysfs_root.clone(),
                nvidia_index: g.nvidia_index,
                nvidia_smi: g.nvidia_smi.clone(),
                command: g.command.clone(),
                visible_devices: None,
                env: Vec::new(),
                margin_mib: g.margin_mib,
                whole_card_baseline_mib: g.whole_card_baseline_mib,
            }];
        }
        g.card
            .iter()
            .map(|c| {
                let env = match &c.visible_devices {
                    Some(v) => match &c.visible_devices_env {
                        Some(vars) => vars.iter().map(|k| (k.clone(), v.clone())).collect(),
                        None => default_env_vars(c.vendor)
                            .iter()
                            .map(|k| ((*k).to_owned(), v.clone()))
                            .collect(),
                    },
                    None => Vec::new(),
                };
                CardSpec {
                    name: c.name.clone(),
                    vendor: c.vendor,
                    pci: c.pci.clone(),
                    sysfs_root: c.sysfs_root.clone().unwrap_or_else(|| g.sysfs_root.clone()),
                    nvidia_index: c.nvidia_index,
                    nvidia_smi: c.nvidia_smi.clone().unwrap_or_else(|| g.nvidia_smi.clone()),
                    command: c.command.clone(),
                    visible_devices: c.visible_devices.clone(),
                    env,
                    margin_mib: c.margin_mib.unwrap_or(g.margin_mib),
                    whole_card_baseline_mib: c
                        .whole_card_baseline_mib
                        .unwrap_or(g.whole_card_baseline_mib),
                }
            })
            .collect()
    }

    /// Index into [`Config::cards`] of the card the LLM lives on (`llm.card`, else the first).
    pub fn llm_card_index(&self) -> usize {
        self.llm
            .card
            .as_deref()
            .and_then(|n| self.cards().iter().position(|c| c.name == n))
            .unwrap_or(0)
    }

    /// With several cards and no `visible_devices` anywhere, `--card any` cannot work.
    pub fn any_card_unavailable(&self) -> bool {
        self.gpu.card.len() > 1 && self.gpu.card.iter().all(|c| c.visible_devices.is_none())
    }

    pub(super) fn card_problems(&self) -> Vec<String> {
        let mut p = Vec::new();
        let list = &self.gpu.card;
        if !list.is_empty() {
            if self.gpu.device.is_some() {
                p.push(
                    "gpu.device is ignored when [[gpu.card]] is used; name the cards instead"
                        .into(),
                );
            }
            let mut seen = HashSet::new();
            for (i, c) in list.iter().enumerate() {
                let label = if c.name.is_empty() {
                    format!("gpu.card #{}", i + 1)
                } else {
                    format!("gpu.card {:?}", c.name)
                };
                if c.name.is_empty() {
                    p.push(format!("{label}: name is required"));
                } else if !valid_name(&c.name) {
                    p.push(format!(
                        "{label}: name may only use a-z, 0-9, '_' and '-' (lowercase)"
                    ));
                } else if c.name == "any" || c.name == "primary" {
                    p.push(format!(
                        "{label}: \"{}\" is reserved, pick another name",
                        c.name
                    ));
                } else if !seen.insert(c.name.as_str()) {
                    p.push(format!("{label}: duplicate card name"));
                }
                if list.len() > 1 && c.vendor == GpuVendor::Auto {
                    p.push(format!(
                        "{label}: set vendor (amd, nvidia, intel or command); \"auto\" only works with a single card"
                    ));
                }
                if c.vendor == GpuVendor::Command && c.command.is_empty() {
                    p.push(format!(
                        "{label}: vendor = \"command\" needs command (argv printing \"used_mib total_mib\")"
                    ));
                }
                if c.command.first().is_some_and(|c| c.is_empty()) {
                    p.push(format!("{label}: command: the program name is empty"));
                }
                if c.visible_devices
                    .as_deref()
                    .is_some_and(|v| v.trim().is_empty())
                {
                    p.push(format!("{label}: visible_devices is empty"));
                }
                match (&c.visible_devices, &c.visible_devices_env) {
                    (None, Some(_)) => p.push(format!(
                        "{label}: visible_devices_env needs visible_devices"
                    )),
                    (_, Some(vars)) if vars.is_empty() => {
                        p.push(format!("{label}: visible_devices_env is empty"))
                    }
                    (_, Some(vars)) => {
                        for v in vars.iter().filter(|v| !valid_var(v)) {
                            p.push(format!(
                                "{label}: visible_devices_env: {v:?} is not a variable name"
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }
        let names: Vec<String> = self.cards().into_iter().map(|c| c.name).collect();
        let known = names.join(", ");
        if let Some(n) = &self.llm.card {
            if !names.contains(n) {
                p.push(format!("llm.card: unknown card {n:?}; known: {known}"));
            }
        }
        for (i, s) in self.service.iter().enumerate() {
            let Some(n) = &s.card else { continue };
            let label = if s.name.is_empty() {
                format!("service #{}", i + 1)
            } else {
                format!("service {:?}", s.name)
            };
            if n == "any" {
                p.push(format!(
                    "{label}: card \"any\" is not allowed for services; a service is bound to its device, name a card"
                ));
            } else if !names.contains(n) {
                p.push(format!("{label}: unknown card {n:?}; known: {known}"));
            }
        }
        p
    }

    /// The GPU lines of `describe`.
    pub(super) fn describe_cards(&self, s: &mut String) {
        if !self.has_card_list() {
            let mut gpu = format!("gpu: {}", self.gpu.vendor.as_str());
            if !self.gpu.pci.is_empty() {
                let _ = write!(gpu, " pci={}", self.gpu.pci);
            }
            if let Some(d) = &self.gpu.device {
                let _ = write!(gpu, " device={d}");
            }
            let _ = writeln!(s, "{gpu} (margin {} MiB)", self.gpu.margin_mib);
            return;
        }
        let llm = self.llm_card_index();
        let _ = writeln!(s, "gpus: {} cards", self.gpu.card.len());
        for (i, c) in self.cards().iter().enumerate() {
            let mut line = format!("  {}: {}", c.name, c.vendor.as_str());
            match c.vendor {
                GpuVendor::Nvidia => {
                    let _ = write!(line, " index={}", c.nvidia_index);
                }
                _ if !c.pci.is_empty() => {
                    let _ = write!(line, " pci={}", c.pci);
                }
                _ => {}
            }
            match c.env_summary() {
                Some(e) => {
                    let _ = write!(line, ", {e}");
                }
                None => line.push_str(", no visible_devices"),
            }
            let _ = write!(line, ", margin {} MiB", c.margin_mib);
            if i == 0 {
                line.push_str(" (primary)");
            }
            if i == llm {
                line.push_str(" (llm)");
            }
            let _ = writeln!(s, "{line}");
        }
        if self.any_card_unavailable() {
            let _ = writeln!(
                s,
                "note: no card has visible_devices, so `--card any` is unavailable"
            );
        }
    }
}
