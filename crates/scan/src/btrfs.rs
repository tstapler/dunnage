//! Reads btrfs space accounting from sysfs (`/sys/fs/btrfs/<uuid>/`). No root needed.

use crate::{Error, Result};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

pub const SYSFS_BTRFS: &str = "/sys/fs/btrfs";
const SECTOR_BYTES: u64 = 512;

/// One block-group type (`allocation/{data,metadata,system}`).
/// `total_bytes` is logical chunk space; `disk_total` is raw bytes after DUP/RAID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SpaceInfo {
    pub total_bytes: u64,
    pub bytes_used: u64,
    pub disk_total: u64,
    pub disk_used: u64,
}

impl SpaceInfo {
    pub fn headroom_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.bytes_used)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Allocation {
    pub data: SpaceInfo,
    pub metadata: SpaceInfo,
    pub system: SpaceInfo,
    pub global_rsv_size: u64,
    pub global_rsv_reserved: u64,
    /// Sum of member device sizes.
    pub device_bytes: u64,
}

impl Allocation {
    /// Raw bytes not yet carved into any chunk: device size minus sum of `disk_total`.
    pub fn unallocated_bytes(&self) -> u64 {
        let allocated = self.data.disk_total + self.metadata.disk_total + self.system.disk_total;
        self.device_bytes.saturating_sub(allocated)
    }

    /// Free logical metadata space inside already-allocated metadata chunks.
    pub fn metadata_headroom_bytes(&self) -> u64 {
        self.metadata.headroom_bytes()
    }
}

fn read_u64(path: &Path) -> Result<u64> {
    let text = fs::read_to_string(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    text.trim().parse().map_err(|_| Error::Parse {
        path: path.to_path_buf(),
        value: text.trim().to_string(),
    })
}

fn read_space(dir: &Path) -> Result<SpaceInfo> {
    Ok(SpaceInfo {
        total_bytes: read_u64(&dir.join("total_bytes"))?,
        bytes_used: read_u64(&dir.join("bytes_used"))?,
        disk_total: read_u64(&dir.join("disk_total"))?,
        disk_used: read_u64(&dir.join("disk_used"))?,
    })
}

fn device_bytes(fs_dir: &Path) -> Result<u64> {
    let devices = fs_dir.join("devices");
    let entries = fs::read_dir(&devices).map_err(|source| Error::Io {
        path: devices.clone(),
        source,
    })?;
    let mut total = 0;
    for entry in entries {
        let entry = entry.map_err(|source| Error::Io {
            path: devices.clone(),
            source,
        })?;
        total += read_u64(&entry.path().join("size"))? * SECTOR_BYTES;
    }
    Ok(total)
}

/// Reads allocation from a filesystem's sysfs directory (or a fixture laid out the same way).
pub fn read_allocation(fs_dir: &Path) -> Result<Allocation> {
    let alloc = fs_dir.join("allocation");
    Ok(Allocation {
        data: read_space(&alloc.join("data"))?,
        metadata: read_space(&alloc.join("metadata"))?,
        system: read_space(&alloc.join("system"))?,
        global_rsv_size: read_u64(&alloc.join("global_rsv_size"))?,
        global_rsv_reserved: read_u64(&alloc.join("global_rsv_reserved"))?,
        device_bytes: device_bytes(fs_dir)?,
    })
}

/// Returns the source device of the longest-prefix btrfs mount containing `path`.
/// btrfs reports an anonymous st_dev, so the mount table is the only portable way to map back.
pub fn btrfs_source_for_path(mountinfo: &str, path: &Path) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for line in mountinfo.lines() {
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        let mut right = right.split_whitespace();
        let (fstype, source) = (right.next()?, right.next()?);
        if fstype != "btrfs" {
            continue;
        }
        let mount_point = left.split_whitespace().nth(4)?.replace("\\040", " ");
        let mp = Path::new(&mount_point);
        if path.starts_with(mp)
            && best
                .as_ref()
                .is_none_or(|(len, _)| mp.as_os_str().len() > *len)
        {
            best = Some((mp.as_os_str().len(), source.to_string()));
        }
    }
    best.map(|(_, source)| source)
}

/// Finds `<sysfs_root>/<uuid>` whose `devices/` contains the basename of `source`.
pub fn fs_dir_for_source(sysfs_root: &Path, source: &str) -> Option<PathBuf> {
    let name = Path::new(source).file_name()?;
    fs::read_dir(sysfs_root)
        .ok()?
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .find(|p| p.join("devices").join(name).is_dir())
}

/// Locates the sysfs directory for the btrfs filesystem containing `path`.
pub fn find_fs_dir(path: &Path) -> Result<PathBuf> {
    let canonical = fs::canonicalize(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mountinfo = fs::read_to_string("/proc/self/mountinfo").map_err(|source| Error::Io {
        path: PathBuf::from("/proc/self/mountinfo"),
        source,
    })?;
    let source = btrfs_source_for_path(&mountinfo, &canonical)
        .ok_or_else(|| Error::Other(format!("{} is not on a btrfs mount", canonical.display())))?;
    fs_dir_for_source(Path::new(SYSFS_BTRFS), &source).ok_or_else(|| {
        Error::Other(format!(
            "no {SYSFS_BTRFS}/<uuid>/devices/ entry for {source}; pass --fs explicitly"
        ))
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/real-2026-11-01")
    }

    #[test]
    fn parses_fixture_allocation() {
        let a = read_allocation(&fixture()).unwrap();
        assert_eq!(a.metadata.total_bytes, 27_388_805_120);
        assert_eq!(a.metadata.bytes_used, 26_842_611_712);
        assert_eq!(a.metadata.disk_total, 54_777_610_240);
        assert_eq!(a.data.disk_total, 1_945_233_850_368);
        assert_eq!(a.system.disk_total, 67_108_864);
        assert_eq!(a.global_rsv_size, 536_870_912);
        assert_eq!(a.device_bytes, 2_000_079_651_328);
    }

    #[test]
    fn derives_unallocated_and_headroom_from_fixture() {
        let a = read_allocation(&fixture()).unwrap();
        // 2_000_079_651_328 - (1_945_233_850_368 + 54_777_610_240 + 67_108_864)
        assert_eq!(a.unallocated_bytes(), 1_081_856);
        assert_eq!(a.metadata_headroom_bytes(), 546_193_408);
    }

    #[test]
    fn missing_dir_is_an_io_error() {
        assert!(matches!(
            read_allocation(Path::new("/nonexistent-metascope")),
            Err(Error::Io { .. })
        ));
    }

    const MOUNTINFO: &str = "\
25 1 0:28 /@ / rw,relatime shared:1 - btrfs /dev/nvme0n1p2 rw,ssd
40 25 0:30 /@home /home rw,relatime shared:2 - btrfs /dev/nvme0n1p2 rw,ssd
41 25 0:5 / /dev rw - devtmpfs dev rw
42 25 0:50 / /mnt/other rw - ext4 /dev/sdb1 rw
";

    #[test]
    fn mount_lookup_picks_longest_btrfs_prefix() {
        let src = btrfs_source_for_path(MOUNTINFO, Path::new("/home/me/x")).unwrap();
        assert_eq!(src, "/dev/nvme0n1p2");
    }

    #[test]
    fn mount_lookup_ignores_non_btrfs() {
        // /mnt/other is ext4, so the longest *btrfs* prefix is "/".
        let src = btrfs_source_for_path(MOUNTINFO, Path::new("/mnt/other/a")).unwrap();
        assert_eq!(src, "/dev/nvme0n1p2");
        assert!(btrfs_source_for_path(
            "42 25 0:50 / /mnt rw - ext4 /dev/sdb1 rw",
            Path::new("/mnt")
        )
        .is_none());
    }
}
