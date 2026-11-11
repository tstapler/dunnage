//! The gate-stamp check: an unconfined `--apply` runs only a binary that a green `make gate` stamped
//! (`.git/gate-reports/stamp-<gate>.json`, installed by `make install-gated`). Pure; the file, the hash and the
//! terminal are in `adapters/gate_stamp.rs` (Story 2.2.1, Task 2.2.1d).

use serde::{Deserialize, Serialize};

use crate::audit_types::BreakGlass;
use crate::kind::Kind;
use crate::plan_state::{BuildInfo, StampSummary};

pub const STAMP_SCHEMA: u32 = 1;

/// What `scripts/gate.sh` writes for a green gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateStamp {
    pub schema: u32,
    pub gate: String,
    pub commit: String,
    pub binary_sha256: String,
}

/// Why the stamp does not admit this binary. The text is the `StampRefusal` row of `design/ux.md` 3.5.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StampRefusal {
    Absent,
    UnknownSchema { found: u32 },
    DirtyBuild,
    WrongCommit { stamp: String, build: String },
    WrongBinary,
    GateTooLow { have: String, need: char },
}

impl StampRefusal {
    pub fn why(&self) -> String {
        match self {
            StampRefusal::Absent => "no stamp was installed".into(),
            StampRefusal::UnknownSchema { found } => format!("the stamp has schema {found}"),
            StampRefusal::DirtyBuild => "this build has uncommitted changes".into(),
            StampRefusal::WrongCommit { stamp, build } => format!(
                "the stamp is for commit {} but this binary is {}",
                short(stamp),
                short(build)
            ),
            StampRefusal::WrongBinary => {
                "the stamp holds the hash of a different binary than this one".into()
            }
            StampRefusal::GateTooLow { have, need } => {
                format!("the stamp is for gate {have} but these kinds need gate {need} or later")
            }
        }
    }

    pub fn text(&self) -> String {
        format!(
            "error: this binary has no valid gate stamp ({}). If the disk has room for a build, run 'make gate GATE={}' then 'make install-gated' in the disk-cleanup repo. If it does not (a build needs free space, which a full disk lacks), start the command from your own terminal with DISK_CLEANUP_UNGATED=<reason> and type BYPASS when asked; the bypass is recorded in the audit log. Nothing was deleted.",
            self.why(),
            match self {
                StampRefusal::GateTooLow { need, .. } => *need,
                _ => 'B',
            }
        )
    }

    pub fn exit_code(&self) -> i32 {
        64
    }

    /// The `code` of the `--json` error document.
    pub fn json_code(&self) -> &'static str {
        "stamp_refused"
    }
}

impl std::fmt::Display for StampRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text())
    }
}

impl std::error::Error for StampRefusal {}

fn short(s: &str) -> &str {
    s.get(..8).unwrap_or(s)
}

/// The earliest gate whose registry holds every selected kind: caches B, worktrees and build dirs C,
/// docker, steam and btrfs D. No kinds means the first live gate, B.
pub fn required_gate(kinds: &[Kind]) -> char {
    kinds
        .iter()
        .map(|k| match k {
            Kind::Cache => 'B',
            Kind::Worktree | Kind::NodeModules | Kind::CargoTarget => 'C',
            Kind::DockerImage
            | Kind::DockerBuildCache
            | Kind::DockerVolume
            | Kind::SteamPrefix
            | Kind::SteamShaderCache
            | Kind::SteamRemnant
            | Kind::BtrfsBalance
            | Kind::BtrfsTrim => 'D',
        })
        .max()
        .unwrap_or('B')
}

/// Accepts the stamp only for exactly this binary: a clean build, the commit baked into it, the sha256 of
/// the running executable and a gate at or above `required_gate`.
pub fn check_stamp(
    build: &BuildInfo,
    stamp: Option<&GateStamp>,
    exe_sha256: &str,
    required_gate: char,
) -> Result<StampSummary, StampRefusal> {
    let stamp = stamp.ok_or(StampRefusal::Absent)?;
    if stamp.schema != STAMP_SCHEMA {
        return Err(StampRefusal::UnknownSchema {
            found: stamp.schema,
        });
    }
    if build.dirty {
        return Err(StampRefusal::DirtyBuild);
    }
    if stamp.commit != build.git_commit {
        return Err(StampRefusal::WrongCommit {
            stamp: stamp.commit.clone(),
            build: build.git_commit.clone(),
        });
    }
    if !stamp.binary_sha256.eq_ignore_ascii_case(exe_sha256) {
        return Err(StampRefusal::WrongBinary);
    }
    let have = stamp.gate.chars().next().filter(|_| stamp.gate.len() == 1);
    match have {
        Some(g) if g >= required_gate => Ok(StampSummary {
            gate: stamp.gate.clone(),
            commit: stamp.commit.clone(),
            binary_sha256: stamp.binary_sha256.clone(),
            verified: true,
        }),
        _ => Err(StampRefusal::GateTooLow {
            have: stamp.gate.clone(),
            need: required_gate,
        }),
    }
}

/// `DISK_CLEANUP_UNGATED` as the break-glass reason; an empty (or all-blank) value is as if unset.
pub fn break_glass_reason(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_string)
}

/// The line directly under L1 of an apply that bypassed the check.
pub fn break_glass_warning(bg: &BreakGlass) -> String {
    format!("WARNING: gate check bypassed ({})", bg.reason)
}

/// Stderr text of the break-glass without a controlling terminal (agent session, cron, `ssh` without `-t`).
pub const BREAK_GLASS_NO_TTY: &str = "error: DISK_CLEANUP_UNGATED needs a person at a terminal to confirm, and this run has none. Nothing was deleted. Run 'make gate' and 'make install-gated', or start the command from your own terminal.";

/// `--json` error code of [`BREAK_GLASS_NO_TTY`].
pub const BREAK_GLASS_NO_TTY_CODE: &str = "break_glass_no_tty";

/// The word the owner types on the terminal.
pub const BREAK_GLASS_WORD: &str = "BYPASS";
