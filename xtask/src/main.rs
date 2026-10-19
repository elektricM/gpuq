// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! Generates the man pages under `man/` and the shell completions under `completions/`
//! from the clap definitions.
//!
//! ```text
//! cargo xtask gen      write man/ and completions/
//! cargo xtask check    fail if the committed files differ from the generated ones
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap_complete::Shell;

fn main() -> ExitCode {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let files = match generate() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("xtask: {e}");
            return ExitCode::FAILURE;
        }
    };
    match mode.as_str() {
        "gen" => match write(&root, &files) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("xtask: {e}");
                ExitCode::FAILURE
            }
        },
        "check" => check(&root, &files),
        _ => {
            eprintln!("usage: cargo xtask gen|check");
            ExitCode::from(2)
        }
    }
}

/// Every generated file, by path relative to the repository root.
fn generate() -> std::io::Result<BTreeMap<PathBuf, Vec<u8>>> {
    let mut files = BTreeMap::new();
    let mut cmd = gpuq::cli::command();
    cmd.build();
    let version = cmd.get_version().unwrap_or_default().to_owned();
    let source = format!("gpuq {version}");

    files.insert(
        PathBuf::from("man/gpuq.1"),
        man_page(cmd.clone(), &source, ROOT_EXTRA)?,
    );
    for sub in cmd.get_subcommands().filter(|s| !s.is_hide_set()) {
        let name = format!("gpuq-{}", sub.get_name());
        let sub = sub
            .clone()
            .name(name.clone())
            .bin_name(name.replace('-', " "));
        files.insert(
            PathBuf::from(format!("man/{name}.1")),
            man_page(sub, &source, SUB_EXTRA)?,
        );
    }

    for (shell, file) in [
        (Shell::Bash, "completions/gpuq.bash"),
        (Shell::Zsh, "completions/_gpuq"),
        (Shell::Fish, "completions/gpuq.fish"),
    ] {
        let mut out = Vec::new();
        clap_complete::generate(shell, &mut cmd.clone(), "gpuq", &mut out);
        files.insert(PathBuf::from(file), out);
    }
    Ok(files)
}

/// The generated sections, without the after-help text (written for a terminal), followed
/// by `extra`, plain roff.
fn man_page(cmd: clap::Command, source: &str, extra: &str) -> std::io::Result<Vec<u8>> {
    let has_args = cmd.get_arguments().any(|a| !a.is_hide_set());
    let has_subs = cmd.has_subcommands();
    let man = clap_mangen::Man::new(cmd).source(source);
    let mut out = Vec::new();
    man.render_title(&mut out)?;
    man.render_name_section(&mut out)?;
    man.render_synopsis_section(&mut out)?;
    man.render_description_section(&mut out)?;
    if has_args {
        man.render_options_section(&mut out)?;
    }
    if has_subs {
        man.render_subcommands_section(&mut out)?;
    }
    out.extend_from_slice(extra.as_bytes());
    Ok(out)
}

const ROOT_EXTRA: &str = r#".SH FILES
.TP
\fI/etc/gpuq/config.toml\fR
System configuration.
.TP
\fI~/.config/gpuq/config.toml\fR
User configuration, read when the system one does not exist.
.SH ENVIRONMENT
.TP
\fBGPUQ_API\fR
Address of the control API used by the client commands. Default: 127.0.0.1:9390.
.TP
\fBGPUQ_LOG\fR
Log filter for the daemon, for example \fBdebug\fR. \fBRUST_LOG\fR is read when it is unset.
.PP
Commands started by \fBgpuq run\fR receive \fBGPUQ_LEASE_ID\fR, \fBGPUQ_YIELD_FILE\fR
and \fBGPUQ_CARD\fR, the card they got.
.SH EXIT STATUS
The client commands exit with 69 when the daemon does not answer.
.SH SEE ALSO
Configuration reference and integration guides: https://github.com/elektricM/gpuq
"#;

const SUB_EXTRA: &str = r#".SH SEE ALSO
\fBgpuq\fR(1)
"#;

fn write(root: &Path, files: &BTreeMap<PathBuf, Vec<u8>>) -> std::io::Result<()> {
    for (rel, data) in files {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, data)?;
        println!("wrote {}", rel.display());
    }
    Ok(())
}

fn check(root: &Path, files: &BTreeMap<PathBuf, Vec<u8>>) -> ExitCode {
    let mut stale = false;
    for (rel, data) in files {
        if std::fs::read(root.join(rel)).ok().as_ref() != Some(data) {
            eprintln!("{} is out of date", rel.display());
            stale = true;
        }
    }
    for dir in ["man", "completions"] {
        let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
            continue;
        };
        for e in entries.flatten() {
            let rel = Path::new(dir).join(e.file_name());
            if !files.contains_key(&rel) {
                eprintln!("{} is not generated any more", rel.display());
                stale = true;
            }
        }
    }
    if stale {
        eprintln!("run `cargo xtask gen` and commit the result");
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
