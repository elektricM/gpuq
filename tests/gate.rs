//! The gate: pass-through, classification, CPU-only models, streaming and disconnects.
//! Real sockets and real (short) time, like tests/test_gate.py.

mod common;
#[path = "support/fake_llamaswap.rs"]
mod fake_llamaswap;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use common::{base_settings, ms, request, secs};
use fake_llamaswap::{FakeLlamaSwap, MODELS_YAML};
use gpud::arbiter::{self, Handle, Status, Vram};
use gpud::backend::llamaswap::LlamaSwap;
use gpud::backend::models::ModelTable;
use gpud::gate::{model_of, Gate, GateOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::time::{sleep, timeout};

struct GateRig {
    fake: FakeLlamaSwap,
    handle: Handle,
    backend: Arc<LlamaSwap>,
    addr: SocketAddr,
    http: reqwest::Client,
    _stop: oneshot::Sender<()>,
    _dir: tempfile::TempDir,
}

fn classes(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

async fn serve(gate: Gate) -> (SocketAddr, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(gate.serve(listener, async {
        let _ = rx.await;
    }));
    (addr, tx)
}

impl GateRig {
    async fn start() -> GateRig {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("llama-swap.yaml");
        std::fs::write(&path, MODELS_YAML).unwrap();
        let fake = FakeLlamaSwap::start().await;
        fake.preload("big");
        let backend = Arc::new(
            LlamaSwap::new(&fake.url(), ModelTable::new(&path), secs(5.0))
                .unwrap()
                .with_poll_interval(ms(30)),
        );
        let handle = arbiter::spawn(base_settings(), backend.clone(), fake.probe(), None);
        let gate = Gate::new(
            GateOptions {
                backend_url: fake.url(),
                model_classes: classes(&[("rec", "realtime")]),
            },
            backend.clone(),
            handle.clone(),
        );
        let (addr, stop) = serve(gate).await;
        GateRig {
            fake,
            handle,
            backend,
            addr,
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
            _stop: stop,
            _dir: dir,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    async fn status(&self) -> Status {
        self.handle.status().await.unwrap()
    }

    async fn chat(&self, model: &str, n: u32, delay: f64) -> (u16, String) {
        let r = self
            .http
            .post(self.url("/v1/chat/completions"))
            .json(&serde_json::json!({"model": model, "n": n, "delay": delay}))
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        (status, r.text().await.unwrap())
    }

    async fn wait_for(&self, what: &str, mut pred: impl FnMut(&Status) -> bool) {
        let end = Instant::now() + secs(5.0);
        loop {
            let st = self.status().await;
            if pred(&st) {
                return;
            }
            assert!(
                Instant::now() < end,
                "timed out waiting for {what}; state={:?} events={:?}",
                st.state,
                self.fake.events()
            );
            sleep(ms(20)).await;
        }
    }

    /// A raw chat request on its own socket, so the test can hang up mid-way.
    async fn raw_chat(&self, model: &str, n: u32, delay: f64) -> TcpStream {
        let body = serde_json::json!({"model": model, "n": n, "delay": delay}).to_string();
        let req = format!(
            "POST /v1/chat/completions HTTP/1.1\r\nHost: gate\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let mut s = TcpStream::connect(self.addr).await.unwrap();
        s.write_all(req.as_bytes()).await.unwrap();
        s
    }
}

async fn read_until(s: &mut TcpStream, needle: &str) -> String {
    let mut got = Vec::new();
    let mut buf = [0u8; 1024];
    timeout(secs(5.0), async {
        while !String::from_utf8_lossy(&got).contains(needle) {
            let n = s.read(&mut buf).await.unwrap();
            assert!(
                n > 0,
                "eof before {needle:?}: {}",
                String::from_utf8_lossy(&got)
            );
            got.extend_from_slice(&buf[..n]);
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {needle:?}"));
    String::from_utf8_lossy(&got).into_owned()
}

#[test]
fn model_of_cases() {
    assert_eq!(
        model_of("/upstream/big/health", b"").as_deref(),
        Some("big")
    );
    assert_eq!(
        model_of("/v1/chat/completions", br#"{"model":"x","m":[]}"#).as_deref(),
        Some("x")
    );
    assert_eq!(model_of("/v1/models", b""), None);
    assert_eq!(model_of("/v1/chat/completions", b"{not json"), None);
    assert_eq!(model_of("/v1/chat/completions", br#"{"model": 3}"#), None);
    assert_eq!(model_of("/v1/chat/completions", br#"{"model": ""}"#), None);
    assert_eq!(model_of("/upstream/", b""), None);
    assert_eq!(model_of("/upstream//x", b""), None);
    assert_eq!(model_of("/upstream/big", b"").as_deref(), Some("big"));
    assert_eq!(model_of("/x", br#"["model"]"#), None);
}

#[tokio::test]
async fn passthrough_streams_and_forwards_client_ip() {
    let r = GateRig::start().await;
    let (status, text) = r.chat("big", 5, 0.01).await;
    assert_eq!(status, 200);
    assert_eq!(text.matches("data:").count(), 6);
    assert_eq!(r.fake.lock().xff.last().unwrap(), "127.0.0.1");
    r.wait_for("inflight 0", |s| s.inflight_llm == 0).await;
}

#[tokio::test]
async fn existing_x_forwarded_for_is_extended() {
    let r = GateRig::start().await;
    let resp = r
        .http
        .post(r.url("/v1/chat/completions"))
        .header("X-Forwarded-For", "10.1.2.3")
        .json(&serde_json::json!({"model": "big", "n": 1}))
        .send()
        .await
        .unwrap();
    resp.bytes().await.unwrap();
    assert_eq!(r.fake.lock().xff.last().unwrap(), "10.1.2.3, 127.0.0.1");
}

#[tokio::test]
async fn host_header_is_preserved() {
    let r = GateRig::start().await;
    let resp = r
        .http
        .post(r.url("/v1/chat/completions"))
        .header("Host", "gpu.example:8080")
        .json(&serde_json::json!({"model": "big"}))
        .send()
        .await
        .unwrap();
    resp.bytes().await.unwrap();
    assert_eq!(r.fake.lock().hosts.last().unwrap(), "gpu.example:8080");
}

#[tokio::test]
async fn classify() {
    let r = GateRig::start().await;
    let gate = |c: &[(&str, &str)]| {
        Gate::new(
            GateOptions {
                backend_url: "http://127.0.0.1:1".into(),
                model_classes: classes(c),
            },
            r.backend.clone(),
            r.handle.clone(),
        )
    };
    let g = gate(&[("rec", "realtime")]);
    assert_eq!(g.classify(Some("big")).as_deref(), Some("llm"));
    assert_eq!(g.classify(Some("rec")).as_deref(), Some("realtime"));
    assert_eq!(g.classify(Some("other-alias")).as_deref(), Some("llm"));
    assert_eq!(g.classify(Some("embed")), None); // CPU model: never gated
    assert_eq!(g.classify(None), None);
    // A class on the canonical id applies to its aliases.
    let g = gate(&[("other", "batch")]);
    assert_eq!(g.classify(Some("other-alias")).as_deref(), Some("batch"));
}

#[tokio::test]
async fn cpu_model_passes_during_lease() {
    let r = GateRig::start().await;
    let lease = timeout(
        secs(5.0),
        r.handle
            .acquire(request("interactive", Vram::Mib(20000), false, "job")),
    )
    .await
    .unwrap()
    .unwrap();
    let resp = r
        .http
        .post(r.url("/v1/embeddings"))
        .json(&serde_json::json!({"model": "embed", "input": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(!r.fake.is_loaded("big"));
    assert_eq!(
        r.fake.events().iter().filter(|e| *e == "load big").count(),
        1
    );
    drop(lease);
}

#[tokio::test]
async fn alias_and_other_model_load_on_demand() {
    let r = GateRig::start().await;
    let lease = timeout(
        secs(5.0),
        r.handle
            .acquire(request("batch", Vram::Mib(1000), false, "job")),
    )
    .await
    .unwrap()
    .unwrap();
    let rig = Arc::new(r);
    let task = {
        let rig = rig.clone();
        tokio::spawn(async move { rig.chat("other-alias", 3, 0.01).await })
    };
    sleep(ms(200)).await;
    assert!(!task.is_finished()); // an alias of a GPU model is gated too
    drop(lease);
    let Ok(res) = timeout(secs(5.0), task).await else {
        let st = rig.status().await;
        panic!(
            "still held: state={:?} queue={:?} hist={:?} events={:?}",
            st.state,
            st.queue,
            st.history,
            rig.fake.events()
        );
    };
    let (status, _) = res.unwrap();
    assert_eq!(status, 200);
    assert!(rig.fake.is_loaded("other"));
    assert!(!rig.fake.has_event("restore 0")); // no point restoring "big" for it
}

#[tokio::test]
async fn backend_down_gives_502() {
    let r = GateRig::start().await;
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let gate = Gate::new(
        GateOptions {
            backend_url: format!("http://127.0.0.1:{port}"),
            model_classes: HashMap::new(),
        },
        r.backend.clone(),
        r.handle.clone(),
    );
    let (addr, _stop) = serve(gate).await;
    let resp = r
        .http
        .get(format!("http://{addr}/v1/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 502);
    assert!(resp
        .text()
        .await
        .unwrap()
        .starts_with("gpud: backend unreachable: "));
}

#[tokio::test]
async fn path_query_and_method_are_preserved() {
    let r = GateRig::start().await;
    // The fake answers 404 JSON for unknown paths; the gate relays status and body.
    let resp = r.http.get(r.url("/nope?a=1&b=2")).send().await.unwrap();
    assert_eq!(resp.status(), 404);
    assert!(resp.text().await.unwrap().contains("nope"));
    let resp = r
        .http
        .post(r.url("/upstream/big/slots/0?action=save"))
        .json(&serde_json::json!({"filename": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(r.fake.has_event("save 0"));
}

#[tokio::test]
async fn held_request_whose_client_disconnects_leaves_the_queue() {
    let r = GateRig::start().await;
    let lease = timeout(
        secs(5.0),
        r.handle
            .acquire(request("interactive", Vram::Mib(20000), false, "job")),
    )
    .await
    .unwrap()
    .unwrap();
    let conn = r.raw_chat("big", 3, 0.01).await;
    r.wait_for("a held request", |s| !s.queue.is_empty()).await;
    drop(conn);
    r.wait_for("the waiter to leave", |s| s.queue.is_empty())
        .await;
    assert_eq!(r.status().await.inflight_llm, 0);
    drop(lease);
    // The card is not left waiting for a request that is gone.
    sleep(ms(100)).await;
    assert!(!r.fake.has_event("chat big"));
}

#[tokio::test]
async fn streaming_response_whose_client_drops_releases_inflight() {
    let r = GateRig::start().await;
    let mut conn = r.raw_chat("big", 50, 0.05).await;
    read_until(&mut conn, "data: {\"i\":0}").await;
    r.wait_for("inflight 1", |s| s.inflight_llm == 1).await;
    drop(conn);
    r.wait_for("inflight 0", |s| s.inflight_llm == 0).await;
    // The backend stream is cut too, not left running to the end.
    let end = Instant::now() + secs(5.0);
    while r.fake.lock().chats_aborted == 0 {
        assert!(Instant::now() < end, "backend never saw the client leave");
        sleep(ms(20)).await;
    }
}

#[tokio::test]
async fn response_bytes_stream_as_they_arrive() {
    let r = GateRig::start().await;
    let t = Instant::now();
    let mut resp = r
        .http
        .post(r.url("/v1/chat/completions"))
        .json(&serde_json::json!({"model": "big", "n": 5, "delay": 0.3}))
        .send()
        .await
        .unwrap();
    let first = resp.chunk().await.unwrap().unwrap();
    let first_at = t.elapsed();
    assert!(String::from_utf8_lossy(&first).starts_with("data:"));
    // The backend needs about 1.5 s for the whole stream; the first frame must not wait for it.
    assert!(first_at < ms(1000), "first frame after {first_at:?}");
    assert_eq!(r.status().await.inflight_llm, 1);
    while resp.chunk().await.unwrap().is_some() {}
    assert!(t.elapsed() > ms(1200));
    r.wait_for("inflight 0", |s| s.inflight_llm == 0).await;
}

#[tokio::test]
async fn websocket_upgrade_is_tunnelled() {
    let r = GateRig::start().await;
    let mut s = TcpStream::connect(r.addr).await.unwrap();
    s.write_all(b"GET /ws HTTP/1.1\r\nHost: gate\r\nConnection: Upgrade\r\nUpgrade: echo\r\n\r\n")
        .await
        .unwrap();
    let head = read_until(&mut s, "\r\n\r\n").await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    s.write_all(b"ping").await.unwrap();
    let echoed = read_until(&mut s, "ping").await;
    assert!(echoed.contains("ping"));
    assert_eq!(r.status().await.inflight_llm, 0);
}
