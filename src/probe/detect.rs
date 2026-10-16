//! Pick a probe from the GPU options. Independent of the config module.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::amd::{find_amd_device, AmdSysfsProbe, DEFAULT_SYSFS_ROOT};
use super::command::CommandProbe;
use super::intel::IntelProbe;
use super::nvidia::NvidiaSmiProbe;
use super::{ProbeError, VramProbe};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Vendor {
    #[default]
    Auto,
    Amd,
    Nvidia,
    Intel,
    Command,
}

#[derive(Clone, Debug)]
pub struct ProbeOptions {
    pub vendor: Vendor,
    /// PCI address for AMD; empty means find the card with the most VRAM.
    pub pci: String,
    pub sysfs_root: PathBuf,
    pub nvidia_index: u32,
    pub nvidia_smi: String,
    /// Argv for `Vendor::Command`.
    pub command: Vec<String>,
}

impl Default for ProbeOptions {
    fn default() -> Self {
        Self {
            vendor: Vendor::Auto,
            pci: String::new(),
            sysfs_root: PathBuf::from(DEFAULT_SYSFS_ROOT),
            nvidia_index: 0,
            nvidia_smi: "nvidia-smi".into(),
            command: Vec::new(),
        }
    }
}

fn is_executable(p: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(p) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

/// Whether `name` resolves to an executable file (directly if it has a slash, else on PATH).
fn which(name: &str) -> bool {
    if name.contains('/') {
        return is_executable(Path::new(name));
    }
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|d| is_executable(&d.join(name))))
}

/// Build the probe for `opts`. `Auto` tries AMD (explicit pci or a found device), then
/// nvidia-smi on PATH, then a configured command, then fails.
pub fn make_probe(opts: &ProbeOptions) -> Result<Arc<dyn VramProbe>, ProbeError> {
    let mut vendor = opts.vendor;
    if vendor == Vendor::Auto {
        vendor = if !opts.pci.is_empty() || find_amd_device(&opts.sysfs_root).is_some() {
            Vendor::Amd
        } else if which(&opts.nvidia_smi) {
            Vendor::Nvidia
        } else if !opts.command.is_empty() {
            Vendor::Command
        } else {
            return Err(ProbeError::NotFound(
                "no supported GPU found; set gpu.vendor and its options".into(),
            ));
        };
    }
    match vendor {
        Vendor::Nvidia => Ok(Arc::new(NvidiaSmiProbe::new(
            opts.nvidia_smi.clone(),
            opts.nvidia_index,
        ))),
        Vendor::Command => Ok(Arc::new(CommandProbe::new(opts.command.clone())?)),
        Vendor::Intel => Ok(Arc::new(IntelProbe::new()?)),
        Vendor::Amd | Vendor::Auto => {
            let pci = if opts.pci.is_empty() {
                find_amd_device(&opts.sysfs_root)
            } else {
                Some(opts.pci.clone())
            };
            let pci = pci.ok_or_else(|| {
                ProbeError::NotFound(format!(
                    "no amdgpu device found under {}; set gpu.pci",
                    opts.sysfs_root.display()
                ))
            })?;
            Ok(Arc::new(AmdSysfsProbe::new(pci, &opts.sysfs_root)))
        }
    }
}
