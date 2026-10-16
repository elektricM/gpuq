// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The gpud authors

//! Streaming reverse proxy with an optional guard held for the whole response.
//!
//! The gate uses it in front of llama-swap; managed services (ComfyUI and friends) can put
//! the same proxy in front of themselves. Forwarding goes through hyper-util's legacy client:
//! it streams the response body frame by frame, never decompresses, never follows redirects,
//! and hands out the upgraded connection for WebSocket tunnelling.

use std::any::Any;
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::header::{HeaderMap, HeaderName, HeaderValue, CONNECTION, HOST, UPGRADE};
use hyper::http::request::Parts;
use hyper::{Request, Response, StatusCode, Uri};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpListener;
use tokio::task::JoinSet;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type ProxyBody = UnsyncBoxBody<Bytes, BoxError>;

/// Largest request body the proxy buffers (vision and image requests are big).
pub const MAX_BODY: usize = 1024 * 1024 * 1024;

/// Hop-by-hop headers, plus host and content-length which are set per hop.
const HOP: [&str; 10] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
];

/// Anything dropped when the response body ends or is abandoned, e.g. an `LlmGuard`.
pub type Keepalive = Box<dyn Any + Send>;

pub fn full(status: StatusCode, text: String) -> Response<ProxyBody> {
    let mut resp = Response::new(
        Full::new(Bytes::from(text))
            .map_err(|never: Infallible| match never {})
            .boxed_unsync(),
    );
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

/// Response body that owns a [`Keepalive`] until the body ends, fails or is dropped. Dropping
/// the body is what happens when the client goes away mid-stream.
struct GuardedBody {
    inner: Incoming,
    guard: Option<Keepalive>,
}

impl Body for GuardedBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        let poll = Pin::new(&mut self.inner).poll_frame(cx);
        if matches!(poll, Poll::Ready(None | Some(Err(_)))) {
            self.guard = None;
        }
        poll
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[derive(Debug)]
pub enum BodyError {
    TooLarge,
    Read(String),
}

/// Buffer a request body, refusing more than `limit` bytes.
pub async fn read_body(body: Incoming, limit: usize) -> Result<Bytes, BodyError> {
    match Limited::new(body, limit).collect().await {
        Ok(c) => Ok(c.to_bytes()),
        Err(e)
            if e.downcast_ref::<http_body_util::LengthLimitError>()
                .is_some() =>
        {
            Err(BodyError::TooLarge)
        }
        Err(e) => Err(BodyError::Read(e.to_string())),
    }
}

pub fn is_upgrade(headers: &HeaderMap) -> bool {
    let has = |name: HeaderName| headers.get_all(name).iter().count() > 0;
    has(UPGRADE)
        && headers
            .get_all(CONNECTION)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .any(|v| {
                v.split(',')
                    .any(|t| t.trim().eq_ignore_ascii_case("upgrade"))
            })
}

pub struct Proxy {
    base: String,
    client: Client<HttpConnector, Full<Bytes>>,
}

impl Proxy {
    /// `backend_url` is the scheme and authority of the server behind the proxy.
    pub fn new(backend_url: &str) -> Self {
        let mut connector = HttpConnector::new();
        connector.set_nodelay(true);
        Proxy {
            base: backend_url.trim_end_matches('/').to_owned(),
            client: Client::builder(TokioExecutor::new())
                .pool_idle_timeout(Duration::from_secs(30))
                .build(connector),
        }
    }

    fn uri(&self, parts: &Parts) -> Result<Uri, hyper::http::uri::InvalidUri> {
        let pq = parts.uri.path_and_query().map_or("/", |p| p.as_str());
        format!("{}{pq}", self.base).parse()
    }

    /// Headers for the backend: hop-by-hop dropped, client Host kept, X-Forwarded-For appended.
    fn headers(&self, parts: &Parts, peer: SocketAddr, keep_upgrade: bool) -> HeaderMap {
        let mut out = HeaderMap::new();
        for (k, v) in &parts.headers {
            let hop = HOP.contains(&k.as_str());
            let upgrade_hdr = keep_upgrade && (k == CONNECTION || k == UPGRADE);
            if !hop || upgrade_hdr {
                out.append(k.clone(), v.clone());
            }
        }
        // Keep the client's Host: some backends (ComfyUI) reject requests whose Origin does not
        // match it. HTTP/1.0 clients may send none; the client then derives it from the URI.
        if let Some(host) = parts.headers.get(HOST) {
            out.insert(HOST, host.clone());
        }
        // llama-swap attributes activity per client from X-Forwarded-For; keep that working.
        let prior: Vec<&str> = parts
            .headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        let chain = if prior.is_empty() {
            peer.ip().to_string()
        } else {
            format!("{}, {}", prior.join(", "), peer.ip())
        };
        if let Ok(v) = HeaderValue::from_str(&chain) {
            out.insert("x-forwarded-for", v);
        }
        out
    }

    fn unreachable(e: &dyn std::error::Error) -> Response<ProxyBody> {
        let mut msg = e.to_string();
        let mut src = e.source();
        while let Some(s) = src {
            msg.push_str(": ");
            msg.push_str(&s.to_string());
            src = s.source();
        }
        full(
            StatusCode::BAD_GATEWAY,
            format!("gpud: backend unreachable: {msg}\n"),
        )
    }

    /// Forward a buffered request and stream the answer back. `guard` lives until the response
    /// body has been delivered or the client is gone.
    pub async fn forward(
        &self,
        parts: Parts,
        body: Bytes,
        peer: SocketAddr,
        guard: Option<Keepalive>,
    ) -> Response<ProxyBody> {
        let uri = match self.uri(&parts) {
            Ok(u) => u,
            Err(e) => {
                return full(
                    StatusCode::BAD_REQUEST,
                    format!("gpud: bad request uri: {e}\n"),
                )
            }
        };
        let mut req = Request::new(Full::new(body));
        *req.method_mut() = parts.method.clone();
        *req.uri_mut() = uri;
        *req.headers_mut() = self.headers(&parts, peer, false);
        match self.client.request(req).await {
            Ok(up) => {
                let (up_parts, incoming) = up.into_parts();
                let mut resp = Response::new(
                    GuardedBody {
                        inner: incoming,
                        guard,
                    }
                    .map_err(BoxError::from)
                    .boxed_unsync(),
                );
                *resp.status_mut() = up_parts.status;
                for (k, v) in &up_parts.headers {
                    // content-length stays: it matches the body and keeps HEAD replies right.
                    if k == "content-length" || !HOP.contains(&k.as_str()) {
                        resp.headers_mut().append(k.clone(), v.clone());
                    }
                }
                resp
            }
            Err(e) => Self::unreachable(&e),
        }
    }

    /// Tunnel an Upgrade request (WebSocket) to the backend, ungated. If the backend answers
    /// with anything but 101 that answer is relayed as a normal response.
    pub async fn upgrade(
        &self,
        mut req: Request<Incoming>,
        peer: SocketAddr,
    ) -> Response<ProxyBody> {
        let down = hyper::upgrade::on(&mut req);
        let (parts, _body) = req.into_parts();
        let uri = match self.uri(&parts) {
            Ok(u) => u,
            Err(e) => {
                return full(
                    StatusCode::BAD_REQUEST,
                    format!("gpud: bad request uri: {e}\n"),
                )
            }
        };
        let mut up_req = Request::new(Full::new(Bytes::new()));
        *up_req.method_mut() = parts.method.clone();
        *up_req.uri_mut() = uri;
        *up_req.headers_mut() = self.headers(&parts, peer, true);
        let mut up = match self.client.request(up_req).await {
            Ok(r) => r,
            Err(e) => return Self::unreachable(&e),
        };
        if up.status() != StatusCode::SWITCHING_PROTOCOLS {
            let (p, incoming) = up.into_parts();
            let mut resp = Response::new(
                GuardedBody {
                    inner: incoming,
                    guard: None,
                }
                .map_err(BoxError::from)
                .boxed_unsync(),
            );
            *resp.status_mut() = p.status;
            *resp.headers_mut() = p.headers;
            return resp;
        }
        let mut resp = Response::new(
            Full::new(Bytes::new())
                .map_err(|never: Infallible| match never {})
                .boxed_unsync(),
        );
        *resp.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
        *resp.headers_mut() = up.headers().clone();
        let up_io = hyper::upgrade::on(&mut up);
        tokio::spawn(async move {
            let (Ok(down), Ok(up)) = (down.await, up_io.await) else {
                return;
            };
            let (mut down, mut up) = (TokioIo::new(down), TokioIo::new(up));
            let _ = tokio::io::copy_bidirectional(&mut down, &mut up).await;
        });
        resp
    }
}

/// Accept connections on `listener` and answer each request with `handler` until `shutdown`
/// resolves. Open connections get a few seconds to finish, then are dropped.
pub async fn serve_fn<H, Fut, S>(listener: TcpListener, shutdown: S, handler: H)
where
    H: Fn(Request<Incoming>, SocketAddr) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Response<ProxyBody>> + Send + 'static,
    S: Future,
{
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let mut conns = JoinSet::new();
    tokio::pin!(shutdown);
    loop {
        let (stream, peer) = tokio::select! {
            _ = &mut shutdown => break,
            r = listener.accept() => match r {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("gate accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            },
        };
        let handler = handler.clone();
        let mut stop = stop_rx.clone();
        conns.spawn(async move {
            let svc = hyper::service::service_fn(move |req| {
                let handler = handler.clone();
                async move { Ok::<_, Infallible>(handler(req, peer).await) }
            });
            let conn = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), svc)
                .with_upgrades();
            tokio::pin!(conn);
            tokio::select! {
                r = conn.as_mut() => {
                    if let Err(e) = r {
                        tracing::debug!("gate connection from {peer}: {e}");
                    }
                }
                _ = stop.changed() => {
                    conn.as_mut().graceful_shutdown();
                    let _ = conn.await;
                }
            }
        });
        // Reap finished connections so the set stays small.
        while conns.try_join_next().is_some() {}
    }
    let _ = stop_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while conns.join_next().await.is_some() {}
    })
    .await;
}
