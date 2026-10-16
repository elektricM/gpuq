//! VRAM size strings such as "16G", "6144M", "512" or "all".

use crate::arbiter::Vram;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("bad VRAM size {0:?}: use e.g. 8G, 6144M, 512 (MiB) or all")]
pub struct VramParseError(pub String);

/// "all" is the whole card; suffixes g/gb/gib (x1024) and m/mb/mib; no suffix means MiB.
/// Fractions are allowed ("1.5g" is 1536 MiB).
pub fn parse_vram(s: &str) -> Result<Vram, VramParseError> {
    let err = || VramParseError(s.to_string());
    let t = s.trim().to_ascii_lowercase();
    if t == "all" {
        return Ok(Vram::WholeCard);
    }
    let (num, mult) = [
        ("gib", 1024.0),
        ("mib", 1.0),
        ("gb", 1024.0),
        ("mb", 1.0),
        ("g", 1024.0),
        ("m", 1.0),
    ]
    .iter()
    .find_map(|(suffix, mult)| t.strip_suffix(suffix).map(|n| (n, *mult)))
    .unwrap_or((t.as_str(), 1.0));
    let value: f64 = num.trim().parse().map_err(|_| err())?;
    let mib = (value * mult).round();
    if !mib.is_finite() || mib < 1.0 || mib >= u64::MAX as f64 {
        return Err(err());
    }
    Ok(Vram::Mib(mib as u64))
}
