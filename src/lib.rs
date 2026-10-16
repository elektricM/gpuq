// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The gpud authors

//! gpud: one GPU, many workloads. See README.md.

pub mod api;
pub mod arbiter;
pub mod backend;
pub mod cli;
pub mod config;
pub mod gate;
pub mod probe;
pub mod workload;

/// VRAM amounts are MiB throughout.
pub type Mib = u64;
