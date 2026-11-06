//! Newtypes that keep device ids, inode numbers and byte counts from being mixed up (Story 0.1.3, C1/C2/C6).

use std::os::unix::fs::MetadataExt;

/// `(st_dev, st_ino)` of a filesystem object.
///
/// Comparing it with a bare integer does not compile; go through [`Identity::on_dev`]:
///
/// ```compile_fail
/// use disk_cleanup_core::Identity;
/// let id = Identity { dev: 28, ino: 5 };
/// assert!(28u64 == id);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Identity {
    pub dev: u64,
    pub ino: u64,
}

impl Identity {
    pub fn of(m: &std::fs::Metadata) -> Identity {
        Identity {
            dev: m.dev(),
            ino: m.ino(),
        }
    }

    pub fn on_dev(&self, d: RootDev) -> bool {
        self.dev == d.0
    }
}

/// `st_dev` of the scan root; every descent is compared against it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RootDev(pub u64);

/// Bytes actually allocated on disk (`st_blocks * 512`).
///
/// Allocated and apparent sizes cannot be added to each other:
///
/// ```compile_fail
/// use disk_cleanup_core::{AllocatedBytes, ApparentBytes};
/// let _ = AllocatedBytes(1) + ApparentBytes(2);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct AllocatedBytes(pub u64);

/// File length as `st_size` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct ApparentBytes(pub u64);
