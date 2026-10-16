// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The gpud authors

//! `gpud daemon` and `gpud check-config`: build everything from the config and run it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinSet;

use super::say;
use crate::api::{Api, StatusExtra};
use crate::arbiter::{self, OffloadHook};
use crate::backend::llama_server::LlamaServer;
use crate::backend::llamaswap::LlamaSwap;
use crate::backend::models::ModelTable;
use crate::backend::none::NoBackend;
use crate::backend::ollama::Ollama;
use crate::backend::stopstart::StopStart;
use crate::backend::Backend;
use crate::config::{Config, ConfigError, GpuVendor, LlmBackend};
use crate::gate::{Gate, GateOptions};
use crate::probe::detect::{make_probe, ProbeOptions, Vendor};
use crate::probe::VramProbe;
use crate::workload::hook::CommandHook;
use crate::workload::managed;
use crate::workload::service::ServiceControl;

/// How long the tasks get to finish after a shutdown signal.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// Every problem in a config error, one per line.
fn problems(e: &ConfigError) -> Vec<String> {
    match e {
        ConfigError::Invalid(items) => items.clone(),
        other => other.to_string().lines().map(str::to_owned).collect(),
    }
}

fn report_config_error(e: &ConfigError) {
    for p in problems(e) {
        say(&p);
    }
}

fn probe_options(cfg: &Config) -> ProbeOptions {
    let g = &cfg.gpu;
    ProbeOptions {
        vendor: match g.vendor {
            GpuVendor::Auto => Vendor::Auto,
            GpuVendor::Amd => Vendor::Amd,
            GpuVendor::Nvidia => Vendor::Nvidia,
            GpuVendor::Intel => Vendor::Intel,
            GpuVendor::Command => Vendor::Command,
        },
        pci: g.pci.clone(),
        sysfs_root: g.sysfs_root.clone(),
        nvidia_index: g.nvidia_index,
        nvidia_smi: g.nvidia_smi.clone(),
        command: g.command.clone(),
    }
}

/// `gpud check-config`.
pub fn check_config(path: Option<PathBuf>, strict: bool) -> ExitCode {
    let cfg = match Config::load(path.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            report_config_error(&e);
            return ExitCode::FAILURE;
        }
    };
    match Config::resolve_path(path.as_deref()) {
        Ok(Some(p)) => println!("config: {}", p.display()),
        _ => println!("config: built-in defaults"),
    }
    print!("{}", cfg.describe());
    let reading = match make_probe(&probe_options(&cfg)) {
        Ok(probe) => block_on_value(async move {
            let device = probe.device();
            (device, probe.read().await.map_err(|e| e.to_string()))
        })
        .map_err(|e| e.to_string())
        .and_then(|(device, r)| r.map(|r| (device, r))),
        Err(e) => Err(e.to_string()),
    };
    match reading {
        Ok((device, r)) => {
            println!("probe: {device}, {} / {} MiB used", r.used_mib, r.total_mib);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("gpud: warning: VRAM probe failed: {e}");
            if strict {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
    }
}

fn block_on_value<T: Send + 'static>(
    fut: impl std::future::Future<Output = T> + Send + 'static,
) -> Result<T, std::io::Error> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map(|rt| rt.block_on(fut))
}

fn init_logging(verbose: bool) {
    use tracing_subscriber::EnvFilter;
    let default = if verbose { "debug" } else { "info" };
    let filter = ["GPUD_LOG", "RUST_LOG"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .and_then(|spec| EnvFilter::try_new(spec).ok())
        .unwrap_or_else(|| EnvFilter::new(default));
    let b = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_writer(std::io::stderr);
    // journald stamps its own time.
    let _ = if std::env::var_os("JOURNAL_STREAM").is_some() {
        b.compact().without_time().with_target(false).try_init()
    } else {
        b.try_init()
    };
}

/// The backend, plus the concrete llama-swap client when that is what it is.
struct Built {
    backend: Arc<dyn Backend>,
    swap: Option<Arc<LlamaSwap>>,
}

fn build_backend(cfg: &Config) -> Result<Built, String> {
    let l = &cfg.llm;
    let url = cfg.gate.backend.as_str();
    let model = if l.resident.is_empty() {
        "default"
    } else {
        l.resident.as_str()
    };
    let control = || {
        l.service
            .as_ref()
            .map(ServiceControl::from_spec)
            .ok_or_else(|| {
                format!(
                    "llm.backend = \"{}\" needs [llm.service]",
                    l.backend.as_str()
                )
            })
    };
    let start_timeout = l
        .service
        .as_ref()
        .map_or(l.load_timeout, |s| s.start_timeout.max(l.load_timeout));
    let plain = |backend: Arc<dyn Backend>| Built {
        backend,
        swap: None,
    };
    Ok(match l.backend {
        LlmBackend::LlamaSwap => {
            let table = ModelTable::new(&l.llama_swap_config)
                .with_cpu_models(l.cpu_models.iter().cloned())
                .with_slot_save_models(l.slot_save_models.iter().cloned())
                .with_cpu_markers(l.cpu_markers.iter().cloned());
            let swap =
                Arc::new(LlamaSwap::new(url, table, l.load_timeout).map_err(|e| e.to_string())?);
            Built {
                backend: swap.clone(),
                swap: Some(swap),
            }
        }
        // gpud decides when models go: keep them loaded instead of Ollama's 5 min default.
        LlmBackend::Ollama => plain(Arc::new(
            Ollama::new(url)
                .with_cpu_models(l.cpu_models.iter().cloned())
                .with_keep_alive(serde_json::json!(-1))
                .with_load_timeout(l.load_timeout),
        )),
        LlmBackend::LlamaServer => plain(Arc::new(
            LlamaServer::new(url, model, control()?, !l.slot_save_models.is_empty())
                .with_start_timeout(start_timeout),
        )),
        LlmBackend::Vllm => plain(Arc::new(
            StopStart::vllm(url, model, control()?).with_start_timeout(start_timeout),
        )),
        LlmBackend::Sglang => plain(Arc::new(
            StopStart::sglang(url, model, control()?).with_start_timeout(start_timeout),
        )),
        LlmBackend::None => plain(Arc::new(NoBackend)),
    })
}

/// `/status` extra: what the backend has loaded.
struct BackendStatus {
    backend: Arc<dyn Backend>,
    swap: Option<Arc<LlamaSwap>>,
}

#[async_trait]
impl StatusExtra for BackendStatus {
    async fn extra(&self) -> Value {
        let kind = self.backend.kind();
        let models = match &self.swap {
            Some(swap) => swap.running().await.map(|running| {
                running
                    .into_iter()
                    .map(|m| json!({"model": m.model, "state": m.state}))
                    .collect::<Vec<_>>()
            }),
            None => self.backend.loaded_gpu_models().await.map(|names| {
                names
                    .into_iter()
                    .map(|m| json!({"model": m, "state": "ready"}))
                    .collect()
            }),
        };
        match models {
            Ok(models) => json!({"backend": {"kind": kind, "models": models}}),
            Err(e) => json!({"backend": {"kind": kind, "error": e.to_string()}}),
        }
    }
}

async fn bind(what: &str, host: &str, port: u16) -> Result<TcpListener, String> {
    let host = match host {
        "*" => "0.0.0.0",
        h => h.trim_start_matches('[').trim_end_matches(']'),
    };
    let l = TcpListener::bind((host, port))
        .await
        .map_err(|e| format!("cannot listen on {host}:{port} for the {what}: {e}"))?;
    tracing::info!("{what}: listening on {host}:{port}");
    Ok(l)
}

/// Resolves once `rx` holds true (or the sender is gone).
async fn stopped(mut rx: watch::Receiver<bool>) {
    let _ = rx.wait_for(|v| *v).await;
}

/// `gpud daemon`.
pub fn run_daemon(path: Option<PathBuf>, verbose: bool) -> ExitCode {
    init_logging(verbose);
    let cfg = match Config::load(path.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            report_config_error(&e);
            return ExitCode::FAILURE;
        }
    };
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            say(&format!("cannot start the runtime: {e}"));
            return ExitCode::FAILURE;
        }
    };
    let code = match rt.block_on(serve(cfg, path.as_deref())) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            say(&msg);
            ExitCode::FAILURE
        }
    };
    rt.shutdown_timeout(Duration::from_secs(1));
    code
}

async fn serve(cfg: Config, path: Option<&Path>) -> Result<(), String> {
    let source = match Config::resolve_path(path) {
        Ok(Some(p)) => p.display().to_string(),
        _ => "built-in defaults".to_owned(),
    };
    tracing::info!(
        "gpud {} starting, config: {source}, backend: {}",
        env!("CARGO_PKG_VERSION"),
        cfg.llm.backend.as_str()
    );

    let probe: Arc<dyn VramProbe> =
        make_probe(&probe_options(&cfg)).map_err(|e| format!("no usable VRAM probe: {e}"))?;
    tracing::info!("vram probe: {}", probe.device());
    let Built { backend, swap } = build_backend(&cfg)?;
    let hook: Option<Arc<dyn OffloadHook>> = (!cfg.hooks.offload.is_empty()).then(|| {
        Arc::new(CommandHook::new(
            cfg.hooks.offload.clone(),
            cfg.hooks.offload_timeout,
        )) as Arc<dyn OffloadHook>
    });

    let gate_listener = match cfg.llm.backend {
        LlmBackend::None => None,
        _ => Some(bind("gate", &cfg.gate.host, cfg.gate.port).await?),
    };
    let api_listener = bind("api", &cfg.api.host, cfg.api.port).await?;

    let handle = arbiter::spawn(cfg.arbiter_settings(), backend.clone(), probe, hook);
    let (stop_tx, stop_rx) = watch::channel(false);
    let mut tasks = JoinSet::new();

    if let Some(listener) = gate_listener {
        let options = GateOptions {
            backend_url: cfg.gate.backend.clone(),
            model_classes: cfg
                .llm
                .model_classes
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<HashMap<_, _>>(),
        };
        let gate = Gate::new(options, backend.clone(), handle.clone());
        tasks.spawn(gate.serve(listener, stopped(stop_rx.clone())));
    }
    let extra = Arc::new(BackendStatus { backend, swap });
    let api = Api::new(handle.clone(), extra).with_yield_signal(cfg.run.yield_signal.clone());
    tasks.spawn(api.serve(api_listener, stopped(stop_rx.clone())));
    let services = cfg.service.clone();
    let managed_rx = stop_rx.clone();
    tasks.spawn(async move {
        if let Err(e) = managed::run(services, handle, managed_rx).await {
            tracing::error!("managed services stopped: {e}");
        }
    });

    sd_notify_ready();
    wait_for_signal().await;
    tracing::info!("shutting down");
    let _ = stop_tx.send(true);
    let drain = async { while tasks.join_next().await.is_some() {} };
    if tokio::time::timeout(SHUTDOWN_GRACE, drain).await.is_err() {
        tracing::warn!(
            "tasks still running after {}s; exiting anyway",
            SHUTDOWN_GRACE.as_secs()
        );
    }
    Ok(())
}

#[cfg(unix)]
async fn wait_for_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let (Ok(mut term), Ok(mut int), Ok(mut hup)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
        signal(SignalKind::hangup()),
    ) else {
        tracing::error!("cannot install signal handlers; waiting for ctrl-c only");
        let _ = tokio::signal::ctrl_c().await;
        return;
    };
    loop {
        tokio::select! {
            _ = term.recv() => return,
            _ = int.recv() => return,
            _ = hup.recv() => {
                tracing::warn!("config reload is not supported yet; restart gpud");
            }
        }
    }
}

#[cfg(not(unix))]
async fn wait_for_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

/// Tell systemd (`Type=notify`) that the listeners are up. Best effort.
fn sd_notify_ready() {
    #[cfg(unix)]
    if let Some(addr) = std::env::var_os("NOTIFY_SOCKET").filter(|v| !v.is_empty()) {
        if let Err(e) = notify(&addr, b"READY=1") {
            tracing::warn!("sd_notify failed: {e}");
        }
    }
}

#[cfg(unix)]
fn notify(addr: &std::ffi::OsStr, msg: &[u8]) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::net::UnixDatagram;
    let sock = UnixDatagram::unbound()?;
    let bytes = addr.as_bytes();
    if let Some(name) = bytes.strip_prefix(b"@") {
        #[cfg(target_os = "linux")]
        {
            use std::os::linux::net::SocketAddrExt;
            let a = std::os::unix::net::SocketAddr::from_abstract_name(name)?;
            sock.send_to_addr(msg, &a)?;
            return Ok(());
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = name;
            return Err(std::io::Error::other("abstract sockets need Linux"));
        }
    }
    sock.send_to(msg, Path::new(addr))?;
    Ok(())
}
