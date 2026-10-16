// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! Managed GPU services (ComfyUI, an ML server, a TTS server...): gpud holds a lease for them
//! while they work, starts and stops them, and can gate their HTTP port.
//!
//! Each service is one state machine (`machine.rs`) in its own task. With a `[service.gate]`
//! gpud listens on `gate.listen` and proxies to `gate.backend`:
//!
//! - A request whose path starts with one of `gpu_paths` (any path if the list is empty) needs
//!   the card. Without a lease it is held until the lease is granted and the service is up;
//!   with one it goes straight through and counts as in flight until its response is done.
//! - Other requests pass untouched while the service is up, and get a 503 while it is stopped,
//!   so a status page never wakes the GPU.
//! - After `idle_release_s` without in-flight GPU requests (and with `busy_url`, if set, not
//!   reporting work) the service is stopped (`stop_on_release` and a `control`) and the lease
//!   dropped. When the arbiter asks the lease to yield, new GPU requests are held, running
//!   work finishes, the service is stopped (if it has a `control`) and the lease released;
//!   held requests then queue for a new lease.
//! - A service without a gate has no trigger: it acquires its lease at startup, keeps it, and
//!   gives it up (stopping the service) only when asked to yield; then it queues again.
//!
//! The `comfyui` flavor adds ComfyUI's prompt queue handling, see `comfy.rs`.

mod comfy;
mod machine;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use hyper::body::Incoming;
use hyper::header::{HeaderValue, CONTENT_TYPE};
use hyper::{Request, Response, StatusCode};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;

use crate::arbiter::{Handle, Vram};
use crate::config::{Flavor, Service, ServiceKind as Kind, ServiceSpec};
use crate::gate::proxy::{
    full, is_upgrade, read_body, serve_fn, BodyError, Keepalive, Proxy, ProxyBody, MAX_BODY,
};
use crate::workload::service::{Engine, ServiceControl};
use machine::{Msg, View};

/// Everything the machine and the gate need from a `[[service]]` entry.
pub(crate) struct Cfg {
    name: String,
    class: String,
    vram: Vram,
    yieldable: bool,
    control: Option<ServiceControl>,
    health_url: Option<String>,
    start_timeout: Duration,
    stop_on_release: bool,
    idle_release: Duration,
    gated: bool,
    busy_url: Option<String>,
    backend: String,
    comfy: bool,
    /// How often the machine looks at idleness, `busy_url` and yield progress.
    poll: Duration,
}

impl Cfg {
    fn new(svc: &Service) -> anyhow::Result<Cfg> {
        let vram = svc
            .vram()
            .with_context(|| format!("service {}: vram", svc.name))?;
        let gate = svc.gate.as_ref();
        let comfy = svc.flavor == Flavor::Comfyui && gate.is_some();
        let backend = gate.map(|g| g.backend.clone()).unwrap_or_default();
        let mut busy_url = gate.and_then(|g| g.busy_url.clone());
        if comfy && busy_url.is_none() {
            busy_url = Some(format!("{}/queue", backend.trim_end_matches('/')));
        }
        Ok(Cfg {
            name: svc.name.clone(),
            class: svc.class.clone(),
            vram,
            yieldable: svc.yieldable,
            control: svc.control.as_ref().map(control_of),
            health_url: svc.control.as_ref().and_then(|c| c.health_url.clone()),
            start_timeout: svc
                .control
                .as_ref()
                .map_or(Duration::from_secs(120), |c| c.start_timeout),
            stop_on_release: svc.stop_on_release,
            idle_release: svc.idle_release,
            gated: gate.is_some(),
            busy_url,
            backend,
            comfy,
            poll: (svc.idle_release / 4).clamp(Duration::from_millis(20), Duration::from_secs(1)),
        })
    }

    /// The service is down whenever it holds no lease.
    fn stops_when_idle(&self) -> bool {
        self.control.is_some() && self.stop_on_release
    }
}

fn control_of(spec: &ServiceSpec) -> ServiceControl {
    let c = match &spec.kind {
        Kind::Systemd { unit, user } => ServiceControl::systemd(unit.clone(), *user),
        Kind::Docker { container } => ServiceControl::container(Engine::Docker, container.clone()),
        Kind::Podman { container } => ServiceControl::container(Engine::Podman, container.clone()),
        Kind::Command {
            start,
            stop,
            status,
        } => ServiceControl::commands(start.clone(), stop.clone(), status.clone()),
    };
    c.with_poll_interval(Duration::from_millis(250))
}

/// Run every configured service until `shutdown` turns true.
pub async fn run(
    services: Vec<Service>,
    arbiter: Handle,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let mut tasks = JoinSet::new();
    for svc in &services {
        let cfg = Cfg::new(svc)?;
        let (tx, rx) = mpsc::unbounded_channel();
        let (view_tx, view_rx) = watch::channel(View::default());
        if let Some(gate) = &svc.gate {
            let (host, port) = gate
                .host_port()
                .map_err(|e| anyhow::anyhow!("service {}: {e}", svc.name))?;
            let listener = TcpListener::bind((host.as_str(), port))
                .await
                .with_context(|| {
                    format!("service {}: cannot listen on {}", svc.name, gate.listen)
                })?;
            let ctx = Arc::new(GateCtx {
                name: svc.name.clone(),
                paths: gate.gpu_paths.clone(),
                comfy: cfg.comfy,
                backend: gate.backend.trim_end_matches('/').to_owned(),
                proxy: Proxy::new(&gate.backend),
                http: reqwest::Client::builder()
                    .no_proxy()
                    .timeout(Duration::from_secs(30))
                    .build()
                    .unwrap_or_default(),
                tx: tx.clone(),
                view: view_rx,
            });
            let mut stop = shutdown.clone();
            tasks.spawn(async move {
                serve_fn(
                    listener,
                    async move {
                        let _ = stop.wait_for(|s| *s).await;
                    },
                    move |req, peer| {
                        let ctx = ctx.clone();
                        async move { ctx.handle(req, peer).await }
                    },
                )
                .await;
            });
            tracing::info!(
                "service {}: gate on {} -> {}",
                svc.name,
                gate.listen,
                gate.backend
            );
        }
        tasks.spawn(machine::run(
            cfg,
            arbiter.clone(),
            rx,
            tx,
            view_tx,
            shutdown.clone(),
        ));
    }
    let _ = shutdown.wait_for(|s| *s).await;
    while tasks.join_next().await.is_some() {}
    Ok(())
}

// -------------------------------------------------------------------------------- gate

struct GateCtx {
    name: String,
    paths: Vec<String>,
    comfy: bool,
    backend: String,
    proxy: Proxy,
    http: reqwest::Client,
    tx: mpsc::UnboundedSender<Msg>,
    view: watch::Receiver<View>,
}

fn json_response(status: StatusCode, v: &Value) -> Response<ProxyBody> {
    let mut r = full(status, v.to_string());
    r.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    r
}

impl GateCtx {
    fn is_gpu(&self, path: &str) -> bool {
        self.paths.is_empty() || self.paths.iter().any(|p| path.starts_with(p.as_str()))
    }

    fn up(&self) -> bool {
        self.view.borrow().up
    }

    fn stopped(&self) -> Response<ProxyBody> {
        full(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("gpud: {} is stopped; it starts on demand\n", self.name),
        )
    }

    /// Forward without taking the card, if the service is up.
    async fn pass(
        &self,
        parts: hyper::http::request::Parts,
        body: bytes::Bytes,
        peer: SocketAddr,
    ) -> Response<ProxyBody> {
        if self.up() {
            self.proxy.forward(parts, body, peer, None).await
        } else {
            self.stopped()
        }
    }

    async fn handle(&self, req: Request<Incoming>, peer: SocketAddr) -> Response<ProxyBody> {
        if is_upgrade(req.headers()) {
            return if self.up() {
                self.proxy.upgrade(req, peer).await
            } else {
                self.stopped()
            };
        }
        let (parts, body) = req.into_parts();
        let body = match read_body(body, MAX_BODY).await {
            Ok(b) => b,
            Err(BodyError::TooLarge) => {
                return full(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "gpud: request body too large\n".into(),
                )
            }
            Err(BodyError::Read(e)) => {
                return full(
                    StatusCode::BAD_REQUEST,
                    format!("gpud: cannot read body: {e}\n"),
                )
            }
        };
        if self.comfy {
            return self.comfy_request(parts, body, peer).await;
        }
        if !self.is_gpu(parts.uri.path()) {
            return self.pass(parts, body, peer).await;
        }
        let (reply, rx) = tokio::sync::oneshot::channel();
        if self.tx.send(Msg::Admit(reply)).is_err() {
            return self.stopped();
        }
        match rx.await {
            Ok(Ok(permit)) => {
                let guard: Keepalive = Box::new(permit);
                self.proxy.forward(parts, body, peer, Some(guard)).await
            }
            Ok(Err(why)) => full(StatusCode::SERVICE_UNAVAILABLE, format!("{why}\n")),
            Err(_) => self.stopped(),
        }
    }

    // ---------------------------------------------------------------- ComfyUI

    async fn comfy_request(
        &self,
        parts: hyper::http::request::Parts,
        body: bytes::Bytes,
        peer: SocketAddr,
    ) -> Response<ProxyBody> {
        let path = parts.uri.path().to_owned();
        let post = parts.method == hyper::Method::POST;
        let get = parts.method == hyper::Method::GET;
        if post && (path == "/prompt" || path == "/api/prompt") {
            // Anything but a JSON object is for ComfyUI to complain about.
            if let Ok(Value::Object(obj)) = serde_json::from_slice::<Value>(&body) {
                return self.hold_prompt(&parts, Value::Object(obj)).await;
            }
            return self.pass(parts, body, peer).await;
        }
        if get && path == "/queue" {
            return self.queue_view().await;
        }
        if get && path == "/api/jobs" {
            return self.jobs_view(&parts).await;
        }
        if get {
            if let Some(id) = path.strip_prefix("/api/jobs/") {
                if let Some(job) = self.job_of(id) {
                    return json_response(StatusCode::OK, &job);
                }
            }
        }
        if post && path == "/queue" {
            let data: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let all = data["clear"].as_bool().unwrap_or(false);
            let ids = strings(&data["delete"]);
            self.cancel(ids, all).await;
            if !self.up() {
                return json_response(StatusCode::OK, &json!({}));
            }
            return self.pass(parts, body, peer).await;
        }
        if post && path == "/api/jobs/cancel" {
            let data: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            if self.cancel(strings(&data["job_ids"]), false).await {
                return json_response(StatusCode::OK, &json!({"cancelled": true}));
            }
        }
        if post {
            if let Some(id) = path
                .strip_prefix("/api/jobs/")
                .and_then(|r| r.strip_suffix("/cancel"))
            {
                if self.cancel(vec![id.to_owned()], false).await {
                    return json_response(StatusCode::OK, &json!({"cancelled": true}));
                }
            }
        }
        self.pass(parts, body, peer).await
    }

    async fn hold_prompt(
        &self,
        parts: &hyper::http::request::Parts,
        body: Value,
    ) -> Response<ProxyBody> {
        let headers = ["authorization", "cookie"]
            .iter()
            .filter_map(|h| {
                let v = parts.headers.get(*h)?.to_str().ok()?;
                Some(((*h).to_owned(), v.to_owned()))
            })
            .collect();
        let (reply, rx) = tokio::sync::oneshot::channel();
        let sent = self.tx.send(Msg::Prompt {
            body,
            headers,
            reply,
        });
        let answer = if sent.is_ok() { rx.await.ok() } else { None };
        match answer {
            Some((id, number)) => json_response(
                StatusCode::OK,
                &json!({"prompt_id": id, "number": number, "node_errors": {}}),
            ),
            None => self.stopped(),
        }
    }

    async fn cancel(&self, ids: Vec<String>, all: bool) -> bool {
        if ids.is_empty() && !all {
            return false;
        }
        let (reply, rx) = tokio::sync::oneshot::channel();
        if self.tx.send(Msg::Cancel { ids, all, reply }).is_err() {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    fn job_of(&self, id: &str) -> Option<Value> {
        let view = self.view.borrow();
        if let Some((_, why)) = view.failed.iter().find(|(f, _)| f == id) {
            return Some(failed_job(id, why));
        }
        view.held.iter().find(|h| h.id == id).map(held_job)
    }

    /// ComfyUI's own JSON, or `fallback` when it is down (it may be restarting).
    async fn backend_json(&self, path_and_query: &str, fallback: Value) -> Value {
        let url = format!("{}{path_and_query}", self.backend);
        match self.http.get(url).send().await {
            Ok(r) if r.status().is_success() => r.json().await.unwrap_or(fallback),
            _ => fallback,
        }
    }

    async fn queue_view(&self) -> Response<ProxyBody> {
        let mut q = self
            .backend_json("/queue", json!({"queue_running": [], "queue_pending": []}))
            .await;
        if !q["queue_pending"].is_array() {
            q["queue_pending"] = json!([]);
        }
        let held: Vec<Value> = self
            .view
            .borrow()
            .held
            .iter()
            .map(|h| json!([h.number, h.id, h.body["prompt"], h.body["extra_data"], []]))
            .collect();
        if let Some(p) = q["queue_pending"].as_array_mut() {
            p.extend(held);
        }
        json_response(StatusCode::OK, &q)
    }

    async fn jobs_view(&self, parts: &hyper::http::request::Parts) -> Response<ProxyBody> {
        let pq = parts
            .uri
            .path_and_query()
            .map_or("/api/jobs", |p| p.as_str());
        let mut data = self
            .backend_json(
                pq,
                json!({"jobs": [], "pagination": {"offset": 0, "limit": 0, "total": 0, "has_more": false}}),
            )
            .await;
        let query: HashMap<String, String> = parts
            .uri
            .query()
            .unwrap_or("")
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        let wanted: Vec<&str> = query
            .get("status")
            .map_or(vec!["pending", "failed"], |s| s.split(',').collect());
        let mut extra: Vec<Value> = Vec::new();
        {
            let view = self.view.borrow();
            if wanted.contains(&"pending") {
                extra.extend(view.held.iter().map(held_job));
            }
            if wanted.contains(&"failed") {
                extra.extend(view.failed.iter().map(|(id, why)| failed_job(id, why)));
            }
        }
        if !extra.is_empty() && data["jobs"].is_array() {
            let total = data["pagination"]["total"].as_u64().unwrap_or(0) + extra.len() as u64;
            if let Some(jobs) = data["jobs"].as_array_mut() {
                extra.append(jobs);
                *jobs = extra;
            }
            data["pagination"]["total"] = json!(total);
        }
        json_response(StatusCode::OK, &data)
    }
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn held_job(h: &comfy::HeldView) -> Value {
    let extra = &h.body["extra_data"];
    let mut job = json!({
        "id": h.id,
        "status": "pending",
        "priority": h.number,
        "create_time": extra.get("create_time").cloned().unwrap_or(json!(h.since_ms)),
        "outputs_count": 0,
        "previewable_outputs_count": 0,
    });
    if let Some(wid) = extra["extra_pnginfo"]["workflow"]["id"].as_str() {
        job["workflow_id"] = json!(wid);
    }
    job
}

fn failed_job(id: &str, why: &str) -> Value {
    json!({
        "id": id,
        "status": "failed",
        "priority": 0,
        "outputs_count": 0,
        "previewable_outputs_count": 0,
        "execution_error": {"exception_message": why, "exception_type": "gpud submit error"},
    })
}
