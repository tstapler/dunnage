//! Records of the audit JSONL log (`$XDG_STATE_HOME/disk-cleanup/audit.jsonl`), schema 1. The writer is
//! Story 2.2.1; this is only the data.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::candidate::{Candidate, Locator};
use crate::ids::CandidateId;
use crate::kind::{Kind, SelectorId};
use crate::outcome::Outcome;
use crate::plan_state::{BuildInfo, StampSummary};
use crate::summary::{Cause, DiskVerdict};

pub const AUDIT_SCHEMA: u32 = 1;

/// One line of the log. Records are short-lived, so the size gap between variants does not matter.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuditRecord {
    RunStart(RunStart),
    Intent(Intent),
    Result(ItemResult),
    RunEnd(RunEnd),
}

/// The scan roots, cache table paths, worktree roots and Steam libraries in force; `audit verify` checks
/// every logged deletion against them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRoots {
    pub scan_roots: Vec<PathBuf>,
    pub cache_paths: Vec<PathBuf>,
    pub worktree_roots: Vec<PathBuf>,
    pub steam_libraries: Vec<PathBuf>,
}

/// `DISK_CLEANUP_UNGATED` used, and whether a person confirmed it on the terminal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BreakGlass {
    pub reason: String,
    pub tty_confirmed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunStart {
    pub schema: u32,
    pub run_id: String,
    pub ts: i64,
    pub euid: u32,
    pub kinds: String,
    pub plan_id: Option<String>,
    pub build: BuildInfo,
    pub stamp: Option<StampSummary>,
    pub break_glass: Option<BreakGlass>,
    pub roots: RunRoots,
    /// Verdict of the snapshot taken at the start of the run (the qualifying-run rule of `outcome-report.sh`).
    #[serde(default)]
    pub verdict_start: Option<DiskVerdict>,
}

/// Written before the delete it announces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intent {
    pub run_id: String,
    pub ts: i64,
    pub id: CandidateId,
    pub kind: Kind,
    pub selector: SelectorId,
    pub locator: Locator,
    pub est_bytes: u64,
}

impl Intent {
    pub fn of(c: &Candidate, run_id: &str, ts: i64) -> Intent {
        Intent {
            run_id: run_id.to_string(),
            ts,
            id: c.id(),
            kind: c.kind(),
            selector: c.selector(),
            locator: c.locator().clone(),
            est_bytes: c.est_bytes().0,
        }
    }
}

/// An [`Outcome`] as one audit line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ItemResult(pub Outcome);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEnd {
    pub run_id: String,
    pub ts: i64,
    pub verdict_after: DiskVerdict,
    pub attributed_bytes: u64,
    pub measured_drop: Option<i64>,
    /// `None` when there was no positive drop to compare with.
    #[serde(default)]
    pub attribution_pct: Option<f64>,
    pub causes: Vec<Cause>,
    /// Measured causes account for the shortfall; `scripts/outcome-report.sh` reads this, never the prose.
    #[serde(default)]
    pub explained: bool,
}
