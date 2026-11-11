//! Cache paint and the explorer's cache file (Task 6.3.1b).
//!
//! The walker already holds the cache's records in its own map and emits them as `painted` events before the
//! live pass (Story 6.1.2), so one `cache::load` serves both; this module only names the file, maps the
//! flags (`--no-cache`, `--rescan`) to the walker's cache config, tags batches with their reducer mode, and
//! decides whether the end-of-scan save may take the last free space (UX-T22).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use disk_cleanup_core::explore::msg::{NodeEvent, ScanMode};
use disk_cleanup_core::ports::SpaceMeter;
use disk_cleanup_core::{Dirs, FsSnapshot};
use disk_cleanup_scan::health::{assess, Thresholds, Verdict, GIB};
use disk_cleanup_scan::index::{DirEvent, SaveGuard};
use disk_cleanup_scan::walker::CacheConfig;

use crate::space_meter::UNKNOWN_FS;

/// File name inside `$XDG_CACHE_HOME/disk-cleanup`, separate from the cleaner's `walker.cache` so the
/// cleaner's load time and the speed protocol are unchanged (ADR-005, owner decision O-3).
pub const EXPLORER_CACHE: &str = "explorer.cache";

/// Re-list cached directories older than this (the `scan` default).
pub const DEFAULT_MAX_AGE: Duration = Duration::from_hours(24);

/// A save needs this much free beyond four times the previous cache (INFERRED margin).
const FREE_FLOOR: u64 = GIB;
/// btrfs: refuse below this much unallocated space (INFERRED).
const BTRFS_UNALLOCATED_FLOOR: u64 = GIB;

/// What the cache does in one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheMode {
    /// Read it, paint from it, rewrite it.
    Use,
    /// `--no-cache`: nothing is read or written.
    Off,
    /// `--rescan`: nothing is read, the cache is rewritten.
    Rescan,
}

/// `$XDG_CACHE_HOME/disk-cleanup/explorer.cache`.
pub fn explorer_cache_path(dirs: &Dirs) -> PathBuf {
    dirs.cache.join("disk-cleanup").join(EXPLORER_CACHE)
}

/// The walker's cache config for `mode`; `None` for `--no-cache`.
pub fn cache_config(dirs: &Dirs, mode: CacheMode, max_age: Duration) -> Option<CacheConfig> {
    (mode != CacheMode::Off).then(|| CacheConfig {
        path: explorer_cache_path(dirs),
        rescan: mode == CacheMode::Rescan,
        max_age,
    })
}

/// Whether `ScanOpts::paint_from_cache` should be set: only a run that reads the cache can paint.
pub fn paints(mode: CacheMode) -> bool {
    mode == CacheMode::Use
}

/// Tags the walker's events with the reducer mode and groups consecutive events of one mode: painted ones
/// are `Paint`, the rest `Revalidate` when a paint was requested and `Fresh` otherwise. The walker flushes
/// after the paint, so a mixed batch is not expected; splitting keeps the per-batch mode honest if one
/// ever appears.
pub fn paint_batches(
    events: Vec<DirEvent>,
    paint_requested: bool,
) -> Vec<(ScanMode, Vec<NodeEvent>)> {
    let mut out: Vec<(ScanMode, Vec<NodeEvent>)> = Vec::new();
    for e in events {
        let painted = matches!(e, DirEvent::Dir { painted: true, .. });
        let mode = match (painted, paint_requested) {
            (true, _) => ScanMode::Paint,
            (false, true) => ScanMode::Revalidate,
            (false, false) => ScanMode::Fresh,
        };
        let Some(node) = NodeEvent::from_dir_event(e) else {
            continue;
        };
        match out.last_mut() {
            Some((m, group)) if *m == mode => group.push(node),
            _ => out.push((mode, vec![node])),
        }
    }
    out
}

pub const NEARLY_FULL: &str = "disk nearly full";
pub const STATE_UNREADABLE: &str = "filesystem state unreadable";

/// Whether a cache of about `prev_len` bytes may be written given `snapshot`. The reasons complete the note
/// `cache not saved: <reason>`. An unknown or unreadable snapshot fails closed. With no previous cache
/// (`prev_len` 0) the first save relies on the 1 GiB floor and on `save_atomic`'s own refusal.
pub fn save_room(snapshot: &io::Result<FsSnapshot>, prev_len: u64) -> Result<(), &'static str> {
    let Ok(snap) = snapshot else {
        return Err(STATE_UNREADABLE);
    };
    if snap.fs_type == UNKNOWN_FS {
        return Err(STATE_UNREADABLE);
    }
    if snap.avail_bytes < prev_len.saturating_mul(4).saturating_add(FREE_FLOOR) {
        return Err(NEARLY_FULL);
    }
    if let Some(alloc) = snap.btrfs {
        let crit = assess(
            alloc.unallocated,
            alloc.metadata_headroom_bytes(),
            &Thresholds::default(),
        )
        .verdict
            == Verdict::Crit;
        if crit || alloc.unallocated < BTRFS_UNALLOCATED_FLOOR {
            return Err(NEARLY_FULL);
        }
    }
    Ok(())
}

/// The nearest existing ancestor of `p` (the cache directory may not exist yet).
fn existing_ancestor(p: &Path) -> &Path {
    p.ancestors()
        .find(|a| a.exists())
        .unwrap_or_else(|| Path::new("/"))
}

/// The guard the walker asks just before it writes the cache: the filesystem is read afresh then.
pub fn save_guard(meter: Arc<dyn SpaceMeter>) -> SaveGuard {
    Arc::new(move |cache: &Path| {
        let prev_len = std::fs::metadata(cache).map_or(0, |m| m.len());
        let dir = cache.parent().unwrap_or_else(|| Path::new("."));
        let snapshot = meter.try_snapshot(existing_ancestor(dir));
        save_room(&snapshot, prev_len).map_err(str::to_owned)
    })
}
