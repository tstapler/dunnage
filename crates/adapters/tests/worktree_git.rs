#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::many_single_char_names,
    non_snake_case
)]
//! Story 3.1.3: `WorktreeTarget` removing real agent worktrees with `git worktree remove` alone. Every `git`
//! runs with `-C <tempdir path>` against a fixture repository and the fixture's own bare remote; nothing
//! outside the tempdir is read or written.

mod common;
use common::discovery::*;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use dunnage_adapters::targets::worktree::WorktreeTarget;
use dunnage_adapters::{RealCommandRunner, RealDirOps, RealProbe};
use dunnage_core::audit_types::RunRoots;
use dunnage_core::executor::{Executor, Mode, RunMeta, RunOptions};
use dunnage_core::ports::{worktree_remove_argv, Cmd, CmdOutput, CommandRunner, Env};
use dunnage_core::selector::{Discard, KindSelector, Selection};
use dunnage_core::target::{Guarded, Target};
use dunnage_core::testing::{
    FakeAuditSink, FakeClock, FakeFs, FakePlanStore, FakeProbe, FakeRunGuard, FakeSpaceMeter,
};
use dunnage_core::{
    ApplyToken, BuildInfo, Candidate, DeleteError, SelectorId, SkipReason, Status, WorktreeRemoval,
};

const DAY_SECS: u64 = 24 * 3600;

/// The real runner, logging every argv, `remove_worktree` included.
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
    fn push(&self, program: &str, args: &[std::ffi::OsString]) {
        let mut argv = vec![program.to_string()];
        argv.extend(args.iter().map(|a| a.to_string_lossy().into_owned()));
        self.log.lock().unwrap().push(argv);
    }
    fn all(&self) -> Vec<Vec<String>> {
        self.log.lock().unwrap().clone()
    }
    fn removes(&self) -> usize {
        self.count_word("remove")
    }
    fn count_word(&self, w: &str) -> usize {
        self.all()
            .iter()
            .filter(|a| a.iter().any(|x| x == w))
            .count()
    }
}

impl CommandRunner for Counting {
    fn exists(&self, p: &str) -> bool {
        self.inner.exists(p)
    }
    fn run(&self, cmd: &Cmd) -> Result<CmdOutput, DeleteError> {
        self.push(&cmd.program.to_string_lossy(), &cmd.args);
        self.inner.run(cmd)
    }
    fn run_mutating(&self, c: &Cmd, p: &ApplyToken) -> Result<CmdOutput, DeleteError> {
        self.push(&c.program.to_string_lossy(), &c.args);
        self.inner.run_mutating(c, p)
    }
    fn remove_worktree(
        &self,
        w: &WorktreeRemoval,
        p: &ApplyToken,
    ) -> Result<CmdOutput, DeleteError> {
        self.push("git", &worktree_remove_argv(w));
        self.inner.remove_worktree(w, p)
    }
}

fn git_out(dir: &Path, args: &[&str], date: Option<i64>) -> String {
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
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn git(dir: &Path, args: &[&str], date: Option<i64>) {
    git_out(dir, args, date);
}

struct Repo {
    f: Fixture,
    r: PathBuf,
}

impl Repo {
    /// Main repo `home/code/r`, one commit 40 days old pushed to a bare `origin`; `.gitignore` lists `.env`,
    /// `*.sqlite` and `target/`.
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

    fn add(&self, name: &str) -> PathBuf {
        let rel = format!(".claude/worktrees/{name}");
        git(&self.r, &["worktree", "add", "-q", &rel, "-b", name], None);
        self.r.join(rel)
    }

    fn admin(&self, wt: &Path) -> PathBuf {
        self.r.join(".git/worktrees").join(wt.file_name().unwrap())
    }

    /// Every entry's mtime, then the admin entry (after a status refreshed the index).
    fn age(&self, wt: &Path, secs: u64) {
        age_tree(wt, secs);
        git(wt, &["status", "--porcelain"], None);
        set_age(&self.admin(wt), Duration::from_secs(secs));
    }

    fn listed(&self) -> String {
        git_out(&self.r, &["worktree", "list", "--porcelain"], None)
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

struct Rig {
    fs: FakeFs,
    clock: FakeClock,
    probe: FakeProbe,
    space: FakeSpaceMeter,
    audit: FakeAuditSink,
    guard: FakeRunGuard,
    plans: FakePlanStore,
    home: PathBuf,
}

impl Rig {
    fn new(f: &Fixture) -> Rig {
        Rig {
            fs: FakeFs::new(),
            clock: FakeClock::at(now_epoch()),
            probe: FakeProbe::new(),
            space: FakeSpaceMeter::new(
                vec![FakeSpaceMeter::plain_snapshot("/", 500, 1000)],
                FakeSpaceMeter::stable(),
            ),
            audit: FakeAuditSink::new(),
            guard: FakeRunGuard::new(),
            plans: FakePlanStore::new(),
            home: f.home.clone(),
        }
    }

    fn env<'a>(&'a self, runner: &'a dyn CommandRunner, probe: &'a dyn SystemProbeDyn) -> Env<'a> {
        Env {
            fs: &self.fs,
            runner,
            clock: &self.clock,
            probe,
            space: &self.space,
            audit: &self.audit,
            guard: &self.guard,
            plans: &self.plans,
            dirs: dirs_of(&self.home),
        }
    }
}

use dunnage_core::ports::SystemProbe as SystemProbeDyn;

fn target(f: &Fixture) -> WorktreeTarget<RealDirOps> {
    WorktreeTarget::new(roots_for(f), RealDirOps::confined_to(&f.home))
}

fn selection(discard: bool, min_age_secs: Option<u64>) -> Selection {
    let mut s = Selection::new(
        KindSelector::of(&[SelectorId::Worktrees]),
        KindSelector::default(),
    );
    s.min_age_secs = min_age_secs;
    if discard {
        s.worktree_ignored = Some(Discard::Discard);
    }
    s
}

fn options() -> RunOptions {
    RunOptions {
        jobs: 1,
        settle_timeout: Duration::from_secs(1),
        meter_path: None,
        meta: RunMeta {
            run_id: "run-1".into(),
            build: BuildInfo {
                bin_version: "0.1.0".into(),
                git_commit: "abc".into(),
                dirty: false,
                metascope_rev: "none".into(),
                patched: false,
                metascope_dirty: false,
            },
            roots: RunRoots::default(),
            verdict_start: None,
        },
    }
}

fn cand<'a>(cs: &'a [Candidate], wt: &Path) -> &'a Candidate {
    cs.iter()
        .find(|c| matches!(c.locator(), dunnage_core::Locator::Path(p) if p == wt))
        .unwrap_or_else(|| panic!("no candidate {} in {cs:?}", wt.display()))
}

/// A repo with `n` aged, removable worktrees.
fn repo_with(n: usize) -> (Repo, Vec<PathBuf>) {
    let repo = Repo::new();
    let wts: Vec<PathBuf> = (0..n).map(|i| repo.add(&format!("agent-w{i}"))).collect();
    for w in &wts {
        repo.age(w, 3 * DAY_SECS);
    }
    (repo, wts)
}

#[test]
fn safety_worktree_removal_never_uses_force_or_prune() {
    let (repo, wts) = repo_with(3);
    let t = target(&repo.f);
    let rig = Rig::new(&repo.f);
    let runner = Counting::new();
    let env = rig.env(&runner, &rig.probe);
    let cs = t.discover(&env, &selection(false, Some(0)));
    assert_eq!(cs.len(), 3);

    for w in &wts {
        let c = cand(&cs, w);
        assert_eq!(t.guard(c, &env), Guarded::Allow);
        t.execute(c, &env, &ApplyToken::for_tests()).unwrap();
        assert!(!w.exists());
        assert!(!repo.listed().contains(w.to_str().unwrap()), "{w:?}");
        assert!(!repo.admin(w).exists(), "git removed its admin entry");
    }

    assert_eq!(runner.removes(), 3);
    for argv in runner.all() {
        assert!(
            !argv.iter().any(|a| a == "--force" || a == "-f"),
            "{argv:?}"
        );
        assert!(!argv.iter().any(|a| a == "prune"), "{argv:?}");
    }
}

#[test]
fn execute_should_report_the_pre_delete_size_including_the_ignored_target_directory() {
    let repo = Repo::new();
    let wt = repo.add("agent-a41");
    fs::create_dir_all(wt.join("target")).unwrap();
    fs::write(wt.join("target/o"), vec![1u8; 2 << 20]).unwrap();
    repo.age(&wt, 3 * DAY_SECS);
    let t = target(&repo.f);
    let rig = Rig::new(&repo.f);
    let runner = Counting::new();
    let env = rig.env(&runner, &rig.probe);
    let cs = t.discover(&env, &selection(false, Some(0)));
    let c = cand(&cs, &wt);
    assert!(
        c.est_bytes().0 >= 2 << 20,
        "plan row size {:?}",
        c.est_bytes()
    );
    assert_eq!(t.guard(c, &env), Guarded::Allow);

    let freed = t.execute(c, &env, &ApplyToken::for_tests()).unwrap();

    assert!(freed.attributed_bytes.0 >= 2 << 20, "{freed:?}");
    assert!(!wt.exists());
}

#[test]
fn safety_dirty_worktree_not_removed_when_a_file_appears_after_planning() {
    let (repo, wts) = repo_with(1);
    let wt = &wts[0];
    let t = target(&repo.f);
    let rig = Rig::new(&repo.f);
    let runner = Counting::new();
    let env = rig.env(&runner, &rig.probe);
    let cs = t.discover(&env, &selection(false, Some(0)));
    let c = cand(&cs, wt);
    assert_eq!(t.guard(c, &env), Guarded::Allow, "plan time");

    fs::write(wt.join("scratch.txt"), "x").unwrap();

    assert_eq!(t.guard(c, &env), Guarded::Skip(SkipReason::Dirty));
    // execute re-checks on its own: a skip in that window is an error row, never a removal
    let err = t.execute(c, &env, &ApplyToken::for_tests()).unwrap_err();
    assert_eq!(err.error, DeleteError::Busy);
    assert!(wt.join("scratch.txt").exists());
    assert_eq!(runner.removes(), 0);
}

#[test]
fn safety_locked_worktree_not_removed_when_locked_after_planning() {
    let (repo, wts) = repo_with(1);
    let wt = &wts[0];
    let t = target(&repo.f);
    let rig = Rig::new(&repo.f);
    let runner = Counting::new();
    let env = rig.env(&runner, &rig.probe);
    let cs = t.discover(&env, &selection(false, Some(0)));
    let c = cand(&cs, wt);
    assert_eq!(t.guard(c, &env), Guarded::Allow);

    git(&repo.r, &["worktree", "lock", wt.to_str().unwrap()], None);

    assert_eq!(t.guard(c, &env), Guarded::Skip(SkipReason::Locked));
    assert!(t.execute(c, &env, &ApplyToken::for_tests()).is_err());
    assert!(wt.exists());
    assert_eq!(runner.removes(), 0);
}

#[test]
fn guard_should_skip_ignored_files_gained_after_planning_unless_discard_was_reviewed() {
    let (repo, wts) = repo_with(1);
    let wt = &wts[0];
    let t = target(&repo.f);
    let rig = Rig::new(&repo.f);
    let runner = Counting::new();
    let env = rig.env(&runner, &rig.probe);
    let cs = t.discover(&env, &selection(false, Some(0)));
    let c = cand(&cs, wt);
    assert_eq!(t.guard(c, &env), Guarded::Allow);

    fs::write(wt.join(".env"), "SECRET=1\n").unwrap();
    repo.age(wt, 3 * DAY_SECS);
    assert_eq!(t.guard(c, &env), Guarded::Skip(SkipReason::HasIgnoredFiles));
    assert!(wt.join(".env").exists());
    assert_eq!(runner.removes(), 0);

    // the opt-in was in the reviewed selection: the .env does not block, and the row says what it discards
    let t = target(&repo.f);
    let cs = t.discover(&env, &selection(true, Some(0)));
    let c = cand(&cs, wt);
    assert_eq!(t.guard(c, &env), Guarded::Allow);
    assert_eq!(t.item_facts(c).discards.map(|d| d.0), Some(1));
    t.execute(c, &env, &ApplyToken::for_tests()).unwrap();
    assert!(!wt.exists());
}

#[test]
fn guard_should_skip_in_use_for_a_live_process_even_with_discard() {
    let (repo, wts) = repo_with(1);
    let wt = &wts[0];
    let t = target(&repo.f);
    let rig = Rig::new(&repo.f);
    let runner = Counting::new();
    let env = rig.env(&runner, &rig.probe);
    let cs = t.discover(&env, &selection(true, Some(0)));
    let c = cand(&cs, wt);
    assert_eq!(t.guard(c, &env), Guarded::Allow);
    let env = rig.env(&runner, &RealProbe);

    let mut child = Command::new("sleep")
        .arg("60")
        .current_dir(wt)
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let g = t.guard(c, &env);
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        matches!(
            g,
            Guarded::Skip(SkipReason::InUse | SkipReason::RunStateUnknown)
        ),
        "{g:?}"
    );
    assert!(wt.exists());
    assert_eq!(runner.removes(), 0);
}

#[test]
fn safety_symlinked_worktree_dir_not_removed_and_target_untouched() {
    let (repo, wts) = repo_with(1);
    let wt = &wts[0];
    let t = target(&repo.f);
    let rig = Rig::new(&repo.f);
    let runner = Counting::new();
    let env = rig.env(&runner, &rig.probe);
    let cs = t.discover(&env, &selection(false, Some(0)));
    let c = cand(&cs, wt);

    assert_eq!(t.guard(c, &env), Guarded::Allow, "plan time");
    let moved = wt.with_file_name("moved-away");
    fs::rename(wt, &moved).unwrap();
    std::os::unix::fs::symlink(&moved, wt).unwrap();

    let g = t.guard(c, &env);
    assert!(
        matches!(
            g,
            Guarded::Skip(SkipReason::Symlink | SkipReason::IdentityChanged)
        ),
        "{g:?}"
    );
    assert!(t.execute(c, &env, &ApplyToken::for_tests()).is_err());
    assert!(
        moved.join("src.txt").exists(),
        "the symlink target is untouched"
    );
    assert_eq!(
        runner.removes(),
        0,
        "no Allowed existed, so nothing was issued"
    );
}

#[test]
fn execute_should_report_gits_own_refusal_for_a_submodule_without_retrying_with_force() {
    let (repo, _) = repo_with(0);
    // a submodule in the main repo, pushed, then a worktree with it initialised
    let sub = repo.f.home.parent().unwrap().join("sub");
    fs::create_dir_all(&sub).unwrap();
    git(&sub, &["init", "-q", "-b", "main"], None);
    fs::write(sub.join("s.txt"), "s\n").unwrap();
    git(&sub, &["add", "."], None);
    git(
        &sub,
        &["commit", "-q", "-m", "s"],
        Some(now_epoch() - 40 * 86_400),
    );
    git(
        &repo.r,
        &["submodule", "add", "-q", sub.to_str().unwrap(), "vendor"],
        None,
    );
    git(
        &repo.r,
        &["commit", "-q", "-m", "sub"],
        Some(now_epoch() - 40 * 86_400),
    );
    git(&repo.r, &["push", "-q", "origin", "main"], None);
    let wt = repo.add("agent-a41");
    git(&wt, &["submodule", "update", "--init", "-q"], None);
    repo.age(&wt, 3 * DAY_SECS);
    // the submodule checkout carries its own .git file and admin dir; age them too
    set_age(
        &repo.r.join(".git/modules"),
        Duration::from_secs(3 * DAY_SECS),
    );

    let t = target(&repo.f);
    let rig = Rig::new(&repo.f);
    let runner = Counting::new();
    let env = rig.env(&runner, &rig.probe);
    let ex = Executor::new(&env, vec![&t as &dyn Target], options());
    let report = ex
        .run(
            &selection(false, Some(0)),
            &Mode::Apply(ApplyToken::for_tests()),
            None,
        )
        .unwrap_or_else(|r| panic!("refused: {}", r.text));

    let o = report
        .summary
        .outcomes
        .iter()
        .find(|o| o.path.as_deref() == Some(wt.as_path()))
        .unwrap_or_else(|| panic!("{:?}", report.summary.outcomes));
    let Status::Error(text) = &o.status else {
        panic!("expected an error row, got {:?}", o.status);
    };
    assert!(text.starts_with("command exited 128: "), "{text}");
    assert!(text.contains("submodule"), "{text}");
    assert_eq!(report.exit.code(), 1);
    assert!(wt.exists());
    assert_eq!(runner.removes(), 1, "one attempt, no retry");
    for argv in runner.all() {
        assert!(
            !argv.iter().any(|a| a == "--force" || a == "-f"),
            "{argv:?}"
        );
    }
}

#[test]
fn execute_should_leave_a_pre_existing_prunable_entry_alone_and_never_prune() {
    let (repo, wts) = repo_with(2);
    // agent-w1's directory is deleted by hand: git lists it as prunable
    fs::remove_dir_all(&wts[1]).unwrap();
    let t = target(&repo.f);
    let rig = Rig::new(&repo.f);
    let runner = Counting::new();
    let env = rig.env(&runner, &rig.probe);
    let ex = Executor::new(&env, vec![&t as &dyn Target], options());

    let report = ex
        .run(
            &selection(false, Some(0)),
            &Mode::Apply(ApplyToken::for_tests()),
            None,
        )
        .unwrap_or_else(|r| panic!("refused: {}", r.text));

    let status = |p: &Path| {
        report
            .summary
            .outcomes
            .iter()
            .find(|o| o.path.as_deref() == Some(p))
            .map(|o| o.status.clone())
    };
    assert_eq!(status(&wts[0]), Some(Status::Removed));
    assert_eq!(status(&wts[1]), Some(Status::Skipped(SkipReason::Gone)));
    assert!(
        repo.admin(&wts[1]).exists(),
        "the stale entry is the owner's to prune"
    );
    assert_eq!(runner.count_word("prune"), 0);
}

#[test]
fn guard_should_not_move_the_admin_entry_across_discover_guard_and_execute_probes() {
    let (repo, wts) = repo_with(1);
    let wt = &wts[0];
    let before = fs::metadata(repo.admin(wt)).unwrap().modified().unwrap();
    let t = target(&repo.f);
    let rig = Rig::new(&repo.f);
    let runner = Counting::new();
    let env = rig.env(&runner, &rig.probe);
    let cs = t.discover(&env, &selection(false, Some(0)));
    let c = cand(&cs, wt);
    for _ in 0..3 {
        assert_eq!(t.guard(c, &env), Guarded::Allow);
        assert_eq!(
            fs::metadata(repo.admin(wt)).unwrap().modified().unwrap(),
            before,
            "git status or list moved the admin entry's mtime"
        );
    }
}

#[test]
fn execute_should_never_run_the_remove_argv_through_run_mutating() {
    let runner = RealCommandRunner::new();
    let cmd = Cmd::new("git", ["worktree", "remove", "/tmp/x"], true);
    assert_eq!(
        runner.run_mutating(&cmd, &ApplyToken::for_tests()),
        Err(DeleteError::Refused)
    );
}
