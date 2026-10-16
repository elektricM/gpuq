// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! Helpers for the API and CLI tests: the shared rig behind a real listener on 127.0.0.1:0,
//! and a small NDJSON client for the lease stream.
#![allow(dead_code)]

#[path = "../common/mod.rs"]
pub mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use gpud::api::{Api, StatusExtra};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

pub use common::*;

/// What the daemon would report from the backend.
pub struct StubExtra;

#[async_trait]
impl StatusExtra for StubExtra {
    async fn extra(&self) -> Value {
        json!({"backend": {"kind": "stub", "models": [{"model": "big", "state": "ready"}]}})
    }
}

/// The API in front of a rig, served on a real socket.
pub struct TestApi {
    pub rig: Rig,
    pub addr: SocketAddr,
    pub ping: Duration,
    /// Announced with every grant, like the daemon's `[run] yield_signal`.
    pub yield_signal: Option<String>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl TestApi {
    pub async fn start(ping: Duration) -> TestApi {
        Self::start_on(Rig::start(|_| {}).await, ping).await
    }

    pub async fn start_with_yield_signal(ping: Duration, signal: &str) -> TestApi {
        let rig = Rig::start(|_| {}).await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut t = TestApi {
            rig,
            addr,
            ping,
            yield_signal: Some(signal.to_owned()),
            stop: None,
            task: None,
        };
        t.serve(listener);
        t
    }

    pub async fn start_on(rig: Rig, ping: Duration) -> TestApi {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut t = TestApi {
            rig,
            addr,
            ping,
            yield_signal: None,
            stop: None,
            task: None,
        };
        t.serve(listener);
        t
    }

    fn serve(&mut self, listener: TcpListener) {
        let mut api =
            Api::new(self.rig.handle.clone(), Arc::new(StubExtra)).with_ping_interval(self.ping);
        if let Some(sig) = &self.yield_signal {
            api = api.with_yield_signal(sig.clone());
        }
        let (stop, rx) = oneshot::channel();
        self.stop = Some(stop);
        self.task = Some(tokio::spawn(async move {
            api.serve(listener, async {
                let _ = rx.await;
            })
            .await
        }));
    }

    /// Stop serving and drop every open connection.
    pub async fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }

    /// Serve again on the same port, like a gpud restart.
    pub async fn restart(&mut self) {
        self.stop().await;
        let listener = TcpListener::bind(self.addr).await.unwrap();
        self.serve(listener);
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    pub async fn get(&self, path: &str) -> (u16, String) {
        let r = client().get(self.url(path)).send().await.unwrap();
        (r.status().as_u16(), r.text().await.unwrap())
    }

    /// POST /lease; returns the status and, on 200, the open event stream.
    pub async fn lease(&self, body: Value) -> (u16, Events) {
        let resp = client()
            .post(self.url("/lease"))
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        (
            resp.status().as_u16(),
            Events {
                resp,
                buf: String::new(),
            },
        )
    }

    pub async fn lease_ok(&self, body: Value) -> Events {
        let (status, ev) = self.lease(body).await;
        assert_eq!(status, 200);
        ev
    }

    pub async fn post_raw(&self, body: &'static str) -> (u16, Value) {
        let r = client()
            .post(self.url("/lease"))
            .body(body)
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        (status, r.json().await.unwrap_or(Value::Null))
    }
}

impl Drop for TestApi {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

pub fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

/// An open lease stream.
pub struct Events {
    resp: reqwest::Response,
    buf: String,
}

impl Events {
    /// The next event, or None at end of stream. Panics after 5 s of silence.
    pub async fn next(&mut self) -> Option<Value> {
        loop {
            if let Some(i) = self.buf.find('\n') {
                let line: String = self.buf.drain(..=i).collect();
                return Some(serde_json::from_str(line.trim()).unwrap());
            }
            let chunk = timeout(Duration::from_secs(5), self.resp.chunk())
                .await
                .expect("no event for 5 s")
                .ok()??;
            self.buf.push_str(std::str::from_utf8(&chunk).unwrap());
        }
    }

    pub async fn event(&mut self) -> String {
        let v = self.next().await.expect("stream ended");
        v["event"].as_str().unwrap().to_owned()
    }

    /// Read events until `name` arrives; returns everything read, `name` last.
    pub async fn until(&mut self, name: &str) -> Vec<Value> {
        let mut seen = Vec::new();
        loop {
            let v = self
                .next()
                .await
                .unwrap_or_else(|| panic!("stream ended before {name}: {seen:?}"));
            let done = v["event"] == name;
            seen.push(v);
            if done {
                return seen;
            }
        }
    }

    /// Read to the grant and return the lease id.
    pub async fn granted(&mut self) -> String {
        let seen = self.until("granted").await;
        seen.last().unwrap()["id"].as_str().unwrap().to_owned()
    }

    /// Close the connection.
    pub fn close(self) {}
}

/// Poll the arbiter status every 20 ms until `pred` holds; false if `within` runs out.
pub async fn status_within(
    rig: &Rig,
    within: Duration,
    pred: impl Fn(&gpud::arbiter::Status) -> bool,
) -> bool {
    let end = tokio::time::Instant::now() + within;
    loop {
        if pred(&rig.status().await) {
            return true;
        }
        if tokio::time::Instant::now() >= end {
            return false;
        }
        sleep(Duration::from_millis(20)).await;
    }
}
