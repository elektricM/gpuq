// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The gpud authors

//! Ollama, llama-server and stop/start backends against fake servers and fake service binaries.

#[path = "support_backends/mod.rs"]
mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpud::backend::ollama::Ollama;
use gpud::backend::Backend;
use serde_json::json;
use support::*;

const FAST: Duration = Duration::from_millis(20);

/// An Ollama stand-in: `/api/ps` lists `loaded`, a `keep_alive: 0` generate empties it, any
/// other generate loads the named model.
async fn fake_ollama(initial: serde_json::Value) -> (FakeServer, Arc<Mutex<serde_json::Value>>) {
    let loaded = Arc::new(Mutex::new(initial));
    let state = loaded.clone();
    let server = FakeServer::start(move |req| {
        let mut loaded = state.lock().unwrap();
        match (req.method.as_str(), req.target.as_str()) {
            ("GET", "/api/ps") => (200, json!({ "models": *loaded }).to_string()),
            ("POST", "/api/generate") => {
                let body = req.json();
                let name = body["model"].as_str().unwrap_or_default().to_string();
                let list = loaded.as_array_mut().unwrap();
                if body["keep_alive"] == json!(0) {
                    list.retain(|m| m["name"] != name);
                } else {
                    list.push(json!({ "name": name, "size_vram": 1000 }));
                }
                (200, json!({ "done": true }).to_string())
            }
            _ => (404, "{}".into()),
        }
    })
    .await;
    (server, loaded)
}

#[tokio::test]
async fn ollama_lists_only_models_in_vram() {
    let (server, _) = fake_ollama(json!([
        { "name": "chat:latest", "size_vram": 4000 },
        { "name": "embed:latest", "size_vram": 0 },
        { "name": "coder:7b", "size_vram": 100 },
    ]))
    .await;
    let ollama = Ollama::new(&server.url());
    let mut got = ollama.loaded_gpu_models().await.unwrap();
    got.sort();
    assert_eq!(got, ["chat:latest", "coder:7b"]);
}

#[tokio::test]
async fn ollama_cpu_models_never_count_as_gpu() {
    let (server, _) = fake_ollama(json!([
        { "name": "embed:latest", "size_vram": 50 },
        { "name": "chat:latest", "size_vram": 4000 },
    ]))
    .await;
    let ollama = Ollama::new(&server.url()).with_cpu_models(["embed"]);
    assert!(!ollama.uses_gpu("embed"));
    assert!(!ollama.uses_gpu("embed:latest"));
    assert!(ollama.uses_gpu("chat"));
    assert_eq!(ollama.loaded_gpu_models().await.unwrap(), ["chat:latest"]);
}

#[tokio::test]
async fn ollama_resolve_adds_latest_only_without_tag() {
    let ollama = Ollama::new("http://127.0.0.1:1");
    assert_eq!(ollama.resolve("chat"), "chat:latest");
    assert_eq!(ollama.resolve("chat:7b"), "chat:7b");
    assert_eq!(
        ollama.resolve("host:5000/ns/chat"),
        "host:5000/ns/chat:latest"
    );
    let caps = ollama.capabilities();
    assert!(caps.unload && caps.health && !caps.slots);
    assert_eq!(ollama.kind(), "ollama");
}

#[tokio::test]
async fn ollama_unload_sends_keep_alive_zero_for_each_model() {
    let (server, loaded) = fake_ollama(json!([
        { "name": "chat:latest", "size_vram": 4000 },
        { "name": "coder:7b", "size_vram": 100 },
    ]))
    .await;
    let ollama = Ollama::new(&server.url()).with_poll_interval(FAST);
    ollama.unload_all().await.unwrap();

    let gens = server.matching("POST /api/generate");
    assert_eq!(gens.len(), 2);
    for g in &gens {
        assert_eq!(g.json()["keep_alive"], json!(0));
    }
    let mut names: Vec<String> = gens
        .iter()
        .map(|g| g.json()["model"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    assert_eq!(names, ["chat:latest", "coder:7b"]);
    assert!(loaded.lock().unwrap().as_array().unwrap().is_empty());
    assert!(ollama.loaded_gpu_models().await.unwrap().is_empty());
}

#[tokio::test]
async fn ollama_unload_fails_when_models_stay_loaded() {
    let server = FakeServer::start(|req| match req.target.as_str() {
        "/api/ps" => (
            200,
            json!({ "models": [{ "name": "stuck:latest", "size_vram": 1 }] }).to_string(),
        ),
        _ => (200, "{}".into()),
    })
    .await;
    let ollama = Ollama::new(&server.url())
        .with_poll_interval(FAST)
        .with_unload_timeout(Duration::from_millis(150));
    assert!(ollama.unload_all().await.is_err());
}

#[tokio::test]
async fn ollama_load_posts_no_prompt_and_checks_ps() {
    let (server, _) = fake_ollama(json!([])).await;
    let ollama = Ollama::new(&server.url());
    assert!(ollama.load("chat").await);
    let gens = server.matching("POST /api/generate");
    assert_eq!(gens.len(), 1);
    let body = gens[0].json();
    assert_eq!(body["model"], "chat:latest");
    assert!(body.get("prompt").is_none());
    assert!(body.get("keep_alive").is_none());
    assert_eq!(ollama.loaded_gpu_models().await.unwrap(), ["chat:latest"]);
}

#[tokio::test]
async fn ollama_load_sends_configured_keep_alive() {
    let (server, _) = fake_ollama(json!([])).await;
    let ollama = Ollama::new(&server.url()).with_keep_alive(json!(-1));
    assert!(ollama.load("chat").await);
    assert_eq!(
        server.matching("POST /api/generate")[0].json()["keep_alive"],
        json!(-1)
    );
}

#[tokio::test]
async fn ollama_load_is_false_when_model_not_listed_afterwards() {
    let server = FakeServer::start(|req| match req.target.as_str() {
        "/api/ps" => (200, json!({ "models": [] }).to_string()),
        _ => (200, "{}".into()),
    })
    .await;
    assert!(!Ollama::new(&server.url()).load("chat").await);
}

#[tokio::test]
async fn ollama_load_is_false_on_http_error_and_unreachable() {
    let server = FakeServer::start(|_| (500, "{\"error\":\"no such model\"}".into())).await;
    assert!(!Ollama::new(&server.url()).load("nope").await);
    assert!(!Ollama::new(&dead_url().await).load("nope").await);
    assert!(Ollama::new(&dead_url().await)
        .loaded_gpu_models()
        .await
        .is_err());
}

#[cfg(unix)]
mod service_backed {
    use super::*;
    use gpud::backend::llama_server::LlamaServer;
    use gpud::backend::stopstart::StopStart;
    use gpud::workload::service::{Engine, ServiceControl};

    /// Health is 200 while `state` in `dir` equals `up_value`.
    async fn health_server(dir: &std::path::Path, up_value: &'static str) -> FakeServer {
        let d = dir.to_path_buf();
        FakeServer::start(
            move |req| match (req.method.as_str(), req.target.as_str()) {
                ("GET", "/health") if read_state(&d) == up_value => {
                    (200, "{\"status\":\"ok\"}".into())
                }
                ("GET", "/health") => (503, "{\"error\":\"loading\"}".into()),
                ("POST", t) if t.starts_with("/slots/0?action=save") => {
                    (200, json!({ "n_saved": 12, "n_written": 4096 }).to_string())
                }
                ("POST", t) if t.starts_with("/slots/0?action=restore") => {
                    (200, json!({ "n_restored": 12 }).to_string())
                }
                _ => (404, "{}".into()),
            },
        )
        .await
    }

    #[tokio::test]
    async fn llama_server_stops_and_starts_through_systemctl() {
        let dir = tempfile::tempdir().unwrap();
        write_state(dir.path(), "active");
        let server = health_server(dir.path(), "active").await;
        let ctl = ServiceControl::systemd("llama.service", true)
            .with_binary(fake_systemctl(dir.path(), false))
            .with_poll_interval(FAST);
        let llama = LlamaServer::new(&server.url(), "big", ctl, true);

        let caps = llama.capabilities();
        assert!(!caps.unload && caps.slots && caps.health);
        assert_eq!(llama.resolve("anything"), "big");
        assert!(llama.uses_gpu("anything"));
        assert_eq!(llama.loaded_gpu_models().await.unwrap(), ["big"]);

        llama.unload_all().await.unwrap();
        assert_eq!(read_state(dir.path()), "inactive");
        assert!(llama.loaded_gpu_models().await.unwrap().is_empty());

        assert!(llama.load("big").await);
        assert_eq!(read_state(dir.path()), "active");
        assert_eq!(llama.loaded_gpu_models().await.unwrap(), ["big"]);
        let calls = read_calls(dir.path());
        assert!(calls.contains(&"--user stop --quiet llama.service".to_string()));
        assert!(calls.contains(&"--user start --quiet llama.service".to_string()));
    }

    #[tokio::test]
    async fn llama_server_slot_save_and_restore() {
        let dir = tempfile::tempdir().unwrap();
        write_state(dir.path(), "active");
        let server = health_server(dir.path(), "active").await;
        let ctl = ServiceControl::systemd("llama.service", false)
            .with_binary(fake_systemctl(dir.path(), false));
        let llama = LlamaServer::new(&server.url(), "big/model", ctl, true);

        assert!(
            !llama.restore_slot("big/model").await.unwrap(),
            "nothing saved yet"
        );
        assert!(llama.save_slot("big/model").await.unwrap());
        assert!(llama.restore_slot("big/model").await.unwrap());
        assert!(
            !llama.restore_slot("big/model").await.unwrap(),
            "restore is one-shot"
        );

        let save = server.matching("POST /slots/0?action=save");
        let restore = server.matching("POST /slots/0?action=restore");
        assert_eq!(save.len(), 1);
        assert_eq!(restore.len(), 1);
        assert_eq!(save[0].json()["filename"], "big_model.slot0.bin");
        assert_eq!(restore[0].json()["filename"], "big_model.slot0.bin");
    }

    #[tokio::test]
    async fn llama_server_without_slot_option_never_calls_slots() {
        let dir = tempfile::tempdir().unwrap();
        write_state(dir.path(), "active");
        let server = health_server(dir.path(), "active").await;
        let ctl = ServiceControl::systemd("llama.service", false)
            .with_binary(fake_systemctl(dir.path(), false));
        let llama = LlamaServer::new(&server.url(), "big", ctl, false);
        assert!(!llama.capabilities().slots);
        assert!(!llama.save_slot("big").await.unwrap());
        assert!(!llama.restore_slot("big").await.unwrap());
        assert!(server.matching("POST /slots").is_empty());
    }

    #[tokio::test]
    async fn llama_server_empty_slot_is_not_restored() {
        let dir = tempfile::tempdir().unwrap();
        write_state(dir.path(), "active");
        let server = FakeServer::start(|_| (200, json!({ "n_saved": 0 }).to_string())).await;
        let ctl = ServiceControl::systemd("llama.service", false)
            .with_binary(fake_systemctl(dir.path(), false));
        let llama = LlamaServer::new(&server.url(), "big", ctl, true);
        assert!(!llama.save_slot("big").await.unwrap());
        assert!(!llama.restore_slot("big").await.unwrap());
        assert!(server.matching("POST /slots/0?action=restore").is_empty());
    }

    #[tokio::test]
    async fn llama_server_load_fails_when_start_command_fails() {
        let dir = tempfile::tempdir().unwrap();
        let server = health_server(dir.path(), "active").await;
        let ctl = ServiceControl::systemd("llama.service", false)
            .with_binary(fake_systemctl(dir.path(), true));
        let llama = LlamaServer::new(&server.url(), "big", ctl, false);
        assert!(!llama.load("big").await);
    }

    #[tokio::test]
    async fn vllm_stop_start_via_fake_docker() {
        let dir = tempfile::tempdir().unwrap();
        write_state(dir.path(), "running");
        let server = health_server(dir.path(), "running").await;
        let ctl = ServiceControl::container(Engine::Docker, "vllm")
            .with_binary(fake_container_engine(dir.path(), "docker"))
            .with_poll_interval(FAST);
        let vllm = StopStart::vllm(&server.url(), "chat", ctl);

        assert_eq!(vllm.kind(), "vllm");
        let caps = vllm.capabilities();
        assert!(!caps.unload && !caps.slots && caps.health);
        assert_eq!(vllm.resolve("whatever"), "chat");
        assert_eq!(vllm.loaded_gpu_models().await.unwrap(), ["chat"]);
        assert!(!vllm.save_slot("chat").await.unwrap());

        vllm.unload_all().await.unwrap();
        assert_eq!(read_state(dir.path()), "stopped");
        assert!(vllm.loaded_gpu_models().await.unwrap().is_empty());

        assert!(vllm.load("chat").await);
        assert_eq!(read_state(dir.path()), "running");
        assert_eq!(vllm.loaded_gpu_models().await.unwrap(), ["chat"]);
        let calls = read_calls(dir.path());
        assert_eq!(calls, ["stop vllm", "start vllm"]);
    }

    #[tokio::test]
    async fn sglang_uses_its_own_kind_and_command_control() {
        let dir = tempfile::tempdir().unwrap();
        write_state(dir.path(), "up");
        let server = health_server(dir.path(), "up").await;
        let d = dir.path().display().to_string();
        let sh = |s: String| vec!["sh".to_string(), "-c".to_string(), s];
        let ctl = ServiceControl::commands(
            sh(format!("echo up > {d}/state")),
            sh(format!("echo down > {d}/state")),
            None,
        )
        .with_poll_interval(FAST);
        let sg = StopStart::sglang(&server.url(), "coder", ctl);
        assert_eq!(sg.kind(), "sglang");
        sg.unload_all().await.unwrap();
        assert!(sg.loaded_gpu_models().await.unwrap().is_empty());
        assert!(sg.load("coder").await);
        assert_eq!(sg.loaded_gpu_models().await.unwrap(), ["coder"]);
    }

    #[tokio::test]
    async fn stopstart_load_times_out_when_server_never_becomes_healthy() {
        let dir = tempfile::tempdir().unwrap();
        write_state(dir.path(), "stopped");
        // The server only reports healthy for a state the fake docker never writes.
        let server = health_server(dir.path(), "never").await;
        let ctl = ServiceControl::container(Engine::Podman, "x")
            .with_binary(fake_container_engine(dir.path(), "podman"))
            .with_poll_interval(FAST);
        let b = StopStart::new("custom", &server.url(), "m", ctl, "/health")
            .with_start_timeout(Duration::from_millis(200));
        assert_eq!(b.kind(), "custom");
        let t = std::time::Instant::now();
        assert!(!b.load("m").await);
        assert!(t.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn stopstart_unload_fails_when_server_keeps_answering() {
        let dir = tempfile::tempdir().unwrap();
        let server = FakeServer::start(|_| (200, "{}".into())).await;
        let ctl = ServiceControl::container(Engine::Docker, "x")
            .with_binary(fake_container_engine(dir.path(), "docker"))
            .with_poll_interval(FAST);
        let b =
            StopStart::vllm(&server.url(), "m", ctl).with_stop_timeout(Duration::from_millis(150));
        assert!(b.unload_all().await.is_err());
    }
}
