#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::many_single_char_names,
    non_snake_case
)]
//! Worktree probes and guards over real git repositories in tempdirs (Story 3.1.2). Nothing here touches a
//! repository outside the fixture: every `git` runs with `-C <tempdir path>` and the fixture's own remote.

mod common;
use common::discovery::*;

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use dunnage_adapters::mtime_probe::NewestStore;
use dunnage_adapters::targets::worktree_discover::{discover_worktrees, WorktreeSite};
use dunnage_adapters::targets::worktree_probe::{guard_worktree, ProbeEnv, WorktreeProbed};
use dunnage_adapters::{RealCommandRunner, RealDirOps, RealProbe};
use dunnage_core::ports::{Cmd, CmdOutput, CommandRunner, SystemProbe};
use dunnage_core::targets::worktree_guard::{
    WorktreeGuard, WorktreeOpts, FLOOR_DETAIL, WORKTREE_MIN_AGE,
};
use dunnage_core::testing::FakeProbe;
use dunnage_core::{ApplyToken, DeleteError, SkipReason, Tri, WorktreeRemoval};

const DAY_SECS: u64 = 24 * 3600;

/// Wraps the real runner and records every argv.
struct Counting {
    inner: RealCommandRunner,
    log: Mutex<Vec<Vec<String>>>,
}

impl Counting {
    fn new() -> Counting {
        Counting {
            inner: RealCommandRunner::new(),
            log: Mutex::new(Vec::new()),
        }
    }
    fn count(&self, needle: &[&str]) -> usize {
        let log = self.log.lock().unwrap();
        log.iter()
            .filter(|a| a.windows(needle.len()).any(|w| w == needle))
            .count()
    }
    fn total(&self) -> usize {
        self.log.lock().unwrap().len()
    }
}

impl CommandRunner for Counting {
    fn exists(&self, p: &str) -> bool {
        self.inner.exists(p)
    }
    fn run(&self, cmd: &Cmd) -> Result<CmdOutput, DeleteError> {
        let mut argv = vec![cmd.program.to_string_lossy().into_owned()];
        argv.extend(cmd.args.iter().map(|a| a.to_string_lossy().into_owned()));
        self.log.lock().unwrap().push(argv);
        self.inner.run(cmd)
    }
    fn run_mutating(&self, c: &Cmd, p: &ApplyToken) -> Result<CmdOutput, DeleteError> {
        self.inner.run_mutating(c, p)
    }
    fn remove_worktree(
        &self,
        w: &WorktreeRemoval,
        p: &ApplyToken,
    ) -> Result<CmdOutput, DeleteError> {
        self.inner.remove_worktree(w, p)
    }
}

fn git(dir: &Path, args: &[&str], date: Option<i64>) {
    let mut c = Command::new("git");
    c.arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@e",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "protocol.file.allow=always",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    if let Some(d) = date {
        c.env("GIT_AUTHOR_DATE", format!("{d} +0000"))
            .env("GIT_COMMITTER_DATE", format!("{d} +0000"));
    }
    let out = c.output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

struct Repo {
    f: Fixture,
    r: PathBuf,
}

impl Repo {
    /// Main repo `home/code/r` with one commit 40 days old, pushed to `origin`; `.gitignore` lists `.env`,
    /// `*.sqlite` and `target/`, and a `Cargo.toml` makes `target/` a recognised build directory.
    fn new() -> Repo {
        let f = fixture();
        let origin = f.home.parent().unwrap().join("origin.git");
        fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "-q", "--bare", "-b", "main"], None);
        let r = f.code.join("r");
        fs::create_dir_all(&r).unwrap();
        git(&r, &["init", "-q", "-b", "main"], None);
        fs::write(r.join(".gitignore"), ".env\n*.sqlite\ntarget/\n").unwrap();
        fs::write(r.join("Cargo.toml"), "[package]\n").unwrap();
        fs::write(r.join("src.txt"), "one\n").unwrap();
        git(&r, &["add", "."], None);
        git(
            &r,
            &["commit", "-q", "-m", "one"],
            Some(now_epoch() - 40 * 86_400),
        );
        git(
            &r,
            &["remote", "add", "origin", origin.to_str().unwrap()],
            None,
        );
        git(&r, &["push", "-q", "origin", "main"], None);
        Repo { f, r }
    }

    /// `git worktree add` on a new branch below `.claude/worktrees/`, already pushed.
    fn add(&self, name: &str) -> PathBuf {
        let rel = format!(".claude/worktrees/{name}");
        git(&self.r, &["worktree", "add", "-q", &rel, "-b", name], None);
        self.r.join(rel)
    }

    /// Makes the whole worktree look `secs` old: every entry's mtime, then the admin entry (after a status
    /// refreshed the index, which would otherwise move it).
    fn age(&self, wt: &Path, secs: u64) {
        age_tree(wt, secs);
        git(wt, &["status", "--porcelain"], None);
        let name = wt.file_name().unwrap();
        set_age(
            &self.r.join(".git/worktrees").join(name),
            Duration::from_secs(secs),
        );
    }

    fn site(&self, wt: &Path) -> WorktreeSite {
        discover_worktrees(&RealCommandRunner::new(), std::slice::from_ref(&self.r))
            .sites
            .into_iter()
            .find(|s| s.entry.path == wt)
            .unwrap()
    }
}

fn age_tree(p: &Path, secs: u64) {
    let md = fs::symlink_metadata(p).unwrap();
    if md.is_dir() {
        for e in fs::read_dir(p).unwrap().flatten() {
            age_tree(&e.path(), secs);
        }
    }
    if !md.file_type().is_symlink() {
        set_age(p, Duration::from_secs(secs));
    }
}

fn opts(min_age: Duration) -> WorktreeOpts {
    WorktreeOpts {
        min_age,
        discard_ignored: false,
    }
}

fn run_with(
    repo: &Repo,
    runner: &dyn CommandRunner,
    probe: &dyn SystemProbe,
    site: &WorktreeSite,
    o: WorktreeOpts,
) -> (WorktreeProbed, WorktreeGuard) {
    let roots = roots_for(&repo.f);
    let ops = RealDirOps::confined_to(&repo.f.home);
    let env = ProbeEnv {
        runner,
        probe,
        ops: &ops,
        roots: &roots,
    };
    guard_worktree(&env, site, now_epoch(), o, &mut NewestStore::in_memory())
}

fn run(repo: &Repo, site: &WorktreeSite, o: WorktreeOpts) -> (WorktreeProbed, WorktreeGuard) {
    run_with(repo, &RealCommandRunner::new(), &FakeProbe::new(), site, o)
}

fn reason(g: &WorktreeGuard) -> Option<SkipReason> {
    g.skip_reason()
}

#[test]
fn guard_should_allow_a_clean_pushed_old_worktree_whose_only_ignored_entry_is_target() {
    let repo = Repo::new();
    let wt = repo.add("agent-a41");
    fs::create_dir_all(wt.join("target")).unwrap();
    fs::write(wt.join("target/o"), vec![0u8; 8192]).unwrap();
    repo.age(&wt, 3 * DAY_SECS);
    let site = repo.site(&wt);

    let (p, g) = run(&repo, &site, opts(Duration::ZERO));

    assert_eq!(reason(&g), None, "{p:?}");
    let ig = p.candidate.ignored.unwrap();
    assert_eq!(ig.entries, 0);
    assert!(ig.build_output_bytes >= 8192);
    assert_eq!(
        p.candidate.push,
        dunnage_core::targets::worktree_guard::PushState::Pushed
    );
}

#[test]
fn guard_should_not_move_the_admin_entry_so_a_second_probe_gives_the_same_answer() {
    let repo = Repo::new();
    let wt = repo.add("agent-a41");
    repo.age(&wt, 3 * DAY_SECS);
    let site = repo.site(&wt);
    for round in 0..3 {
        let (_, g) = run(&repo, &site, opts(Duration::ZERO));
        assert_eq!(reason(&g), None, "round {round}");
    }
}

#[test]
fn guard_should_apply_the_24h_floor_to_a_fresh_checkout_of_an_old_commit() {
    let repo = Repo::new();
    let wt = repo.add("agent-a41");
    repo.age(&wt, 3 * 3600);
    let site = repo.site(&wt);
    for flag in [
        Duration::ZERO,
        Duration::from_mins(5),
        Duration::from_hours(24 * 14),
    ] {
        let (_, g) = run(&repo, &site, opts(flag));
        assert_eq!(reason(&g), Some(SkipReason::TooRecent), "flag {flag:?}");
        assert_eq!(g.detail(), Some(FLOOR_DETAIL), "flag {flag:?}");
    }
    // 25 h on all three: allowed with --min-age 0
    repo.age(&wt, 25 * 3600);
    let (_, g) = run(&repo, &site, opts(Duration::ZERO));
    assert_eq!(reason(&g), None);
    assert!(WORKTREE_MIN_AGE < Duration::from_hours(25));
}

#[test]
fn guard_should_skip_a_worktree_with_a_young_admin_entry_alone() {
    let repo = Repo::new();
    let wt = repo.add("agent-a41");
    repo.age(&wt, 3 * DAY_SECS);
    set_age(
        &repo.r.join(".git/worktrees/agent-a41"),
        Duration::from_hours(2),
    );
    let (_, g) = run(&repo, &repo.site(&wt), opts(Duration::ZERO));
    assert_eq!(reason(&g), Some(SkipReason::TooRecent));
    assert_eq!(g.detail(), Some(FLOOR_DETAIL));
}

#[test]
fn guard_should_skip_dirty_for_a_modified_tracked_file_and_for_an_untracked_file() {
    let repo = Repo::new();
    let a = repo.add("agent-a41");
    let b = repo.add("agent-b07");
    fs::write(a.join("src.txt"), "changed\n").unwrap();
    fs::write(b.join("scratch.txt"), "new\n").unwrap();
    repo.age(&a, 3 * DAY_SECS);
    repo.age(&b, 3 * DAY_SECS);
    for wt in [&a, &b] {
        let (_, g) = run(&repo, &repo.site(wt), opts(Duration::ZERO));
        assert_eq!(reason(&g), Some(SkipReason::Dirty), "{wt:?}");
    }
}

#[test]
fn guard_should_skip_locked_even_with_min_age_zero() {
    let repo = Repo::new();
    let wt = repo.add("agent-a41");
    repo.age(&wt, 3 * DAY_SECS);
    git(&repo.r, &["worktree", "lock", wt.to_str().unwrap()], None);
    let counting = Counting::new();
    let (_, g) = run_with(
        &repo,
        &counting,
        &FakeProbe::new(),
        &repo.site(&wt),
        opts(Duration::ZERO),
    );
    assert_eq!(reason(&g), Some(SkipReason::Locked));
    assert_eq!(
        counting.total(),
        0,
        "a locked worktree costs no git command"
    );
}

#[test]
fn guard_should_skip_ignored_files_that_git_would_delete_and_allow_them_with_discard() {
    let repo = Repo::new();
    let wt = repo.add("agent-a41");
    fs::write(wt.join(".env"), "SECRET=1\n").unwrap();
    fs::write(wt.join("data.sqlite"), vec![1u8; 8192]).unwrap();
    fs::create_dir_all(wt.join("target")).unwrap();
    fs::write(wt.join("target/o"), "o").unwrap();
    repo.age(&wt, 3 * DAY_SECS);
    let site = repo.site(&wt);

    let (p, g) = run(&repo, &site, opts(Duration::ZERO));

    assert_eq!(reason(&g), Some(SkipReason::HasIgnoredFiles));
    let ig = p.candidate.ignored.unwrap();
    assert_eq!(ig.entries, 2, "{ig:?}");
    assert!(ig.bytes >= 8192);
    assert!(ig.build_output_bytes > 0);

    let mut discard = opts(Duration::ZERO);
    discard.discard_ignored = true;
    let (p, g) = run(&repo, &site, discard);
    assert_eq!(reason(&g), None);
    assert_eq!(
        p.candidate.ignored.unwrap().entries,
        2,
        "the row still reports what is discarded"
    );
}

#[test]
fn guard_should_count_the_files_of_an_ignored_directory_that_is_not_build_output() {
    let repo = Repo::new();
    let wt = repo.add("agent-a41");
    // `*.sqlite` ignores a whole directory named like one
    fs::create_dir_all(wt.join("db.sqlite")).unwrap();
    for i in 0..3 {
        fs::write(wt.join(format!("db.sqlite/{i}")), "x").unwrap();
    }
    repo.age(&wt, 3 * DAY_SECS);
    let (p, g) = run(&repo, &repo.site(&wt), opts(Duration::ZERO));
    assert_eq!(reason(&g), Some(SkipReason::HasIgnoredFiles));
    assert_eq!(p.candidate.ignored.unwrap().entries, 3);
}

#[test]
fn guard_should_skip_unpushed_and_detached_heads_and_allow_a_pushed_detached_head() {
    let repo = Repo::new();
    // a branch worktree with a local-only commit, dated old
    let a = repo.add("agent-a41");
    fs::write(a.join("src.txt"), "two\n").unwrap();
    git(
        &a,
        &["commit", "-q", "-am", "two"],
        Some(now_epoch() - 40 * 86_400),
    );
    // a detached worktree on that unpushed commit
    git(
        &repo.r,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            ".claude/worktrees/agent-b07",
            "agent-a41",
        ],
        None,
    );
    let b = repo.r.join(".claude/worktrees/agent-b07");
    // a detached worktree on the pushed commit
    git(
        &repo.r,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            ".claude/worktrees/agent-c13",
            "main",
        ],
        None,
    );
    let c = repo.r.join(".claude/worktrees/agent-c13");
    for wt in [&a, &b, &c] {
        repo.age(wt, 3 * DAY_SECS);
    }
    let want = [
        (&a, Some(SkipReason::UnpushedCommits)),
        (&b, Some(SkipReason::DetachedHead)),
        (&c, None),
    ];
    for (wt, w) in want {
        let (p, g) = run(&repo, &repo.site(wt), opts(Duration::ZERO));
        assert_eq!(reason(&g), w, "{wt:?}: {p:?}");
    }
}

#[test]
fn guard_should_skip_in_use_for_a_real_process_with_its_cwd_inside_even_with_min_age_zero() {
    let repo = Repo::new();
    let wt = repo.add("agent-a41");
    repo.age(&wt, 3 * DAY_SECS);
    let mut child = Command::new("sleep")
        .arg("60")
        .current_dir(&wt)
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let (p, g) = run_with(
        &repo,
        &RealCommandRunner::new(),
        &RealProbe,
        &repo.site(&wt),
        opts(Duration::ZERO),
    );
    let _ = child.kill();
    let _ = child.wait();
    if p.candidate.in_use == Tri::Unknown {
        // an unreadable process of this user hid the answer: the guard must have failed closed
        assert_eq!(reason(&g), Some(SkipReason::RunStateUnknown));
        return;
    }
    assert_eq!(reason(&g), Some(SkipReason::InUse));
    let (name, pid) = p.candidate.holder.unwrap();
    // `lsof` does not name the process (lsof_scan reports "a process"); /proc gives the command name
    let want = if cfg!(target_os = "linux") {
        "sleep"
    } else {
        "a process"
    };
    assert_eq!((name.as_str(), pid), (want, child.id()));
}

#[test]
fn guard_should_issue_status_and_log_once_per_worktree_not_per_subdirectory() {
    let repo = Repo::new();
    let mut sites = Vec::new();
    for i in 0..8 {
        let wt = repo.add(&format!("agent-w{i}"));
        for d in 0..200 {
            fs::create_dir_all(wt.join(format!("sub/d{d}"))).unwrap();
        }
        repo.age(&wt, 3 * DAY_SECS);
        sites.push(repo.site(&wt));
    }
    let counting = Counting::new();
    for s in &sites {
        let (_, g) = run_with(&repo, &counting, &FakeProbe::new(), s, opts(Duration::ZERO));
        assert_eq!(reason(&g), None);
    }
    assert_eq!(
        counting.count(&["status", "--porcelain=v1", "--untracked-files=normal"]),
        8
    );
    assert_eq!(counting.count(&["log", "-1", "--format=%ct"]), 8);
    assert_eq!(counting.count(&["--ignored=matching"]), 8);
    assert_eq!(
        counting.count(&["worktree"]),
        0,
        "probing never lists again"
    );
}

#[test]
fn guard_should_run_the_path_checks_before_any_git_command() {
    let repo = Repo::new();
    let wt = repo.add("agent-a41");
    repo.age(&wt, 3 * DAY_SECS);
    let site = repo.site(&wt);

    // outside every scan root: a clone of the layout below an unrelated directory
    let counting = Counting::new();
    let mut outside = site.clone();
    let elsewhere = repo.f.home.parent().unwrap().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    outside.entry.path = elsewhere.clone();
    outside.ident = Some(dunnage_core::Identity::of(
        &fs::metadata(&elsewhere).unwrap(),
    ));
    let (_, g) = run_with(
        &repo,
        &counting,
        &FakeProbe::new(),
        &outside,
        opts(Duration::ZERO),
    );
    assert_eq!(reason(&g), Some(SkipReason::OutsideRoot));

    // the worktree path replaced by a symlink to a directory that must stay untouched
    let moved = repo.f.home.parent().unwrap().join("moved");
    fs::rename(&wt, &moved).unwrap();
    symlink(&moved, &wt).unwrap();
    fs::write(moved.join("keep"), "k").unwrap();
    let (_, g) = run_with(
        &repo,
        &counting,
        &FakeProbe::new(),
        &site,
        opts(Duration::ZERO),
    );
    assert_eq!(reason(&g), Some(SkipReason::Symlink));
    assert!(moved.join("keep").exists());
    assert_eq!(counting.total(), 0, "no git command ran for a refused path");
}

#[test]
fn guard_should_skip_gone_for_a_missing_path_and_identity_changed_for_a_replaced_one() {
    let repo = Repo::new();
    let wt = repo.add("agent-a41");
    repo.age(&wt, 3 * DAY_SECS);
    let site = repo.site(&wt);

    let mut changed = site.clone();
    let mut ident = changed.ident.unwrap();
    ident.ino += 1;
    changed.ident = Some(ident);
    let (_, g) = run(&repo, &changed, opts(Duration::ZERO));
    assert_eq!(reason(&g), Some(SkipReason::IdentityChanged));

    fs::rename(&wt, repo.f.home.parent().unwrap().join("moved")).unwrap();
    let (_, g) = run(&repo, &site, opts(Duration::ZERO));
    assert_eq!(reason(&g), Some(SkipReason::Gone));
}

#[test]
fn guard_should_treat_a_worktree_without_commits_ahead_as_too_recent_when_the_commit_is_young() {
    let repo = Repo::new();
    let wt = repo.add("agent-a41");
    fs::write(wt.join("src.txt"), "two\n").unwrap();
    git(&wt, &["commit", "-q", "-am", "now"], None);
    repo.age(&wt, 3 * DAY_SECS);
    let (_, g) = run(&repo, &repo.site(&wt), opts(Duration::from_hours(24 * 14)));
    assert_eq!(reason(&g), Some(SkipReason::TooRecent));
    assert_eq!(g.detail(), None, "the flag, not the floor, held it back");
}
