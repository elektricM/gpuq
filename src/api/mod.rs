// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! Control API: status, and leases bound to a streaming connection.
//!
//! * `GET /status`: arbiter status as JSON, plus whatever the [`StatusExtra`] adds.
//! * `GET /healthz`: `ok`.
//! * `POST /lease`: JSON request, NDJSON response. The lease lives exactly as long as the
//!   connection, so a crashed client can never leak the card. See `docs/lease-api.md`.
//!
//! Each lease stream is driven by one task that owns the pending acquire and then the
//! [`Lease`](crate::arbiter::Lease). The response body only holds the receiving end of a
//! channel plus a guard that aborts that task, so whenever hyper drops the body (client gone,
//! connection torn down) the waiter leaves the queue or the lease is released.
//!
//! Dead client detection: hyper reads the socket for EOF or a reset while a response is in
//! flight and drops the body at once, so a client that exits or crashes (the kernel closes
//! its socket) frees its lease within moments. As a backstop the keepalive ping is a write,
//! and the second write after the peer closed fails: worst case two ping intervals (30 s by
//! default). A peer that vanishes without closing (cable pulled, host frozen) is only
//! noticed when the kernel gives up on the TCP connection.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::header::CONTENT_TYPE;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinSet};
use tokio::time::{interval_at, Instant, MissedTickBehavior};

use crate::arbiter::{CardSel, Handle, LeaseRequest, Vram};

/// Default gap between keepalive pings on a lease stream.
pub const PING_INTERVAL: Duration = Duration::from_secs(15);

/// Largest accepted lease request body.
const MAX_REQUEST: usize = 64 * 1024;

type RespBody = BoxBody<Bytes, Infallible>;

/// Extra fields merged into the `/status` object, e.g. the backend's running models.
#[async_trait]
pub trait StatusExtra: Send + Sync + 'static {
    /// A JSON object; anything else is ignored.
    async fn extra(&self) -> Value;
}

/// Adds nothing.
pub struct NoExtra;

#[async_trait]
impl StatusExtra for NoExtra {
    async fn extra(&self) -> Value {
        Value::Null
    }
}

#[derive(Clone)]
pub struct Api {
    arbiter: Handle,
    extra: Arc<dyn StatusExtra>,
    ping: Duration,
    yield_signal: Option<String>,
}

impl Api {
    pub fn new(arbiter: Handle, extra_status: Arc<dyn StatusExtra>) -> Self {
        Api {
            arbiter,
            extra: extra_status,
            ping: PING_INTERVAL,
            yield_signal: None,
        }
    }

    /// Change the keepalive ping interval (tests use a short one).
    pub fn with_ping_interval(mut self, ping: Duration) -> Self {
        self.ping = ping;
        self
    }

    /// Signal name sent with every grant (`[run] yield_signal`), so `gpud run` uses it
    /// unless told otherwise.
    pub fn with_yield_signal(mut self, signal: impl Into<String>) -> Self {
        self.yield_signal = Some(signal.into());
        self
    }

    /// Accept connections until `shutdown` resolves. Open connections are then dropped,
    /// which releases their leases.
    pub async fn serve(
        self,
        listener: TcpListener,
        shutdown: impl std::future::Future<Output = ()>,
    ) {
        tokio::pin!(shutdown);
        let mut conns = JoinSet::new();
        loop {
            tokio::select! {
                _ = &mut shutdown => break,
                accepted = listener.accept() => {
                    let (stream, peer) = match accepted {
                        Ok(a) => a,
                        Err(e) => {
                            tracing::warn!("api: accept failed: {e}");
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            continue;
                        }
                    };
                    while conns.try_join_next().is_some() {}
                    let api = self.clone();
                    conns.spawn(async move { api.connection(stream, peer).await });
                }
            }
        }
    }

    async fn connection(self, stream: tokio::net::TcpStream, peer: SocketAddr) {
        let svc = service_fn(move |req| {
            let api = self.clone();
            async move { Ok::<_, Infallible>(api.route(req).await) }
        });
        let conn =
            hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), svc);
        if let Err(e) = conn.await {
            tracing::debug!("api: connection from {peer} ended: {e}");
        }
    }

    async fn route(&self, req: Request<Incoming>) -> Response<RespBody> {
        match (req.method(), req.uri().path()) {
            (&Method::GET, "/status") => self.status().await,
            (&Method::GET, "/healthz") => text(StatusCode::OK, "ok"),
            (&Method::POST, "/lease") => self.lease(req).await,
            (_, "/status" | "/healthz" | "/lease") => json_resp(
                StatusCode::METHOD_NOT_ALLOWED,
                json!({"error": "method not allowed"}),
            ),
            _ => json_resp(StatusCode::NOT_FOUND, json!({"error": "not found"})),
        }
    }

    async fn status(&self) -> Response<RespBody> {
        let st = match self.arbiter.status().await {
            Ok(st) => st,
            Err(e) => {
                return json_resp(
                    StatusCode::SERVICE_UNAVAILABLE,
                    json!({"error": e.to_string()}),
                )
            }
        };
        let mut v = match serde_json::to_value(&st) {
            Ok(v) => v,
            Err(e) => {
                return json_resp(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({"error": e.to_string()}),
                )
            }
        };
        if let (Some(obj), Value::Object(extra)) = (v.as_object_mut(), self.extra.extra().await) {
            obj.extend(extra);
        }
        json_resp(StatusCode::OK, v)
    }

    async fn lease(&self, req: Request<Incoming>) -> Response<RespBody> {
        let bad = |msg: String| {
            json_resp(
                StatusCode::BAD_REQUEST,
                json!({"error": format!("bad lease request: {msg}")}),
            )
        };
        let raw = match Limited::new(req.into_body(), MAX_REQUEST).collect().await {
            Ok(c) => c.to_bytes(),
            Err(e) => return bad(e.to_string()),
        };
        let lease_req = match parse_lease_request(&raw) {
            Ok(r) => r,
            Err(e) => return bad(e),
        };
        let classes = self.arbiter.classes();
        if !classes.contains(&lease_req.class) {
            let mut known: Vec<&str> = classes.names().collect();
            known.sort_by_key(|n| (classes.job_prio(n), *n));
            return json_resp(
                StatusCode::BAD_REQUEST,
                json!({"error": format!(
                    "unknown class '{}'; known: {}", lease_req.class, known.join(", "))}),
            );
        }
        if let Err(e) = self.arbiter.check_card(&lease_req.card) {
            return json_resp(StatusCode::BAD_REQUEST, json!({"error": e.to_string()}));
        }
        let (tx, rx) = mpsc::channel(8);
        let task = tokio::spawn(lease_stream(
            self.arbiter.clone(),
            lease_req,
            tx,
            self.ping,
            self.yield_signal.clone(),
        ));
        let body = LeaseBody {
            rx,
            task: task.abort_handle(),
        };
        let mut resp = Response::new(body.boxed());
        resp.headers_mut().insert(
            CONTENT_TYPE,
            "application/x-ndjson".parse().expect("static header"),
        );
        resp
    }
}

fn parse_lease_request(raw: &[u8]) -> Result<LeaseRequest, String> {
    let v: Value = serde_json::from_slice(raw).map_err(|e| e.to_string())?;
    let obj = v.as_object().ok_or("body must be a JSON object")?;
    let class = match obj.get("class") {
        None | Some(Value::Null) => "batch".to_owned(),
        Some(Value::String(s)) => s.clone(),
        Some(_) => return Err("class must be a string".into()),
    };
    let vram = match (obj.get("vram"), obj.get("vram_mib")) {
        (Some(Value::String(s)), _) => crate::cli::parse_vram(s)?,
        (Some(v), _) if !v.is_null() => return Err(format!("bad vram {v}")),
        (_, None | Some(Value::Null)) => Vram::WholeCard,
        (_, Some(Value::Number(n))) => match n.as_i64() {
            Some(-1) => Vram::WholeCard,
            Some(m) if m >= 0 => Vram::Mib(m as u64),
            _ => return Err(format!("bad vram_mib {n}")),
        },
        (_, Some(v)) => return Err(format!("bad vram_mib {v}")),
    };
    let pid = match obj.get("pid") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(|p| u32::try_from(p).ok())
                .ok_or_else(|| format!("bad pid {v}"))?,
        ),
    };
    let resume_id = match obj.get("resume_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::String(_)) => None,
        Some(v) => return Err(format!("bad resume_id {v}")),
    };
    let card = match obj.get("card") {
        None | Some(Value::Null) => CardSel::Primary,
        Some(Value::String(s)) if s == "any" => CardSel::Any,
        Some(Value::String(s)) if s == "primary" => CardSel::Primary,
        Some(Value::String(s)) if !s.is_empty() => CardSel::Named(s.clone()),
        Some(v) => return Err(format!("card must be a card name or \"any\", not {v}")),
    };
    Ok(LeaseRequest {
        class,
        vram,
        yieldable: obj
            .get("yieldable")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        name: obj
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        pid,
        card,
        resume_id,
    })
}

fn json_resp(status: StatusCode, v: Value) -> Response<RespBody> {
    let mut resp = Response::new(Full::new(Bytes::from(v.to_string())).boxed());
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        CONTENT_TYPE,
        "application/json".parse().expect("static header"),
    );
    resp
}

fn text(status: StatusCode, s: &'static str) -> Response<RespBody> {
    let mut resp = Response::new(Full::new(Bytes::from_static(s.as_bytes())).boxed());
    *resp.status_mut() = status;
    resp
}

fn line(v: Value) -> Bytes {
    let mut s = v.to_string();
    s.push('\n');
    Bytes::from(s)
}

/// Send one event; false when the client side is gone.
async fn emit(tx: &mpsc::Sender<Bytes>, v: Value) -> bool {
    tokio::select! {
        r = tx.send(line(v)) => r.is_ok(),
        _ = tx.closed() => false,
    }
}

/// Everything that happens on one lease connection. Returning drops the waiter or the lease.
async fn lease_stream(
    arbiter: Handle,
    req: LeaseRequest,
    tx: mpsc::Sender<Bytes>,
    ping: Duration,
    yield_signal: Option<String>,
) {
    if !emit(&tx, json!({"event": "queued"})).await {
        return;
    }
    let mut tick = interval_at(Instant::now() + ping, ping);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let acquire = arbiter.acquire(req);
    tokio::pin!(acquire);
    let lease = loop {
        tokio::select! {
            r = &mut acquire => match r {
                Ok(lease) => break lease,
                Err(_) => return,
            },
            _ = tick.tick() => {
                if !emit(&tx, json!({"event": "ping"})).await {
                    return;
                }
            }
            _ = tx.closed() => return,
        }
    };
    let env: serde_json::Map<String, Value> = lease
        .env()
        .iter()
        .map(|(k, v)| (k.clone(), json!(v)))
        .collect();
    let mut granted = json!({
        "event": "granted",
        "id": lease.id(),
        "card": lease.card(),
        "env": env,
    });
    if let Some(sig) = &yield_signal {
        granted["yield_signal"] = json!(sig);
    }
    if !emit(&tx, granted).await {
        return;
    }
    let mut yielded = false;
    loop {
        tokio::select! {
            _ = lease.yield_wanted(), if !yielded => {
                yielded = true;
                if !emit(&tx, json!({"event": "yield"})).await {
                    return;
                }
            }
            _ = tick.tick() => {
                if !emit(&tx, json!({"event": "ping"})).await {
                    return;
                }
            }
            _ = tx.closed() => return,
        }
    }
}

/// Response body of a lease stream. Dropping it aborts the driving task.
struct LeaseBody {
    rx: mpsc::Receiver<Bytes>,
    task: AbortHandle,
}

impl Body for LeaseBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        self.rx.poll_recv(cx).map(|o| o.map(|b| Ok(Frame::data(b))))
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::default()
    }
}

impl Drop for LeaseBody {
    fn drop(&mut self) {
        self.task.abort();
    }
}
