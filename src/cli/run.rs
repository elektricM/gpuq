// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! `gpud run` and `gpud hold`: keep a lease open for as long as the work lasts.
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr;
use std::time::{Duration, Instant};

use clap::Args;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use serde_json::{json, Value};
use tokio::process::Command;
use tokio::signal::unix::{signal, Signal as SigStream, SignalKind};

use super::client::{ClientError, Event, LeaseStream};
use super::{parse_vram, say, vram_mib_json, EX_UNAVAILABLE};

/// How often a lost lease connection is retried.
const READOPT_EVERY: Duration = Duration::from_secs(2);

#[derive(Args)]
pub struct RunArgs {
    /// Priority class from gpud's [classes] table.
    #[arg(long, default_value = "batch")]
    class: String,
    /// VRAM to reserve, e.g. 8G, 3000M or all.
    #[arg(long, default_value = "all")]
    vram: String,
    /// Lease name shown in `gpud status` (default: the command's basename).
    #[arg(long)]
    name: Option<String>,
    /// Signal the job when something more important needs the card.
    #[arg(long)]
    yieldable: bool,
    /// Signal sent on a yield request, e.g. SIGUSR1 or TERM. Default: gpud's
    /// `[run] yield_signal`, else SIGUSR1.
    #[arg(long, value_name = "SIGNAL")]
    yield_signal: Option<String>,
    /// If gpud is not running, run the command anyway.
    #[arg(long)]
    fallback: bool,
    /// The command, after `--`.
    #[arg(last = true)]
    cmd: Vec<String>,
}

#[derive(Args)]
pub struct HoldArgs {
    #[arg(long, default_value = "batch")]
    class: String,
    #[arg(long, default_value = "all")]
    vram: String,
    #[arg(long, default_value = "manual")]
    name: String,
}

/// SIGINT, SIGTERM and SIGHUP, which `run` forwards and `hold` treats as "release now".
struct Signals {
    int: SigStream,
    term: SigStream,
    hup: SigStream,
}

impl Signals {
    fn new() -> std::io::Result<Signals> {
        Ok(Signals {
            int: signal(SignalKind::interrupt())?,
            term: signal(SignalKind::terminate())?,
            hup: signal(SignalKind::hangup())?,
        })
    }

    async fn recv(&mut self) -> Signal {
        tokio::select! {
            _ = self.int.recv() => Signal::SIGINT,
            _ = self.term.recv() => Signal::SIGTERM,
            _ = self.hup.recv() => Signal::SIGHUP,
        }
    }
}

fn exit_for(sig: Signal) -> ExitCode {
    ExitCode::from(128u8.saturating_add(sig as i32 as u8))
}

/// Why no lease was obtained.
enum Fail {
    Down,
    Msg(String),
    Signal(Signal),
}

impl Fail {
    fn exit(self, addr: &str) -> ExitCode {
        match self {
            Fail::Down => {
                say(&format!("gpud is not running ({addr})"));
                ExitCode::from(EX_UNAVAILABLE)
            }
            Fail::Msg(m) => {
                say(&m);
                ExitCode::FAILURE
            }
            Fail::Signal(sig) => exit_for(sig),
        }
    }
}

/// A granted lease, held open as a stream and re-adopted if the connection drops.
struct Session {
    addr: String,
    body: Value,
    id: String,
    /// The yield signal gpud announced with the grant.
    yield_signal: Option<String>,
    stream: Option<LeaseStream>,
}

impl Session {
    /// Open the stream and wait for the grant, telling the user if it takes a while.
    async fn acquire(addr: &str, body: Value, sigs: &mut Signals) -> Result<Session, Fail> {
        let mut stream = LeaseStream::open(addr, &body).await.map_err(|e| match e {
            ClientError::Down(_) => Fail::Down,
            e => Fail::Msg(e.to_string()),
        })?;
        let t0 = Instant::now();
        let mut told = false;
        let notice = tokio::time::sleep(Duration::from_secs(1));
        tokio::pin!(notice);
        loop {
            tokio::select! {
                ev = stream.next() => match ev {
                    Ok(Some(Event::Granted(id, yield_signal))) => {
                        if told {
                            say(&format!("granted {id} after {:.0}s", t0.elapsed().as_secs_f64()));
                        }
                        return Ok(Session {
                            addr: addr.to_owned(),
                            body,
                            id,
                            yield_signal,
                            stream: Some(stream),
                        });
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => return Err(Fail::Msg(
                        "gpud closed the connection before granting the lease".into())),
                    Err(e) => return Err(Fail::Msg(e.to_string())),
                },
                _ = &mut notice, if !told => {
                    say("queued for the GPU (see `gpud status`)");
                    told = true;
                }
                sig = sigs.recv() => return Err(Fail::Signal(sig)),
            }
        }
    }

    /// Resolves when gpud asks the lease to yield. Meanwhile it reads pings and, if the
    /// connection is lost, re-adopts the lease with `resume_id` every 2 s. Cancel safe.
    async fn wait_yield(&mut self) {
        loop {
            if let Some(stream) = self.stream.as_mut() {
                match stream.next().await {
                    Ok(Some(Event::Yield)) => return,
                    Ok(Some(_)) => continue,
                    Ok(None) | Err(_) => {
                        self.stream = None;
                        say("lost gpud; re-adopting the lease");
                    }
                }
            }
            self.stream = Some(self.readopt().await);
        }
    }

    async fn readopt(&self) -> LeaseStream {
        let mut body = self.body.clone();
        body["resume_id"] = json!(self.id);
        loop {
            if let Ok(mut stream) = LeaseStream::open(&self.addr, &body).await {
                loop {
                    match stream.next().await {
                        Ok(Some(Event::Granted(..))) => {
                            say(&format!("re-adopted {}", self.id));
                            return stream;
                        }
                        Ok(Some(_)) => {}
                        _ => break,
                    }
                }
            }
            tokio::time::sleep(READOPT_EVERY).await;
        }
    }
}

/// `wait_yield` for a session that may not exist (fallback mode).
async fn yield_of(session: &mut Option<Session>) {
    match session {
        Some(s) => s.wait_yield().await,
        None => std::future::pending().await,
    }
}

fn lease_body(class: &str, vram: &str, name: &str, yieldable: bool) -> Result<Value, String> {
    let vram = parse_vram(vram)?;
    Ok(json!({
        "class": class,
        "vram_mib": vram_mib_json(vram),
        "yieldable": yieldable,
        "name": name,
        "pid": std::process::id(),
    }))
}

/// `SIGUSR1`, `USR1` and `usr1` all mean the same.
fn parse_signal(s: &str) -> Result<Signal, String> {
    let up = s.trim().to_ascii_uppercase();
    let name = if up.starts_with("SIG") {
        up
    } else {
        format!("SIG{up}")
    };
    Signal::from_str(&name).map_err(|_| format!("unknown signal '{s}'"))
}

/// A path that does not exist yet, where programs can poll for a yield request.
/// Kill the job if `gpud run` itself dies without cleaning up (kill -9, OOM killer): its lease
/// ends with our connection, so a job left running would use the card behind gpud's back.
/// The runtime is single-threaded, so the thread that forks lives as long as the process.
#[cfg(target_os = "linux")]
fn die_with_parent(cmd: &mut Command) {
    let parent = nix::unistd::getpid();
    // SAFETY: the closure only makes async-signal-safe system calls.
    unsafe {
        cmd.pre_exec(move || {
            nix::sys::prctl::set_pdeathsig(Signal::SIGKILL)?;
            // We may have died between fork and prctl.
            if nix::unistd::getppid() != parent {
                return Err(std::io::Error::from_raw_os_error(nix::libc::ESRCH));
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn die_with_parent(_cmd: &mut Command) {}

fn yield_file_path(lease_id: &str) -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|d| d.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    dir.join(format!("gpud-yield-{}-{lease_id}", std::process::id()))
}

pub async fn run(addr: &str, a: RunArgs) -> ExitCode {
    let Some(program) = a.cmd.first() else {
        say("nothing to run (use: gpud run [options] -- cmd ...)");
        return ExitCode::from(2);
    };
    // Check an explicit --yield-signal before queueing; the default comes with the grant.
    if let Some(Err(e)) = a.yield_signal.as_deref().map(parse_signal) {
        say(&e);
        return ExitCode::from(2);
    }
    let name = a.name.clone().unwrap_or_else(|| {
        Path::new(program)
            .file_name()
            .map_or_else(|| program.clone(), |n| n.to_string_lossy().into_owned())
    });
    let body = match lease_body(&a.class, &a.vram, &name, a.yieldable) {
        Ok(b) => b,
        Err(e) => {
            say(&e);
            return ExitCode::from(2);
        }
    };
    let mut sigs = match Signals::new() {
        Ok(s) => s,
        Err(e) => {
            say(&format!("cannot install signal handlers: {e}"));
            return ExitCode::FAILURE;
        }
    };

    let mut session = match Session::acquire(addr, body, &mut sigs).await {
        Ok(s) => Some(s),
        Err(Fail::Down) if a.fallback => {
            say(&format!(
                "gpud is not running ({addr}); running without a lease"
            ));
            None
        }
        Err(f) => return f.exit(addr),
    };
    let announced = session.as_ref().and_then(|s| s.yield_signal.clone());
    let yield_sig = a
        .yield_signal
        .clone()
        .or(announced)
        .map_or(Ok(Signal::SIGUSR1), |name| parse_signal(&name))
        .unwrap_or_else(|e| {
            say(&format!("{e}; using SIGUSR1"));
            Signal::SIGUSR1
        });

    let yield_file = session.as_ref().map(|s| yield_file_path(&s.id));
    let mut cmd = Command::new(program);
    cmd.args(&a.cmd[1..]);
    if let (Some(s), Some(f)) = (&session, &yield_file) {
        cmd.env("GPUD_LEASE_ID", &s.id).env("GPUD_YIELD_FILE", f);
    }
    die_with_parent(&mut cmd);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            say(&format!("cannot run '{program}': {e}"));
            return ExitCode::from(if e.kind() == std::io::ErrorKind::NotFound {
                127
            } else {
                126
            });
        }
    };
    let pid = child.id().map(|p| Pid::from_raw(p as i32));

    let status = loop {
        tokio::select! {
            st = child.wait() => break st,
            sig = sigs.recv() => {
                if let Some(pid) = pid {
                    let _ = kill(pid, sig);
                }
            }
            _ = yield_of(&mut session) => {
                if let Some(f) = &yield_file {
                    let _ = std::fs::write(f, b"");
                }
                if a.yieldable {
                    if let Some(pid) = pid {
                        let _ = kill(pid, yield_sig);
                        say(&format!("asked to yield: sent {yield_sig} to {pid}"));
                    }
                }
            }
        }
    };
    drop(session);
    if let Some(f) = &yield_file {
        let _ = std::fs::remove_file(f);
    }
    match status {
        Ok(st) => match (
            st.code(),
            std::os::unix::process::ExitStatusExt::signal(&st),
        ) {
            (Some(c), _) => ExitCode::from(c as u8),
            (None, Some(sig)) => ExitCode::from(128u8.saturating_add(sig as u8)),
            _ => ExitCode::FAILURE,
        },
        Err(e) => {
            say(&format!("lost the job: {e}"));
            ExitCode::FAILURE
        }
    }
}

pub async fn hold(addr: &str, a: HoldArgs) -> ExitCode {
    let body = match lease_body(&a.class, &a.vram, &a.name, false) {
        Ok(b) => b,
        Err(e) => {
            say(&e);
            return ExitCode::from(2);
        }
    };
    let mut sigs = match Signals::new() {
        Ok(s) => s,
        Err(e) => {
            say(&format!("cannot install signal handlers: {e}"));
            return ExitCode::FAILURE;
        }
    };
    let mut session = match Session::acquire(addr, body, &mut sigs).await {
        Ok(s) => s,
        Err(Fail::Signal(_)) => return ExitCode::SUCCESS,
        Err(f) => return f.exit(addr),
    };
    say(&format!(
        "holding the GPU as {}; Ctrl-C to release",
        session.id
    ));
    loop {
        tokio::select! {
            _ = sigs.recv() => return ExitCode::SUCCESS,
            _ = session.wait_yield() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_names() {
        for s in ["SIGUSR1", "USR1", "usr1", "sigusr1"] {
            assert_eq!(parse_signal(s), Ok(Signal::SIGUSR1));
        }
        assert_eq!(parse_signal("TERM"), Ok(Signal::SIGTERM));
        assert!(parse_signal("bogus").is_err());
    }
}
