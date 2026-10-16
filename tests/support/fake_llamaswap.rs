// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! A fake llama-swap (with llama-server slots) on a real socket, plus a probe for its VRAM.
//! Ports tests/fakes.py. Include with `#[path = "support/fake_llamaswap.rs"] mod fake_llamaswap;`.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use gpuq::probe::{ProbeError, VramProbe, VramReading};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Frame, Incoming};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

pub const TOTAL_MIB: u64 = 24560;
pub const FLOOR_MIB: u64 = 50;

pub const MODELS_YAML: &str = r#"
models:
  "big":
    cmd: |
      llama-server -m big.gguf --cache-ram 28672 --slot-save-path /x
    ttl: 0
  "other":
    cmd: |
      llama-server -m other.gguf
    aliases: ["other-alias"]
  "rec":
    cmd: |
      llama-server -m rec.gguf
  "embed":
    cmd: |
      env HIP_VISIBLE_DEVICES=-1 ROCR_VISIBLE_DEVICES=-1 llama-server -m e.gguf -ngl 0
"#;

pub fn size_of(model: &str) -> u64 {
    match model {
        "big" => 22000,
        "other" => 20000,
        "rec" => 21000,
        _ => 0,
    }
}

pub struct State {
    pub loaded: BTreeSet<String>,
    /// VRAM a "job" is using, MiB.
    pub extra: u64,
    pub slot_tokens: u64,
    pub saved: BTreeMap<String, u64>,
    pub events: Vec<String>,
    pub xff: Vec<String>,
    pub hosts: Vec<String>,
    pub load_delay: Duration,
    /// Health requests hang forever.
    pub hang_health: bool,
    /// The next N health requests answer 503.
    pub health_503s: usize,
    pub health_calls: usize,
    /// Slot calls answer 500.
    pub slots_fail: bool,
    /// Chat streams started, and how many of them ended because the reader went away.
    pub chats_started: usize,
    pub chats_aborted: usize,
    pub chats_finished: usize,
}

type Out = UnsyncBoxBody<Bytes, Infallible>;

#[derive(Clone)]
pub struct FakeLlamaSwap {
    pub addr: SocketAddr,
    state: Arc<Mutex<State>>,
    _stop: Arc<oneshot::Sender<()>>,
}

impl FakeLlamaSwap {
    pub async fn start() -> Self {
        let state = Arc::new(Mutex::new(State {
            loaded: BTreeSet::new(),
            extra: 0,
            slot_tokens: 0,
            saved: BTreeMap::new(),
            events: Vec::new(),
            xff: Vec::new(),
            hosts: Vec::new(),
            load_delay: Duration::from_millis(50),
            hang_health: false,
            health_503s: 0,
            health_calls: 0,
            slots_fail: false,
            chats_started: 0,
            chats_aborted: 0,
            chats_finished: 0,
        }));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind fake");
        let addr = listener.local_addr().expect("fake addr");
        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        let st = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut stop_rx => return,
                    r = listener.accept() => {
                        let Ok((stream, _)) = r else { continue };
                        let st = st.clone();
                        tokio::spawn(async move {
                            let svc = hyper::service::service_fn(move |req| {
                                let st = st.clone();
                                async move { Ok::<_, Infallible>(route(st, req).await) }
                            });
                            let _ = hyper::server::conn::http1::Builder::new()
                                .serve_connection(TokioIo::new(stream), svc)
                                .with_upgrades()
                                .await;
                        });
                    }
                }
            }
        });
        FakeLlamaSwap {
            addr,
            state,
            _stop: Arc::new(stop_tx),
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    pub fn events(&self) -> Vec<String> {
        self.lock().events.clone()
    }

    pub fn has_event(&self, ev: &str) -> bool {
        self.lock().events.iter().any(|e| e == ev)
    }

    pub fn loaded(&self) -> BTreeSet<String> {
        self.lock().loaded.clone()
    }

    pub fn is_loaded(&self, model: &str) -> bool {
        self.lock().loaded.contains(model)
    }

    pub fn set_job_vram(&self, mib: u64) {
        self.lock().extra = mib;
    }

    /// Bring a model up instantly.
    pub fn preload(&self, model: &str) {
        load_now(&mut self.lock(), model);
    }

    pub fn probe(&self) -> Arc<FakeLsProbe> {
        Arc::new(FakeLsProbe {
            state: self.state.clone(),
        })
    }
}

/// Reads the fake's VRAM use; no sysfs.
pub struct FakeLsProbe {
    state: Arc<Mutex<State>>,
}

#[async_trait]
impl VramProbe for FakeLsProbe {
    fn device(&self) -> String {
        "fake:0".into()
    }

    async fn read(&self) -> Result<VramReading, ProbeError> {
        let s = self.state.lock().unwrap();
        let used = FLOOR_MIB + s.loaded.iter().map(|m| size_of(m)).sum::<u64>() + s.extra;
        Ok(VramReading {
            used_mib: used,
            total_mib: TOTAL_MIB,
            gtt_used_mib: Some(0),
        })
    }
}

fn load_now(s: &mut State, model: &str) {
    // llama-swap's exclusive group: loading one GPU model evicts the other GPU models.
    if size_of(model) > 0 {
        s.loaded.retain(|m| size_of(m) == 0);
    }
    s.loaded.insert(model.to_owned());
    s.events.push(format!("load {model}"));
    if model == "big" {
        s.slot_tokens = 0;
    }
}

async fn load(st: &Arc<Mutex<State>>, model: &str) {
    let model = match model {
        "other-alias" => "other",
        m => m,
    };
    let delay = {
        let s = st.lock().unwrap();
        if s.loaded.contains(model) {
            return;
        }
        s.load_delay
    };
    tokio::time::sleep(delay).await;
    let mut s = st.lock().unwrap();
    if !s.loaded.contains(model) {
        load_now(&mut s, model);
    }
}

fn json_out(status: StatusCode, v: Value) -> Response<Out> {
    let mut r = Response::new(
        Full::new(Bytes::from(v.to_string()))
            .map_err(|n: Infallible| match n {})
            .boxed_unsync(),
    );
    *r.status_mut() = status;
    r.headers_mut()
        .insert("content-type", "application/json".parse().unwrap());
    r
}

/// Body fed from a channel, so the test controls when each frame goes out.
struct ChanBody(mpsc::Receiver<Bytes>);

impl Body for ChanBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        self.0.poll_recv(cx).map(|o| o.map(|b| Ok(Frame::data(b))))
    }
}

async fn route(st: Arc<Mutex<State>>, req: Request<Incoming>) -> Response<Out> {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let query = req.uri().query().unwrap_or("").to_owned();
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (&method, segs.as_slice()) {
        (&Method::GET, ["running"]) => {
            let s = st.lock().unwrap();
            let running: Vec<Value> = s
                .loaded
                .iter()
                .map(|m| json!({"model": m, "state": "ready", "ttl": 0}))
                .collect();
            json_out(StatusCode::OK, json!({ "running": running }))
        }
        (&Method::POST, ["api", "models", "unload"]) => {
            let mut s = st.lock().unwrap();
            s.events.push("unload".into());
            s.loaded.clear();
            json_out(StatusCode::OK, json!({"msg": "ok"}))
        }
        (_, ["upstream", model, tail @ ..]) => upstream(st, req, model, tail, &query).await,
        (&Method::POST, ["v1", "chat", "completions"]) => chat(st, req).await,
        (&Method::POST, ["v1", "embeddings"]) => {
            let body = req.into_body().collect().await.map(|b| b.to_bytes());
            let v: Value = serde_json::from_slice(&body.unwrap_or_default()).unwrap_or_default();
            let model = v["model"].as_str().unwrap_or("").to_owned();
            st.lock().unwrap().events.push(format!("embed {model}"));
            json_out(StatusCode::OK, json!({"data": [{"embedding": [0.1]}]}))
        }
        (&Method::GET, ["ws"]) => echo_upgrade(req),
        _ => json_out(StatusCode::NOT_FOUND, json!({"error": "nope"})),
    }
}

async fn upstream(
    st: Arc<Mutex<State>>,
    req: Request<Incoming>,
    model: &str,
    tail: &[&str],
    query: &str,
) -> Response<Out> {
    match tail {
        ["health"] => {
            let (hang, fail) = {
                let mut s = st.lock().unwrap();
                s.health_calls += 1;
                let fail = s.health_503s > 0;
                s.health_503s = s.health_503s.saturating_sub(1);
                (s.hang_health, fail)
            };
            if hang {
                std::future::pending::<()>().await;
            }
            if fail {
                return json_out(
                    StatusCode::SERVICE_UNAVAILABLE,
                    json!({"status": "loading"}),
                );
            }
            load(&st, model).await;
            json_out(StatusCode::OK, json!({"status": "ok"}))
        }
        ["slots", "0"] => {
            load(&st, model).await;
            let body = req.into_body().collect().await.map(|b| b.to_bytes());
            let v: Value = serde_json::from_slice(&body.unwrap_or_default()).unwrap_or_default();
            let file = v["filename"].as_str().unwrap_or("").to_owned();
            let mut s = st.lock().unwrap();
            if s.slots_fail {
                return json_out(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "boom"}));
            }
            if query.contains("action=save") {
                let n = s.slot_tokens;
                s.saved.insert(file, n);
                s.events.push(format!("save {n}"));
                json_out(StatusCode::OK, json!({"n_saved": n, "n_written": n * 1000}))
            } else {
                let n = s.saved.get(&file).copied().unwrap_or(0);
                s.slot_tokens = n;
                s.events.push(format!("restore {n}"));
                json_out(StatusCode::OK, json!({"n_restored": n, "n_read": n * 1000}))
            }
        }
        _ => json_out(StatusCode::NOT_FOUND, json!({"error": "nope"})),
    }
}

/// SSE: `n` numbered frames `delay` apart, then `[DONE]`. Request body: {model, n, delay}
/// with `delay` in seconds.
async fn chat(st: Arc<Mutex<State>>, req: Request<Incoming>) -> Response<Out> {
    let xff = req
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let host = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let body = req.into_body().collect().await.map(|b| b.to_bytes());
    let v: Value = serde_json::from_slice(&body.unwrap_or_default()).unwrap_or_default();
    let model = v["model"].as_str().unwrap_or("").to_owned();
    let n = v["n"].as_u64().unwrap_or(3);
    let delay = Duration::from_secs_f64(v["delay"].as_f64().unwrap_or(0.01));
    {
        let mut s = st.lock().unwrap();
        s.xff.push(xff);
        s.hosts.push(host);
    }
    load(&st, &model).await;
    {
        let mut s = st.lock().unwrap();
        if model == "big" {
            s.slot_tokens += 100;
        }
        s.events.push(format!("chat {model}"));
        s.chats_started += 1;
    }
    let (tx, rx) = mpsc::channel::<Bytes>(1);
    tokio::spawn(async move {
        for i in 0..n {
            let frame = Bytes::from(format!("data: {}\n\n", json!({ "i": i })));
            if tx.send(frame).await.is_err() {
                st.lock().unwrap().chats_aborted += 1;
                return;
            }
            tokio::time::sleep(delay).await;
        }
        if tx
            .send(Bytes::from_static(b"data: [DONE]\n\n"))
            .await
            .is_err()
        {
            st.lock().unwrap().chats_aborted += 1;
            return;
        }
        st.lock().unwrap().chats_finished += 1;
    });
    let mut resp = Response::new(ChanBody(rx).boxed_unsync());
    resp.headers_mut()
        .insert("content-type", "text/event-stream".parse().unwrap());
    resp
}

/// `GET /ws`: switch protocols and echo bytes back.
fn echo_upgrade(mut req: Request<Incoming>) -> Response<Out> {
    let on = hyper::upgrade::on(&mut req);
    tokio::spawn(async move {
        let Ok(up) = on.await else { return };
        let mut io = TokioIo::new(up);
        let mut buf = [0u8; 256];
        while let Ok(n) = io.read(&mut buf).await {
            if n == 0 || io.write_all(&buf[..n]).await.is_err() {
                break;
            }
        }
    });
    let mut resp = Response::new(
        Full::new(Bytes::new())
            .map_err(|n: Infallible| match n {})
            .boxed_unsync(),
    );
    *resp.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    resp.headers_mut()
        .insert("connection", "upgrade".parse().unwrap());
    resp.headers_mut()
        .insert("upgrade", "echo".parse().unwrap());
    resp
}
