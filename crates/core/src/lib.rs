//! Pure, testable eligibility logic for disk-cleanup — kept free of
//! filesystem and git side effects so staleness rules can be exercised
//! without touching a real machine. The CLI crate does all the I/O and
//! calls into this crate to decide what's safe to remove.

#![forbid(unsafe_code)]

pub mod apply_token;
pub mod candidate;
pub mod delete_types;
pub mod flavor;
pub mod floor_rule;
pub mod ids;
pub mod kind;
pub mod outcome;
pub mod reasons;
mod serde_remote;

pub use apply_token::ApplyToken;
pub use candidate::{
    BtrfsFacts, BuildDirFacts, BuildDirFlavor, CacheFacts, Candidate, DockerFacts, KindFacts,
    Locator, SteamFacts, WorktreeFacts, CANDIDATE_SCHEMA,
};
pub use delete_types::{
    first_line_capped, AbortFlag, AllowedTarget, DeleteError, DeleteFailure, DeletePolicy,
    DirEntryInfo, FileKind, Freed, NamePattern, StatInfo, CMD_MSG_MAX,
};
pub use flavor::build_flavor;
pub use ids::{AllocatedBytes, AppId, ApparentBytes, CandidateId, DockerId, Identity, RootDev};
pub use kind::{selector_of, Kind, Risk, SelectorId};
pub use outcome::{Outcome, SkipReason, Status};
pub use reasons::{errno_phrase, FatalError, Warning};
use std::path::PathBuf;

/// A Claude Code agent worktree (`.claude/worktrees/agent-*`) considered for
/// removal, with everything the staleness rule needs already gathered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeCandidate {
    pub path: PathBuf,
    /// Unix seconds of the worktree's last commit, or `None` if it has none.
    pub last_commit_epoch: Option<i64>,
    pub locked: bool,
    pub dirty: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Stale,
    SkipLocked,
    SkipDirty,
    SkipTooRecent,
}

/// Decide whether a worktree is safe to remove.
///
/// Order matters: locked beats dirty beats age — a lock is the strongest
/// "don't touch" signal a caller can give, and uncommitted changes must
/// never be discarded just because the last *commit* is old.
#[must_use]
pub fn worktree_verdict(candidate: &WorktreeCandidate, now_epoch: i64, stale_days: u32) -> Verdict {
    if candidate.locked {
        return Verdict::SkipLocked;
    }
    if candidate.dirty {
        return Verdict::SkipDirty;
    }
    let age_days = candidate
        .last_commit_epoch
        .map_or(0, |commit| (now_epoch - commit).max(0) / 86_400);
    if age_days >= i64::from(stale_days) {
        Verdict::Stale
    } else {
        Verdict::SkipTooRecent
    }
}

/// A cache directory disk-cleanup knows how to reclaim, expressed relative
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

    const NOW: i64 = 1_000_000;

    fn candidate(days_old: i64, locked: bool, dirty: bool) -> WorktreeCandidate {
        WorktreeCandidate {
            path: PathBuf::from("/tmp/wt"),
            last_commit_epoch: Some(NOW - days_old * 86_400),
            locked,
            dirty,
        }
    }

    #[test]
    fn stale_when_old_clean_unlocked() {
        assert_eq!(
            worktree_verdict(&candidate(30, false, false), NOW, 14),
            Verdict::Stale
        );
    }

    #[test]
    fn skip_when_too_recent() {
        assert_eq!(
            worktree_verdict(&candidate(1, false, false), NOW, 14),
            Verdict::SkipTooRecent
        );
    }

    #[test]
    fn boundary_exactly_stale_days_is_stale() {
        assert_eq!(
            worktree_verdict(&candidate(14, false, false), NOW, 14),
            Verdict::Stale
        );
    }

    #[test]
    fn locked_wins_even_when_old_and_clean() {
        assert_eq!(
            worktree_verdict(&candidate(30, true, false), NOW, 14),
            Verdict::SkipLocked
        );
    }

    #[test]
    fn dirty_wins_over_age_but_not_over_lock() {
        assert_eq!(
            worktree_verdict(&candidate(30, false, true), NOW, 14),
            Verdict::SkipDirty
        );
        assert_eq!(
            worktree_verdict(&candidate(30, true, true), NOW, 14),
            Verdict::SkipLocked
        );
    }

    #[test]
    fn no_commits_is_treated_as_brand_new_not_stale() {
        let c = WorktreeCandidate {
            path: PathBuf::from("/tmp/wt"),
            last_commit_epoch: None,
            locked: false,
            dirty: false,
        };
        assert_eq!(worktree_verdict(&c, NOW, 14), Verdict::SkipTooRecent);
    }

    #[test]
    fn cache_target_lists_are_nonempty_and_relative() {
        for target in LINUX_CACHE_TARGETS.iter().chain(MACOS_CACHE_TARGETS) {
            assert!(!target.home_relative.starts_with('/'));
            assert!(!target.label.is_empty());
        }
    }
}
