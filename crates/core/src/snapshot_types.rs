//! Filesystem space at an instant, as `SpaceMeter` returns it. Produced only by the adapter; `core` never
//! reads sysfs.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// `bytes_used` and `total_bytes` of one btrfs chunk class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceUse {
    pub bytes_used: u64,
    pub total_bytes: u64,
}

impl SpaceUse {
    pub fn headroom_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.bytes_used)
    }
}

/// btrfs allocation (DC's own copy of what `disk_cleanup_scan::btrfs::read_allocation` returns).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllocSnapshot {
    pub data: SpaceUse,
    pub metadata: SpaceUse,
    pub system: SpaceUse,
    /// Raw bytes not yet carved into any chunk.
    pub unallocated: u64,
}

impl AllocSnapshot {
    pub fn metadata_headroom_bytes(&self) -> u64 {
        self.metadata.headroom_bytes()
    }
}

/// df numbers plus btrfs allocation when the path is on btrfs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsSnapshot {
    pub path: PathBuf,
    /// Unix seconds.
    pub taken_at: i64,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub avail_bytes: u64,
    /// Filesystem type as the mount table names it (`btrfs`, `tmpfs`, ...).
    pub fs_type: String,
    /// `None` when the filesystem is not btrfs.
    pub btrfs: Option<AllocSnapshot>,
}

/// Result of `SpaceMeter::settle`: `syncfs`, then `data.bytes_used` polled until two equal samples.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settled {
    /// False when the timeout ended the wait; the run's attribution is then `provisional`.
    pub stable: bool,
    /// The last `data.bytes_used` sample, `None` when the filesystem is not btrfs.
    pub data_bytes_used: Option<u64>,
    pub samples: u32,
    pub waited_ms: u64,
}

/// Whether a path exists inside any snapshot (`Found` wins over `Unreadable`, which wins over `Absent`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Held {
    Found,
    Absent,
    Unreadable,
}
