// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The gpud authors

//! llama-swap model table (ports tests/test_models.py).

#[path = "support/fake_llamaswap.rs"]
mod fake_llamaswap;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use fake_llamaswap::MODELS_YAML;
use gpud::backend::models::ModelTable;

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
