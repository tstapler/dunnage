//! `df` numbers for a path, through `statvfs` (read-only; the same call on Linux and macOS).

use std::io;
use std::path::Path;

/// Bytes as `df` reports them: `avail` is what an unprivileged user may use, so `used + avail` can be
/// below `total` by the root-reserved blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Df {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub avail_bytes: u64,
}

pub fn df(path: &Path) -> io::Result<Df> {
    let s = rustix::fs::statvfs(path)?;
    let unit = s.f_frsize;
    Ok(Df {
        total_bytes: s.f_blocks.saturating_mul(unit),
        used_bytes: s.f_blocks.saturating_sub(s.f_bfree).saturating_mul(unit),
        avail_bytes: s.f_bavail.saturating_mul(unit),
    })
}
