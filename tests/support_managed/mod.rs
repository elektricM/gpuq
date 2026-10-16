// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The gpud authors

//! Fakes for the managed-service tests: a tiny HTTP server helper, a fake GPU service driven
//! by command-kind start/stop scripts, a fake ComfyUI, and a harness that runs
//! `workload::managed::run` against the in-memory arbiter rig.
#![allow(dead_code)]

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use gpud::config::{Flavor, Service, ServiceGate, ServiceKind, ServiceSpec};
use gpud::workload::managed;
use http_body_util::{BodyExt, Full};
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::time::sleep;

use crate::common::Rig;

/// Serve `f(method, path_and_query, body) -> (status, body)` on a fresh loopback port.
pub async fn serve<F, Fut>(f: F) -> SocketAddr
where
    F: Fn(String, String, Bytes) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = (u16, String)> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let f = f.clone();
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(move |req: Request<hyper::body::Incoming>| {
                    let f = f.clone();
                    async move {
                        let method = req.method().to_string();
                        let pq = req
                            .uri()
                            .path_and_query()
                            .map_or(String::new(), |p| p.as_str().to_owned());
                        let body = req.into_body().collect().await.unwrap().to_bytes();
                        let (status, text) = f(method, pq, body).await;
                        Ok::<_, std::convert::Infallible>(
                            Response::builder()
                                .status(status)
                                .body(Full::new(Bytes::from(text)))
                                .unwrap(),
                        )
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    addr
}

pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Poll until `pred` holds, up to 5 s.
pub async fn eventually(what: &str, mut pred: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    while !pred() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        sleep(Duration::from_millis(15)).await;
    }
}

pub fn http() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

// ------------------------------------------------------------------- generic fake service

/// A GPU service whose "up" state is a file the start and stop scripts create and remove.
/// While the file is missing it answers 503 to everything, like a stopped server would.
#[derive(Clone)]
pub struct FakeService {
    pub addr: SocketAddr,
    dir: Arc<tempfile::TempDir>,
    pub busy: Arc<AtomicBool>,
    pub status_hits: Arc<AtomicUsize>,
    pub active: Arc<AtomicUsize>,
    pub max_active: Arc<AtomicUsize>,
}

impl FakeService {
    pub async fn start() -> FakeService {
        let dir = Arc::new(tempfile::tempdir().unwrap());
        let up = dir.path().join("up");
        let busy = Arc::new(AtomicBool::new(false));
        let status_hits = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let (b, s, a, m) = (
            busy.clone(),
            status_hits.clone(),
            active.clone(),
            max_active.clone(),
        );
        let addr = serve(move |_method, pq, _body| {
            let (up, b, s, a, m) = (up.clone(), b.clone(), s.clone(), a.clone(), m.clone());
            async move {
                if !up.exists() {
                    return (503, "stopped".into());
                }
                let (path, query) = pq.split_once('?').unwrap_or((&pq, ""));
                match path {
                    "/work" => {
                        let ms: u64 = query
                            .strip_prefix("ms=")
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(0);
                        let now = a.fetch_add(1, Ordering::SeqCst) + 1;
                        m.fetch_max(now, Ordering::SeqCst);
                        sleep(Duration::from_millis(ms)).await;
                        a.fetch_sub(1, Ordering::SeqCst);
                        (200, "done".into())
                    }
                    "/status" => {
                        s.fetch_add(1, Ordering::SeqCst);
                        (200, "ok".into())
                    }
                    "/busy" if b.load(Ordering::SeqCst) => (200, json!({"jobs": [1]}).to_string()),
                    "/busy" => (200, json!({"jobs": []}).to_string()),
                    _ => (200, "ok".into()),
                }
            }
        })
        .await;
        FakeService {
            addr,
            dir,
            busy,
            status_hits,
            active,
            max_active,
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    pub fn is_up(&self) -> bool {
        self.file("up").exists()
    }

    fn log_count(&self, word: &str) -> usize {
        std::fs::read_to_string(self.file("log"))
            .unwrap_or_default()
            .lines()
            .filter(|l| *l == word)
            .count()
    }

    pub fn starts(&self) -> usize {
        self.log_count("start")
    }

    pub fn stops(&self) -> usize {
        self.log_count("stop")
    }

    /// Command control that flips the "up" file; `fail_start` makes the start script fail.
    pub fn spec(&self, fail_start: bool) -> ServiceSpec {
        let (up, log) = (self.file("up"), self.file("log"));
        let start = if fail_start {
            format!("echo start >> {}; exit 1", log.display())
        } else {
            format!("touch {}; echo start >> {}", up.display(), log.display())
        };
        let stop = format!("rm -f {}; echo stop >> {}", up.display(), log.display());
        ServiceSpec {
            kind: ServiceKind::Command {
                start: vec!["sh".into(), "-c".into(), start],
                stop: vec!["sh".into(), "-c".into(), stop],
                status: None,
            },
            health_url: (!fail_start).then(|| self.url("/health")),
            start_timeout: Duration::from_secs(3),
        }
    }
}

// ------------------------------------------------------------------- fake ComfyUI

#[derive(Default)]
pub struct ComfyState {
    pub queue: Vec<String>,
    pub running: Option<String>,
    pub freed: Vec<Value>,
    pub prompt_ids: Vec<String>,
    pub bodies: HashMap<String, Value>,
    pub busy_at_submit: Vec<bool>,
}

#[derive(Clone)]
pub struct FakeComfy {
    pub addr: SocketAddr,
    pub st: Arc<Mutex<ComfyState>>,
}

impl FakeComfy {
    pub async fn start(render: Duration) -> FakeComfy {
        let st = Arc::new(Mutex::new(ComfyState::default()));
        let s = st.clone();
        let addr = serve(move |method, pq, body| {
            let st = s.clone();
            async move { comfy_route(st, render, &method, &pq, &body) }
        })
        .await;
        FakeComfy { addr, st }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn ids(&self) -> Vec<String> {
        self.st.lock().unwrap().prompt_ids.clone()
    }

    pub fn count(&self, id: &str) -> usize {
        self.ids().iter().filter(|p| *p == id).count()
    }

    pub fn freed(&self) -> Vec<Value> {
        self.st.lock().unwrap().freed.clone()
    }
}

fn comfy_route(
    st: Arc<Mutex<ComfyState>>,
    render: Duration,
    method: &str,
    pq: &str,
    body: &[u8],
) -> (u16, String) {
    let path = pq.split('?').next().unwrap_or("");
    let json_body: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    match (method, path) {
        ("POST", "/prompt") => {
            let pid = json_body["prompt_id"].as_str().unwrap_or("").to_owned();
            if json_body["prompt"].get("bad").is_some() {
                return (400, json!({"error": "invalid prompt"}).to_string());
            }
            let mut s = st.lock().unwrap();
            s.prompt_ids.push(pid.clone());
            let busy = s.running.is_some() || !s.queue.is_empty();
            s.busy_at_submit.push(busy);
            s.bodies.insert(pid.clone(), json_body);
            s.queue.push(pid.clone());
            if s.running.is_none() && s.queue.len() == 1 {
                tokio::spawn(render_loop(st.clone(), render));
            }
            (
                200,
                json!({"prompt_id": pid, "number": 1, "node_errors": {}}).to_string(),
            )
        }
        ("POST", "/queue") => {
            let gone: Vec<String> = json_body["delete"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            st.lock().unwrap().queue.retain(|p| !gone.contains(p));
            (200, "{}".into())
        }
        ("GET", "/queue") => {
            let s = st.lock().unwrap();
            let running: Vec<Value> = s
                .running
                .iter()
                .map(|r| json!([0, r, {}, {}, []]))
                .collect();
            let pending: Vec<Value> = s
                .queue
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let b = &s.bodies[p];
                    // Like ComfyUI, hide the API key from the queue view.
                    let mut extra = b["extra_data"].clone();
                    if let Some(o) = extra.as_object_mut() {
                        o.remove("api_key_comfy_org");
                    }
                    json!([i + 1, p, b["prompt"], extra, []])
                })
                .collect();
            (
                200,
                json!({"queue_running": running, "queue_pending": pending}).to_string(),
            )
        }
        ("GET", "/api/jobs") => (
            200,
            json!({"jobs": [], "pagination": {"offset": 0, "limit": 50, "total": 0, "has_more": false}})
                .to_string(),
        ),
        ("POST", "/free") => {
            st.lock().unwrap().freed.push(json_body);
            (200, "{}".into())
        }
        _ => (404, "{}".into()),
    }
}

async fn render_loop(st: Arc<Mutex<ComfyState>>, render: Duration) {
    loop {
        {
            let mut s = st.lock().unwrap();
            if s.queue.is_empty() {
                return;
            }
            let next = s.queue.remove(0);
            s.running = Some(next);
        }
        sleep(render).await;
        st.lock().unwrap().running = None;
    }
}

// ------------------------------------------------------------------- harness

/// `managed::run` for one service on top of the in-memory arbiter rig.
pub struct Managed {
    pub rig: Rig,
    pub gate: String,
    pub http: reqwest::Client,
    stop: watch::Sender<bool>,
}

impl Drop for Managed {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

/// A service entry with the fields tests care about; the gate listens on a fresh port.
pub fn service(name: &str, backend: &str, vram: &str) -> Service {
    Service {
        name: name.into(),
        class: "interactive".into(),
        vram: vram.into(),
        idle_release: Duration::from_millis(400),
        gate: Some(ServiceGate {
            listen: format!("127.0.0.1:{}", free_port()),
            backend: backend.into(),
            ..ServiceGate::default()
        }),
        flavor: Flavor::Generic,
        ..Service::default()
    }
}

impl Managed {
    pub async fn start(rig: Rig, svc: Service) -> Managed {
        let gate = svc.gate.as_ref().map(|g| format!("http://{}", g.listen));
        let (stop, rx) = watch::channel(false);
        let handle = rig.handle.clone();
        tokio::spawn(async move {
            managed::run(vec![svc], handle, rx).await.unwrap();
        });
        let gate = gate.unwrap_or_default();
        if !gate.is_empty() {
            let addr = gate.trim_start_matches("http://").to_owned();
            eventually("gate to listen", || {
                std::net::TcpStream::connect(&addr).is_ok()
            })
            .await;
        }
        Managed {
            rig,
            gate,
            http: http(),
            stop,
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.gate)
    }

    pub async fn get(&self, path: &str) -> (u16, String) {
        let r = self.http.get(self.url(path)).send().await.unwrap();
        (r.status().as_u16(), r.text().await.unwrap())
    }

    /// Names of the leases the arbiter has granted.
    pub async fn lease_names(&self) -> Vec<String> {
        self.rig
            .status()
            .await
            .leases
            .into_iter()
            .map(|l| l.name)
            .collect()
    }

    pub async fn wait_lease(&self, name: &str, held: bool) {
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            let names = self.lease_names().await;
            if names.iter().any(|n| n == name) == held {
                return;
            }
            assert!(
                Instant::now() < end,
                "timed out waiting for lease {name} held={held}; leases={names:?}"
            );
            sleep(Duration::from_millis(15)).await;
        }
    }
}
