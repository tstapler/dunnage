#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! `decide_worktree` table (Story 3.1.2): precedence, the 24 h floor, ignored files, common path checks.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use disk_cleanup_core::guard::Facts;
use disk_cleanup_core::roots::RootSet;
use disk_cleanup_core::targets::worktree_guard::{
    decide_worktree, floor_raised, IgnoredFiles, PushState, WorktreeCandidate, WorktreeOpts,
    AGE_UNKNOWN_DETAIL, FLOOR_DETAIL, IGNORED_UNKNOWN_DETAIL, WORKTREE_MIN_AGE,
};
use disk_cleanup_core::{Dirs, Identity, Kind, RootDev, SkipReason, Tri};

const NOW: i64 = 1_000_000_000;
const HOUR: i64 = 3_600;
const DAY: i64 = 24 * HOUR;
const IDENT: Identity = Identity { dev: 28, ino: 42 };
const WT: &str = "/h/code/r/.claude/worktrees/agent-a41";

fn roots() -> RootSet {
    let dirs = Dirs {
        home: "/h".into(),
        state: "/h/.local/state".into(),
        cache: "/h/.cache".into(),
        config: "/h/.config".into(),
    };
    let canon = |p: &Path| -> io::Result<(PathBuf, Identity)> {
        Ok((
            p.to_path_buf(),
            Identity {
                dev: 28,
                ino: p.as_os_str().len() as u64,
            },
        ))
    };
    RootSet::resolve(&dirs, &["/h/code".into()], None, Path::new("/h"), &canon).unwrap()
}

fn facts() -> Facts {
    Facts {
        kind: Kind::Worktree,
        path: PathBuf::from(WT),
        ident: IDENT,
        is_symlink: false,
        root_dev: RootDev(28),
        is_mount_point: false,
    }
}

/// A clean, pushed, idle worktree whose three ages are `days` old.
fn good(days: i64) -> WorktreeCandidate {
    let mut c = WorktreeCandidate::new(WT, Some(NOW - days * DAY), false, false);
    c.push = PushState::Pushed;
    c.newest_mtime = Some(NOW - days * DAY);
    c.admin_mtime = Some(NOW - days * DAY);
    c.in_use = Tri::NotRunning;
    c.ignored = Some(IgnoredFiles::default());
    c
}

fn opts(min_age_secs: u64) -> WorktreeOpts {
    WorktreeOpts {
        min_age: Duration::from_secs(min_age_secs),
        discard_ignored: false,
    }
}

fn run(c: &WorktreeCandidate, o: WorktreeOpts) -> Option<SkipReason> {
    decide_worktree(c, Some(&facts()), IDENT, &roots(), NOW, o).skip_reason()
}

#[test]
fn decide_worktree_should_give_the_reason_of_the_plan_table_in_order() {
    let mut locked = good(30);
    locked.locked = true;
    let mut dirty = good(30);
    dirty.dirty = true;
    let mut unpushed = good(30);
    unpushed.push = PushState::Unpushed { commits: Some(2) };
    let mut detached = good(30);
    detached.push = PushState::Unpushed { commits: None };
    detached.detached = true;
    let table = [
        ("locked", locked, Some(SkipReason::Locked)),
        ("dirty", dirty, Some(SkipReason::Dirty)),
        ("clean, 3 d", good(3), Some(SkipReason::TooRecent)),
        ("2 unpushed", unpushed, Some(SkipReason::UnpushedCommits)),
        ("detached", detached, Some(SkipReason::DetachedHead)),
        ("pushed", good(30), None),
    ];
    for (name, c, want) in table {
        assert_eq!(run(&c, opts(14 * 86_400)), want, "{name}");
    }
}

#[test]
fn decide_worktree_should_report_locked_before_dirty_even_when_young() {
    let mut c = good(0);
    c.locked = true;
    c.dirty = true;
    assert_eq!(run(&c, opts(0)), Some(SkipReason::Locked));
}

#[test]
fn decide_worktree_should_treat_no_commits_as_brand_new() {
    let mut c = good(30);
    c.last_commit_epoch = None;
    assert_eq!(run(&c, opts(14 * 86_400)), Some(SkipReason::TooRecent));
    assert_eq!(run(&c, opts(0)), Some(SkipReason::TooRecent));
}

#[test]
fn decide_worktree_should_allow_a_pushed_detached_head_and_skip_an_unknown_push_state() {
    let mut c = good(30);
    c.detached = true;
    assert_eq!(run(&c, opts(14 * 86_400)), None);
    c.push = PushState::Unknown;
    assert_eq!(run(&c, opts(14 * 86_400)), Some(SkipReason::DetachedHead));
    c.detached = false;
    assert_eq!(
        run(&c, opts(14 * 86_400)),
        Some(SkipReason::UnpushedCommits)
    );
}

#[test]
fn decide_worktree_should_hold_the_24h_floor_whatever_min_age_is() {
    // last commit 40 d old, tree and admin entry 3 h old
    let mut c = good(40);
    c.newest_mtime = Some(NOW - 3 * HOUR);
    c.admin_mtime = Some(NOW - 3 * HOUR);
    for flag in [0, 300, 14 * 86_400] {
        let g = decide_worktree(&c, Some(&facts()), IDENT, &roots(), NOW, opts(flag));
        assert_eq!(g.skip_reason(), Some(SkipReason::TooRecent), "flag {flag}");
        assert_eq!(g.detail(), Some(FLOOR_DETAIL), "flag {flag}");
    }
    // the same worktree with all three ages 25 h and --min-age 0
    let mut old = good(40);
    old.last_commit_epoch = Some(NOW - 25 * HOUR);
    old.newest_mtime = Some(NOW - 25 * HOUR);
    old.admin_mtime = Some(NOW - 25 * HOUR);
    assert_eq!(run(&old, opts(0)), None);
}

#[test]
fn decide_worktree_should_use_the_youngest_of_the_three_ages() {
    for which in 0..3 {
        let mut c = good(40);
        let young = Some(NOW - 3 * HOUR);
        match which {
            0 => c.last_commit_epoch = young,
            1 => c.newest_mtime = young,
            _ => c.admin_mtime = young,
        }
        assert_eq!(run(&c, opts(0)), Some(SkipReason::TooRecent), "{which}");
    }
}

#[test]
fn decide_worktree_should_put_the_floor_boundary_at_exactly_24_hours() {
    let mut c = good(40);
    c.last_commit_epoch = Some(NOW - 24 * HOUR);
    c.newest_mtime = Some(NOW - 24 * HOUR);
    c.admin_mtime = Some(NOW - 24 * HOUR);
    assert_eq!(run(&c, opts(0)), None);
    c.newest_mtime = Some(NOW - 24 * HOUR + 1);
    assert_eq!(run(&c, opts(0)), Some(SkipReason::TooRecent));
}

#[test]
fn decide_worktree_should_fail_closed_when_the_tree_age_is_unknown() {
    for which in 0..2 {
        let mut c = good(40);
        if which == 0 {
            c.newest_mtime = None;
        } else {
            c.admin_mtime = None;
        }
        let g = decide_worktree(&c, Some(&facts()), IDENT, &roots(), NOW, opts(0));
        assert_eq!(g.skip_reason(), Some(SkipReason::TooRecent));
        assert_eq!(g.detail(), Some(AGE_UNKNOWN_DETAIL));
    }
}

#[test]
fn floor_raised_should_be_true_below_the_floor_only() {
    assert!(floor_raised(Duration::ZERO));
    assert!(floor_raised(Duration::from_mins(5)));
    assert!(!floor_raised(WORKTREE_MIN_AGE));
    assert!(!floor_raised(Duration::from_hours(24 * 14)));
}

#[test]
fn decide_worktree_should_skip_in_use_and_unknown_use_even_with_min_age_zero() {
    let mut c = good(40);
    c.in_use = Tri::Running;
    c.holder = Some(("claude".into(), 99));
    assert_eq!(run(&c, opts(0)), Some(SkipReason::InUse));
    c.in_use = Tri::Unknown;
    assert_eq!(run(&c, opts(0)), Some(SkipReason::RunStateUnknown));
    // a lock still wins with --min-age 0
    c.locked = true;
    assert_eq!(run(&c, opts(0)), Some(SkipReason::Locked));
}

#[test]
fn decide_worktree_should_skip_ignored_files_unless_build_output_only_or_discard() {
    let mut c = good(30);
    c.ignored = Some(IgnoredFiles {
        entries: 2,
        bytes: 12_288,
        build_output_bytes: 0,
    });
    assert_eq!(
        run(&c, opts(14 * 86_400)),
        Some(SkipReason::HasIgnoredFiles)
    );
    let mut discard = opts(14 * 86_400);
    discard.discard_ignored = true;
    assert_eq!(run(&c, discard), None);

    // only build output (target/): allowed
    c.ignored = Some(IgnoredFiles {
        entries: 0,
        bytes: 0,
        build_output_bytes: 2 << 20,
    });
    assert_eq!(run(&c, opts(14 * 86_400)), None);

    // a failed probe is a skip with its own detail; the opt-in does not need the answer
    c.ignored = None;
    let g = decide_worktree(&c, Some(&facts()), IDENT, &roots(), NOW, opts(14 * 86_400));
    assert_eq!(g.skip_reason(), Some(SkipReason::HasIgnoredFiles));
    assert_eq!(g.detail(), Some(IGNORED_UNKNOWN_DETAIL));
    assert_eq!(run(&c, discard), None);
}

#[test]
fn decide_worktree_should_run_the_common_path_checks_first() {
    // young and dirty everywhere: the path rule answers before any dirty/age rule
    let mut c = good(0);
    c.dirty = true;
    let o = opts(14 * 86_400);

    let mut f = facts();
    f.path = PathBuf::from("/tmp/elsewhere/agent-x");
    let g = decide_worktree(&c, Some(&f), IDENT, &roots(), NOW, o);
    assert_eq!(g.skip_reason(), Some(SkipReason::OutsideRoot));

    let mut f = facts();
    f.is_symlink = true;
    let g = decide_worktree(&c, Some(&f), IDENT, &roots(), NOW, o);
    assert_eq!(g.skip_reason(), Some(SkipReason::Symlink));

    let mut f = facts();
    f.root_dev = RootDev(8);
    let g = decide_worktree(&c, Some(&f), IDENT, &roots(), NOW, o);
    assert_eq!(g.skip_reason(), Some(SkipReason::CrossDevice));

    let mut f = facts();
    f.is_mount_point = true;
    let g = decide_worktree(&c, Some(&f), IDENT, &roots(), NOW, o);
    assert_eq!(g.skip_reason(), Some(SkipReason::MountPoint));
}

#[test]
fn decide_worktree_should_skip_gone_and_changed_identity() {
    let c = good(30);
    let o = opts(14 * 86_400);
    let g = decide_worktree(&c, None, IDENT, &roots(), NOW, o);
    assert_eq!(g.skip_reason(), Some(SkipReason::Gone));
    let other = Identity { dev: 28, ino: 43 };
    let g = decide_worktree(&c, Some(&facts()), other, &roots(), NOW, o);
    assert_eq!(g.skip_reason(), Some(SkipReason::IdentityChanged));
}

#[test]
fn unpushed_should_expose_the_count_only_when_known() {
    let mut c = good(30);
    c.push = PushState::Unpushed { commits: Some(2) };
    assert_eq!(c.unpushed(), Some(2));
    c.push = PushState::Unknown;
    assert_eq!(c.unpushed(), None);
}
