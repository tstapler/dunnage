//! `Outcome`, `Status` and `SkipReason`. Reason texts live in [`crate::reasons`].

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::delete_types::Freed;
use crate::ids::CandidateId;
use crate::kind::{Kind, SelectorId};
use crate::serde_remote::FreedDef;

/// Why an item was not removed. 38 variants; the text and next action of each is a row of `design/ux.md` 3.5
/// (see [`SkipReason::reason_text`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkipReason {
    Dirty,
    Locked,
    TooRecent,
    RunningBuild,
    RunStateUnknown,
    OutsideRoot,
    Symlink,
    CrossDevice,
    MountPoint,
    ProtectedPath,
    VolumeReferenced,
    DaemonUnavailable,
    UnpushedCommits,
    DetachedHead,
    LibraryUnmounted,
    SteamRunning,
    NeedsRoot,
    Gone,
    IdentityChanged,
    ImageReferenced,
    ToolMissing,
    AlreadyRunning,
    RootRestricted,
    Aborted,
    /// A duplicate-set member kept on purpose.
    KeptAsNewest,
    /// A guarded or destructive kind that `--kinds` did not name; shown as `RISK`.
    KindNotSelected,
    NonLocalDockerContext,
    UnsupportedOnThisOS,
    UnrecognisedOutput,
    DockerVersionUntested,
    LooksLikeDatabase,
    ContentsUnknown,
    MayHoldSaves,
    OptInRequired,
    InUse,
    HasIgnoredFiles,
    NotReproducible,
    LocalOnlyImage,
}

impl SkipReason {
    pub const ALL: [SkipReason; 38] = [
        SkipReason::Dirty,
        SkipReason::Locked,
        SkipReason::TooRecent,
        SkipReason::RunningBuild,
        SkipReason::RunStateUnknown,
        SkipReason::OutsideRoot,
        SkipReason::Symlink,
        SkipReason::CrossDevice,
        SkipReason::MountPoint,
        SkipReason::ProtectedPath,
        SkipReason::VolumeReferenced,
        SkipReason::DaemonUnavailable,
        SkipReason::UnpushedCommits,
        SkipReason::DetachedHead,
        SkipReason::LibraryUnmounted,
        SkipReason::SteamRunning,
        SkipReason::NeedsRoot,
        SkipReason::Gone,
        SkipReason::IdentityChanged,
        SkipReason::ImageReferenced,
        SkipReason::ToolMissing,
        SkipReason::AlreadyRunning,
        SkipReason::RootRestricted,
        SkipReason::Aborted,
        SkipReason::KeptAsNewest,
        SkipReason::KindNotSelected,
        SkipReason::NonLocalDockerContext,
        SkipReason::UnsupportedOnThisOS,
        SkipReason::UnrecognisedOutput,
        SkipReason::DockerVersionUntested,
        SkipReason::LooksLikeDatabase,
        SkipReason::ContentsUnknown,
        SkipReason::MayHoldSaves,
        SkipReason::OptInRequired,
        SkipReason::InUse,
        SkipReason::HasIgnoredFiles,
        SkipReason::NotReproducible,
        SkipReason::LocalOnlyImage,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Removed,
    Skipped(SkipReason),
    Error(String),
    /// The command was started and this tool stopped waiting for it while it still ran (a `btrfs balance`
    /// after SIGINT, Story 4.3.2). Not removed, not an error: the kernel finishes it.
    Running,
}

/// One row per item; equals one audit `result` line. The byte fields are named `est_bytes` and
/// `measured_bytes` everywhere (audit line, `--json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    pub ts: i64,
    pub run_id: String,
    pub kind: Kind,
    pub selector: SelectorId,
    pub id: CandidateId,
    /// `None` for items without a filesystem path (docker objects, btrfs steps).
    pub path: Option<PathBuf>,
    pub status: Status,
    /// Size at discovery.
    pub est_bytes: u64,
    /// Allocated bytes counted at delete time; `None` in plan mode and when nothing was measured.
    pub measured_bytes: Option<u64>,
    /// Partial when `status` is `Error`.
    #[serde(with = "FreedDef")]
    pub freed: Freed,
    pub elapsed_ms: u64,
}
