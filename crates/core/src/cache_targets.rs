//! The cache table and cache targets (Story 1.3.2). Pure: which cache directories exist per OS, and which
//! tools are asked for theirs. Discovery (sizes, guards) is `adapters::discover_cache`.
//!
//! Maven is deliberately absent: `~/.m2/repository` and its `*-SNAPSHOT` directories are not cleaned in v1.

use crate::platform::Platform;
use crate::roots::CacheTool;

/// Where a cache row lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheSource {
    /// Relative to `$HOME`.
    Home(&'static str),
    /// Absolute, outside `$HOME` (only apt).
    Absolute(&'static str),
    /// The tool names its own directory (`go env GOCACHE`, `pip cache dir`).
    Tool(CacheTool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsSet {
    Linux,
    Macos,
    Both,
}

impl OsSet {
    pub fn includes(self, p: Platform) -> bool {
        matches!(
            (self, p),
            (OsSet::Both, _) | (OsSet::Linux, Platform::Linux) | (OsSet::Macos, Platform::Macos)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheRow {
    pub label: &'static str,
    pub source: CacheSource,
    pub os: OsSet,
    /// Only root may clear it (apt).
    pub needs_root: bool,
}

impl CacheRow {
    /// The apt cache sits behind `cache-apt`, every other cache behind `caches`.
    pub fn is_apt(&self) -> bool {
        self.needs_root && matches!(self.source, CacheSource::Absolute(_))
    }
}

const fn row(label: &'static str, source: CacheSource, os: OsSet) -> CacheRow {
    CacheRow {
        label,
        source,
        os,
        needs_root: false,
    }
}

use CacheSource::{Absolute, Home, Tool};
use OsSet::{Both, Linux, Macos};

/// Every row of both platforms. Ported from the old `LINUX_CACHE_TARGETS` / `MACOS_CACHE_TARGETS` plus the
/// Linux Playwright cache, apt, Homebrew and the two tool-owned caches.
pub const CACHE_ROWS: &[CacheRow] = &[
    row("npm _cacache", Home(".npm/_cacache"), Both),
    row("Gradle caches", Home(".gradle/caches"), Both),
    row("Cargo registry cache", Home(".cargo/registry/cache"), Both),
    row("Cargo registry src", Home(".cargo/registry/src"), Both),
    row("Playwright browsers", Home(".cache/ms-playwright"), Linux),
    CacheRow {
        label: "apt archives",
        source: Absolute("/var/cache/apt/archives"),
        os: Linux,
        needs_root: true,
    },
    row("JetBrains caches", Home("Library/Caches/JetBrains"), Macos),
    row(
        "Xcode DerivedData cache",
        Home("Library/Caches/com.apple.dt.Xcode"),
        Macos,
    ),
    row(
        "Xcode DerivedData",
        Home("Library/Developer/Xcode/DerivedData"),
        Macos,
    ),
    row(
        "Playwright browsers",
        Home("Library/Caches/ms-playwright"),
        Macos,
    ),
    row("Homebrew cache", Home("Library/Caches/Homebrew"), Macos),
    row("Go build cache", Tool(CacheTool::Go), Both),
    row("pip cache", Tool(CacheTool::Pip), Both),
];

/// The rows that apply on `platform`, in table order.
pub fn cache_table(platform: Platform) -> Vec<CacheRow> {
    CACHE_ROWS
        .iter()
        .copied()
        .filter(|r| r.os.includes(platform))
        .collect()
}

/// A row with whether this platform supports it; an unsupported one carries the stated reason
/// (`not supported on macOS`) so it is never silently omitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowAvailability {
    pub row: CacheRow,
    pub unsupported: Option<String>,
}

pub fn platform_name(p: Platform) -> &'static str {
    match p {
        Platform::Linux => "Linux",
        Platform::Macos => "macOS",
    }
}

/// Every row with its availability on `platform`.
pub fn cache_table_with_availability(platform: Platform) -> Vec<RowAvailability> {
    CACHE_ROWS
        .iter()
        .map(|r| RowAvailability {
            row: *r,
            unsupported: (!r.os.includes(platform))
                .then(|| format!("not supported on {}", platform_name(platform))),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_are_relative_or_absolute_and_labelled() {
        for r in CACHE_ROWS {
            assert!(!r.label.is_empty());
            match r.source {
                Home(p) => assert!(!p.starts_with('/'), "{p}"),
                Absolute(p) => assert!(p.starts_with('/'), "{p}"),
                Tool(_) => {}
            }
        }
    }
}
