//! The `[immich]` section: the queue-aware Immich ML adapter.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

use super::secs;

/// `[immich]`. Present means enabled. See `docs/integrations.md`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Immich {
    /// Immich server, e.g. `http://127.0.0.1:2283`. `/api` is appended.
    pub url: String,
    /// File holding an Immich API key. Read on every call, so it can be rotated.
    pub key_file: PathBuf,
    /// Directory of the compose file that defines the GPU ML service.
    pub compose_dir: PathBuf,
    /// Compose service name of the GPU ML container.
    pub service: String,
    /// Container name, for `docker inspect`.
    pub container: String,
    #[serde(default = "default_profile")]
    pub compose_profile: String,
    /// Argv prefix for docker; tests point it at a fake script.
    #[serde(default = "default_docker")]
    pub docker: Vec<String>,
    #[serde(default = "default_class")]
    pub class: String,
    #[serde(default = "default_vram")]
    pub vram: String,
    /// A `[[gpu.card]]` name; default the primary card.
    #[serde(default)]
    pub card: Option<String>,
    #[serde(
        rename = "poll_s",
        default = "default_poll",
        deserialize_with = "secs::deserialize"
    )]
    pub poll: Duration,
    /// Backlog that makes the adapter ask for the card.
    #[serde(default = "default_min_backlog")]
    pub min_backlog: u64,
    /// A leftover backlog this small is finished by the CPU container.
    #[serde(default = "default_cpu_ok")]
    pub cpu_ok_backlog: u64,
    #[serde(default = "default_queues")]
    pub queues: Vec<String>,
    /// Queue concurrency while on the GPU.
    #[serde(default = "default_concurrency")]
    pub gpu_concurrency: BTreeMap<String, u32>,
    #[serde(
        rename = "health_timeout_s",
        default = "default_health_timeout",
        deserialize_with = "secs::deserialize"
    )]
    pub health_timeout: Duration,
    /// Pause after the container is healthy, so Immich notices it before the queues resume.
    #[serde(
        rename = "settle_s",
        default = "default_settle",
        deserialize_with = "secs::deserialize"
    )]
    pub settle: Duration,
    /// Time between checks for active jobs while draining (at most 60 checks).
    #[serde(
        rename = "drain_poll_s",
        default = "default_drain_poll",
        deserialize_with = "secs::deserialize"
    )]
    pub drain_poll: Duration,
    /// JSON file remembering that gpud paused the queues.
    #[serde(default)]
    pub state_file: Option<PathBuf>,
}

fn default_profile() -> String {
    "gpu".into()
}
fn default_docker() -> Vec<String> {
    vec!["docker".into()]
}
fn default_class() -> String {
    "immich".into()
}
fn default_vram() -> String {
    "6G".into()
}
fn default_poll() -> Duration {
    Duration::from_secs(30)
}
fn default_min_backlog() -> u64 {
    200
}
fn default_cpu_ok() -> u64 {
    50
}
fn default_queues() -> Vec<String> {
    ["smartSearch", "faceDetection", "ocr"]
        .map(String::from)
        .into()
}
fn default_concurrency() -> BTreeMap<String, u32> {
    [("smartSearch", 8), ("faceDetection", 4), ("ocr", 2)]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect()
}
fn default_health_timeout() -> Duration {
    Duration::from_secs(600)
}
fn default_settle() -> Duration {
    Duration::from_secs(35)
}
fn default_drain_poll() -> Duration {
    Duration::from_secs(5)
}

impl Immich {
    /// `state_file`, else `$XDG_STATE_HOME/gpud/immich.json`, else
    /// `~/.local/state/gpud/immich.json`. None when no home can be found.
    pub fn state_path(&self) -> Option<PathBuf> {
        if let Some(p) = &self.state_file {
            return Some(p.clone());
        }
        let env = |k: &str| {
            std::env::var_os(k)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let dir = env("XDG_STATE_HOME").or_else(|| env("HOME").map(|h| h.join(".local/state")))?;
        Some(dir.join("gpud/immich.json"))
    }

    /// Problems with this section; `cards` are the configured card names.
    pub(super) fn problems(&self, classes: &[&str], cards: &[String]) -> Vec<String> {
        let mut p = Vec::new();
        let mut bad = |m: String| p.push(format!("immich.{m}"));
        if !super::is_http_url(&self.url) {
            bad(format!("url must be an http(s) URL, not {:?}", self.url));
        }
        for (k, v) in [
            ("service", &self.service),
            ("container", &self.container),
            ("compose_profile", &self.compose_profile),
        ] {
            if v.is_empty() {
                bad(format!("{k} is empty"));
            }
        }
        if self.key_file.as_os_str().is_empty() {
            bad("key_file is empty".into());
        }
        if self.compose_dir.as_os_str().is_empty() {
            bad("compose_dir is empty".into());
        }
        if self.docker.first().map_or(true, |d| d.is_empty()) {
            bad("docker needs a program name".into());
        }
        if !classes.contains(&self.class.as_str()) {
            bad(format!(
                "class {:?} is not defined; add it to [classes]",
                self.class
            ));
        }
        if let Err(e) = super::parse_vram(&self.vram) {
            bad(format!("vram: {e}"));
        }
        if let Some(c) = &self.card {
            if !cards.contains(c) {
                bad(format!(
                    "card {c:?} is not a configured card; known: {}",
                    cards.join(", ")
                ));
            }
        }
        if self.queues.is_empty() {
            bad("queues must not be empty".into());
        }
        if self.poll.is_zero() {
            bad("poll_s must be above zero".into());
        }
        if self.drain_poll.is_zero() {
            bad("drain_poll_s must be above zero".into());
        }
        for q in self.gpu_concurrency.keys() {
            if !self.queues.contains(q) {
                bad(format!(
                    "gpu_concurrency names {q:?}, which is not in queues"
                ));
            }
        }
        if self.state_path().is_none() {
            bad("state_file is needed: neither XDG_STATE_HOME nor HOME is set".into());
        }
        p
    }

    /// One line for `gpud check-config`.
    pub fn summary(&self) -> String {
        format!(
            "immich: {} queues [{}] class {} vram {} card {}, GPU at backlog >= {}, CPU ok at <= {}, service {}",
            self.url,
            self.queues.join(", "),
            self.class,
            self.vram,
            self.card.as_deref().unwrap_or("primary"),
            self.min_backlog,
            self.cpu_ok_backlog,
            self.service,
        )
    }
}
