// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! Model table parsed from llama-swap's YAML: which models use the GPU, aliases, slot saving,
//! and how much VRAM a model needs.
//!
//! The file is re-read when its mtime changes. This is a cache, so a plain mutex is enough.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::SystemTime;

use serde_yaml::Value;

use crate::Mib;

/// Command-line fragments that pin a llama-server to the CPU.
pub const DEFAULT_CPU_MARKERS: [&str; 3] = [
    "HIP_VISIBLE_DEVICES=-1",
    "ROCR_VISIBLE_DEVICES=-1",
    "CUDA_VISIBLE_DEVICES=-1",
];

#[derive(Default)]
struct Table {
    /// Model id or alias -> uses the GPU.
    gpu: HashMap<String, bool>,
    /// Model id or alias -> model id.
    canonical: HashMap<String, String>,
    /// Model ids started with `--slot-save-path`.
    slot_save: BTreeSet<String>,
    /// Model id -> VRAM estimate from the size of its GGUF files.
    vram: HashMap<String, Mib>,
    mtime: Option<SystemTime>,
}

pub struct ModelTable {
    path: Option<PathBuf>,
    cpu_markers: Vec<String>,
    cpu_models: Vec<String>,
    slot_save_models: Vec<String>,
    table: Mutex<Table>,
}

impl ModelTable {
    /// A table read from `path`. A missing file is not an error: every model is then GPU.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path: PathBuf = path.into();
        let path = (!path.as_os_str().is_empty()).then_some(path);
        let mut t = ModelTable {
            path,
            cpu_markers: DEFAULT_CPU_MARKERS.iter().map(|s| s.to_string()).collect(),
            cpu_models: Vec::new(),
            slot_save_models: Vec::new(),
            table: Mutex::new(Table::default()),
        };
        t.reset_overrides();
        t
    }

    /// A table with no file, only the overrides.
    pub fn without_file() -> Self {
        Self::new("")
    }

    /// Models forced onto the CPU, on top of what the file says.
    pub fn with_cpu_models<I, S>(mut self, models: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.cpu_models = models.into_iter().map(Into::into).collect();
        self.reset_overrides();
        self
    }

    /// Models whose KV slot is saved on unload, on top of what the file says.
    pub fn with_slot_save_models<I, S>(mut self, models: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.slot_save_models = models.into_iter().map(Into::into).collect();
        self.reset_overrides();
        self
    }

    /// Replace the command-line fragments that mark a CPU-only model.
    pub fn with_cpu_markers<I, S>(mut self, markers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.cpu_markers = markers.into_iter().map(Into::into).collect();
        self
    }

    fn reset_overrides(&mut self) {
        let mut t = Table::default();
        self.apply_overrides(&mut t);
        *self.table.get_mut().unwrap_or_else(|e| e.into_inner()) = t;
    }

    fn apply_overrides(&self, t: &mut Table) {
        for name in &self.cpu_models {
            t.gpu.insert(name.clone(), false);
            t.canonical
                .entry(name.clone())
                .or_insert_with(|| name.clone());
        }
        t.slot_save.extend(self.slot_save_models.iter().cloned());
    }

    fn lock(&self) -> MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn parse(&self, doc: &Value) -> Table {
        let mut t = Table::default();
        let macros: HashMap<String, String> = doc
            .get("macros")
            .and_then(Value::as_mapping)
            .into_iter()
            .flatten()
            .filter_map(|(k, v)| Some((k.as_str()?.to_owned(), v.as_str()?.to_owned())))
            .collect();
        if let Some(models) = doc.get("models").and_then(Value::as_mapping) {
            for (id, m) in models {
                let Some(id) = id.as_str() else { continue };
                let cmd = m.get("cmd").and_then(Value::as_str).unwrap_or("");
                if let Some(mib) = files_mib(&model_files(&expand(cmd, &macros))) {
                    t.vram.insert(id.to_owned(), mib.saturating_mul(6) / 5);
                }
                // llama-swap can also set variables in a per-model `env` list.
                let env: Vec<&str> = m
                    .get("env")
                    .and_then(Value::as_sequence)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                let on_gpu = !self.cpu_markers.iter().any(|mark| {
                    cmd.contains(mark.as_str()) || env.iter().any(|e| e.contains(mark.as_str()))
                });
                let aliases = m
                    .get("aliases")
                    .and_then(Value::as_sequence)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str);
                for name in std::iter::once(id).chain(aliases) {
                    t.gpu.insert(name.to_owned(), on_gpu);
                    t.canonical.insert(name.to_owned(), id.to_owned());
                }
                if cmd.contains("--slot-save-path") {
                    t.slot_save.insert(id.to_owned());
                }
            }
        }
        self.apply_overrides(&mut t);
        t
    }

    /// Re-read the file if its mtime changed. A failure is logged once per change.
    fn refresh(&self) {
        let Some(path) = &self.path else { return };
        let Ok(mtime) = std::fs::metadata(path).and_then(|m| m.modified()) else {
            return;
        };
        let mut guard = self.lock();
        if guard.mtime == Some(mtime) {
            return;
        }
        // Remember the mtime even on failure, so a broken file is not re-parsed on every call.
        guard.mtime = Some(mtime);
        let doc = match std::fs::read_to_string(path)
            .map_err(|e| e.to_string())
            .and_then(|s| serde_yaml::from_str::<Value>(&s).map_err(|e| e.to_string()))
        {
            Ok(doc) => doc,
            Err(e) => {
                tracing::warn!("cannot read {}: {e}", path.display());
                return;
            }
        };
        let mut t = self.parse(&doc);
        t.mtime = Some(mtime);
        let gpu = t.gpu.values().filter(|g| **g).count();
        tracing::info!(
            "model table: {gpu} gpu, {} cpu, slot-save on {:?}",
            t.gpu.len() - gpu,
            t.slot_save
        );
        *guard = t;
    }

    /// Whether requests for `model` need the GPU. Unknown names do: holding a request for a
    /// model that does not exist costs nothing, letting one load onto a leased card can crash
    /// a job.
    pub fn uses_gpu(&self, model: &str) -> bool {
        self.refresh();
        self.lock().gpu.get(model).copied().unwrap_or(true)
    }

    /// Canonical id for a model name or alias; unknown names resolve to themselves.
    pub fn resolve(&self, model: &str) -> String {
        self.refresh();
        self.lock()
            .canonical
            .get(model)
            .cloned()
            .unwrap_or_else(|| model.to_owned())
    }

    /// Whether the model's KV slot can be saved and restored.
    pub fn slot_save(&self, model: &str) -> bool {
        self.refresh();
        self.lock().slot_save.contains(model)
    }

    /// VRAM a model id needs: the size of the GGUF files its command loads, plus a fifth for
    /// the KV cache and buffers. None when the files cannot be found.
    pub fn vram_estimate(&self, model: &str) -> Option<Mib> {
        self.refresh();
        self.lock().vram.get(model).copied()
    }

    /// All slot-save model ids.
    pub fn slot_save_ids(&self) -> BTreeSet<String> {
        self.refresh();
        self.lock().slot_save.clone()
    }
}

/// Replace llama-swap `${name}` macros.
fn expand(cmd: &str, macros: &HashMap<String, String>) -> String {
    let mut out = cmd.to_owned();
    for (k, v) in macros {
        out = out.replace(&format!("${{{k}}}"), v);
    }
    out
}

/// The GGUF files a llama-server command line loads: `-m`/`--model` and `-mm`/`--mmproj`,
/// with every shard of a split model.
pub fn model_files(cmd: &str) -> Vec<PathBuf> {
    let words: Vec<&str> = cmd
        .split_whitespace()
        .map(|w| w.trim_matches(|c| c == '"' || c == '\''))
        .collect();
    let mut files = Vec::new();
    for (i, w) in words.iter().enumerate() {
        let path = match *w {
            "-m" | "--model" | "-mm" | "--mmproj" => words.get(i + 1).copied(),
            _ => w
                .strip_prefix("--model=")
                .or_else(|| w.strip_prefix("--mmproj=")),
        };
        if let Some(p) = path.filter(|p| !p.is_empty()) {
            files.extend(shards(Path::new(p)));
        }
    }
    files
}

/// `x-00001-of-00003.gguf` stands for all three shards.
fn shards(p: &Path) -> Vec<PathBuf> {
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let split = name.strip_suffix(".gguf").and_then(|stem| {
        let (rest, of) = stem.rsplit_once("-of-")?;
        let (prefix, _) = rest.rsplit_once('-')?;
        let n: usize = of.parse().ok()?;
        (of.len() == 5 && n > 1).then_some((prefix, n))
    });
    match split {
        Some((prefix, n)) => (1..=n)
            .map(|i| p.with_file_name(format!("{prefix}-{i:05}-of-{n:05}.gguf")))
            .collect(),
        None => vec![p.to_path_buf()],
    }
}

/// Total size in MiB; None when there are no files or one is missing.
fn files_mib(files: &[PathBuf]) -> Option<Mib> {
    if files.is_empty() {
        return None;
    }
    let mut bytes = 0u64;
    for f in files {
        bytes = bytes.saturating_add(std::fs::metadata(f).ok()?.len());
    }
    Some(bytes / (1024 * 1024))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_files_are_found_on_the_command_line() {
        let cmd = "llama-server --port ${PORT} -m /m/a.gguf --mmproj=/m/p.gguf -ngl 99";
        assert_eq!(
            model_files(cmd),
            [PathBuf::from("/m/a.gguf"), PathBuf::from("/m/p.gguf")]
        );
        let split = model_files("llama-server --model '/m/big-00001-of-00003.gguf'");
        assert_eq!(split.len(), 3);
        assert_eq!(split[2], PathBuf::from("/m/big-00003-of-00003.gguf"));
        assert!(model_files("llama-server -hf org/repo").is_empty());
    }

    #[test]
    fn macros_expand() {
        let macros = HashMap::from([("dir".to_owned(), "/models".to_owned())]);
        assert_eq!(expand("-m ${dir}/a.gguf", &macros), "-m /models/a.gguf");
    }
}
