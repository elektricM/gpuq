//! ComfyUI specifics: prompts are held by gpud and submitted once the lease is held.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

const MODEL_EXTS: [&str; 7] = [
    ".safetensors",
    ".gguf",
    ".ckpt",
    ".pt",
    ".pth",
    ".bin",
    ".sft",
];

/// Failed submissions kept for /api/jobs.
const MAX_FAILED: usize = 100;

/// The weight files an API-format prompt loads: any string input naming one.
pub(super) fn model_files(prompt: &Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Some(nodes) = prompt.as_object() else {
        return out;
    };
    for node in nodes.values() {
        let Some(inputs) = node.get("inputs").and_then(Value::as_object) else {
            continue;
        };
        for v in inputs.values() {
            if let Some(s) = v.as_str() {
                let lower = s.to_ascii_lowercase();
                if MODEL_EXTS.iter().any(|e| lower.ends_with(e)) {
                    out.insert(s.to_owned());
                }
            }
        }
    }
    out
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// A prompt gpud holds or has sent.
#[derive(Clone, Debug)]
pub(super) struct Item {
    pub id: String,
    pub number: u64,
    /// The whole POST body, `extra_data` included.
    pub body: Value,
    /// Auth headers of the original request, replayed on submission.
    pub headers: Vec<(String, String)>,
    pub since_ms: u64,
}

/// What the HTTP side may read about a held prompt.
#[derive(Clone, Debug)]
pub(super) struct HeldView {
    pub id: String,
    pub number: u64,
    pub since_ms: u64,
    pub body: Value,
}

pub(super) struct Comfy {
    backend: String,
    pub held: VecDeque<Item>,
    pub failed: Vec<(String, String)>,
    /// Prompts handed to ComfyUI, kept so they can be put back with their full `extra_data`
    /// (ComfyUI's /queue hides some of it).
    sent: HashMap<String, Item>,
    next: u64,
    last_models: Option<BTreeSet<String>>,
}

impl Comfy {
    pub fn new(backend: &str) -> Self {
        Comfy {
            backend: backend.trim_end_matches('/').to_owned(),
            held: VecDeque::new(),
            failed: Vec::new(),
            sent: HashMap::new(),
            next: 1,
            last_models: None,
        }
    }

    fn number(&mut self) -> u64 {
        self.next += 1;
        self.next - 1
    }

    /// Hold a prompt; returns its id and queue number.
    pub fn add(&mut self, mut body: Value, headers: Vec<(String, String)>) -> (String, u64) {
        let id = body
            .get("prompt_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map_or_else(new_id, str::to_owned);
        body["prompt_id"] = json!(id);
        let number = self.number();
        self.held.push_back(Item {
            id: id.clone(),
            number,
            body,
            headers,
            since_ms: now_ms(),
        });
        (id, number)
    }

    /// Drop held prompts by id (or all of them); true if any went.
    pub fn cancel(&mut self, ids: &[String], all: bool) -> bool {
        let before = self.held.len();
        self.held.retain(|i| !all && !ids.contains(&i.id));
        self.held.len() != before
    }

    /// The prompt uses other weights than the last one sent.
    pub fn switches(&self, item: &Item) -> bool {
        let files = model_files(&item.body["prompt"]);
        !files.is_empty() && self.last_models.as_ref().is_some_and(|l| *l != files)
    }

    pub fn note_models(&mut self, item: &Item) {
        let files = model_files(&item.body["prompt"]);
        if !files.is_empty() {
            self.last_models = Some(files);
        }
    }

    /// ComfyUI was restarted or freed: no weights are loaded any more.
    pub fn reset_models(&mut self) {
        self.last_models = None;
    }

    /// Back to a clean slate after the lease was released.
    pub fn reset(&mut self) {
        self.sent.clear();
        self.last_models = None;
    }

    /// The service could not be started: nothing held will run.
    pub fn fail_all(&mut self, reason: &str) {
        for item in std::mem::take(&mut self.held) {
            self.record_failure(item.id, reason.to_owned());
        }
    }

    fn record_failure(&mut self, id: String, why: String) {
        self.failed.push((id, why));
        if self.failed.len() > MAX_FAILED {
            self.failed.remove(0);
        }
    }

    pub fn held_view(&self) -> Vec<HeldView> {
        self.held
            .iter()
            .map(|i| HeldView {
                id: i.id.clone(),
                number: i.number,
                since_ms: i.since_ms,
                body: i.body.clone(),
            })
            .collect()
    }

    /// Submit one prompt to ComfyUI under its own id. Non-200 answers become failed jobs.
    pub async fn submit(&mut self, http: &reqwest::Client, item: Item) {
        let mut req = http
            .post(format!("{}/prompt", self.backend))
            .timeout(Duration::from_secs(120))
            .json(&item.body);
        for (k, v) in &item.headers {
            req = req.header(k, v);
        }
        let why = match req.send().await {
            Ok(r) if r.status().as_u16() == 200 => None,
            Ok(r) => {
                let text = r.text().await.unwrap_or_default();
                Some(text.chars().take(1000).collect::<String>())
            }
            Err(e) => Some(format!("could not submit to ComfyUI: {e}")),
        };
        if let Some(why) = why {
            tracing::warn!("held prompt {} not accepted by ComfyUI: {why}", item.id);
            self.record_failure(item.id.clone(), why);
        }
        self.sent.insert(item.id.clone(), item);
    }

    pub async fn free(&self, http: &reqwest::Client, body: Value) {
        let r = http
            .post(format!("{}/free", self.backend))
            .timeout(Duration::from_secs(30))
            .json(&body)
            .send()
            .await;
        if let Err(e) = r {
            tracing::warn!("ComfyUI /free failed: {e}");
        }
    }

    async fn queue(&self, http: &reqwest::Client) -> Option<Value> {
        http.get(format!("{}/queue", self.backend))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()
    }

    /// Take the prompts ComfyUI queued but did not start back to the front of the held queue.
    pub async fn reclaim(&mut self, http: &reqwest::Client) {
        let Some(q) = self.queue(http).await else {
            return;
        };
        let pending = q["queue_pending"].as_array().cloned().unwrap_or_default();
        if pending.is_empty() {
            return;
        }
        let ids: Vec<&str> = pending.iter().filter_map(|p| p[1].as_str()).collect();
        let del = http
            .post(format!("{}/queue", self.backend))
            .timeout(Duration::from_secs(10))
            .json(&json!({ "delete": ids }))
            .send()
            .await;
        if let Err(e) = del {
            tracing::warn!("could not take queued prompts back from ComfyUI: {e}");
            return;
        }
        // One may have started between the two calls; deleting it did nothing, so leave it.
        let started: BTreeSet<String> = self
            .queue(http)
            .await
            .and_then(|q| q["queue_running"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|r| r[1].as_str().map(str::to_owned))
            .collect();
        let mut back: Vec<(u64, Item)> = Vec::new();
        for p in &pending {
            let Some(id) = p[1].as_str() else { continue };
            if started.contains(id) {
                continue;
            }
            let item = self.sent.remove(id).unwrap_or_else(|| Item {
                id: id.to_owned(),
                number: 0,
                body: json!({"prompt": p[2], "extra_data": p[3], "prompt_id": id}),
                headers: Vec::new(),
                since_ms: now_ms(),
            });
            back.push((p[0].as_u64().unwrap_or(0), item));
        }
        back.sort_by_key(|(n, _)| *n);
        let count = back.len();
        let mut items: Vec<Item> = back.into_iter().map(|(_, i)| i).collect();
        for item in &mut items {
            item.number = self.number();
        }
        for item in items.into_iter().rev() {
            self.held.push_front(item);
        }
        tracing::info!("took {count} queued prompt(s) back from ComfyUI to hand the card over");
    }
}

fn new_id() -> String {
    // Unique enough for a queue id: time plus a process-wide counter.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("gpud-{t:x}-{n:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_weight_files_only() {
        let prompt = json!({
            "1": {"inputs": {"unet_name": "a.safetensors", "steps": 20, "text": "hello"}},
            "2": {"inputs": {"vae": "v.SFT", "ref": ["1", 0]}},
            "3": "junk",
        });
        let files = model_files(&prompt);
        assert_eq!(
            files.into_iter().collect::<Vec<_>>(),
            vec!["a.safetensors", "v.SFT"]
        );
        assert!(model_files(&Value::Null).is_empty());
    }
}
