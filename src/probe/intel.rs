// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The gpud authors

//! Intel GPUs are not supported yet.
//!
//! i915 and xe do not expose a stable, documented used/total VRAM file in sysfs that we can
//! rely on across kernel versions (xe's per-tile files and i915's lmem counters moved
//! between releases), so guessing a path would give wrong numbers silently. Until that is
//! confirmed on real hardware, users run `vendor = "command"` with their own script.

use async_trait::async_trait;

use super::{ProbeError, VramProbe, VramReading};

/// Never constructed today: `new` always fails. The trait impl keeps `make_probe` uniform
/// for the day a real reader lands.
#[derive(Debug)]
pub struct IntelProbe;

impl IntelProbe {
    pub fn new() -> Result<Self, ProbeError> {
        Err(ProbeError::NotFound(
            "Intel GPUs are not supported yet; use vendor = \"command\" with a script that \
             prints used and total MiB"
                .into(),
        ))
    }
}

#[async_trait]
impl VramProbe for IntelProbe {
    fn device(&self) -> String {
        "intel".into()
    }

    async fn read(&self) -> Result<VramReading, ProbeError> {
        Self::new().map(|_| VramReading::default())
    }
}
