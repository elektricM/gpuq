//! Helpers for the service and backend tests: a tiny fake HTTP server and fake service
//! binaries (shell scripts that record their arguments and flip a state file).
#![allow(dead_code)]

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

#[derive(Clone, Debug)]
pub struct Seen {
    pub method: String,
    /// Path and query, e.g. `/slots/0?action=save`.
    pub target: String,
    pub body: String,
}

impl Seen {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

pub type Handler = dyn Fn(&Seen) -> (u16, String) + Send + Sync;

/// Serves `handler` on 127.0.0.1 and records every request. Stops when dropped.
pub struct FakeServer {
    pub addr: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeServer {
    pub async fn start(
        handler: impl Fn(&Seen) -> (u16, String) + Send + Sync + 'static,
    ) -> FakeServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handler: Arc<Handler> = Arc::new(handler);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let handler = handler.clone();
                let log = log.clone();
                tokio::spawn(async move {
                    let svc = service_fn(move |req: Request<Incoming>| {
                        let handler = handler.clone();
                        let log = log.clone();
                        async move {
                            let method = req.method().to_string();
                            let target = req
                                .uri()
                                .path_and_query()
                                .map(|p| p.to_string())
                                .unwrap_or_default();
                            let body = req
                                .into_body()
                                .collect()
                                .await
                                .map(|b| b.to_bytes())
                                .unwrap_or_default();
                            let seen = Seen {
                                method,
                                target,
                                body: String::from_utf8_lossy(&body).into_owned(),
                            };
                            let (status, text) = handler(&seen);
                            log.lock().expect("log").push(seen);
                            let resp = Response::builder()
                                .status(status)
                                .header("content-type", "application/json")
                                .body(Full::new(Bytes::from(text)))
                                .expect("response");
                            Ok::<_, Infallible>(resp)
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), svc)
                        .await;
                });
            }
        });
        FakeServer { addr, seen, task }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn requests(&self) -> Vec<Seen> {
        self.seen.lock().expect("log").clone()
    }

    /// Requests whose `METHOD target` starts with `prefix`, e.g. `POST /api/generate`.
    pub fn matching(&self, prefix: &str) -> Vec<Seen> {
        self.requests()
            .into_iter()
            .filter(|s| format!("{} {}", s.method, s.target).starts_with(prefix))
            .collect()
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// An address nothing listens on.
pub async fn dead_url() -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = l.local_addr().expect("addr");
    drop(l);
    format!("http://{addr}")
}

#[cfg(unix)]
pub fn write_script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).expect("write script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

/// A fake `systemctl`: logs its arguments to `calls.log` and keeps `active`/`inactive` in
/// `state` next to the script. `fail_start` makes `start` exit 1 with a message on stderr.
#[cfg(unix)]
pub fn fake_systemctl(dir: &Path, fail_start: bool) -> PathBuf {
    let bin = dir.join("systemctl");
    let start = if fail_start {
        "echo 'Unit not found' >&2; exit 5"
    } else {
        "echo active > \"$D/state\"; exit 0"
    };
    write_script(
        &bin,
        &format!(
            r#"D="$(dirname "$0")"
echo "$@" >> "$D/calls.log"
for a in "$@"; do
  case "$a" in
    start) {start} ;;
    stop) echo inactive > "$D/state"; exit 0 ;;
    is-active) [ "$(cat "$D/state" 2>/dev/null)" = active ]; exit $? ;;
  esac
done
exit 2"#
        ),
    );
    bin
}

/// A fake `docker`/`podman` with the same conventions; `inspect` prints true or false.
#[cfg(unix)]
pub fn fake_container_engine(dir: &Path, name: &str) -> PathBuf {
    let bin = dir.join(name);
    write_script(
        &bin,
        r#"D="$(dirname "$0")"
echo "$@" >> "$D/calls.log"
case "$1" in
  start) echo running > "$D/state" ;;
  stop) echo stopped > "$D/state" ;;
  inspect) if [ "$(cat "$D/state" 2>/dev/null)" = running ]; then echo true; else echo false; fi ;;
  *) exit 2 ;;
esac"#,
    );
    bin
}

pub fn read_calls(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("calls.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

pub fn read_state(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("state"))
        .unwrap_or_default()
        .trim()
        .to_string()
}

pub fn write_state(dir: &Path, s: &str) {
    std::fs::write(dir.join("state"), s).expect("state");
}
