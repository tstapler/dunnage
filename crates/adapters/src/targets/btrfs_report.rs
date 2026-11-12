//! The btrfs-specific notes of a report (Story 4.3.1, Task 4.3.1a): snapshots can make a freed-bytes estimate
//! overcount, and enabled qgroups make a balance slow. Read-only: the snapshot layout comes from
//! [`SnapshotInfo`] (no command is run) and quotas from the sysfs `qgroups/` directory, which the kernel
//! creates only while quotas are enabled (INFERRED from the kernel's `btrfs_sysfs_add_qgroups`; this host
//! has quotas off, so only the absent case was seen on real sysfs). Neither note invents a number.

use std::fs;
use std::path::Path;

use dunnage_core::ports::Env;
use dunnage_scan::btrfs::{btrfs_source_for_path, fs_dir_for_source, SYSFS_BTRFS};

use crate::snapshots::{SnapshotInfo, SnapshotLayout};

const MOUNTINFO: &str = "/proc/self/mountinfo";
const NAMES_SHOWN: usize = 3;
pub const QGROUP_NOTE: &str = "qgroups enabled: balance may be slow";

/// `snapshots present (snapper: #12 2026-09-01, #13 2026-10-01): freed-bytes estimates may overcount shared
/// extents`, with at most the first three names and `+N more`. `None` when no snapshot was detected. A
/// snapshot directory this user cannot list still counts as present, since it holds some.
#[must_use]
pub fn snapshot_note(info: Option<&SnapshotInfo>) -> Option<String> {
    let info = info?;
    let layout = match info.layout {
        SnapshotLayout::Snapper => "snapper",
        SnapshotLayout::Timeshift => "timeshift",
    };
    let detail = if info.snapshots.is_empty() {
        if info.listing_readable {
            return None;
        }
        "listing needs root".to_string()
    } else {
        let mut names: Vec<String> = info
            .snapshots
            .iter()
            .take(NAMES_SHOWN)
            .map(|s| match info.layout {
                SnapshotLayout::Snapper => {
                    match s.date.as_deref().and_then(|d| d.split_whitespace().next()) {
                        Some(day) => format!("#{} {day}", s.name),
                        None => format!("#{}", s.name),
                    }
                }
                SnapshotLayout::Timeshift => s.name.clone(),
            })
            .collect();
        if info.snapshots.len() > NAMES_SHOWN {
            names.push(format!("+{} more", info.snapshots.len() - NAMES_SHOWN));
        }
        names.join(", ")
    };
    Some(format!(
        "snapshots present ({layout}: {detail}): freed-bytes estimates may overcount shared extents"
    ))
}

/// The qgroup note when quotas are enabled on the btrfs filesystem that holds `path`. `mountinfo` is the mount
/// table, `sysfs_root` is `/sys/fs/btrfs`; both are parameters so a fixture can stand in for them.
#[must_use]
pub fn qgroup_note(mountinfo: &Path, sysfs_root: &Path, path: &Path) -> Option<String> {
    let table = fs::read_to_string(mountinfo).ok()?;
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let source = btrfs_source_for_path(&table, &canonical)?;
    let dir = fs_dir_for_source(sysfs_root, &source)?;
    dir.join("qgroups")
        .is_dir()
        .then(|| QGROUP_NOTE.to_string())
}

/// Both notes for the real machine, snapshot note first: what a plan, an apply summary or `status` adds
/// under its notes. Empty on macOS and on a filesystem that is not btrfs.
#[must_use]
pub fn report_notes(env: &Env<'_>, path: &Path) -> Vec<String> {
    report_notes_from(env, path, Path::new(MOUNTINFO), Path::new(SYSFS_BTRFS))
}

/// [`report_notes`] over a given mount table and sysfs root.
#[must_use]
pub fn report_notes_from(
    env: &Env<'_>,
    path: &Path,
    mountinfo: &Path,
    sysfs_root: &Path,
) -> Vec<String> {
    let mut notes = Vec::new();
    notes.extend(snapshot_note(SnapshotInfo::detect(env).as_ref()));
    notes.extend(qgroup_note(mountinfo, sysfs_root, path));
    notes
}
