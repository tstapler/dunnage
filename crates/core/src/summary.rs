//! `RunSummary` as data. `RunSummary::finish` and the attribution math arrive with Task 1.2.2a.

use serde::{Deserialize, Serialize};

use crate::outcome::Outcome;
use crate::snapshot_types::FsSnapshot;

/// How the disk stands after a run (the `status` verdict).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiskVerdict {
    Ok,
    Warn,
    Crit,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CauseKind {
    SnapshotHeld,
    ConcurrentWriteDelta,
    DockerDaemonDelta,
}

/// A measured explanation for part of the shortfall `attributed - drop`; unmeasured is 0 with source
/// `unmeasured`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cause {
    pub kind: CauseKind,
    pub bytes: i64,
    pub source: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StageTimings {
    pub discover_ms: u64,
    pub plan_ms: u64,
    pub execute_ms: u64,
    pub settle_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WalkerCounters {
    pub dirs_scanned: u64,
    pub cache_hits: u64,
}

/// The `--json` document body for an apply run. `Executor::run` returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSummary {
    pub fs_before: FsSnapshot,
    /// `None` in plan mode.
    pub fs_after: Option<FsSnapshot>,
    pub outcomes: Vec<Outcome>,
    /// Sum of `freed.attributed_bytes` over every outcome, failed ones included.
    pub attributed_bytes: u64,
    pub measured_drop: Option<i64>,
    /// `measured_drop - attributed_bytes`, signed.
    pub unattributed: Option<i64>,
    /// The settle did not stabilise.
    pub provisional: bool,
    /// The two `data.bytes_used` samples taken before the first delete.
    pub pre_delete_data_bytes_used: Vec<u64>,
    pub causes: Vec<Cause>,
    pub verdict_after: DiskVerdict,
    /// The three largest `KindNotSelected` outcomes.
    pub top_unselected: Vec<Outcome>,
    pub timings: StageTimings,
    pub walker: WalkerCounters,
    pub notes: Vec<String>,
}
