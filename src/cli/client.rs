// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! Minimal HTTP/1 client for the control API: JSON GETs and POSTs, the NDJSON lease stream.
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{CONTENT_TYPE, HOST};
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::task::AbortHandle;

#[derive(Debug)]
pub enum ClientError {
    /// Nothing is listening: gpuq is not running.
    Down(String),
    /// gpuq answered but said no.
    Refused(String),
    /// Anything else, e.g. the stream broke.
    Other(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Down(m) | ClientError::Refused(m) | ClientError::Other(m) => {
                f.write_str(m)
            }
        }
    }
}

/// What a `granted` event carries.
#[derive(Debug, PartialEq, Eq)]
pub struct Grant {
    pub id: String,
    /// The yield signal gpuq is configured with (if it says).
    pub yield_signal: Option<String>,
    /// The card the lease is on (several joined with commas); None from a gpuq that predates
    /// cards.
    pub card: Option<String>,
    /// Every card of the lease; empty from a gpuq that predates multi-card leases.
    pub cards: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// One line of the lease stream.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    Queued,
    Ping,
    Granted(Grant),
    Yield,
    Unknown,
}

struct Response {
    status: StatusCode,
    body: Incoming,
    driver: AbortHandle,
}

impl Drop for Response {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

async fn send(addr: &str, req: Request<Full<Bytes>>) -> Result<Response, ClientError> {
    let stream = TcpStream::connect(addr)
        .await
        .map_err(|e| ClientError::Down(format!("{addr}: {e}")))?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| ClientError::Other(e.to_string()))?;
    let driver = tokio::spawn(async move {
        let _ = conn.await;
    })
    .abort_handle();
    let resp = sender
        .send_request(req)
        .await
        .map_err(|e| ClientError::Other(e.to_string()));
    match resp {
        Ok(resp) => Ok(Response {
            status: resp.status(),
            body: resp.into_body(),
            driver,
        }),
        Err(e) => {
            driver.abort();
            Err(e)
        }
    }
}

async fn read_all(mut resp: Response) -> String {
    match (&mut resp.body).collect().await {
        Ok(c) => String::from_utf8_lossy(&c.to_bytes()).into_owned(),
        Err(_) => String::new(),
    }
}

/// The `error` field of a JSON error body, or the raw text.
fn error_text(status: StatusCode, body: &str) -> String {
    let msg = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| body.trim().chars().take(200).collect());
    format!("gpuq refused the request ({status}): {msg}")
}

pub async fn get_json(addr: &str, path: &str) -> Result<Value, ClientError> {
    let req = Request::get(path)
        .header(HOST, addr)
        .body(Full::new(Bytes::new()))
        .map_err(|e| ClientError::Other(e.to_string()))?;
    let resp = send(addr, req).await?;
    let status = resp.status;
    let text = read_all(resp).await;
    if status != StatusCode::OK {
        return Err(ClientError::Refused(error_text(status, &text)));
    }
    serde_json::from_str(&text).map_err(|e| ClientError::Other(format!("bad reply: {e}")))
}

/// POST a JSON body; the status and the JSON answer, whatever the status.
pub async fn post_json(
    addr: &str,
    path: &str,
    body: &Value,
) -> Result<(StatusCode, Value), ClientError> {
    let req = Request::post(path)
        .header(HOST, addr)
        .header(CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .map_err(|e| ClientError::Other(e.to_string()))?;
    let resp = send(addr, req).await?;
    let status = resp.status;
    let text = read_all(resp).await;
    match serde_json::from_str(&text) {
        Ok(v) => Ok((status, v)),
        Err(_) => Err(ClientError::Refused(error_text(status, &text))),
    }
}

/// An open `POST /lease` response. Dropping it closes the connection and so the lease.
pub struct LeaseStream {
    resp: Response,
    buf: Vec<u8>,
}

impl LeaseStream {
    pub async fn open(addr: &str, body: &Value) -> Result<LeaseStream, ClientError> {
        let req = Request::post("/lease")
            .header(HOST, addr)
            .header(CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(body.to_string())))
            .map_err(|e| ClientError::Other(e.to_string()))?;
        let resp = send(addr, req).await?;
        if resp.status != StatusCode::OK {
            let status = resp.status;
            let text = read_all(resp).await;
            return Err(ClientError::Refused(error_text(status, &text)));
        }
        Ok(LeaseStream {
            resp,
            buf: Vec::new(),
        })
    }

    /// The next event, or None when gpuq closed the stream. Cancel safe.
    pub async fn next(&mut self) -> Result<Option<Event>, ClientError> {
        loop {
            if let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
                let raw: Vec<u8> = self.buf.drain(..=i).collect();
                if raw.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                return Ok(Some(parse_event(&raw)));
            }
            match self.resp.body.frame().await {
                None => return Ok(None),
                Some(Err(e)) => return Err(ClientError::Other(e.to_string())),
                Some(Ok(frame)) => {
                    if let Some(data) = frame.data_ref() {
                        self.buf.extend_from_slice(data);
                    }
                }
            }
        }
    }
}

fn parse_event(raw: &[u8]) -> Event {
    let Ok(v) = serde_json::from_slice::<Value>(raw) else {
        return Event::Unknown;
    };
    match v.get("event").and_then(Value::as_str) {
        Some("queued") => Event::Queued,
        Some("ping") => Event::Ping,
        Some("yield") => Event::Yield,
        Some("granted") => match v.get("id").and_then(Value::as_str) {
            Some(id) => Event::Granted(Grant {
                id: id.to_owned(),
                yield_signal: v
                    .get("yield_signal")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                card: v.get("card").and_then(Value::as_str).map(str::to_owned),
                cards: v
                    .get("cards")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|n| n.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default(),
                env: v
                    .get("env")
                    .and_then(Value::as_object)
                    .map(|o| {
                        o.iter()
                            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                            .collect()
                    })
                    .unwrap_or_default(),
            }),
            None => Event::Unknown,
        },
        _ => Event::Unknown,
    }
}
