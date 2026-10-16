//! The gate: a reverse proxy in front of the LLM server.
//!
//! A request for a GPU model is announced to the arbiter and held until the card is the LLM's;
//! the admission guard lives in the response body, so it is released when the last byte went
//! out or the client left. Requests for CPU models, and everything without a model, go
//! straight through. Upgrade (WebSocket) requests are tunnelled without gating.

pub mod proxy;

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use serde_json::Value;
use tokio::net::TcpListener;

use crate::arbiter::Handle;
use crate::backend::Backend;
use proxy::{full, is_upgrade, read_body, BodyError, Keepalive, Proxy, ProxyBody, MAX_BODY};

pub struct GateOptions {
    /// Base URL of the backend, e.g. `http://127.0.0.1:8080`.
    pub backend_url: String,
    /// Model name to job class; models not listed are scheduled as "llm".
    pub model_classes: HashMap<String, String>,
}

/// The llama-swap model a request will load, or None if it loads nothing.
pub fn model_of(path: &str, body: &[u8]) -> Option<String> {
    if let Some(rest) = path.strip_prefix("/upstream/") {
        let model = rest.split('/').next().unwrap_or("");
        return (!model.is_empty()).then(|| model.to_owned());
    }
    if body.first() != Some(&b'{') {
        return None;
    }
    let doc: Value = serde_json::from_slice(body).ok()?;
    match doc.get("model") {
        Some(Value::String(m)) if !m.is_empty() => Some(m.clone()),
        _ => None,
    }
}

struct Inner {
    options: GateOptions,
    backend: Arc<dyn Backend>,
    arbiter: Handle,
    proxy: Proxy,
}

#[derive(Clone)]
pub struct Gate {
    inner: Arc<Inner>,
}

impl Gate {
    pub fn new(options: GateOptions, backend: Arc<dyn Backend>, arbiter: Handle) -> Self {
        let proxy = Proxy::new(&options.backend_url);
        Gate {
            inner: Arc::new(Inner {
                options,
                backend,
                arbiter,
                proxy,
            }),
        }
    }

    /// Job class a request is scheduled as, or None if it never touches the GPU.
    pub fn classify(&self, model: Option<&str>) -> Option<String> {
        let model = model.filter(|m| !m.is_empty())?;
        let inner = &self.inner;
        if !inner.backend.uses_gpu(model) {
            return None;
        }
        let classes = &inner.options.model_classes;
        let class = classes
            .get(model)
            .or_else(|| classes.get(&inner.backend.resolve(model)))
            .map_or("llm", String::as_str);
        Some(class.to_owned())
    }

    async fn handle(&self, req: Request<Incoming>, peer: SocketAddr) -> Response<ProxyBody> {
        if is_upgrade(req.headers()) {
            return self.inner.proxy.upgrade(req, peer).await;
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
        let model = model_of(parts.uri.path(), &body);
        let mut guard: Option<Keepalive> = None;
        if let (Some(model), Some(class)) = (model.as_deref(), self.classify(model.as_deref())) {
            match self.inner.arbiter.enter_llm(model, &class).await {
                Ok(g) => guard = Some(Box::new(g)),
                Err(e) => {
                    return full(StatusCode::SERVICE_UNAVAILABLE, format!("gpud: {e}\n"));
                }
            }
        }
        self.inner.proxy.forward(parts, body, peer, guard).await
    }

    /// Serve on `listener` until `shutdown` resolves.
    pub async fn serve<S: Future>(self, listener: TcpListener, shutdown: S) {
        proxy::serve_fn(listener, shutdown, move |req, peer| {
            let gate = self.clone();
            async move { gate.handle(req, peer).await }
        })
        .await;
    }
}
