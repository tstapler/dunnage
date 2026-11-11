//! Worktree guards (Story 3.1.2): the pure decision made at plan time and again just before
//! `git worktree remove`, over facts the probes in `adapters` gathered.
//!
//! Order: gone, common path checks, identity, locked, dirty, in use, age, the 24 h floor, pushed, ignored
//! files. Every probe that could not answer fails closed (a skip, never an `Allow`). The one `Allowed` it
//! returns is the witness `WorktreeRemoval::new` needs.

use std::path::PathBuf;
use std::time::Duration;

use crate::allowed::Allowed;
use crate::guard::{decide_path_common, Facts, GuardDecision};
use crate::ids::Identity;
use crate::outcome::SkipReason;
use crate::platform::Tri;
use crate::roots::RootSet;

/// A worktree is never removed below this age, whatever `--min-age` says. The age is the youngest of its
/// last commit, the newest mtime under it and its admin entry, so a fresh checkout of an old commit is young.
pub const WORKTREE_MIN_AGE: Duration = Duration::from_hours(24);

/// Plan header line when the flag was below the floor.
pub const FLOOR_RAISED_NOTE: &str = "worktrees: --min-age raised to the 24 h floor";

/// The `TooRecent` detail when the floor, not the flag, held the worktree back.
pub const FLOOR_DETAIL: &str = "younger than 24 h (worktree floor; --min-age cannot lower it)";

/// Detail when the age of the tree could not be established; the guard fails closed.
pub const AGE_UNKNOWN_DETAIL: &str = "the age of the worktree could not be established";

/// Detail when the ignored-files probe failed.
pub const IGNORED_UNKNOWN_DETAIL: &str = "the ignored files could not be listed";

/// Whether the plan header must say the flag was raised to the floor.
#[must_use]
pub fn floor_raised(flag: Duration) -> bool {
    flag < WORKTREE_MIN_AGE
}

/// Whether the last commit is on a remote branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushState {
    Pushed,
    /// `commits` is the count when the probe could establish it.
    Unpushed {
        commits: Option<u32>,
    },
    /// The probe failed; treated like unpushed.
    Unknown,
}

/// Ignored entries `git worktree remove` would delete with the tree, build-output directories excluded
/// (`target/` and `node_modules/` that `classify_path` recognises: they are what the tool exists to remove).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IgnoredFiles {
    /// Files (a directory counts every entry below it) that are not build output.
    pub entries: u64,
    /// `treestat` allocated size of those.
    pub bytes: u64,
    /// Size of the build-output directories, shown in the plan row, never blocking.
    pub build_output_bytes: u64,
}

/// A Claude Code agent worktree (`.claude/worktrees/agent-*`) considered for removal, with everything the
/// rule needs already gathered. [`WorktreeCandidate::new`] starts with every probe unanswered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeCandidate {
    pub path: PathBuf,
    /// Unix seconds of the worktree's last commit, or `None` if it has none.
    pub last_commit_epoch: Option<i64>,
    pub locked: bool,
    pub dirty: bool,
    pub push: PushState,
    /// HEAD is detached (`git worktree list`).
    pub detached: bool,
    /// Newest mtime under the tree (Unix seconds).
    pub newest_mtime: Option<i64>,
    /// mtime of `<git-common-dir>/worktrees/<name>`.
    pub admin_mtime: Option<i64>,
    /// `Running`: a process has its cwd or an open file under the worktree.
    pub in_use: Tri,
    /// Name and pid of the first holder, for the `InUse` text.
    pub holder: Option<(String, u32)>,
    /// `None`: the ignored-files probe failed.
    pub ignored: Option<IgnoredFiles>,
}

impl WorktreeCandidate {
    /// The four facts the first rule needed; every other probe reads as "could not tell".
    #[must_use]
    pub fn new(
        path: impl Into<PathBuf>,
        last_commit_epoch: Option<i64>,
        locked: bool,
        dirty: bool,
    ) -> WorktreeCandidate {
        WorktreeCandidate {
            path: path.into(),
            last_commit_epoch,
            locked,
            dirty,
            push: PushState::Unknown,
            detached: false,
            newest_mtime: None,
            admin_mtime: None,
            in_use: Tri::Unknown,
            holder: None,
            ignored: None,
        }
    }

    /// Number for the `UnpushedCommits` text, when known.
    #[must_use]
    pub fn unpushed(&self) -> Option<u32> {
        match self.push {
            PushState::Unpushed { commits } => commits,
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Stale,
    SkipLocked,
    SkipDirty,
    SkipTooRecent,
}

fn age_verdict(candidate: &WorktreeCandidate, now_epoch: i64, min_age: Duration) -> Verdict {
    if candidate.locked {
        return Verdict::SkipLocked;
    }
    if candidate.dirty {
        return Verdict::SkipDirty;
    }
    let need = i64::try_from(min_age.as_secs()).unwrap_or(i64::MAX);
    // no commits is brand new
    match candidate.last_commit_epoch {
        Some(commit) if (now_epoch - commit).max(0) >= need => Verdict::Stale,
        _ => Verdict::SkipTooRecent,
    }
}

/// Decide whether a worktree is safe to remove by lock, uncommitted changes and last-commit age.
///
/// Order matters: locked beats dirty beats age — a lock is the strongest
/// "don't touch" signal a caller can give, and uncommitted changes must
/// never be discarded just because the last *commit* is old.
#[must_use]
pub fn worktree_verdict(candidate: &WorktreeCandidate, now_epoch: i64, stale_days: u32) -> Verdict {
    age_verdict(
        candidate,
        now_epoch,
        Duration::from_secs(u64::from(stale_days) * 86_400),
    )
}

/// Options of one guard run.
#[derive(Debug, Clone, Copy)]
pub struct WorktreeOpts {
    /// `--min-age` as given (default 14 d); the 24 h floor is applied here.
    pub min_age: Duration,
    /// `--worktree-ignored discard`: ignored files are not a reason to skip.
    pub discard_ignored: bool,
}

#[derive(Debug)]
pub enum WorktreeGuard {
    Allow(Allowed),
    Skip {
        reason: SkipReason,
        /// Replaces the catalogue text where it would mislead (the floor, an unknown age).
        detail: Option<String>,
    },
}

impl WorktreeGuard {
    fn skip(reason: SkipReason) -> WorktreeGuard {
        WorktreeGuard::Skip {
            reason,
            detail: None,
        }
    }

    fn skip_with(reason: SkipReason, detail: &str) -> WorktreeGuard {
        WorktreeGuard::Skip {
            reason,
            detail: Some(detail.to_string()),
        }
    }

    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        match self {
            WorktreeGuard::Allow(_) => None,
            WorktreeGuard::Skip { detail, .. } => detail.as_deref(),
        }
    }

    #[must_use]
    pub fn skip_reason(&self) -> Option<SkipReason> {
        match self {
            WorktreeGuard::Allow(_) => None,
            WorktreeGuard::Skip { reason, .. } => Some(*reason),
        }
    }
}

/// `facts` is `None` when the path no longer exists (a worktree on removable media, a directory deleted by
/// hand): `Skip(Gone)`, never pruned. `planned` is the identity taken at discovery.
#[must_use]
pub fn decide_worktree(
    c: &WorktreeCandidate,
    facts: Option<&Facts>,
    planned: Identity,
    roots: &RootSet,
    now: i64,
    opts: WorktreeOpts,
) -> WorktreeGuard {
    let Some(facts) = facts else {
        return WorktreeGuard::skip(SkipReason::Gone);
    };
    let allowed = match decide_path_common(facts, roots) {
        GuardDecision::Allow(a) => a,
        GuardDecision::Skip(r) => return WorktreeGuard::skip(r),
    };
    if facts.ident != planned {
        return WorktreeGuard::skip(SkipReason::IdentityChanged);
    }
    if c.locked {
        return WorktreeGuard::skip(SkipReason::Locked);
    }
    if c.dirty {
        return WorktreeGuard::skip(SkipReason::Dirty);
    }
    match c.in_use {
        Tri::Running => return WorktreeGuard::skip(SkipReason::InUse),
        Tri::Unknown => return WorktreeGuard::skip(SkipReason::RunStateUnknown),
        Tri::NotRunning => {}
    }
    if age_verdict(c, now, opts.min_age) == Verdict::SkipTooRecent {
        return WorktreeGuard::skip(SkipReason::TooRecent);
    }
    if let Some(guard) = floor_guard(c, now) {
        return guard;
    }
    if c.push != PushState::Pushed {
        return WorktreeGuard::skip(if c.detached {
            SkipReason::DetachedHead
        } else {
            SkipReason::UnpushedCommits
        });
    }
    if !opts.discard_ignored {
        match c.ignored {
            Some(i) if i.entries == 0 => {}
            Some(_) => return WorktreeGuard::skip(SkipReason::HasIgnoredFiles),
            None => {
                return WorktreeGuard::skip_with(
                    SkipReason::HasIgnoredFiles,
                    IGNORED_UNKNOWN_DETAIL,
                )
            }
        }
    }
    WorktreeGuard::Allow(allowed)
}

/// The 24 h floor over the youngest of last commit, newest mtime and admin entry.
fn floor_guard(c: &WorktreeCandidate, now: i64) -> Option<WorktreeGuard> {
    let (Some(commit), Some(newest), Some(admin)) =
        (c.last_commit_epoch, c.newest_mtime, c.admin_mtime)
    else {
        return Some(WorktreeGuard::skip_with(
            SkipReason::TooRecent,
            AGE_UNKNOWN_DETAIL,
        ));
    };
    let youngest = commit.max(newest).max(admin);
    let floor = i64::try_from(WORKTREE_MIN_AGE.as_secs()).unwrap_or(i64::MAX);
    (now.saturating_sub(youngest) < floor)
        .then(|| WorktreeGuard::skip_with(SkipReason::TooRecent, FLOOR_DETAIL))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000_000;

    fn candidate(days_old: i64, locked: bool, dirty: bool) -> WorktreeCandidate {
        WorktreeCandidate::new("/tmp/wt", Some(NOW - days_old * 86_400), locked, dirty)
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
        let c = WorktreeCandidate::new("/tmp/wt", None, false, false);
        assert_eq!(worktree_verdict(&c, NOW, 14), Verdict::SkipTooRecent);
    }
}
