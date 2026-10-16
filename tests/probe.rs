// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! VRAM probes against fake sysfs trees and fake tools. Ported from tests/test_vram.py.

use std::fs;
use std::path::Path;
use std::time::Duration;

use gpud::probe::amd::{find_amd_device, AmdSysfsProbe};
use gpud::probe::command::CommandProbe;
use gpud::probe::detect::{make_probe, ProbeOptions, Vendor};
use gpud::probe::intel::IntelProbe;
use gpud::probe::{ProbeError, VramProbe, VramReading};

const MIB: u64 = 1024 * 1024;

fn fake_card(root: &Path, pci: &str, total_mib: u64, used_mib: u64, gtt_mib: Option<u64>) {
    let dev = root.join(pci);
    fs::create_dir_all(&dev).unwrap();
    fs::write(
        dev.join("mem_info_vram_total"),
        (total_mib * MIB).to_string(),
    )
    .unwrap();
    fs::write(dev.join("mem_info_vram_used"), (used_mib * MIB).to_string()).unwrap();
    if let Some(g) = gtt_mib {
        fs::write(dev.join("mem_info_gtt_used"), (g * MIB).to_string()).unwrap();
    }
}

fn amd_opts(root: &Path, vendor: Vendor) -> ProbeOptions {
    ProbeOptions {
        vendor,
        sysfs_root: root.to_path_buf(),
        nvidia_smi: "definitely-not-installed".into(),
        ..ProbeOptions::default()
    }
}

#[tokio::test]
async fn amd_reads_counters_in_mib() {
    let tmp = tempfile::tempdir().unwrap();
    fake_card(tmp.path(), "0000:03:00.0", 24560, 1234, Some(77));
    let p = AmdSysfsProbe::new("0000:03:00.0", tmp.path());
    assert_eq!(p.device(), "amd:0000:03:00.0");
    assert_eq!(
        p.read().await.unwrap(),
        VramReading {
            used_mib: 1234,
            total_mib: 24560,
            gtt_used_mib: Some(77)
        }
    );
}

#[tokio::test]
async fn amd_missing_gtt_counter_is_none() {
    let tmp = tempfile::tempdir().unwrap();
    fake_card(tmp.path(), "0000:03:00.0", 8192, 0, None);
    let r = AmdSysfsProbe::new("0000:03:00.0", tmp.path())
        .read()
        .await
        .unwrap();
    assert_eq!(r.gtt_used_mib, None);
    assert_eq!(r.total_mib, 8192);
}

#[tokio::test]
async fn amd_unreadable_card_is_io_error() {
    let tmp = tempfile::tempdir().unwrap();
    let err = AmdSysfsProbe::new("0000:99:00.0", tmp.path())
        .read()
        .await
        .unwrap_err();
    assert!(matches!(err, ProbeError::Io(_)), "{err:?}");
    assert!(err.to_string().contains("0000:99:00.0"));
}

#[tokio::test]
async fn amd_garbage_is_parse_error_naming_the_file() {
    let tmp = tempfile::tempdir().unwrap();
    fake_card(tmp.path(), "0000:03:00.0", 8192, 0, None);
    fs::write(tmp.path().join("0000:03:00.0/mem_info_vram_used"), "n/a").unwrap();
    let err = AmdSysfsProbe::new("0000:03:00.0", tmp.path())
        .read()
        .await
        .unwrap_err();
    assert!(matches!(err, ProbeError::Parse(_)), "{err:?}");
    assert!(err.to_string().contains("mem_info_vram_used"));
}

#[test]
fn find_picks_the_card_with_most_vram() {
    let tmp = tempfile::tempdir().unwrap();
    fake_card(tmp.path(), "0000:00:08.1", 512, 0, None); // integrated GPU
    fake_card(tmp.path(), "0000:03:00.0", 24560, 0, None);
    assert_eq!(find_amd_device(tmp.path()).as_deref(), Some("0000:03:00.0"));
}

#[test]
fn find_returns_none_without_amd_cards() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(find_amd_device(tmp.path()), None);
    assert_eq!(find_amd_device(tmp.path().join("missing")), None);
}

#[tokio::test]
async fn make_probe_amd_auto_detects_device() {
    let tmp = tempfile::tempdir().unwrap();
    fake_card(tmp.path(), "0000:03:00.0", 24560, 5, None);
    let p = make_probe(&amd_opts(tmp.path(), Vendor::Amd)).unwrap();
    assert_eq!(p.device(), "amd:0000:03:00.0");
    assert_eq!(p.read().await.unwrap().used_mib, 5);
}

#[test]
fn make_probe_amd_without_device_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let Err(err) = make_probe(&amd_opts(tmp.path(), Vendor::Amd)) else {
        panic!("expected an error");
    };
    assert!(err.to_string().contains("set gpu.pci"), "{err}");
}

#[test]
fn auto_prefers_amd_when_present() {
    let tmp = tempfile::tempdir().unwrap();
    fake_card(tmp.path(), "0000:03:00.0", 24560, 0, None);
    let p = make_probe(&amd_opts(tmp.path(), Vendor::Auto)).unwrap();
    assert_eq!(p.device(), "amd:0000:03:00.0");
}

#[test]
fn auto_without_any_gpu_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let Err(err) = make_probe(&amd_opts(tmp.path(), Vendor::Auto)) else {
        panic!("expected an error");
    };
    assert!(matches!(err, ProbeError::NotFound(_)));
    assert_eq!(
        err.to_string(),
        "no supported GPU found; set gpu.vendor and its options"
    );
}

#[test]
fn intel_is_not_supported_yet() {
    let err = IntelProbe::new().unwrap_err();
    assert!(err.to_string().contains("vendor = \"command\""));
    let tmp = tempfile::tempdir().unwrap();
    assert!(make_probe(&amd_opts(tmp.path(), Vendor::Intel)).is_err());
}

#[cfg(unix)]
mod tools {
    use super::*;
    use gpud::probe::nvidia::NvidiaSmiProbe;
    use std::os::unix::fs::PermissionsExt;

    fn script(path: &Path, body: &str) -> String {
        fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        path.to_str().unwrap().to_string()
    }

    fn fake_smi(path: &Path, output: &str, exit_code: i32) -> String {
        let body = format!(
            "echo \"$@\" > \"{}.args\"\necho \"{output}\"\nexit {exit_code}",
            path.display()
        );
        script(path, &body)
    }

    fn sh(cmd: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), cmd.into()]
    }

    #[tokio::test]
    async fn nvidia_parses_output_for_the_chosen_index() {
        let tmp = tempfile::tempdir().unwrap();
        let smi = fake_smi(&tmp.path().join("nvidia-smi"), "812, 24576", 0);
        let p = NvidiaSmiProbe::new(smi, 1).with_ttl(Duration::ZERO);
        assert_eq!(p.device(), "nvidia:1");
        assert_eq!(
            p.read().await.unwrap(),
            VramReading {
                used_mib: 812,
                total_mib: 24576,
                gtt_used_mib: None
            }
        );
        let args = fs::read_to_string(tmp.path().join("nvidia-smi.args")).unwrap();
        assert!(args.contains("-i 1"), "{args}");
        assert!(
            args.contains("--query-gpu=memory.used,memory.total"),
            "{args}"
        );
    }

    #[tokio::test]
    async fn nvidia_result_is_cached_briefly() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nvidia-smi");
        let smi = fake_smi(&path, "1, 2", 0);
        let p = NvidiaSmiProbe::new(smi, 0).with_ttl(Duration::from_secs(60));
        p.read().await.unwrap();
        fs::remove_file(&path).unwrap();
        assert_eq!(p.read().await.unwrap().total_mib, 2); // binary not needed
    }

    #[tokio::test]
    async fn nvidia_failures_are_not_cached() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nvidia-smi");
        let smi = fake_smi(&path, "boom", 9);
        let p = NvidiaSmiProbe::new(smi, 0).with_ttl(Duration::from_secs(60));
        assert!(p.read().await.is_err());
        fake_smi(&path, "3, 4", 0);
        assert_eq!(p.read().await.unwrap().used_mib, 3);
    }

    #[tokio::test]
    async fn nvidia_failing_tool_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let smi = fake_smi(&tmp.path().join("nvidia-smi"), "boom", 9);
        let p = NvidiaSmiProbe::new(smi, 0).with_ttl(Duration::ZERO);
        assert!(p.read().await.is_err());
    }

    #[tokio::test]
    async fn nvidia_missing_tool_is_io_error() {
        let tmp = tempfile::tempdir().unwrap();
        let absent = tmp.path().join("absent").to_str().unwrap().to_string();
        let err = NvidiaSmiProbe::new(absent, 0).read().await.unwrap_err();
        assert!(matches!(err, ProbeError::Io(_)), "{err:?}");
    }

    #[tokio::test]
    async fn nvidia_unparsable_output_is_parse_error() {
        let tmp = tempfile::tempdir().unwrap();
        let smi = fake_smi(&tmp.path().join("nvidia-smi"), "[N/A], [N/A]", 0);
        let err = NvidiaSmiProbe::new(smi, 0).read().await.unwrap_err();
        assert!(matches!(err, ProbeError::Parse(_)), "{err:?}");
    }

    #[tokio::test]
    async fn make_probe_nvidia_and_auto() {
        let tmp = tempfile::tempdir().unwrap();
        let smi = fake_smi(&tmp.path().join("nvidia-smi"), "3, 4", 0);
        let opts = ProbeOptions {
            vendor: Vendor::Nvidia,
            sysfs_root: tmp.path().join("none"),
            nvidia_smi: smi,
            ..ProbeOptions::default()
        };
        let p = make_probe(&opts).unwrap();
        assert_eq!(p.device(), "nvidia:0");
        assert_eq!(p.read().await.unwrap().total_mib, 4);
        let auto = make_probe(&ProbeOptions {
            vendor: Vendor::Auto,
            ..opts
        })
        .unwrap();
        assert_eq!(auto.device(), "nvidia:0");
    }

    #[tokio::test]
    async fn command_probe_two_numbers() {
        let p = CommandProbe::new(sh("echo 100 2000")).unwrap();
        assert_eq!(p.device(), "command");
        assert_eq!(
            p.read().await.unwrap(),
            VramReading {
                used_mib: 100,
                total_mib: 2000,
                gtt_used_mib: None
            }
        );
        let comma = CommandProbe::new(sh("echo '7, 8'")).unwrap();
        assert_eq!(comma.read().await.unwrap().total_mib, 8);
    }

    #[tokio::test]
    async fn command_probe_three_numbers_include_gtt() {
        let p = CommandProbe::new(sh("echo 1,2,3")).unwrap();
        let r = p.read().await.unwrap();
        assert_eq!((r.used_mib, r.total_mib, r.gtt_used_mib), (1, 2, Some(3)));
    }

    #[tokio::test]
    async fn command_probe_garbage_is_parse_error() {
        for out in ["n/a", "1", "1 2 3 4", "", "1 x"] {
            let p = CommandProbe::new(sh(&format!("echo '{out}'"))).unwrap();
            let err = p.read().await.unwrap_err();
            assert!(matches!(err, ProbeError::Parse(_)), "{out:?}: {err:?}");
        }
    }

    #[tokio::test]
    async fn command_probe_nonzero_exit_is_error() {
        let p = CommandProbe::new(sh("echo 1 2; echo oops >&2; exit 4")).unwrap();
        let err = p.read().await.unwrap_err();
        assert!(matches!(err, ProbeError::Io(_)), "{err:?}");
        assert!(err.to_string().contains("oops"), "{err}");
    }

    #[tokio::test]
    async fn command_probe_missing_binary_is_io_error() {
        let p = CommandProbe::new(vec!["/nonexistent/probe".into()]).unwrap();
        assert!(matches!(p.read().await, Err(ProbeError::Io(_))));
    }

    #[test]
    fn command_probe_needs_argv() {
        assert!(CommandProbe::new(vec![]).is_err());
        let tmp = tempfile::tempdir().unwrap();
        let opts = amd_opts(tmp.path(), Vendor::Command);
        assert!(make_probe(&opts).is_err());
    }

    #[tokio::test]
    async fn auto_falls_back_to_command_or_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let mut opts = amd_opts(tmp.path(), Vendor::Auto);
        assert!(make_probe(&opts).is_err());
        opts.command = sh("echo 5 10");
        let p = make_probe(&opts).unwrap();
        assert_eq!(p.device(), "command");
        assert_eq!(p.read().await.unwrap().total_mib, 10);
    }

    #[tokio::test]
    async fn make_probe_command_vendor() {
        let tmp = tempfile::tempdir().unwrap();
        let mut opts = amd_opts(tmp.path(), Vendor::Command);
        opts.command = sh("echo 5 10 1");
        let p = make_probe(&opts).unwrap();
        assert_eq!(p.read().await.unwrap().gtt_used_mib, Some(1));
    }
}
