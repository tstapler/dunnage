//! What `dunnage status` reads (Story 1.2.4, Task 1.2.4e): one snapshot of the filesystem of a path,
//! the btrfs sysfs directory it came from, the old `metascope status` allocation document, and the verdict of
//! the scanner's `health::assess` thresholds (the same ones `metascope status` used). Read-only.

use std::path::{Path, PathBuf};

use dunnage_core::ports::SpaceMeter;
use dunnage_core::{errno_phrase, DiskVerdict, FsSnapshot};
use dunnage_scan::btrfs;
use dunnage_scan::health::{self, Finding, Thresholds, Verdict};
use serde_json::Value;

use crate::space_meter::map_allocation;

/// A `FsSnapshot` with what `status` adds to it.
#[derive(Debug, Clone)]
pub struct StatusRead {
    pub snapshot: FsSnapshot,
    /// `/sys/fs/btrfs/<uuid>`, when the path is on btrfs and the directory was found or given.
    pub fs_dir: Option<PathBuf>,
    /// The old `metascope status` `allocation` value, key for key.
    pub allocation: Option<Value>,
    /// btrfs, but the allocation could not be read without root.
    pub needs_root: bool,
}

/// The filesystem state could not be read at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReadError {
    pub path: PathBuf,
    pub reason: String,
}

/// Reads the state of the filesystem holding `path`. With `fs_dir` the allocation comes from that sysfs
/// directory (the old `--fs`), and failing to read it is an error; without it a btrfs path is looked up and
/// an unreadable sysfs only marks the fields `needs root`.
pub fn read_status(
    meter: &dyn SpaceMeter,
    path: &Path,
    fs_dir: Option<&Path>,
) -> Result<StatusRead, StatusReadError> {
    let err = |p: &Path, reason: String| StatusReadError {
        path: p.to_path_buf(),
        reason,
    };
    let mut snapshot = meter.try_snapshot(path).map_err(|e| {
        err(
            path,
            e.raw_os_error().map_or_else(|| e.to_string(), errno_phrase),
        )
    })?;
    if let Some(dir) = fs_dir {
        let alloc = btrfs::read_allocation(dir).map_err(|e| err(dir, e.to_string()))?;
        snapshot.btrfs = Some(map_allocation(&alloc));
        snapshot.fs_type = "btrfs".into();
        return Ok(StatusRead {
            snapshot,
            fs_dir: Some(dir.to_path_buf()),
            allocation: serde_json::to_value(alloc).ok(),
            needs_root: false,
        });
    }
    let is_btrfs = snapshot.fs_type == "btrfs";
    let found = (is_btrfs && snapshot.btrfs.is_some())
        .then(|| btrfs::find_fs_dir(path).ok())
        .flatten();
    let allocation = found
        .as_deref()
        .and_then(|d| btrfs::read_allocation(d).ok())
        .and_then(|a| serde_json::to_value(a).ok());
    Ok(StatusRead {
        needs_root: is_btrfs && snapshot.btrfs.is_none(),
        snapshot,
        fs_dir: found,
        allocation,
    })
}

/// A finding of the verdict, as the old document printed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusFinding {
    pub severity: &'static str,
    pub message: String,
}

fn disk_verdict(v: Verdict) -> DiskVerdict {
    match v {
        Verdict::Ok => DiskVerdict::Ok,
        Verdict::Warn => DiskVerdict::Warn,
        Verdict::Crit => DiskVerdict::Crit,
    }
}

fn finding(f: &Finding) -> StatusFinding {
    StatusFinding {
        severity: match f.severity {
            Verdict::Ok => "ok",
            Verdict::Warn => "warn",
            Verdict::Crit => "crit",
        },
        message: f.message.clone(),
    }
}

/// df-only thresholds for a filesystem whose allocation is not known (not btrfs, or the sysfs needs root):
/// the plan names "a df-only verdict" and no thresholds, so these are an assumption (used 90% warns, 95%
/// is critical).
const DF_WARN_PCT: u64 = 90;
const DF_CRIT_PCT: u64 = 95;

fn df_verdict(s: &FsSnapshot) -> (DiskVerdict, Vec<StatusFinding>) {
    if s.total_bytes == 0 {
        return (DiskVerdict::Unknown, Vec::new());
    }
    let pct = s.used_bytes.saturating_mul(100) / s.total_bytes;
    let (v, sev) = if pct >= DF_CRIT_PCT {
        (DiskVerdict::Crit, "crit")
    } else if pct >= DF_WARN_PCT {
        (DiskVerdict::Warn, "warn")
    } else {
        return (DiskVerdict::Ok, Vec::new());
    };
    (
        v,
        vec![StatusFinding {
            severity: sev,
            message: format!("filesystem is {pct}% used"),
        }],
    )
}

/// The verdict and findings: btrfs allocation through `health::assess` with its default thresholds, df
/// percent otherwise.
pub fn assess(s: &FsSnapshot) -> (DiskVerdict, Vec<StatusFinding>) {
    match s.btrfs {
        Some(a) => {
            let r = health::assess(
                a.unallocated,
                a.metadata_headroom_bytes(),
                &Thresholds::default(),
            );
            (
                disk_verdict(r.verdict),
                r.findings.iter().map(finding).collect(),
            )
        }
        None => df_verdict(s),
    }
}
