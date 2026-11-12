//! Read-only detection of btrfs snapshots (Task 1.2.1c). `btrfs subvolume list` needs root, so nothing
//! here runs a command: the snapper and Timeshift layouts are recognised from the directory tree through
//! the `Fs` port and the mount table. Sizes stay [`SnapshotSize::Unknown`]: `btrfs qgroup show` needs root
//! and quotas, and the tool invents no number.

use std::io;
use std::path::{Path, PathBuf};

use dunnage_core::ports::{Env, Fs};
use dunnage_core::{FileKind, Platform};

const SNAPPER_DIR: &str = ".snapshots";
const TIMESHIFT_DIR: &str = "/timeshift-btrfs/snapshots";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotLayout {
    Snapper,
    Timeshift,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotSize {
    Unknown,
}

/// One snapshot. `root` is the directory that mirrors the snapshotted subvolume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotEntry {
    /// Snapper number (`12`) or Timeshift directory name (`2026-10-01_10-00-01`).
    pub name: String,
    pub date: Option<String>,
    pub description: Option<String>,
    pub size: SnapshotSize,
    pub root: PathBuf,
}

/// Answer of [`SnapshotInfo::holds`]; defined in `core` so `RunSummary::finish` can take it.
pub use dunnage_core::Held;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotInfo {
    pub layout: SnapshotLayout,
    pub snapshots: Vec<SnapshotEntry>,
    /// False when the snapshot directory exists but this user cannot list it (`/.snapshots` often is
    /// root-only), so `snapshots` is empty although some exist.
    pub listing_readable: bool,
}

impl SnapshotInfo {
    /// `None` on macOS and when neither layout exists. Reads only; no command is run.
    pub fn detect(env: &Env<'_>) -> Option<SnapshotInfo> {
        if env.probe.platform() != Platform::Linux {
            return None;
        }
        let fs = env.fs;
        let mut bases = vec![PathBuf::from("/")];
        if let Ok(mounts) = env.probe.mounts() {
            for m in mounts {
                if !bases.contains(&m.mount_point) {
                    bases.push(m.mount_point);
                }
            }
        }
        detect_snapper(fs, &bases).or_else(|| detect_timeshift(fs))
    }

    /// Whether `rel_path` (relative to the snapshotted subvolume) exists inside any snapshot: an `lstat`
    /// of the same relative path in each. `Found` wins over `Unreadable`, which wins over `Absent`.
    pub fn holds(&self, fs: &dyn Fs, rel_path: &Path) -> Held {
        let rel = rel_path.strip_prefix("/").unwrap_or(rel_path);
        let mut unreadable = !self.listing_readable;
        for s in &self.snapshots {
            match fs.lstat(&s.root.join(rel)) {
                Ok(_) => return Held::Found,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(_) => unreadable = true,
            }
        }
        if unreadable {
            Held::Unreadable
        } else {
            Held::Absent
        }
    }
}

fn is_dir(fs: &dyn Fs, p: &Path) -> bool {
    fs.lstat(p).is_ok_and(|s| s.kind == FileKind::Dir)
}

fn detect_snapper(fs: &dyn Fs, bases: &[PathBuf]) -> Option<SnapshotInfo> {
    let mut found = false;
    let mut readable = true;
    let mut snapshots = Vec::new();
    for base in bases {
        let dir = base.join(SNAPPER_DIR);
        if !is_dir(fs, &dir) {
            continue;
        }
        found = true;
        let Ok(entries) = fs.read_dir(&dir) else {
            readable = false;
            continue;
        };
        for e in entries {
            let name = e.name.to_string_lossy().into_owned();
            if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            let snap_dir = dir.join(&e.name);
            let info = fs.read_to_string(&snap_dir.join("info.xml")).ok();
            snapshots.push(SnapshotEntry {
                date: info.as_deref().and_then(|x| xml_text(x, "date")),
                description: info.as_deref().and_then(|x| xml_text(x, "description")),
                name,
                size: SnapshotSize::Unknown,
                root: snap_dir.join("snapshot"),
            });
        }
    }
    snapshots.sort_by_key(|s| s.name.parse::<u64>().unwrap_or(u64::MAX));
    found.then_some(SnapshotInfo {
        layout: SnapshotLayout::Snapper,
        snapshots,
        listing_readable: readable,
    })
}

fn detect_timeshift(fs: &dyn Fs) -> Option<SnapshotInfo> {
    let dir = Path::new(TIMESHIFT_DIR);
    if !is_dir(fs, dir) {
        return None;
    }
    let (names, readable) = match fs.read_dir(dir) {
        Ok(v) => (v, true),
        Err(_) => (Vec::new(), false),
    };
    let mut snapshots: Vec<SnapshotEntry> = names
        .into_iter()
        .filter(|e| e.kind == FileKind::Dir)
        .map(|e| SnapshotEntry {
            name: e.name.to_string_lossy().into_owned(),
            date: None,
            description: None,
            size: SnapshotSize::Unknown,
            root: dir.join(&e.name).join("@"),
        })
        .collect();
    snapshots.sort_by(|a, b| a.name.cmp(&b.name));
    Some(SnapshotInfo {
        layout: SnapshotLayout::Timeshift,
        snapshots,
        listing_readable: readable,
    })
}

/// Text of the first `<tag>..</tag>` element; snapper's `info.xml` is flat, so no XML parser is needed.
fn xml_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&format!("</{tag}>"))? + start;
    Some(xml[start..end].trim().to_string())
}
