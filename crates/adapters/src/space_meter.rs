//! `RealSpaceMeter`: df numbers plus btrfs allocation for one path (Story 1.2.1). Every read is read-only.
//! The sysfs read lives here, never in `core`; `core` only receives the mapped [`AllocSnapshot`].
//!
//! The mount table and sysfs root are injected so a test can point the adapter at a fixture; on macOS
//! neither is touched (btrfs has no meaning there, and the metascope readers are not `cfg`-gated).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dunnage_core::ports::SpaceMeter;
use dunnage_core::{AllocSnapshot, FsSnapshot, Platform, Settled, SpaceUse};
use dunnage_scan::btrfs::{self, Allocation, SpaceInfo};

use crate::{settle, statvfs};

const MOUNTINFO: &str = "/proc/self/mountinfo";
/// Shown when the filesystem type cannot be named; the report prints `n/a (filesystem is unknown)`.
pub const UNKNOWN_FS: &str = "unknown";
const SETTLE_POLL: Duration = Duration::from_millis(250);

/// Where the filesystem type and the btrfs allocation come from. Not called at all on macOS.
pub trait AllocSource: Send + Sync {
    /// Type of the filesystem holding `path`, as the mount table names it.
    fn fs_type(&self, path: &Path) -> Option<String>;
    /// Allocation of the btrfs filesystem holding `path`.
    fn read_allocation(&self, path: &Path) -> Result<AllocSnapshot, String>;
}

/// Mount table text plus a sysfs directory, both overridable for fixtures.
#[derive(Debug, Clone)]
pub struct SysfsAllocSource {
    mountinfo: PathBuf,
    sysfs_root: PathBuf,
}

impl Default for SysfsAllocSource {
    fn default() -> Self {
        SysfsAllocSource::new(MOUNTINFO, btrfs::SYSFS_BTRFS)
    }
}

impl SysfsAllocSource {
    pub fn new(mountinfo: impl Into<PathBuf>, sysfs_root: impl Into<PathBuf>) -> SysfsAllocSource {
        SysfsAllocSource {
            mountinfo: mountinfo.into(),
            sysfs_root: sysfs_root.into(),
        }
    }

    fn table(&self) -> Option<String> {
        fs::read_to_string(&self.mountinfo).ok()
    }
}

impl AllocSource for SysfsAllocSource {
    fn fs_type(&self, path: &Path) -> Option<String> {
        fs_type_for_path(&self.table()?, &canonical(path))
    }

    fn read_allocation(&self, path: &Path) -> Result<AllocSnapshot, String> {
        let table = self.table().ok_or("mount table unreadable")?;
        let source = btrfs::btrfs_source_for_path(&table, &canonical(path))
            .ok_or_else(|| format!("{} is not on a btrfs mount", path.display()))?;
        let dir = btrfs::fs_dir_for_source(&self.sysfs_root, &source)
            .ok_or_else(|| format!("no sysfs entry for {source}"))?;
        btrfs::read_allocation(&dir)
            .map(|a| map_allocation(&a))
            .map_err(|e| e.to_string())
    }
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Filesystem type of the longest mount point containing `path` (mountinfo field after the `-` separator).
pub fn fs_type_for_path(mountinfo: &str, path: &Path) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for line in mountinfo.lines() {
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        let (Some(mp), Some(fstype)) = (
            left.split_whitespace().nth(4),
            right.split_whitespace().next(),
        ) else {
            continue;
        };
        let mp = mp.replace("\\040", " ");
        if path.starts_with(&mp) && best.as_ref().is_none_or(|(len, _)| mp.len() > *len) {
            best = Some((mp.len(), fstype.to_string()));
        }
    }
    best.map(|(_, t)| t)
}

fn map_space(s: &SpaceInfo) -> SpaceUse {
    SpaceUse {
        bytes_used: s.bytes_used,
        total_bytes: s.total_bytes,
    }
}

/// DC's own `AllocSnapshot` from the scan crate's `Allocation`.
pub fn map_allocation(a: &Allocation) -> AllocSnapshot {
    AllocSnapshot {
        data: map_space(&a.data),
        metadata: map_space(&a.metadata),
        system: map_space(&a.system),
        unallocated: a.unallocated_bytes(),
    }
}

/// True when the figures a rootless run prints can understate what snapshots or other users hold: the
/// summary then carries the `rootless estimate` note (UX S3).
pub fn estimate_is_lower_bound(euid: u32) -> bool {
    euid != 0
}

pub struct RealSpaceMeter {
    platform: Platform,
    source: Box<dyn AllocSource>,
}

impl RealSpaceMeter {
    pub fn new(platform: Platform) -> RealSpaceMeter {
        RealSpaceMeter::with_source(platform, Box::new(SysfsAllocSource::default()))
    }

    pub fn with_source(platform: Platform, source: Box<dyn AllocSource>) -> RealSpaceMeter {
        RealSpaceMeter { platform, source }
    }

    fn fs_type(&self, path: &Path) -> String {
        match self.platform {
            Platform::Macos => darwin_fs_type(path),
            Platform::Linux => self
                .source
                .fs_type(path)
                .unwrap_or_else(|| UNKNOWN_FS.to_string()),
        }
    }
}

/// The allocation of the btrfs filesystem whose sysfs directory is `dir` (`status --fs DIR`).
pub fn read_fs_dir_allocation(dir: &Path) -> Result<AllocSnapshot, String> {
    btrfs::read_allocation(dir)
        .map(|a| map_allocation(&a))
        .map_err(|e| e.to_string())
}

impl SpaceMeter for RealSpaceMeter {
    fn try_snapshot(&self, path: &Path) -> std::io::Result<FsSnapshot> {
        statvfs::df(path)?;
        Ok(self.snapshot(path))
    }

    /// A failed `statvfs` yields zero df numbers and fs type `unknown`: the port is infallible, so the
    /// caller prints `n/a` rather than a wrong figure.
    fn snapshot(&self, path: &Path) -> FsSnapshot {
        let df = statvfs::df(path).ok();
        let fs_type = self.fs_type(path);
        let btrfs = (self.platform == Platform::Linux && fs_type == "btrfs")
            .then(|| self.source.read_allocation(path).ok())
            .flatten();
        FsSnapshot {
            path: path.to_path_buf(),
            taken_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX)),
            total_bytes: df.map_or(0, |d| d.total_bytes),
            used_bytes: df.map_or(0, |d| d.used_bytes),
            avail_bytes: df.map_or(0, |d| d.avail_bytes),
            fs_type,
            btrfs,
        }
    }

    fn settle(&self, path: &Path, timeout: Duration) -> Settled {
        settle::settle(
            || settle::sync_filesystem(path),
            || self.snapshot(path).btrfs.map(|a| a.data.bytes_used),
            timeout,
            SETTLE_POLL,
        )
    }
}

/// `f_fstypename` of `statfs(2)`. Not called on Linux.
#[cfg(target_os = "macos")]
fn darwin_fs_type(path: &Path) -> String {
    use std::ffi::{CStr, CString};
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
        return UNKNOWN_FS.to_string();
    };
    // A u64 buffer is 8-aligned and at least as large as `statfs`, so no field of the layout (which
    // differs between libc versions) is spelled out here.
    let mut buf = [0_u64; std::mem::size_of::<libc::statfs>().div_ceil(8)];
    let st = buf.as_mut_ptr().cast::<libc::statfs>();
    // SAFETY: `c` is a valid NUL-terminated path and `st` points at a suitably aligned buffer of at least
    // `size_of::<statfs>()` bytes, which the call fills.
    if unsafe { libc::statfs(c.as_ptr(), st) } != 0 {
        return UNKNOWN_FS.to_string();
    }
    // SAFETY: on success `st` holds an initialised `statfs` whose `f_fstypename` the kernel NUL-terminates.
    let name = unsafe { CStr::from_ptr((*st).f_fstypename.as_ptr()) };
    name.to_string_lossy().into_owned()
}

#[cfg(not(target_os = "macos"))]
fn darwin_fs_type(_path: &Path) -> String {
    UNKNOWN_FS.to_string()
}

/// The meter of a run. A `test-support` build with `DC_FAKE_STARVED` set reports a starved btrfs (100 MiB
/// unallocated, no metadata headroom) so the spawned binary's exit-3 refusal is testable; any other build
/// returns `inner` unchanged and holds no such variable name.
pub fn run_meter(inner: RealSpaceMeter) -> Box<dyn dunnage_core::ports::SpaceMeter> {
    #[cfg(feature = "test-support")]
    if std::env::var_os("DC_FAKE_STARVED").is_some() {
        return Box::new(starved::StarvedMeter(inner));
    }
    Box::new(inner)
}

#[cfg(feature = "test-support")]
mod starved {
    use std::path::Path;
    use std::time::Duration;

    use dunnage_core::ports::SpaceMeter;
    use dunnage_core::snapshot_types::{AllocSnapshot, FsSnapshot, Settled, SpaceUse};

    pub struct StarvedMeter(pub super::RealSpaceMeter);

    impl SpaceMeter for StarvedMeter {
        fn snapshot(&self, path: &Path) -> FsSnapshot {
            let mut s = self.0.snapshot(path);
            let space = |used: u64, total: u64| SpaceUse {
                bytes_used: used,
                total_bytes: total,
            };
            s.fs_type = "btrfs".into();
            s.btrfs = Some(AllocSnapshot {
                data: space(900 << 30, 950 << 30),
                metadata: space(10 << 30, (10 << 30) + (10 << 20)),
                system: space(1 << 20, 8 << 20),
                unallocated: 100 << 20,
            });
            s
        }

        fn settle(&self, path: &Path, timeout: Duration) -> Settled {
            self.0.settle(path, timeout)
        }
    }
}
