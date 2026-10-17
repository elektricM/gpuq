// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! `[[gpu.card]]` entries, detected cards, and the `CardSpec` the daemon builds probes from.
//!
//! Three ways to get the cards, in this order:
//!
//! 1. `[[gpu.card]]` tables define the set exactly.
//! 2. With none, and no single device pinned in `[gpu]` (`pci`, `nvidia_index`, or a vendor
//!    of `command` or `intel`), gpuq detects every AMD and NVIDIA GPU and manages all of them,
//!    minus `gpu.exclude` and, with `gpu.skip_integrated`, integrated GPUs.
//! 3. Otherwise, or when nothing is detected, `[gpu]` describes one card, as before.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::{Config, GpuVendor};
use crate::probe::amd::DEFAULT_SYSFS_ROOT;
use crate::probe::scan::{self, Found, FoundVendor};
use crate::Mib;

/// One card name or several: `card = "gpu1"` or `card = ["gpu0", "gpu1"]`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum CardList {
    One(String),
    Many(Vec<String>),
}

impl CardList {
    pub fn names(&self) -> Vec<String> {
        match self {
            CardList::One(n) => vec![n.clone()],
            CardList::Many(v) => v.clone(),
        }
    }
}

impl std::fmt::Display for CardList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.names().join(","))
    }
}

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
    /// Found by detection rather than configured.
    pub detected: bool,
    /// An integrated GPU (detected cards only).
    pub integrated: bool,
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

/// Names a card cannot have: they mean something else in `--card`.
const RESERVED: [&str; 3] = ["any", "all", "primary"];

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

    /// True when the cards are found by detection: no `[[gpu.card]]`, and `[gpu]` does not
    /// pin a single device.
    pub fn detects(&self) -> bool {
        let g = &self.gpu;
        g.card.is_empty()
            && g.pci.is_empty()
            && g.nvidia_index.is_none()
            && matches!(
                g.vendor,
                GpuVendor::Auto | GpuVendor::Amd | GpuVendor::Nvidia
            )
    }

    /// Every GPU on the machine, excluded ones included; empty unless [`Config::detects`].
    pub fn detect(&self) -> Vec<Found> {
        if !self.detects() {
            return Vec::new();
        }
        let g = &self.gpu;
        let mut found = Vec::new();
        if matches!(g.vendor, GpuVendor::Auto | GpuVendor::Amd) {
            // The KFD topology only goes with the real sysfs, so tests stay hermetic.
            let kfd =
                (g.sysfs_root == Path::new(DEFAULT_SYSFS_ROOT)).then(|| Path::new(scan::KFD_NODES));
            found.extend(scan::scan_amd(&g.sysfs_root, kfd));
        }
        if matches!(g.vendor, GpuVendor::Auto | GpuVendor::Nvidia)
            && crate::probe::detect::which(&g.nvidia_smi)
        {
            found.extend(scan::scan_nvidia(&g.nvidia_smi));
        }
        found
    }

    /// Whether `gpu.exclude` or `gpu.skip_integrated` leaves this detected card out.
    pub fn excluded(&self, f: &Found) -> bool {
        (self.gpu.skip_integrated && f.integrated)
            || self
                .gpu
                .exclude
                .iter()
                .any(|e| scan::normalize_pci(e).as_deref() == Some(f.pci.as_str()))
    }

    /// Every card in order, detecting them if the config says so; the first is the primary
    /// card.
    pub fn cards(&self) -> Vec<CardSpec> {
        self.cards_with(&self.detect())
    }

    /// Every card in order, given what detection found. Detected cards are ordered discrete
    /// first, then by size (largest first), then by PCI address, and named gpu0, gpu1, ...
    /// (a single one takes `gpu.device`). Empty only when every detected card is excluded.
    pub fn cards_with(&self, found: &[Found]) -> Vec<CardSpec> {
        let g = &self.gpu;
        if g.card.is_empty() && !found.is_empty() {
            let mut kept: Vec<&Found> = found.iter().filter(|f| !self.excluded(f)).collect();
            kept.sort_by(|a, b| {
                (a.integrated, std::cmp::Reverse(a.total_mib), &a.pci).cmp(&(
                    b.integrated,
                    std::cmp::Reverse(b.total_mib),
                    &b.pci,
                ))
            });
            let single = kept.len() == 1;
            return kept
                .into_iter()
                .enumerate()
                .map(|(i, f)| CardSpec {
                    name: match (&g.device, single) {
                        (Some(d), true) => d.clone(),
                        _ => format!("gpu{i}"),
                    },
                    vendor: match f.vendor {
                        FoundVendor::Amd => GpuVendor::Amd,
                        FoundVendor::Nvidia => GpuVendor::Nvidia,
                    },
                    pci: f.pci.clone(),
                    sysfs_root: g.sysfs_root.clone(),
                    nvidia_index: f.nvidia_index.unwrap_or(0),
                    nvidia_smi: g.nvidia_smi.clone(),
                    command: Vec::new(),
                    visible_devices: f.visible.as_ref().map(|(_, v)| v.clone()),
                    env: f.visible.iter().cloned().collect(),
                    margin_mib: g.margin_mib,
                    whole_card_baseline_mib: g.whole_card_baseline_mib,
                    detected: true,
                    integrated: f.integrated,
                })
                .collect();
        }
        if g.card.is_empty() {
            return vec![CardSpec {
                name: g.device.clone().unwrap_or_else(|| "gpu0".into()),
                vendor: g.vendor,
                pci: g.pci.clone(),
                sysfs_root: g.sysfs_root.clone(),
                nvidia_index: g.nvidia_index.unwrap_or(0),
                nvidia_smi: g.nvidia_smi.clone(),
                command: g.command.clone(),
                visible_devices: None,
                env: Vec::new(),
                margin_mib: g.margin_mib,
                whole_card_baseline_mib: g.whole_card_baseline_mib,
                detected: false,
                integrated: false,
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
                    detected: false,
                    integrated: false,
                }
            })
            .collect()
    }

    /// The card names the LLM runs on by default (`llm.card`, else the primary card).
    pub fn llm_card_names(&self, cards: &[CardSpec]) -> Vec<String> {
        match &self.llm.card {
            Some(list) => list.names(),
            None => cards.first().map(|c| c.name.clone()).into_iter().collect(),
        }
    }

    /// With several cards and no env anywhere, `--card any` cannot work.
    pub fn any_card_unavailable(&self, cards: &[CardSpec]) -> bool {
        cards.len() > 1 && cards.iter().all(|c| c.env.is_empty())
    }

    /// Problems with card names used elsewhere (`llm.card`, `llm.models`, services, Immich),
    /// checked against the cards in use. Detected cards are only known at run time, so the
    /// daemon and `check-config` call this after detection.
    pub fn reference_problems(&self, cards: &[CardSpec]) -> Vec<String> {
        let mut p = Vec::new();
        let names: Vec<String> = cards.iter().map(|c| c.name.clone()).collect();
        let known = names.join(", ");
        if cards.is_empty() {
            p.push(
                "every detected GPU is excluded (gpu.exclude, gpu.skip_integrated); nothing left to schedule"
                    .into(),
            );
            return p;
        }
        let check_list = |label: String, list: &[String]| {
            let mut p = Vec::new();
            if list.is_empty() {
                p.push(format!("{label}: the card list is empty"));
            }
            let mut seen = HashSet::new();
            for n in list {
                if !names.contains(n) {
                    p.push(format!("{label}: unknown card {n:?}; known: {known}"));
                } else if !seen.insert(n) {
                    p.push(format!("{label}: card {n:?} is listed twice"));
                }
            }
            p
        };
        if let Some(list) = &self.llm.card {
            p.extend(check_list("llm.card".into(), &list.names()));
        }
        for (id, m) in &self.llm.models {
            if !m.cards.is_empty() {
                p.extend(check_list(format!("llm.models.{id}.cards"), &m.cards));
            }
        }
        for (i, s) in self.service.iter().enumerate() {
            let Some(list) = &s.card else { continue };
            let label = if s.name.is_empty() {
                format!("service #{}", i + 1)
            } else {
                format!("service {:?}", s.name)
            };
            let list = list.names();
            if list.iter().any(|n| n == "any") {
                p.push(format!(
                    "{label}: card \"any\" is not allowed for services; a service is bound to its device, name a card"
                ));
                continue;
            }
            let problems = check_list(label.clone(), &list);
            if problems.is_empty() && list.len() > 1 {
                p.extend(self.set_problems(&label, &list, cards));
            }
            p.extend(problems);
        }
        if let Some(c) = self.immich.as_ref().and_then(|im| im.card.as_ref()) {
            if !names.contains(c) {
                p.push(format!(
                    "immich.card {c:?} is not a configured card; known: {known}"
                ));
            }
        }
        p
    }

    /// A service on several cards gets their env merged: each needs one, and they must merge.
    fn set_problems(&self, label: &str, list: &[String], cards: &[CardSpec]) -> Vec<String> {
        let picked: Vec<&CardSpec> = list
            .iter()
            .filter_map(|n| cards.iter().find(|c| &c.name == n))
            .collect();
        if let Some(c) = picked.iter().find(|c| c.env.is_empty()) {
            return vec![format!(
                "{label}: card {:?} has no visible_devices; a service on several cards needs it on each",
                c.name
            )];
        }
        let merged = crate::arbiter::merge_env(
            picked
                .iter()
                .map(|c| (c.name.as_str(), c.vendor.as_str(), c.env.as_slice())),
        );
        match merged {
            Ok(_) => Vec::new(),
            Err(e) => vec![format!("{label}: {e}")],
        }
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
            if !self.gpu.exclude.is_empty() || self.gpu.skip_integrated {
                p.push(
                    "gpu.exclude and gpu.skip_integrated only apply to detected cards; with [[gpu.card]], leave the card out instead"
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
                } else if RESERVED.contains(&c.name.as_str()) {
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
        if let Some(d) = self
            .gpu
            .device
            .as_deref()
            .filter(|d| RESERVED.contains(d) || !valid_name(d))
        {
            p.push(format!(
                "gpu.device {d:?}: use a-z, 0-9, '_' and '-', and not any, all or primary"
            ));
        }
        for e in &self.gpu.exclude {
            if scan::normalize_pci(e).is_none() {
                p.push(format!(
                    "gpu.exclude: {e:?} is not a PCI address such as \"0000:03:00.0\""
                ));
            }
        }
        // Detected cards are only known at run time; anything else can be checked now.
        if !self.detects() {
            p.extend(self.reference_problems(&self.cards_with(&[])));
        } else {
            p.extend(self.reference_shape_problems());
        }
        p
    }

    /// What can be checked about card references without knowing the cards.
    fn reference_shape_problems(&self) -> Vec<String> {
        let mut p = Vec::new();
        for (i, s) in self.service.iter().enumerate() {
            let Some(list) = &s.card else { continue };
            if list.names().iter().any(|n| n == "any") {
                let label = if s.name.is_empty() {
                    format!("service #{}", i + 1)
                } else {
                    format!("service {:?}", s.name)
                };
                p.push(format!(
                    "{label}: card \"any\" is not allowed for services; a service is bound to its device, name a card"
                ));
            }
        }
        if matches!(&self.llm.card, Some(CardList::Many(v)) if v.is_empty()) {
            p.push("llm.card: the card list is empty".into());
        }
        p
    }

    /// The GPU lines of `describe`.
    pub(super) fn describe_cards(&self, s: &mut String, found: &[Found]) {
        let cards = self.cards_with(found);
        if !self.has_card_list() && found.is_empty() {
            let mut gpu = format!("gpu: {}", self.gpu.vendor.as_str());
            if !self.gpu.pci.is_empty() {
                let _ = write!(gpu, " pci={}", self.gpu.pci);
            }
            if let Some(i) = self.gpu.nvidia_index {
                let _ = write!(gpu, " index={i}");
            }
            if let Some(d) = &self.gpu.device {
                let _ = write!(gpu, " device={d}");
            }
            let _ = writeln!(s, "{gpu} (margin {} MiB)", self.gpu.margin_mib);
            if self.detects() {
                let _ = writeln!(s, "note: no GPU detected; using the [gpu] probe settings");
            }
            return;
        }
        let llm = self.llm_card_names(&cards);
        if self.has_card_list() {
            let _ = writeln!(s, "gpus: {} cards", cards.len());
        } else {
            let _ = writeln!(s, "gpus: {} detected, {} in use", found.len(), cards.len());
        }
        for (i, c) in cards.iter().enumerate() {
            let mut line = format!("  {}: {}", c.name, c.vendor.as_str());
            match c.vendor {
                GpuVendor::Nvidia if !c.detected => {
                    let _ = write!(line, " index={}", c.nvidia_index);
                }
                _ if !c.pci.is_empty() => {
                    let _ = write!(line, " pci={}", c.pci);
                }
                _ => {}
            }
            if let Some(f) = found.iter().find(|f| c.detected && f.pci == c.pci) {
                let _ = write!(line, " {} MiB", f.total_mib);
            }
            if c.integrated {
                line.push_str(" integrated");
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
            if llm.contains(&c.name) {
                line.push_str(" (llm)");
            }
            let _ = writeln!(s, "{line}");
        }
        for f in found.iter().filter(|f| self.excluded(f)) {
            let why = if self.gpu.skip_integrated && f.integrated {
                "integrated"
            } else {
                "gpu.exclude"
            };
            let _ = writeln!(
                s,
                "  excluded: {} {} {} MiB ({why})",
                f.vendor.as_str(),
                f.pci,
                f.total_mib
            );
        }
        for e in &self.gpu.exclude {
            let pci = scan::normalize_pci(e);
            if !found.is_empty() && !found.iter().any(|f| Some(&f.pci) == pci.as_ref()) {
                let _ = writeln!(s, "note: gpu.exclude {e:?} matches no detected GPU");
            }
        }
        if self.detects() && self.gpu.device.is_some() && cards.len() > 1 {
            let _ = writeln!(
                s,
                "note: gpu.device only names a single detected card; the cards are gpu0, gpu1, ..."
            );
        }
        if self.any_card_unavailable(&cards) {
            let _ = writeln!(
                s,
                "note: no card has visible_devices, so `--card any` is unavailable"
            );
        }
        if let Some(c) = cards
            .iter()
            .find(|c| c.detected && c.env.is_empty() && cards.len() > 1)
        {
            let _ = writeln!(
                s,
                "note: {} has no device id gpuq can hand to jobs (ROCm topology unreadable?); `--card any` skips it",
                c.name
            );
        }
    }
}
