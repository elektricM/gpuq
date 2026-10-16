//! Escape hatch for any vendor: a configured command prints the numbers.

use async_trait::async_trait;

use super::{run_tool, ProbeError, VramProbe, VramReading, TOOL_TIMEOUT};

/// The command must print `used_mib total_mib` separated by whitespace or commas, plus an
/// optional third number for GTT (system memory mapped for the GPU).
#[derive(Debug)]
pub struct CommandProbe {
    argv: Vec<String>,
}

impl CommandProbe {
    pub fn new(argv: Vec<String>) -> Result<Self, ProbeError> {
        if argv.is_empty() {
            return Err(ProbeError::NotFound(
                "vendor = \"command\" needs a non-empty gpu.command".into(),
            ));
        }
        Ok(Self { argv })
    }
}

fn parse(out: &str) -> Option<VramReading> {
    let nums: Vec<u64> = out
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|t| !t.is_empty())
        .map(|t| t.parse().ok())
        .collect::<Option<_>>()?;
    match nums[..] {
        [used, total] => Some(VramReading {
            used_mib: used,
            total_mib: total,
            gtt_used_mib: None,
        }),
        [used, total, gtt] => Some(VramReading {
            used_mib: used,
            total_mib: total,
            gtt_used_mib: Some(gtt),
        }),
        _ => None,
    }
}

#[async_trait]
impl VramProbe for CommandProbe {
    fn device(&self) -> String {
        "command".into()
    }

    async fn read(&self) -> Result<VramReading, ProbeError> {
        let out = run_tool(&self.argv, TOOL_TIMEOUT).await?;
        parse(&out).ok_or_else(|| {
            ProbeError::Parse(format!(
                "{}: expected \"used_mib total_mib [gtt_mib]\", got {:?}",
                self.argv[0],
                out.trim()
            ))
        })
    }
}
