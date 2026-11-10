//! Starvation preflight rule (Story 1.2.1): refuse `--apply` when btrfs has neither unallocated space nor
//! metadata headroom, because a delete needs metadata room and could then fail with ENOSPC.

use crate::kind::Kind;
use crate::reasons::FatalError;
use crate::snapshot_types::FsSnapshot;

const MIB: u64 = 1 << 20;
/// Thresholds of `buildclean.py`: starved means below 1 GiB unallocated AND below 64 MiB metadata headroom.
pub const MIN_UNALLOCATED_BYTES: u64 = 1 << 30;
pub const MIN_METADATA_HEADROOM_BYTES: u64 = 64 * MIB;

/// The measured figures a refusal or a warning prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Starvation {
    pub unallocated_mib: u64,
    pub metadata_headroom_mib: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreflightVerdict {
    Proceed,
    /// Starved, but only the btrfs cures were requested: the run goes on, limited to the `-dusage=0` step
    /// and the smallest `Safe` items.
    Warn(Starvation),
    Refuse(Starvation),
}

impl PreflightVerdict {
    /// The run-level error for a refusal. `FatalError::Starved` has no fields until Task 1.1.1a is
    /// reworked, so the measured figures travel in the verdict and the caller prints them.
    pub fn fatal(&self) -> Option<FatalError> {
        matches!(self, PreflightVerdict::Refuse(_)).then_some(FatalError::Starved)
    }
}

/// Pure rule over one snapshot and the kinds the run selects. A non-btrfs filesystem always proceeds.
pub fn check(snapshot: &FsSnapshot, kinds: &[Kind]) -> PreflightVerdict {
    let Some(alloc) = snapshot.btrfs else {
        return PreflightVerdict::Proceed;
    };
    let headroom = alloc.metadata_headroom_bytes();
    if alloc.unallocated >= MIN_UNALLOCATED_BYTES || headroom >= MIN_METADATA_HEADROOM_BYTES {
        return PreflightVerdict::Proceed;
    }
    let figures = Starvation {
        unallocated_mib: alloc.unallocated / MIB,
        metadata_headroom_mib: headroom / MIB,
    };
    let only_cures = !kinds.is_empty()
        && kinds
            .iter()
            .all(|k| matches!(k, Kind::BtrfsBalance | Kind::BtrfsTrim));
    if only_cures {
        PreflightVerdict::Warn(figures)
    } else {
        PreflightVerdict::Refuse(figures)
    }
}
