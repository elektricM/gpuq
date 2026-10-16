//! Model table parsed from llama-swap's YAML: which models use the GPU, aliases, slot saving.
//!
//! The file is re-read when its mtime changes. This is a cache, so a plain mutex is enough.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::SystemTime;

use serde_yaml::Value;

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
        if let Some(models) = doc.get("models").and_then(Value::as_mapping) {
            for (id, m) in models {
                let Some(id) = id.as_str() else { continue };
                let cmd = m.get("cmd").and_then(Value::as_str).unwrap_or("");
                let on_gpu = !self
                    .cpu_markers
                    .iter()
                    .any(|mark| cmd.contains(mark.as_str()));
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

    /// All slot-save model ids.
    pub fn slot_save_ids(&self) -> BTreeSet<String> {
        self.refresh();
        self.lock().slot_save.clone()
    }
}
