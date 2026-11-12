//! `FiemapProbe`: which extents of a file carry `FIEMAP_EXTENT_SHARED` (ADR-004 outcome 3, Task 1.2.2d).
//! The one `unsafe` file for this decision, and read-only by content: `scripts/check-token-sites.sh` holds it
//! to an allow list of names (one `ioctl`, request `FS_IOC_FIEMAP`, flags 0, no flush: a flush is a write).
//! Regular files only, opened `O_RDONLY | O_NOFOLLOW | O_NONBLOCK`; walking a directory is the caller's job.

use std::path::Path;
use std::time::Duration;

#[cfg(not(target_os = "linux"))]
use dunnage_core::ports::Unavailable;
use dunnage_core::ports::{ExtentAttribution, ExtentProbe, SharedEstimate};

#[derive(Debug, Default, Clone, Copy)]
pub struct FiemapProbe;

impl ExtentProbe for FiemapProbe {
    #[cfg(target_os = "linux")]
    fn attribute(&self, items: &[&Path], budget: usize) -> ExtentAttribution {
        linux::attribute(items, budget)
    }

    #[cfg(not(target_os = "linux"))]
    fn attribute(&self, items: &[&Path], budget: usize) -> ExtentAttribution {
        let _ = (items, budget);
        ExtentAttribution::Unavailable(Unavailable::UnsupportedOnThisOS)
    }

    #[cfg(target_os = "linux")]
    fn shared_bytes(&self, dir: &Path, top_n: usize, budget: Duration) -> SharedEstimate {
        linux::shared_bytes(dir, top_n, budget)
    }

    #[cfg(not(target_os = "linux"))]
    fn shared_bytes(&self, dir: &Path, top_n: usize, budget: Duration) -> SharedEstimate {
        let _ = (dir, top_n, budget);
        SharedEstimate::Unavailable(Unavailable::UnsupportedOnThisOS)
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::fd::OwnedFd;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use dunnage_core::ports::{
        ExtentAttribution, ItemExtents, SharedEstimate, SharedExtent, Unavailable,
    };
    use libc::{c_int, c_ulong, c_void};
    use rustix::fs::{lstat, open, statfs, FileType, Mode, OFlags};

    // The layout is read as native-endian 64-bit words below, which assumes the low half of a word comes
    // first in memory.
    const _: () = assert!(cfg!(target_endian = "little"));

    const FS_IOC_FIEMAP: c_ulong = 0xC020_660B;
    const BTRFS_MAGIC: u64 = 0x9123_683E;
    const ENOTTY: i32 = 25;
    const EOPNOTSUPP: i32 = 95;
    const EXTENT_LAST: u64 = 0x1;
    const EXTENT_SHARED: u64 = 0x2000;
    /// `struct fiemap` is 32 bytes and `struct fiemap_extent` 56: 4 and 7 words.
    const HEADER_WORDS: usize = 4;
    const EXTENT_WORDS: usize = 7;
    const BATCH: usize = 256;
    /// A file of 256 * 1024 extents is far past anything this tool deletes; stop rather than loop.
    const MAX_CALLS: usize = 1024;
    const LOW_HALF: u64 = 0xFFFF_FFFF;

    pub fn attribute(items: &[&Path], budget: usize) -> ExtentAttribution {
        let mut out = Vec::new();
        let mut devices: Vec<(u64, bool)> = Vec::new();
        let mut opened = 0_usize;
        for path in items {
            let Ok(st) = lstat(*path) else { continue };
            if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
                continue;
            }
            let dev = st.st_dev;
            let on_btrfs = match devices.iter().find(|(d, _)| *d == dev) {
                Some((_, b)) => *b,
                None => {
                    let b = is_btrfs(path);
                    devices.push((dev, b));
                    b
                }
            };
            if !on_btrfs {
                return ExtentAttribution::Unavailable(Unavailable::NotBtrfs);
            }
            opened += 1;
            if opened > budget {
                return ExtentAttribution::Unavailable(Unavailable::BudgetExhausted);
            }
            let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
            let Ok(fd) = open(*path, flags, Mode::empty()) else {
                continue;
            };
            match shared_extents(&fd) {
                Ok(shared) => out.push(ItemExtents {
                    path: path.to_path_buf(),
                    shared,
                }),
                Err(Some(why)) => return ExtentAttribution::Unavailable(why),
                Err(None) => {}
            }
        }
        ExtentAttribution::Measured(out)
    }

    /// `st_blocks` is always in 512-byte units.
    const BLOCK: u64 = 512;

    /// What one regular file contributed to an estimate.
    enum FileShared {
        Bytes(u64),
        /// Not a regular file, or gone since it was listed.
        Skipped,
        Unreadable,
        /// `ENOTTY` or `EOPNOTSUPP`: the filesystem cannot answer.
        NoIoctl,
    }

    fn file_shared(path: &Path) -> FileShared {
        let Ok(st) = lstat(path) else {
            return FileShared::Skipped;
        };
        if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
            return FileShared::Skipped;
        }
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let Ok(fd) = open(path, flags, Mode::empty()) else {
            return FileShared::Unreadable;
        };
        match shared_extents(&fd) {
            Ok(shared) => FileShared::Bytes(shared.iter().map(|e| e.length).sum()),
            Err(Some(_)) => FileShared::NoIoctl,
            Err(None) => FileShared::Skipped,
        }
    }

    /// The `top_n` largest regular files directly under `dir`, by allocated bytes, within `budget`.
    pub fn shared_bytes(dir: &Path, top_n: usize, budget: Duration) -> SharedEstimate {
        let deadline = Instant::now() + budget;
        // Before opening anything: another filesystem has no `SHARED` flag to read.
        if !is_btrfs(dir) {
            return SharedEstimate::Unavailable(Unavailable::NotBtrfs);
        }
        let mut heap: BinaryHeap<Reverse<(u64, PathBuf)>> = BinaryHeap::new();
        let mut dir_usage = 0_u64;
        let mut partial = false;
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if Instant::now() >= deadline {
                    partial = true;
                    break;
                }
                let Ok(md) = entry.metadata() else { continue };
                if !md.is_file() {
                    continue;
                }
                let size = md.blocks().saturating_mul(BLOCK);
                dir_usage = dir_usage.saturating_add(size);
                heap.push(Reverse((size, entry.path())));
                if heap.len() > top_n {
                    heap.pop();
                }
            }
        }
        let mut largest: Vec<(u64, PathBuf)> = heap.into_iter().map(|Reverse(x)| x).collect();
        largest.sort_unstable_by_key(|a| Reverse(a.0));
        let (mut bytes, mut files_probed, mut unreadable, mut probed_usage) =
            (0_u64, 0_u64, 0_u64, 0_u64);
        for (size, path) in largest {
            if Instant::now() >= deadline {
                partial = true;
                break;
            }
            match file_shared(&path) {
                FileShared::Bytes(b) => {
                    bytes += b;
                    files_probed += 1;
                    probed_usage += size;
                }
                FileShared::Skipped => {}
                FileShared::Unreadable => unreadable += 1,
                FileShared::NoIoctl => return SharedEstimate::Unavailable(Unavailable::NotBtrfs),
            }
        }
        SharedEstimate::Estimated {
            bytes,
            files_probed,
            unreadable,
            probed_usage,
            dir_usage,
            partial,
        }
    }

    fn is_btrfs(path: &Path) -> bool {
        statfs(path).is_ok_and(|s| (s.f_type as u64) & LOW_HALF == BTRFS_MAGIC)
    }

    /// `Err(Some(_))` means the filesystem cannot answer at all; `Err(None)` skips this one file.
    fn shared_extents(fd: &OwnedFd) -> Result<Vec<SharedExtent>, Option<Unavailable>> {
        let mut shared = Vec::new();
        let mut buf = vec![0_u64; HEADER_WORDS + EXTENT_WORDS * BATCH];
        let mut start = 0_u64;
        for _ in 0..MAX_CALLS {
            buf.fill(0);
            buf[0] = start;
            buf[1] = u64::MAX;
            buf[3] = BATCH as u64;
            // SAFETY: `buf` is 8-aligned, holds a 32-byte header followed by `BATCH` 56-byte extent slots,
            // which is what the request reads and fills; `fd` is an open descriptor.
            let rc = unsafe {
                libc::ioctl(
                    fd.as_raw_fd() as c_int,
                    FS_IOC_FIEMAP,
                    buf.as_mut_ptr().cast::<c_void>(),
                )
            };
            if rc != 0 {
                return Err(match io::Error::last_os_error().raw_os_error() {
                    Some(ENOTTY | EOPNOTSUPP) => Some(Unavailable::IoctlUnsupported),
                    _ => None,
                });
            }
            let mapped = (buf[2] >> 32) as usize;
            let mut last = mapped == 0;
            for i in 0..mapped.min(BATCH) {
                let at = HEADER_WORDS + EXTENT_WORDS * i;
                let (logical, physical, length) = (buf[at], buf[at + 1], buf[at + 2]);
                let flags = buf[at + 5] & LOW_HALF;
                if flags & EXTENT_SHARED != 0 {
                    shared.push(SharedExtent { physical, length });
                }
                last |= flags & EXTENT_LAST != 0;
                start = logical.saturating_add(length);
            }
            if last {
                return Ok(shared);
            }
        }
        Err(None)
    }
}
