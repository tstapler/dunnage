//! Pure, testable eligibility logic for dunnage — kept free of
//! filesystem and git side effects so staleness rules can be exercised
//! without touching a real machine. The CLI crate does all the I/O and
//! calls into this crate to decide what's safe to remove.

#![forbid(unsafe_code)]

pub mod allowed;
pub mod apply_token;
pub mod audit_types;
pub mod cache_targets;
pub mod candidate;
pub mod delete_types;
pub mod dirs;
pub mod docker;
pub mod escape;
pub mod executor;
pub mod explore;
pub mod explorer_plan;
pub mod flavor;
pub mod floor_rule;
pub mod guard;
pub mod ids;
pub mod kind;
pub mod outcome;
pub mod par;
pub mod plan;
pub mod plan_state;
pub mod platform;
pub mod ports;
pub mod preflight;
pub mod reasons;
pub mod roots;
pub mod selector;
mod serde_remote;
pub mod shell_quote;
pub mod snapshot_types;
pub mod summary;
pub mod target;
pub mod targets;
#[cfg(feature = "test-support")]
pub mod testing;

pub use allowed::{Allowed, ExplorerScope, ExplorerScopeWrongKind, WorktreeRemoval, WrongKind};
pub use apply_token::ApplyToken;
pub use audit_types::{
    AuditRecord, ConfirmedItem, ExplorerConfirm, Intent, ItemResult, RunEnd, RunRoots, RunStart,
    AUDIT_SCHEMA,
};
pub use cache_targets::{
    cache_table, cache_table_with_availability, CacheRow, CacheSource, OsSet, RowAvailability,
    CACHE_ROWS,
};
#[cfg(feature = "test-support")]
pub use candidate::FixtureFacts;
pub use candidate::{
    BtrfsFacts, BuildDirFacts, BuildDirFlavor, CacheFacts, Candidate, CargoHomeArea,
    CargoHomeFacts, CargoPart, CargoPartFacts, DockerFacts, ExplorerFacts, KindFacts, Locator,
    SteamFacts, WorktreeFacts, CANDIDATE_SCHEMA,
};
pub use delete_types::{
    first_line_capped, AbortFlag, AllowedTarget, DeleteError, DeleteFailure, DeletePolicy,
    DirEntryInfo, FileKind, Freed, NamePattern, StatInfo, CMD_MSG_MAX,
};
pub use dirs::Dirs;
pub use flavor::{build_flavor, explore_delete_delay_ms, explore_panic_at, explore_plan_delay_ms};
pub use ids::{
    AllocatedBytes, AppId, ApparentBytes, BirthTime, CandidateId, DockerId, Identity, RootDev,
};
pub use kind::{selector_of, Kind, Risk, SelectorId};
pub use outcome::{Outcome, SkipReason, Status};
pub use plan_state::{BuildInfo, ItemKey, SaveOutcome, SavedPlan, SavedSelection, ITEM_KEY_MAX};
pub use platform::{MountEntry, Platform, Tri};
pub use preflight::{PreflightVerdict, Starvation};
pub use reasons::{errno_phrase, FatalError, Warning};
pub use snapshot_types::{AllocSnapshot, FsSnapshot, Held, Settled, SpaceUse};
pub use summary::{
    Cause, CauseKind, DiskVerdict, FinishInput, HeldItem, PreDeleteSample, RunSummary,
    SnapshotFacts, StageTimings, WalkerCounters,
};

pub use targets::worktree_guard::{
    decide_worktree, worktree_verdict, IgnoredFiles, PushState, Verdict, WorktreeCandidate,
    WorktreeGuard, WorktreeOpts, WORKTREE_MIN_AGE,
};

/// A cache directory dunnage knows how to reclaim, expressed relative
/// to `$HOME` so the (pure) list can be unit tested without touching disk.
#[derive(Debug, Clone, Copy)]
pub struct CacheTarget {
    pub label: &'static str,
    pub home_relative: &'static str,
}

pub const LINUX_CACHE_TARGETS: &[CacheTarget] = &[
    CacheTarget {
        label: "npm _cacache",
        home_relative: ".npm/_cacache",
    },
    CacheTarget {
        label: "Gradle caches",
        home_relative: ".gradle/caches",
    },
    CacheTarget {
        label: "Cargo registry cache",
        home_relative: ".cargo/registry/cache",
    },
    CacheTarget {
        label: "Cargo registry src",
        home_relative: ".cargo/registry/src",
    },
];

pub const MACOS_CACHE_TARGETS: &[CacheTarget] = &[
    CacheTarget {
        label: "npm _cacache",
        home_relative: ".npm/_cacache",
    },
    CacheTarget {
        label: "JetBrains caches",
        home_relative: "Library/Caches/JetBrains",
    },
    CacheTarget {
        label: "Xcode DerivedData cache",
        home_relative: "Library/Caches/com.apple.dt.Xcode",
    },
    CacheTarget {
        label: "Xcode DerivedData",
        home_relative: "Library/Developer/Xcode/DerivedData",
    },
    CacheTarget {
        label: "Playwright browsers",
        home_relative: "Library/Caches/ms-playwright",
    },
    CacheTarget {
        label: "Gradle caches",
        home_relative: ".gradle/caches",
    },
    CacheTarget {
        label: "Cargo registry cache",
        home_relative: ".cargo/registry/cache",
    },
    CacheTarget {
        label: "Cargo registry src",
        home_relative: ".cargo/registry/src",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_target_lists_are_nonempty_and_relative() {
        for target in LINUX_CACHE_TARGETS.iter().chain(MACOS_CACHE_TARGETS) {
            assert!(!target.home_relative.starts_with('/'));
            assert!(!target.label.is_empty());
        }
    }
}
