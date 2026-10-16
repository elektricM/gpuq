// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! Test doubles for the Immich adapter: a fake Immich API on a loopback port, a fake docker
//! script, and a harness that runs `workload::immich::run` against the in-memory arbiter rig.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use gpud::config::Config;
use gpud::workload::immich::{self, ImmichView};
use http_body_util::{BodyExt, Full};
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::sleep;

use crate::common::Rig;

pub const SERVICE: &str = "ml-gpu";
pub const CONTAINER: &str = "ml_gpu_container";
pub const QUEUES: [&str; 3] = ["smartSearch", "faceDetection", "ocr"];
/// Concurrency the fake Immich starts with.
pub const OLD_CONCURRENCY: [(&str, u64); 3] =
    [("smartSearch", 2), ("faceDetection", 2), ("ocr", 1)];
pub const GPU_CONCURRENCY: [(&str, u64); 3] =
    [("smartSearch", 8), ("faceDetection", 4), ("ocr", 2)];

// ------------------------------------------------------------------------ fake Immich

#[derive(Clone, Debug, Default)]
pub struct Queue {
    pub waiting: u64,
    pub active: u64,
    pub delayed: u64,
    pub paused: u64,
    pub is_paused: bool,
}

pub struct Inner {
    /// The only key the server accepts.
    pub key: String,
    pub queues: BTreeMap<String, Queue>,
    pub concurrency: BTreeMap<String, u64>,
    /// Every state-changing call, in order: `pause NAME`, `resume NAME`, `config`.
    pub events: Vec<(Instant, String)>,
    pub config_puts: Vec<Value>,
    pub unauthorized: usize,
    pub queue_gets: usize,
    /// Each queue listing takes one active job off every queue.
    pub drain_on_get: bool,
}

#[derive(Clone)]
pub struct FakeImmich {
    pub addr: SocketAddr,
    pub st: Arc<Mutex<Inner>>,
}

impl FakeImmich {
    pub async fn start(key: &str) -> FakeImmich {
        let mut queues: BTreeMap<String, Queue> = QUEUES
            .iter()
            .map(|q| (q.to_string(), Queue::default()))
            .collect();
        // A queue gpud does not manage; its backlog must never count.
        queues.insert(
            "thumbnailGeneration".into(),
            Queue {
                waiting: 9999,
                ..Queue::default()
            },
        );
        let st = Arc::new(Mutex::new(Inner {
            key: key.to_owned(),
            queues,
            concurrency: OLD_CONCURRENCY
                .iter()
                .map(|(k, v)| (k.to_string(), *v))
                .collect(),
            events: Vec::new(),
            config_puts: Vec::new(),
            unauthorized: 0,
            queue_gets: 0,
            drain_on_get: false,
        }));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shared = st.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let st = shared.clone();
                tokio::spawn(async move {
                    let svc =
                        hyper::service::service_fn(move |req: Request<hyper::body::Incoming>| {
                            let st = st.clone();
                            async move {
                                let method = req.method().to_string();
                                let path = req.uri().path().to_owned();
                                let key = req
                                    .headers()
                                    .get("x-api-key")
                                    .and_then(|v| v.to_str().ok())
                                    .unwrap_or("")
                                    .to_owned();
                                let body = req.into_body().collect().await.unwrap().to_bytes();
                                let (code, text) = route(&st, &method, &path, &key, &body);
                                Ok::<_, std::convert::Infallible>(
                                    Response::builder()
                                        .status(code)
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
        FakeImmich { addr, st }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.st.lock().unwrap()
    }

    /// Put `n` waiting jobs on smartSearch and none anywhere else.
    pub fn set_backlog(&self, n: u64) {
        let mut s = self.lock();
        for q in QUEUES {
            let paused = s.queues[q].is_paused;
            s.queues.insert(
                q.to_owned(),
                Queue {
                    is_paused: paused,
                    ..Queue::default()
                },
            );
        }
        s.queues.get_mut("smartSearch").unwrap().waiting = n;
    }

    pub fn set_active(&self, n: u64) {
        self.lock().queues.get_mut("faceDetection").unwrap().active = n;
    }

    pub fn set_drain_on_get(&self, on: bool) {
        self.lock().drain_on_get = on;
    }

    pub fn set_key(&self, key: &str) {
        self.lock().key = key.to_owned();
    }

    pub fn set_paused(&self, paused: bool) {
        for q in QUEUES {
            self.lock().queues.get_mut(q).unwrap().is_paused = paused;
        }
    }

    pub fn all_paused(&self) -> bool {
        let s = self.lock();
        QUEUES.iter().all(|q| s.queues[*q].is_paused)
    }

    pub fn none_paused(&self) -> bool {
        let s = self.lock();
        QUEUES.iter().all(|q| !s.queues[*q].is_paused)
    }

    pub fn concurrency_is(&self, want: &[(&str, u64)]) -> bool {
        let s = self.lock();
        want.iter().all(|(q, n)| s.concurrency[*q] == *n)
    }

    pub fn events(&self) -> Vec<String> {
        self.lock().events.iter().map(|(_, e)| e.clone()).collect()
    }

    pub fn event_count(&self) -> usize {
        self.lock().events.len()
    }

    pub fn config_put_count(&self) -> usize {
        self.lock().config_puts.len()
    }

    /// When the first `config` write happened.
    pub fn first_config_put(&self) -> Option<Instant> {
        self.lock()
            .events
            .iter()
            .find(|(_, e)| e == "config")
            .map(|(t, _)| *t)
    }

    pub fn config_puts(&self) -> Vec<Value> {
        self.lock().config_puts.clone()
    }

    pub fn unauthorized(&self) -> usize {
        self.lock().unauthorized
    }
}

fn route(st: &Mutex<Inner>, method: &str, path: &str, key: &str, body: &[u8]) -> (u16, String) {
    let mut s = st.lock().unwrap();
    if key != s.key {
        s.unauthorized += 1;
        return (401, json!({"message": "Invalid API key"}).to_string());
    }
    let body: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    match (method, path) {
        ("GET", "/api/queues") => {
            s.queue_gets += 1;
            let list: Vec<Value> = s
                .queues
                .iter()
                .map(|(name, q)| {
                    json!({
                        "name": name,
                        "isPaused": q.is_paused,
                        "statistics": {
                            "waiting": q.waiting, "active": q.active, "delayed": q.delayed,
                            "paused": q.paused, "completed": 5, "failed": 1
                        }
                    })
                })
                .collect();
            if s.drain_on_get {
                for q in s.queues.values_mut() {
                    q.active = q.active.saturating_sub(1);
                }
            }
            (200, Value::Array(list).to_string())
        }
        ("PUT", p) if p.starts_with("/api/queues/") => {
            let name = p.trim_start_matches("/api/queues/").to_owned();
            let paused = body["isPaused"].as_bool().unwrap();
            let Some(q) = s.queues.get_mut(&name) else {
                return (404, "{}".into());
            };
            q.is_paused = paused;
            let ev = format!("{} {name}", if paused { "pause" } else { "resume" });
            s.events.push((Instant::now(), ev));
            (200, json!({"name": name, "isPaused": paused}).to_string())
        }
        ("GET", "/api/admin/config") => {
            let job: serde_json::Map<String, Value> = s
                .concurrency
                .iter()
                .map(|(k, v)| (k.clone(), json!({"concurrency": v})))
                .collect();
            (
                200,
                json!({"job": job, "machineLearning": {"urls": ["http://x:3003"]}}).to_string(),
            )
        }
        ("PUT", "/api/admin/config") => {
            // The whole config comes back, including sections gpud does not touch.
            assert!(body["machineLearning"]["urls"].is_array(), "{body}");
            let job = body["job"].as_object().unwrap().clone();
            for (q, v) in job {
                s.concurrency.insert(q, v["concurrency"].as_u64().unwrap());
            }
            s.events.push((Instant::now(), "config".into()));
            s.config_puts.push(body.clone());
            (200, body.to_string())
        }
        _ => (404, "{}".into()),
    }
}

// ------------------------------------------------------------------------ fake docker

#[derive(Clone)]
pub struct FakeDocker {
    pub dir: PathBuf,
}

impl FakeDocker {
    /// Write the script into `dir/bin` and use `dir/docker` as its state directory.
    pub fn new(dir: &Path) -> FakeDocker {
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        // Run through `sh`, so the script needs no exec bit.
        std::fs::write(bin.join("docker.sh"), include_str!("docker.sh")).unwrap();
        let state = dir.join("docker");
        std::fs::create_dir_all(&state).unwrap();
        FakeDocker { dir: state }
    }

    /// The `docker = [...]` argv prefix for the config.
    pub fn argv(&self) -> Vec<String> {
        let script = self.dir.parent().unwrap().join("bin/docker.sh");
        vec![
            "sh".into(),
            script.display().to_string(),
            self.dir.display().to_string(),
        ]
    }

    fn file(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    pub fn log(&self) -> Vec<String> {
        std::fs::read_to_string(self.file("log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The `docker compose ...` calls only.
    pub fn compose_calls(&self) -> Vec<String> {
        self.log()
            .into_iter()
            .filter(|l| l.starts_with("compose "))
            .collect()
    }

    pub fn calls(&self, line: &str) -> usize {
        self.log().iter().filter(|l| *l == line).count()
    }

    pub fn up_line() -> String {
        format!("compose --profile gpu up -d {SERVICE}")
    }

    pub fn stop_line() -> String {
        format!("compose --profile gpu stop {SERVICE}")
    }

    pub fn health_line() -> String {
        format!("inspect -f {{{{.State.Health.Status}}}} {CONTAINER}")
    }

    pub fn is_running(&self) -> bool {
        self.file("running").exists()
    }

    pub fn set_running(&self, on: bool) {
        if on {
            std::fs::write(self.file("running"), "").unwrap();
        } else {
            let _ = std::fs::remove_file(self.file("running"));
        }
    }

    pub fn set_health(&self, status: &str) {
        std::fs::write(self.file("health"), format!("{status}\n")).unwrap();
    }

    pub fn fail(&self, what: &str, on: bool) {
        let f = self.file(&format!("fail_{what}"));
        if on {
            std::fs::write(f, "").unwrap();
        } else {
            let _ = std::fs::remove_file(f);
        }
    }

    pub fn cwd(&self) -> String {
        std::fs::read_to_string(self.file("cwd"))
            .unwrap_or_default()
            .trim()
            .to_owned()
    }
}

// ------------------------------------------------------------------------ harness

/// `[immich]` TOML with short intervals; `over` replaces or adds keys (values are TOML).
pub fn immich_toml(
    url: &str,
    key_file: &Path,
    state_file: &Path,
    dir: &Path,
    docker: &FakeDocker,
    over: &[(&str, &str)],
) -> String {
    let argv: Vec<String> = docker.argv().iter().map(|a| format!("{a:?}")).collect();
    let mut kv: BTreeMap<String, String> = [
        ("url", format!("{url:?}")),
        ("key_file", format!("{:?}", key_file.display().to_string())),
        (
            "state_file",
            format!("{:?}", state_file.display().to_string()),
        ),
        ("compose_dir", format!("{:?}", dir.display().to_string())),
        ("service", format!("{SERVICE:?}")),
        ("container", format!("{CONTAINER:?}")),
        ("docker", format!("[{}]", argv.join(", "))),
        ("class", "\"background\"".into()),
        ("vram", "\"6G\"".into()),
        ("poll_s", "0.05".into()),
        ("min_backlog", "100".into()),
        ("cpu_ok_backlog", "20".into()),
        ("health_timeout_s", "3".into()),
        ("settle_s", "0.1".into()),
        ("drain_poll_s", "0.02".into()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    for (k, v) in over {
        kv.insert((*k).to_owned(), (*v).to_owned());
    }
    let mut out = String::from("[immich]\n");
    for (k, v) in kv {
        out.push_str(&format!("{k} = {v}\n"));
    }
    out
}

pub struct Harness {
    pub rig: Rig,
    pub immich: FakeImmich,
    pub docker: FakeDocker,
    pub view: Arc<ImmichView>,
    pub dir: tempfile::TempDir,
    pub key_file: PathBuf,
    pub state_file: PathBuf,
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl Harness {
    /// Everything but the adapter is made; the test can still change the fakes and files, then
    /// calls `start`.
    pub async fn prepare(rig: Rig) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let key_file = dir.path().join("api-key");
        let state_file = dir.path().join("state/immich.json");
        std::fs::write(&key_file, "secret\n").unwrap();
        let docker = FakeDocker::new(dir.path());
        let immich = FakeImmich::start("secret").await;
        let (stop, _) = watch::channel(false);
        Harness {
            rig,
            immich,
            docker,
            view: ImmichView::new(),
            dir,
            key_file,
            state_file,
            stop,
            task: None,
        }
    }

    pub fn config_text(&self, over: &[(&str, &str)]) -> String {
        immich_toml(
            &self.immich.url(),
            &self.key_file,
            &self.state_file,
            self.dir.path(),
            &self.docker,
            over,
        )
    }

    pub fn start(&mut self, over: &[(&str, &str)]) {
        let cfg = Config::from_toml_str(&self.config_text(over))
            .unwrap()
            .immich
            .unwrap();
        let (arbiter, rx, view) = (
            self.rig.handle.clone(),
            self.stop.subscribe(),
            self.view.clone(),
        );
        self.task = Some(tokio::spawn(async move {
            immich::run(cfg, arbiter, rx, view).await.unwrap();
        }));
    }

    /// Ask the adapter to stop and wait for it (up to 10 s).
    pub async fn shutdown(&mut self) {
        let _ = self.stop.send(true);
        if let Some(t) = self.task.take() {
            tokio::time::timeout(Duration::from_secs(10), t)
                .await
                .expect("adapter did not stop")
                .unwrap();
        }
    }

    pub fn state_paused(&self) -> Option<bool> {
        let text = std::fs::read_to_string(&self.state_file).ok()?;
        serde_json::from_str::<Value>(&text).ok()?["paused_by_gpud"].as_bool()
    }

    pub fn view_state(&self) -> String {
        self.view.json()["state"].as_str().unwrap().to_owned()
    }

    pub async fn lease_names(&self) -> Vec<String> {
        self.rig
            .status()
            .await
            .leases
            .into_iter()
            .map(|l| l.name)
            .collect()
    }

    pub async fn queued_names(&self) -> Vec<String> {
        self.rig
            .status()
            .await
            .queue
            .into_iter()
            .map(|q| q.name)
            .collect()
    }

    pub async fn has_lease(&self) -> bool {
        self.lease_names().await.iter().any(|n| n == "immich-ml")
    }

    /// Wait until the adapter serves: queues resumed on the GPU concurrency.
    pub async fn wait_serving(&self) {
        let (i, v) = (self.immich.clone(), self.view.clone());
        eventually("serving", move || {
            i.concurrency_is(&GPU_CONCURRENCY) && i.none_paused() && v.json()["state"] == "serving"
        })
        .await;
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}

/// Poll until `pred` holds, up to 8 s.
pub async fn eventually(what: &str, mut pred: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(8);
    while !pred() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        sleep(Duration::from_millis(10)).await;
    }
}

/// Poll an async condition, up to 8 s.
pub async fn eventually_async<F, Fut>(what: &str, mut pred: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let end = Instant::now() + Duration::from_secs(8);
    while !pred().await {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        sleep(Duration::from_millis(10)).await;
    }
}
