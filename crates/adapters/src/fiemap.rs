//! `FiemapProbe`: which extents of a file carry `FIEMAP_EXTENT_SHARED` (ADR-004 outcome 3, Task 1.2.2d).
//! The one `unsafe` file for this decision, and read-only by content: `scripts/check-token-sites.sh` holds it
//! to an allow list of names (one `ioctl`, request `FS_IOC_FIEMAP`, flags 0, no flush: a flush is a write).
//! Regular files only, opened `O_RDONLY | O_NOFOLLOW | O_NONBLOCK`; walking a directory is the caller's job.

use std::path::Path;

#[cfg(not(target_os = "linux"))]
use disk_cleanup_core::ports::Unavailable;
use disk_cleanup_core::ports::{ExtentAttribution, ExtentProbe};

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
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::fd::OwnedFd;
    use std::path::Path;

    use disk_cleanup_core::ports::{ExtentAttribution, ItemExtents, SharedExtent, Unavailable};
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
