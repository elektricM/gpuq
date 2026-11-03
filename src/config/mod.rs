// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! TOML configuration: built-in defaults, overridden by an optional file.
//!
//! Every section is `deny_unknown_fields`, so a typo is an error instead of a silent default.
//! Durations are written as seconds (integer or float) and exposed as [`Duration`].

mod card;
mod desktop;
mod immich;
mod service;
mod vram;

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Deserializer};

use crate::arbiter::{ClassTuning, Classes, LlmPlan, Settings, Tuning};
use crate::Mib;

pub use card::{default_env_vars, CardList, CardSpec, GpuCard};
pub use desktop::{Desktop, Offload, Policy as DesktopPolicy};
pub use immich::Immich;
pub use service::{Flavor, Service, ServiceGate, ServiceKind, ServiceSpec};
pub use vram::{parse_vram, VramParseError};

/// Signals `gpuq run` may send to a yieldable job.
pub const YIELD_SIGNALS: &[&str] = &[
    "SIGHUP", "SIGINT", "SIGQUIT", "SIGTERM", "SIGUSR1", "SIGUSR2", "SIGALRM", "SIGCONT", "SIGTSTP",
];

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{origin}: {message}")]
    Parse { origin: String, message: String },
    #[error("invalid config:{}", bullets(.0))]
    Invalid(Vec<String>),
}

fn bullets(items: &[String]) -> String {
    items.iter().fold(String::new(), |mut s, i| {
        let _ = write!(s, "\n  - {i}");
        s
    })
}

/// Seconds (int or float) to a [`Duration`]; negative and non-finite values are rejected.
pub(crate) mod secs {
    use super::*;

    fn convert<E: serde::de::Error>(v: f64) -> Result<Duration, E> {
        Duration::try_from_secs_f64(v)
            .map_err(|_| E::custom(format!("{v} is not a valid duration in seconds")))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        convert(f64::deserialize(d)?)
    }

    pub fn option<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        Option::<f64>::deserialize(d)?.map(convert).transpose()
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub gate: Gate,
    pub api: Api,
    pub gpu: Gpu,
    pub hooks: Hooks,
    pub llm: Llm,
    /// Priority classes, lower number = more important. User keys merge into the defaults.
    #[serde(deserialize_with = "merge_classes")]
    pub classes: BTreeMap<String, u32>,
    pub run: Run,
    /// Managed GPU services, one `[[service]]` table each.
    pub service: Vec<Service>,
    /// The Immich ML adapter; present means enabled.
    pub immich: Option<Immich>,
    /// Freeing the desktop's VRAM; off unless `policy` says otherwise.
    pub desktop: Desktop,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            gate: Gate::default(),
            api: Api::default(),
            gpu: Gpu::default(),
            hooks: Hooks::default(),
            llm: Llm::default(),
            classes: default_classes(),
            run: Run::default(),
            service: Vec::new(),
            immich: None,
            desktop: Desktop::default(),
        }
    }
}

fn default_classes() -> BTreeMap<String, u32> {
    [
        ("realtime", 0),
        ("llm", 1),
        ("interactive", 2),
        ("background", 3),
        ("batch", 4),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

fn merge_classes<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<String, u32>, D::Error> {
    let mut merged = default_classes();
    merged.extend(BTreeMap::<String, u32>::deserialize(d)?);
    Ok(merged)
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Gate {
    /// `"*"` listens on all interfaces.
    pub host: String,
    pub port: u16,
    /// Where requests are forwarded (the real LLM server).
    pub backend: String,
}

impl Default for Gate {
    fn default() -> Self {
        Gate {
            host: "127.0.0.1".into(),
            port: 8080,
            backend: "http://127.0.0.1:8081".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Api {
    pub host: String,
    pub port: u16,
}

impl Default for Api {
    fn default() -> Self {
        Api {
            host: "127.0.0.1".into(),
            port: 9390,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GpuVendor {
    #[default]
    Auto,
    Amd,
    Nvidia,
    Intel,
    Command,
}

impl GpuVendor {
    pub fn as_str(self) -> &'static str {
        match self {
            GpuVendor::Auto => "auto",
            GpuVendor::Amd => "amd",
            GpuVendor::Nvidia => "nvidia",
            GpuVendor::Intel => "intel",
            GpuVendor::Command => "command",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Gpu {
    pub vendor: GpuVendor,
    /// AMD: PCI address. Empty picks the amdgpu device with the most VRAM.
    pub pci: String,
    pub sysfs_root: PathBuf,
    /// NVIDIA: device index and the nvidia-smi binary. Setting the index means one card.
    pub nvidia_index: Option<u32>,
    pub nvidia_smi: String,
    /// Command probe: argv printing "used_mib total_mib".
    pub command: Vec<String>,
    /// Name of the single card when no `[[gpu.card]]` is given (default "gpu0").
    pub device: Option<String>,
    /// VRAM kept free on top of what leases declare.
    pub margin_mib: Mib,
    #[serde(rename = "settle_timeout_s", deserialize_with = "secs::deserialize")]
    pub settle_timeout: Duration,
    #[serde(rename = "driver_settle_s", deserialize_with = "secs::deserialize")]
    pub driver_settle: Duration,
    pub whole_card_baseline_mib: Mib,
    /// `[[gpu.card]]` entries. When present they replace the single card described above.
    pub card: Vec<GpuCard>,
    /// PCI addresses of detected GPUs to leave alone.
    pub exclude: Vec<String>,
    /// Leave integrated GPUs alone when detecting.
    pub skip_integrated: bool,
}

impl Default for Gpu {
    fn default() -> Self {
        Gpu {
            vendor: GpuVendor::Auto,
            pci: String::new(),
            sysfs_root: "/sys/bus/pci/devices".into(),
            nvidia_index: None,
            nvidia_smi: "nvidia-smi".into(),
            command: Vec::new(),
            device: None,
            margin_mib: 256,
            settle_timeout: Duration::from_secs(60),
            driver_settle: Duration::from_secs(2),
            whole_card_baseline_mib: 512,
            card: Vec::new(),
            exclude: Vec::new(),
            skip_integrated: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Hooks {
    /// Offload command (argv, no shell). Empty disables the hook.
    pub offload: Vec<String>,
    #[serde(rename = "offload_timeout_s", deserialize_with = "secs::deserialize")]
    pub offload_timeout: Duration,
    #[serde(rename = "offload_settle_s", deserialize_with = "secs::deserialize")]
    pub offload_settle: Duration,
}

impl Default for Hooks {
    fn default() -> Self {
        Hooks {
            offload: Vec::new(),
            offload_timeout: Duration::from_secs(180),
            offload_settle: Duration::from_secs(3),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
pub enum LlmBackend {
    #[default]
    #[serde(rename = "llama-swap")]
    LlamaSwap,
    #[serde(rename = "ollama")]
    Ollama,
    #[serde(rename = "llama-server")]
    LlamaServer,
    #[serde(rename = "vllm")]
    Vllm,
    #[serde(rename = "sglang")]
    Sglang,
    #[serde(rename = "none")]
    None,
}

impl LlmBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            LlmBackend::LlamaSwap => "llama-swap",
            LlmBackend::Ollama => "ollama",
            LlmBackend::LlamaServer => "llama-server",
            LlmBackend::Vllm => "vllm",
            LlmBackend::Sglang => "sglang",
            LlmBackend::None => "none",
        }
    }

    /// Backends that can only be unloaded by stopping the server need `[llm.service]`.
    pub fn needs_service(self) -> bool {
        matches!(
            self,
            LlmBackend::Vllm | LlmBackend::Sglang | LlmBackend::LlamaServer
        )
    }
}

/// Per-class overrides, `[llm.class_tuning.<class>]`.
#[derive(Clone, Debug, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClassTuningCfg {
    #[serde(rename = "gap_s", deserialize_with = "secs::option")]
    pub gap: Option<Duration>,
    #[serde(rename = "reload_delay_s", deserialize_with = "secs::option")]
    pub reload_delay: Option<Duration>,
    /// While this class has LLM requests in flight, and this long after its last one,
    /// requests of less important classes for another model wait.
    #[serde(rename = "hold_other_models_s", deserialize_with = "secs::option")]
    pub hold_other_models: Option<Duration>,
}

/// `[llm.models.<id>]`: where one model runs.
#[derive(Clone, Debug, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LlmModel {
    /// The cards it is split over.
    pub cards: Vec<String>,
    /// VRAM it needs over all its cards, e.g. "40G".
    pub vram: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Llm {
    pub backend: LlmBackend,
    /// The card or cards the LLM server runs on; default the first card.
    pub card: Option<CardList>,
    /// Per model: cards or VRAM, for models that need other cards than `card`.
    pub models: BTreeMap<String, LlmModel>,
    /// Model reloaded when the card goes idle. Empty means load on demand only.
    pub resident: String,
    pub cpu_models: Vec<String>,
    pub slot_save_models: Vec<String>,
    /// A llama-swap model whose cmd contains one of these runs on the CPU.
    pub cpu_markers: Vec<String>,
    /// llama-swap's own config file (named `config` in the Python version).
    #[serde(alias = "config")]
    pub llama_swap_config: PathBuf,
    #[serde(rename = "load_timeout_s", deserialize_with = "secs::deserialize")]
    pub load_timeout: Duration,
    #[serde(rename = "idle_s", deserialize_with = "secs::deserialize")]
    pub idle: Duration,
    pub idle_wait_classes: Vec<String>,
    #[serde(rename = "gap_s", deserialize_with = "secs::deserialize")]
    pub gap: Duration,
    pub reload_when_idle: bool,
    #[serde(rename = "reload_delay_s", deserialize_with = "secs::deserialize")]
    pub reload_delay: Duration,
    pub reload_slack_mib: Mib,
    #[serde(rename = "reload_wait_s", deserialize_with = "secs::deserialize")]
    pub reload_wait: Duration,
    #[serde(rename = "resume_grace_s", deserialize_with = "secs::deserialize")]
    pub resume_grace: Duration,
    #[serde(rename = "batch_hold_s", deserialize_with = "secs::deserialize")]
    pub batch_hold: Duration,
    pub batch_hold_count: usize,
    /// Model name to class, for models scheduled differently from "llm".
    pub model_classes: BTreeMap<String, String>,
    pub class_tuning: BTreeMap<String, ClassTuningCfg>,
    /// How to stop and start the backend, for backends that unload by stopping.
    pub service: Option<ServiceSpec>,
}

impl Default for Llm {
    fn default() -> Self {
        Llm {
            backend: LlmBackend::LlamaSwap,
            card: None,
            models: BTreeMap::new(),
            resident: String::new(),
            cpu_models: Vec::new(),
            slot_save_models: Vec::new(),
            cpu_markers: [
                "HIP_VISIBLE_DEVICES=-1",
                "ROCR_VISIBLE_DEVICES=-1",
                "CUDA_VISIBLE_DEVICES=-1",
            ]
            .map(String::from)
            .into(),
            llama_swap_config: "/etc/llama-swap/config.yaml".into(),
            load_timeout: Duration::from_secs(600),
            idle: Duration::from_secs(300),
            idle_wait_classes: vec!["background".into(), "batch".into()],
            gap: Duration::from_secs(10),
            reload_when_idle: true,
            reload_delay: Duration::from_secs(20),
            reload_slack_mib: 1024,
            reload_wait: Duration::from_secs(120),
            resume_grace: Duration::from_secs(30),
            batch_hold: Duration::ZERO,
            batch_hold_count: 0,
            model_classes: BTreeMap::new(),
            class_tuning: BTreeMap::new(),
            service: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Run {
    /// Signal `gpuq run` sends to a yieldable job when asked to yield.
    pub yield_signal: String,
}

impl Default for Run {
    fn default() -> Self {
        Run {
            yield_signal: "SIGUSR1".into(),
        }
    }
}

impl Config {
    /// Where `load(None)` looks, in order.
    pub fn search_paths() -> Vec<PathBuf> {
        let mut paths = vec![PathBuf::from("/etc/gpuq/config.toml")];
        let user_dir = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|v| !v.is_empty())
                    .map(|h| PathBuf::from(h).join(".config"))
            });
        if let Some(dir) = user_dir {
            paths.push(dir.join("gpuq/config.toml"));
        }
        paths
    }

    /// The file `load` would read: the explicit path (which must exist), else the first
    /// existing search path, else None (defaults).
    pub fn resolve_path(explicit: Option<&Path>) -> Result<Option<PathBuf>, ConfigError> {
        if let Some(p) = explicit {
            return if p.is_file() {
                Ok(Some(p.to_path_buf()))
            } else {
                Err(ConfigError::Read {
                    path: p.to_path_buf(),
                    source: std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"),
                })
            };
        }
        Ok(Self::search_paths().into_iter().find(|p| p.is_file()))
    }

    /// Defaults overridden by `path`, or by the first existing default location. Validated.
    pub fn load(path: Option<&Path>) -> Result<Config, ConfigError> {
        match Self::resolve_path(path)? {
            None => {
                let cfg = Config::default();
                cfg.check()?;
                Ok(cfg)
            }
            Some(p) => {
                let text = std::fs::read_to_string(&p).map_err(|source| ConfigError::Read {
                    path: p.clone(),
                    source,
                })?;
                Self::parse(&text, &p.display().to_string())
            }
        }
    }

    /// Parse and validate a TOML document.
    pub fn from_toml_str(text: &str) -> Result<Config, ConfigError> {
        Self::parse(text, "config")
    }

    fn parse(text: &str, origin: &str) -> Result<Config, ConfigError> {
        let parse_err = |message: String| ConfigError::Parse {
            origin: origin.to_string(),
            message,
        };
        let table: toml::Table = text.parse().map_err(|e| parse_err(format!("{e}")))?;
        if table.contains_key("comfyui") {
            return Err(parse_err(
                "[comfyui] is no longer a built-in section; declare it as a [[service]] entry \
                 (see examples/gpuq.toml)"
                    .to_string(),
            ));
        }
        let cfg: Config = table.try_into().map_err(|e| parse_err(format!("{e}")))?;
        cfg.check()?;
        Ok(cfg)
    }

    fn check(&self) -> Result<(), ConfigError> {
        let problems = self.validate();
        if problems.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Invalid(problems))
        }
    }

    /// Every problem found, not just the first. Empty means valid.
    ///
    /// Durations cannot be negative or non-finite here: parsing already refuses them.
    pub fn validate(&self) -> Vec<String> {
        let mut p = Vec::new();
        let classes = &self.classes;
        if !classes.contains_key("llm") {
            p.push("[classes] must define an \"llm\" class".to_string());
        }
        let mut need_class = |what: String, class: &str| {
            if !classes.contains_key(class) {
                p.push(format!(
                    "{what} uses unknown class {class:?}; define it in [classes]"
                ));
            }
        };
        for c in &self.llm.idle_wait_classes {
            need_class("llm.idle_wait_classes".into(), c);
        }
        for (m, c) in &self.llm.model_classes {
            need_class(format!("llm.model_classes.{m}"), c);
        }
        for c in self.llm.class_tuning.keys() {
            need_class("llm.class_tuning".into(), c);
        }
        for (i, s) in self.service.iter().enumerate() {
            need_class(format!("service #{} ({})", i + 1, s.name), &s.class);
        }

        p.extend(self.card_problems());
        for (id, m) in &self.llm.models {
            match m.vram.as_deref().map(parse_vram) {
                Some(Err(e)) => p.push(format!("llm.models.{id}: vram: {e}")),
                Some(Ok(crate::arbiter::Vram::WholeCard)) => p.push(format!(
                    "llm.models.{id}: vram must be a size such as \"40G\", not \"all\""
                )),
                _ => {}
            }
            if m.vram.is_some() && !m.cards.is_empty() {
                p.push(format!(
                    "llm.models.{id}: set cards or vram, not both (cards wins over vram)"
                ));
            }
        }
        if let Some(im) = &self.immich {
            let classes: Vec<&str> = classes.keys().map(String::as_str).collect();
            p.extend(im.problems(&classes));
        }
        p.extend(self.desktop.problems());
        if self.gate.port == 0 {
            p.push("gate.port must be non-zero".into());
        }
        if self.api.port == 0 {
            p.push("api.port must be non-zero".into());
        }
        if !is_http_url(&self.gate.backend) {
            p.push(format!(
                "gate.backend must be an http(s) URL, not {:?}",
                self.gate.backend
            ));
        }
        if self.gpu.vendor == GpuVendor::Command && self.gpu.command.is_empty() {
            p.push(
                "gpu.vendor = \"command\" needs gpu.command (argv printing \"used_mib total_mib\")"
                    .into(),
            );
        }
        if self.gpu.command.first().is_some_and(|c| c.is_empty()) {
            p.push("gpu.command: the program name is empty".into());
        }
        if self.hooks.offload.first().is_some_and(|c| c.is_empty()) {
            p.push("hooks.offload: the program name is empty".into());
        }
        if !YIELD_SIGNALS.contains(&self.run.yield_signal.as_str()) {
            p.push(format!(
                "run.yield_signal {:?} is not a known signal; use one of {}",
                self.run.yield_signal,
                YIELD_SIGNALS.join(", ")
            ));
        }

        match (&self.llm.service, self.llm.backend.needs_service()) {
            (None, true) => p.push(format!(
                "llm.backend = \"{}\" is unloaded by stopping it; add an [llm.service] table",
                self.llm.backend.as_str()
            )),
            (Some(spec), _) => p.extend(spec.problems("llm.service")),
            (None, false) => {}
        }

        // Listen addresses: gate, api and every service gate must be distinct.
        let mut listens: Vec<(String, String, u16)> = vec![
            ("gate".into(), self.gate.host.clone(), self.gate.port),
            ("api".into(), self.api.host.clone(), self.api.port),
        ];
        let mut names = HashSet::new();
        for (i, s) in self.service.iter().enumerate() {
            let label = if s.name.is_empty() {
                format!("service #{}", i + 1)
            } else {
                format!("service {:?}", s.name)
            };
            if s.name.is_empty() {
                p.push(format!("{label}: name is required"));
            } else if !names.insert(s.name.as_str()) {
                p.push(format!("{label}: duplicate service name"));
            }
            if let Err(e) = s.vram() {
                p.push(format!("{label}: vram: {e}"));
            }
            if let Some(spec) = &s.control {
                p.extend(spec.problems(&format!("{label}: control")));
            }
            if let Some(g) = &s.gate {
                match g.host_port() {
                    Ok((h, port)) => listens.push((label.clone(), h, port)),
                    Err(e) => p.push(format!("{label}: gate.listen: {e}")),
                }
                if !is_http_url(&g.backend) {
                    p.push(format!("{label}: gate.backend must be an http(s) URL"));
                }
                if let Some(u) = g.busy_url.as_deref().filter(|u| !is_http_url(u)) {
                    p.push(format!(
                        "{label}: gate.busy_url must be an http(s) URL, not {u:?}"
                    ));
                }
            }
        }
        for (i, (a, ha, pa)) in listens.iter().enumerate() {
            for (b, hb, pb) in &listens[i + 1..] {
                if *pa != 0 && pa == pb && (is_wildcard(ha) || is_wildcard(hb) || ha == hb) {
                    p.push(format!("{a} and {b} both listen on port {pa}"));
                }
            }
        }
        p
    }

    /// Scheduler settings for the arbiter.
    pub fn arbiter_settings(&self) -> Settings {
        let mut prio = self.classes.clone();
        prio.entry("llm".into()).or_insert(1);
        let classes = Classes::new(prio).expect("llm class inserted above");
        let l = &self.llm;
        let tuning = Tuning {
            idle: l.idle,
            idle_wait_classes: l.idle_wait_classes.iter().cloned().collect(),
            gap: l.gap,
            class_tuning: l
                .class_tuning
                .iter()
                .map(|(k, t)| {
                    (
                        k.clone(),
                        ClassTuning {
                            gap: t.gap,
                            reload_delay: t.reload_delay,
                            hold_other_models: t.hold_other_models,
                        },
                    )
                })
                .collect(),
            reload_when_idle: l.reload_when_idle,
            reload_delay: l.reload_delay,
            reload_slack_mib: l.reload_slack_mib,
            reload_wait: l.reload_wait,
            resume_grace: l.resume_grace,
            batch_hold: l.batch_hold,
            batch_hold_count: l.batch_hold_count,
            margin_mib: self.gpu.margin_mib,
            settle_timeout: self.gpu.settle_timeout,
            driver_settle: self.gpu.driver_settle,
            whole_card_baseline_mib: self.gpu.whole_card_baseline_mib,
            offload_settle: self.hooks.offload_settle,
        };
        let llm = LlmPlan {
            cards: l.card.as_ref().map(CardList::names).unwrap_or_default(),
            model_cards: l
                .models
                .iter()
                .filter(|(_, m)| !m.cards.is_empty())
                .map(|(id, m)| (id.clone(), m.cards.clone()))
                .collect(),
            model_vram: l
                .models
                .iter()
                .filter_map(|(id, m)| {
                    let v = parse_vram(m.vram.as_deref()?).ok()?;
                    match v {
                        crate::arbiter::Vram::Mib(mib) => Some((id.clone(), mib)),
                        crate::arbiter::Vram::WholeCard => None,
                    }
                })
                .collect(),
        };
        Settings {
            classes,
            resident: Some(l.resident.clone()).filter(|r| !r.is_empty()),
            tuning,
            llm,
        }
    }

    /// Short human summary for `gpuq check-config`, detecting the cards if the config says so.
    pub fn describe(&self) -> String {
        self.describe_with(&self.detect())
    }

    /// Short human summary, given what detection found.
    pub fn describe_with(&self, found: &[crate::probe::scan::Found]) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "backend: {}", self.llm.backend.as_str());
        if let Some(spec) = &self.llm.service {
            let _ = writeln!(s, "  service: {}", spec.summary());
        }
        let resident = if self.llm.resident.is_empty() {
            "none"
        } else {
            &self.llm.resident
        };
        let _ = writeln!(s, "  resident model: {resident}");
        let _ = writeln!(
            s,
            "gate: {}:{} -> {}",
            self.gate.host, self.gate.port, self.gate.backend
        );
        let _ = writeln!(s, "api: {}:{}", self.api.host, self.api.port);
        self.describe_cards(&mut s, found);
        let mut ordered: Vec<_> = self.classes.iter().collect();
        ordered.sort_by_key(|(n, p)| (**p, n.as_str()));
        let list: Vec<String> = ordered.iter().map(|(n, p)| format!("{n}={p}")).collect();
        let _ = writeln!(s, "classes (most important first): {}", list.join(", "));
        let _ = writeln!(s, "run: yield signal {}", self.run.yield_signal);
        if self.service.is_empty() {
            let _ = writeln!(s, "services: none");
        } else {
            let _ = writeln!(s, "services:");
            for svc in &self.service {
                let _ = writeln!(s, "  {}", svc.summary());
            }
        }
        if let Some(im) = &self.immich {
            let _ = writeln!(s, "{}", im.summary());
        }
        let cards = self.cards_with(found);
        let _ = writeln!(s, "{}", self.desktop.summary(&self.desktop_card(&cards)));
        s
    }

    /// The card `[desktop]` acts on: `desktop.card`, else the LLM's first card.
    pub fn desktop_card(&self, cards: &[CardSpec]) -> String {
        if !self.desktop.card.is_empty() {
            return self.desktop.card.clone();
        }
        self.llm_card_names(cards)
            .into_iter()
            .next()
            .unwrap_or_else(|| "gpu0".into())
    }
}

fn is_http_url(u: &str) -> bool {
    u.strip_prefix("http://")
        .or_else(|| u.strip_prefix("https://"))
        .is_some_and(|rest| !rest.is_empty())
}

fn is_wildcard(host: &str) -> bool {
    matches!(host, "*" | "0.0.0.0" | "::" | "[::]")
}
