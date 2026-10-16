// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! The `gpuq` command line: the daemon, and clients that talk to its control API.
//!
//! ```text
//! gpuq daemon [-c config] [-v]
//! gpuq check-config [-c config] [--strict]
//! gpuq status [--json]
//! gpuq run [--class C] [--vram 8G|all] [--card NAME|any] [--yieldable] [--fallback] [--name N] -- cmd ...
//! gpuq hold [--class C] [--vram all] [--card NAME|any] [--name N]
//! ```
//!
//! Clients find the daemon through `GPUQ_API` (default `127.0.0.1:9390`) and exit with 69
//! when nothing answers there.

mod client;
mod daemon;
mod run;
mod status;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};

use crate::arbiter::Vram;

/// Exit code (EX_UNAVAILABLE) when gpuq is not running.
pub const EX_UNAVAILABLE: u8 = 69;

#[derive(Parser)]
#[command(
    name = "gpuq",
    version,
    about = "GPU scheduler for Linux",
    long_about = "GPU scheduler for Linux. Programs request a GPU through gpuq, which grants it \
by priority class and available VRAM. To free a card, gpuq can signal running jobs, stop and \
start services, and unload models from LLM servers.",
    after_long_help = "Files:\n  \
/etc/gpuq/config.toml            system configuration\n  \
~/.config/gpuq/config.toml       user configuration, read when the system one is missing\n\n\
Environment:\n  \
GPUQ_API    address of the control API, used by the client commands\n  \
GPUQ_LOG    log filter for the daemon, e.g. debug or gpuq=trace (RUST_LOG also works)\n\n\
Jobs started by `gpuq run` get GPUQ_LEASE_ID, GPUQ_YIELD_FILE and GPUQ_CARD.\n\n\
Documentation: https://github.com/elektricM/gpuq"
)]
struct Cli {
    /// Address of the gpuq control API (default: $GPUQ_API, else 127.0.0.1:9390).
    #[arg(long, global = true, value_name = "HOST:PORT")]
    api: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon.
    Daemon {
        /// Config file (default: search the usual places).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Log at debug level (GPUQ_LOG overrides it).
        #[arg(short, long)]
        verbose: bool,
    },
    /// Load and validate the config, then exit.
    CheckConfig {
        /// Config file (default: search the usual places).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Also fail when the VRAM probe does not work.
        #[arg(long)]
        strict: bool,
    },
    /// Show the arbiter state, leases and queue.
    Status {
        /// Print the state as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Run a command under a GPU lease.
    Run(run::RunArgs),
    /// Hold a lease until Ctrl-C.
    Hold(run::HoldArgs),
}

/// The clap command, used to generate the man pages and shell completions.
pub fn command() -> clap::Command {
    Cli::command()
}

pub fn main() -> ExitCode {
    let cli = Cli::parse();
    let api = cli
        .api
        .or_else(|| std::env::var("GPUQ_API").ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "127.0.0.1:9390".to_owned());
    match cli.cmd {
        Cmd::Daemon { config, verbose } => daemon::run_daemon(config, verbose),
        Cmd::CheckConfig { config, strict } => daemon::check_config(config, strict),
        Cmd::Status { json } => block_on(status::status(&api, json)),
        Cmd::Run(args) => block_on(run::run(&api, args)),
        Cmd::Hold(args) => block_on(run::hold(&api, args)),
    }
}

fn block_on(fut: impl std::future::Future<Output = ExitCode>) -> ExitCode {
    match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt.block_on(fut),
        Err(e) => {
            say(&format!("cannot start the runtime: {e}"));
            ExitCode::FAILURE
        }
    }
}

fn say(msg: &str) {
    eprintln!("gpuq: {msg}");
}

/// `8G`, `3000M`, `512` (MiB) or `all`; see [`crate::config::parse_vram`].
pub fn parse_vram(s: &str) -> Result<Vram, String> {
    crate::config::parse_vram(s).map_err(|e| e.to_string())
}

/// The request value for `vram_mib`: -1 means the whole card.
fn vram_mib_json(v: Vram) -> i64 {
    match v {
        Vram::WholeCard => -1,
        Vram::Mib(m) => i64::try_from(m).unwrap_or(i64::MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units() {
        assert_eq!(parse_vram("8G"), Ok(Vram::Mib(8192)));
        assert_eq!(parse_vram("8GiB"), Ok(Vram::Mib(8192)));
        assert_eq!(parse_vram("8gb"), Ok(Vram::Mib(8192)));
        assert_eq!(parse_vram("1.5g"), Ok(Vram::Mib(1536)));
        assert_eq!(parse_vram("3000M"), Ok(Vram::Mib(3000)));
        assert_eq!(parse_vram("3000mib"), Ok(Vram::Mib(3000)));
        assert_eq!(parse_vram("512"), Ok(Vram::Mib(512)));
        assert_eq!(parse_vram(" 512 "), Ok(Vram::Mib(512)));
        assert_eq!(parse_vram("ALL"), Ok(Vram::WholeCard));
    }

    #[test]
    fn garbage() {
        for bad in ["lots", "", "g", "-1", "8x", "nan", "1.5.2g"] {
            assert!(parse_vram(bad).is_err(), "{bad:?} should not parse");
        }
    }
}
