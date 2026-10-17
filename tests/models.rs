// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! llama-swap model table (ports tests/test_models.py).

#[path = "support/fake_llamaswap.rs"]
mod fake_llamaswap;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use fake_llamaswap::MODELS_YAML;
use gpuq::backend::models::ModelTable;

fn setup() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("llama-swap.yaml");
    std::fs::write(&path, MODELS_YAML).unwrap();
    (dir, path)
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn cpu_marker_and_aliases() {
    let (_d, path) = setup();
    let m = ModelTable::new(&path);
    assert!(m.uses_gpu("big"));
    assert!(!m.uses_gpu("embed"));
    assert_eq!(m.resolve("other-alias"), "other");
    assert!(m.uses_gpu("other-alias"));
}

#[test]
fn cuda_marker_is_recognised() {
    let (_d, path) = setup();
    std::fs::write(
        &path,
        "models:\n  \"e\":\n    cmd: env CUDA_VISIBLE_DEVICES=-1 llama-server\n",
    )
    .unwrap();
    assert!(!ModelTable::new(&path).uses_gpu("e"));
}

#[test]
fn custom_markers() {
    let (_d, path) = setup();
    let m = ModelTable::new(&path).with_cpu_markers(["-ngl 0"]);
    assert!(!m.uses_gpu("embed"));
    assert!(m.uses_gpu("big"));
}

#[test]
fn slot_save_models_come_from_the_command_line() {
    let (_d, path) = setup();
    let m = ModelTable::new(&path);
    assert_eq!(m.slot_save_ids(), set(&["big"]));
    assert!(m.slot_save("big"));
    assert!(!m.slot_save("other"));
}

#[test]
fn unknown_models_are_assumed_to_use_the_gpu() {
    let (_d, path) = setup();
    let m = ModelTable::new(&path);
    assert!(m.uses_gpu("never-heard-of-it"));
    assert_eq!(m.resolve("never-heard-of-it"), "never-heard-of-it");
}

#[test]
fn reloads_when_the_file_changes() {
    let (_d, path) = setup();
    let m = ModelTable::new(&path);
    assert!(m.uses_gpu("new"));
    let mut text = MODELS_YAML.to_owned();
    text.push_str("  \"new\":\n    cmd: env ROCR_VISIBLE_DEVICES=-1 x\n");
    std::fs::write(&path, text).unwrap();
    // Make the mtime differ even on coarse filesystems.
    let f = std::fs::File::options().write(true).open(&path).unwrap();
    f.set_modified(SystemTime::now() + Duration::from_secs(5))
        .unwrap();
    assert!(!m.uses_gpu("new"));
}

#[test]
fn overrides_work_without_a_file() {
    let m = ModelTable::new("/nonexistent/llama-swap.yaml")
        .with_cpu_models(["embed"])
        .with_slot_save_models(["big"]);
    assert!(!m.uses_gpu("embed"));
    assert!(m.uses_gpu("big"));
    assert_eq!(m.resolve("embed"), "embed");
    assert_eq!(m.slot_save_ids(), set(&["big"]));
}

#[test]
fn overrides_apply_on_top_of_the_file() {
    let (_d, path) = setup();
    let m = ModelTable::new(&path)
        .with_cpu_models(["rec"])
        .with_slot_save_models(["other"]);
    assert!(!m.uses_gpu("rec"));
    assert_eq!(m.slot_save_ids(), set(&["big", "other"]));
}

#[test]
fn missing_file_is_not_an_error() {
    let m = ModelTable::new("/nonexistent/llama-swap.yaml");
    assert!(m.uses_gpu("anything"));
    assert_eq!(m.resolve("anything"), "anything");
    assert!(ModelTable::without_file().uses_gpu("x"));
}

#[test]
fn broken_yaml_keeps_the_old_table_until_the_next_change() {
    let (_d, path) = setup();
    let m = ModelTable::new(&path);
    assert!(!m.uses_gpu("embed"));
    std::fs::write(&path, "models: [unclosed").unwrap();
    let f = std::fs::File::options().write(true).open(&path).unwrap();
    f.set_modified(SystemTime::now() + Duration::from_secs(5))
        .unwrap();
    assert!(!m.uses_gpu("embed"));
    std::fs::write(&path, MODELS_YAML.replace("embed", "embed2")).unwrap();
    let f = std::fs::File::options().write(true).open(&path).unwrap();
    f.set_modified(SystemTime::now() + Duration::from_secs(10))
        .unwrap();
    assert!(!m.uses_gpu("embed2"));
}

#[test]
fn vram_is_estimated_from_the_model_files() {
    let dir = tempfile::tempdir().unwrap();
    let file = |name: &str, mib: u64| {
        let f = std::fs::File::create(dir.path().join(name)).unwrap();
        f.set_len(mib * 1024 * 1024).unwrap();
    };
    file("small.gguf", 1000);
    file("big-00001-of-00002.gguf", 20_000);
    file("big-00002-of-00002.gguf", 20_000);
    let d = dir.path().display();
    let yaml = format!(
        "macros:\n  dir: {d}\nmodels:\n  small:\n    cmd: llama-server -m ${{dir}}/small.gguf\n  big:\n    cmd: llama-server --model {d}/big-00001-of-00002.gguf -ngl 99\n  gone:\n    cmd: llama-server -m {d}/missing.gguf\n"
    );
    let path = dir.path().join("config.yaml");
    std::fs::write(&path, yaml).unwrap();
    let table = gpuq::backend::models::ModelTable::new(&path);
    // The files plus a fifth for the KV cache and buffers.
    assert_eq!(table.vram_estimate("small"), Some(1200));
    assert_eq!(table.vram_estimate("big"), Some(48_000));
    assert_eq!(table.vram_estimate("gone"), None);
    assert_eq!(table.vram_estimate("unknown"), None);
}

#[test]
fn cpu_markers_in_the_env_list_count_too() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    std::fs::write(
        &path,
        "models:\n  embed:\n    env: [\"ROCR_VISIBLE_DEVICES=-1\"]\n    cmd: llama-server -m /m/e.gguf\n  chat:\n    env: [\"ROCR_VISIBLE_DEVICES=GPU-1111aaaa2222bbbb\"]\n    cmd: llama-server -m /m/c.gguf\n",
    )
    .unwrap();
    let table = ModelTable::new(&path);
    assert!(!table.uses_gpu("embed"));
    assert!(table.uses_gpu("chat"));
}
