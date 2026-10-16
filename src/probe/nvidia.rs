//! NVIDIA memory from `nvidia-smi --query-gpu`.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::Mutex;

use super::{run_tool, ProbeError, VramProbe, VramReading, TOOL_TIMEOUT};

pub const DEFAULT_TTL: Duration = Duration::from_millis(200);

/// One call returns used and total. A successful result is cached briefly so a burst of
/// readers spawns the tool once; failures are never cached.
#[derive(Debug)]
pub struct NvidiaSmiProbe {
    binary: String,
    index: u32,
    ttl: Duration,
    cache: Mutex<Option<(Instant, VramReading)>>,
}

impl NvidiaSmiProbe {
    pub fn new(binary: impl Into<String>, index: u32) -> Self {
        Self {
            binary: binary.into(),
            index,
            ttl: DEFAULT_TTL,
            cache: Mutex::new(None),
        }
    }

    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }
}

fn parse(out: &str) -> Option<(u64, u64)> {
    let mut it = out.trim().split(',');
    let used = it.next()?.trim().parse().ok()?;
    let total = it.next()?.trim().parse().ok()?;
    it.next().is_none().then_some((used, total))
}

#[async_trait]
impl VramProbe for NvidiaSmiProbe {
    fn device(&self) -> String {
        format!("nvidia:{}", self.index)
    }

    async fn read(&self) -> Result<VramReading, ProbeError> {
        // Held across the spawn so concurrent readers share one invocation.
        let mut cache = self.cache.lock().await;
        if let Some((at, r)) = *cache {
            if at.elapsed() < self.ttl {
                return Ok(r);
            }
        }
        let argv = [
            self.binary.clone(),
            "--query-gpu=memory.used,memory.total".into(),
            "--format=csv,noheader,nounits".into(),
            "-i".into(),
            self.index.to_string(),
        ];
        let out = run_tool(&argv, TOOL_TIMEOUT).await?;
        let (used, total) = parse(&out).ok_or_else(|| {
            ProbeError::Parse(format!(
                "{}: unexpected output {:?}",
                self.binary,
                out.trim()
            ))
        })?;
        let r = VramReading {
            used_mib: used,
            total_mib: total,
            gtt_used_mib: None,
        };
        *cache = Some((Instant::now(), r));
        Ok(r)
    }
}
