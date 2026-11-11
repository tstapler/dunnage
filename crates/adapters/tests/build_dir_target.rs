#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    non_snake_case
)]
//! The `build-dirs` target (Story 3.2.4): discovery with keep-newest, the running-build check, the full
//! re-check before a delete, parallel apply and the byte accounting, over real fixtures in tempdirs and the
//! `Executor` with fake ports. Nothing here touches a real repository or build directory.

mod common;
use common::discovery::*;

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use disk_cleanup_adapters::targets::build_dir::{BuildDirTarget, DETAIL_MOUNTS_UNREADABLE};
use disk_cleanup_adapters::testing::FaultDirOps;
use disk_cleanup_adapters::RealDirOps;
use disk_cleanup_core::audit_types::RunRoots;
use disk_cleanup_core::executor::{
    AbortCause, Executor, Mode, RunMeta, RunOptions, RunReport, DEFAULT_JOBS,
};
use disk_cleanup_core::ports::{Env, RunState};
use disk_cleanup_core::selector::{KindSelector, Selection};
use disk_cleanup_core::target::{Guarded, Target};
use disk_cleanup_core::targets::build_dir::{FLOOR_RAISED_NOTE, KEEP_NEWEST_ZERO_WARNING};
use disk_cleanup_core::testing::{
    FakeAuditSink, FakeClock, FakeCommandRunner, FakeFs, FakePlanStore, FakeProbe, FakeRunGuard,
    FakeSpaceMeter,
};
use disk_cleanup_core::{
    ApplyToken, BuildInfo, Candidate, CandidateId, DeleteError, DeleteFailure, Freed, SelectorId,
    SkipReason, Status,
};

const MIB: usize = 1 << 20;

struct Rig {
    fs: FakeFs,
    runner: FakeCommandRunner,
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
        Rig::with_probe(f, FakeProbe::new())
    }

    fn with_probe(f: &Fixture, probe: FakeProbe) -> Rig {
        Rig {
            fs: FakeFs::new(),
            runner: FakeCommandRunner::new(),
            clock: FakeClock::at(now_epoch()),
            probe,
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

    fn env(&self) -> Env<'_> {
        Env {
            fs: &self.fs,
            runner: &self.runner,
            clock: &self.clock,
            probe: &self.probe,
            space: &self.space,
            audit: &self.audit,
            guard: &self.guard,
            plans: &self.plans,
            dirs: dirs_of(&self.home),
        }
    }
}

fn sel() -> Selection {
    Selection::new(
        KindSelector::of(&[SelectorId::BuildDirs]),
        KindSelector::default(),
    )
}

fn sel_keep(n: u32) -> Selection {
    let mut s = sel();
    s.keep_newest = Some(n);
    s
}

fn target(f: &Fixture) -> BuildDirTarget<RealDirOps> {
    BuildDirTarget::new(roots_for(f), RealDirOps::confined_to(&f.home))
}

fn options(jobs: usize) -> RunOptions {
    RunOptions {
        jobs,
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
            stamp: None,
            break_glass: None,
            roots: RunRoots::default(),
            verdict_start: None,
        },
    }
}

/// A `node_modules` of `days` age with a lockfile holding `lock`.
fn stale_node(f: &Fixture, name: &str, lock: Option<&str>, days: u64) -> PathBuf {
    let nm = node_checkout(&f.code.join(name), lock);
    set_age(&nm.join("x"), Duration::from_secs(days * 86_400));
    set_age(&nm, Duration::from_secs(days * 86_400));
    nm
}

fn find<'a>(cs: &'a [Candidate], path: &Path) -> &'a Candidate {
    cs.iter()
        .find(|c| matches!(c.locator(), disk_cleanup_core::Locator::Path(p) if p == path))
        .unwrap_or_else(|| panic!("no candidate {}", path.display()))
}

/// The two-command flow in process: a plan run saves its selection, the apply supplies its id.
fn plan_then_apply(ex: &Executor<'_>, env: &Env, sel: &Selection) -> RunReport {
    let planned = ex
        .run(sel, &Mode::Plan, None)
        .unwrap_or_else(|r| panic!("plan refused: {}", r.text));
    let now = env.clock.now_epoch();
    env.plans.save_merged(
        vec![planned.plan.saved_selection(sel, env.probe.euid(), now)],
        env.probe.euid(),
        now,
    );
    ex.run(
        sel,
        &Mode::Apply(ApplyToken::for_tests()),
        Some(planned.plan_id),
    )
    .unwrap_or_else(|r| panic!("apply refused: {}", r.text))
}

fn status_at(r: &RunReport, p: &Path) -> Status {
    r.summary
        .outcomes
        .iter()
        .find(|o| o.path.as_deref() == Some(p))
        .unwrap_or_else(|| panic!("no outcome for {}", p.display()))
        .status
        .clone()
}

// ------------------------------------------------------------------ keep-newest (Story 3.2.3 wired)

fn three_duplicates(f: &Fixture) -> [PathBuf; 3] {
    [
        stale_node(f, "a", Some("same-lock"), 20),
        stale_node(f, "b", Some("same-lock"), 40),
        stale_node(f, "c", Some("same-lock"), 63),
    ]
}

#[test]
fn discover_should_keep_the_newest_and_offer_the_rest_When_three_duplicates_and_no_keep_newest_flag(
) {
    let f = fixture();
    let [a, b, c] = three_duplicates(&f);
    let t = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();

    let cands = t.discover(&env, &sel());

    assert_eq!(
        t.guard(find(&cands, &a), &env),
        Guarded::Skip(SkipReason::KeptAsNewest)
    );
    assert_eq!(t.guard(find(&cands, &b), &env), Guarded::Allow);
    assert_eq!(t.guard(find(&cands, &c), &env), Guarded::Allow);
    assert_eq!(t.duplicate_set_size(find(&cands, &a)), Some(3));
    assert_eq!(t.duplicate_set_size(find(&cands, &c)), Some(3));
}

#[test]
fn discover_should_offer_every_member_and_warn_When_keep_newest_is_zero() {
    let f = fixture();
    let [a, b, c] = three_duplicates(&f);
    let t = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();
    let s = sel_keep(0);

    let cands = t.discover(&env, &s);

    for p in [&a, &b, &c] {
        assert_eq!(
            t.guard(find(&cands, p), &env),
            Guarded::Allow,
            "{}",
            p.display()
        );
    }
    assert!(t
        .plan_notes(&env, &s)
        .contains(&KEEP_NEWEST_ZERO_WARNING.to_string()));
    assert!(!t
        .plan_notes(&env, &sel_keep(1))
        .contains(&KEEP_NEWEST_ZERO_WARNING.to_string()));
}

#[test]
fn discover_should_keep_two_When_keep_newest_is_two_and_judge_a_dir_without_a_lockfile_by_age_alone(
) {
    let f = fixture();
    let [a, b, c] = three_duplicates(&f);
    let lonely = stale_node(&f, "lonely", None, 90);
    let t = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();

    let cands = t.discover(&env, &sel_keep(2));

    assert_eq!(
        t.guard(find(&cands, &a), &env),
        Guarded::Skip(SkipReason::KeptAsNewest)
    );
    assert_eq!(
        t.guard(find(&cands, &b), &env),
        Guarded::Skip(SkipReason::KeptAsNewest)
    );
    assert_eq!(t.guard(find(&cands, &c), &env), Guarded::Allow);
    // no lockfile: in no cluster (so never KeptAsNewest), and node_modules without one is not reproducible
    assert_eq!(t.duplicate_set_size(find(&cands, &lonely)), None);
    assert_eq!(
        t.guard(find(&cands, &lonely), &env),
        Guarded::Skip(SkipReason::NotReproducible)
    );
    assert_eq!(
        t.detail(find(&cands, &lonely)).as_deref(),
        Some("no lockfile next to node_modules")
    );
}

#[test]
fn plan_notes_should_name_the_floor_When_min_age_is_below_it_and_not_above() {
    let f = fixture();
    let t = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();
    for (secs, noted) in [
        (0, true),
        (300, true),
        (3599, true),
        (3600, false),
        (14 * 86_400, false),
    ] {
        let mut s = sel();
        s.min_age_secs = Some(secs);
        t.discover(&env, &s);
        assert_eq!(
            t.plan_notes(&env, &s)
                .contains(&FLOOR_RAISED_NOTE.to_string()),
            noted,
            "--min-age {secs}s"
        );
    }
    t.discover(&env, &sel());
    assert!(t.plan_notes(&env, &sel()).is_empty());
}

// ------------------------------------------------------------------ running builds

#[test]
fn guard_should_skip_with_running_build_or_unknown_When_the_probe_says_so() {
    let f = fixture();
    let busy = stale_node(&f, "busy", Some("l1"), 30);
    let dark = stale_node(&f, "dark", Some("l2"), 30);
    let idle = stale_node(&f, "idle", Some("l3"), 30);
    let probe = FakeProbe::new()
        .run_state_of(&busy, RunState::Running)
        .run_state_of(&dark, RunState::Unknown);
    let t = target(&f);
    let rig = Rig::with_probe(&f, probe);
    let env = rig.env();

    let cands = t.discover(&env, &sel());

    for (p, want) in [
        (&busy, Guarded::Skip(SkipReason::RunningBuild)),
        (&dark, Guarded::Skip(SkipReason::RunStateUnknown)),
        (&idle, Guarded::Allow),
    ] {
        let c = find(&cands, p);
        assert_eq!(t.guard(c, &env), want, "plan guard {}", p.display());
        assert_eq!(
            t.guard_before_delete(c, &env),
            want,
            "delete guard {}",
            p.display()
        );
    }
}

// ------------------------------------------------------------------ the re-check before the delete

#[test]
fn guard_before_delete_should_skip_too_recent_and_keep_the_tree_When_a_file_is_written_after_planning(
) {
    let f = fixture();
    let nm = stale_node(&f, "app", Some("l"), 30);
    let t = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();
    let cands = t.discover(&env, &sel());
    let c = find(&cands, &nm);
    assert_eq!(t.guard(c, &env), Guarded::Allow);

    fs::write(nm.join("fresh"), "now").unwrap();

    assert_eq!(
        t.guard_before_delete(c, &env),
        Guarded::Skip(SkipReason::TooRecent)
    );
    let err = t.execute(c, &env, &ApplyToken::for_tests()).unwrap_err();
    assert_eq!(err.error, DeleteError::Refused);
    assert!(nm.join("x").exists() && nm.join("fresh").exists());
}

#[test]
fn guard_before_delete_should_skip_symlink_and_leave_the_target_alone_When_the_dir_is_swapped_for_a_link(
) {
    let f = fixture();
    let nm = stale_node(&f, "app", Some("l"), 30);
    let elsewhere = f.home.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::write(elsewhere.join("precious"), "p").unwrap();
    let t = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();
    let cands = t.discover(&env, &sel());
    let c = find(&cands, &nm);

    fs::remove_dir_all(&nm).unwrap();
    symlink(&elsewhere, &nm).unwrap();

    assert_eq!(
        t.guard_before_delete(c, &env),
        Guarded::Skip(SkipReason::Symlink)
    );
    let err = t.execute(c, &env, &ApplyToken::for_tests()).unwrap_err();
    assert_eq!(err.error, DeleteError::Symlink);
    assert!(elsewhere.join("precious").exists());
}

#[test]
fn guards_should_fail_closed_When_the_mount_table_cannot_be_read() {
    let f = fixture();
    let nm = stale_node(&f, "app", Some("l"), 30);
    let t = target(&f);
    let rig = Rig::with_probe(&f, FakeProbe::new().mounts_fail(libc::EIO));
    let env = rig.env();

    let cands = t.discover(&env, &sel());
    let c = find(&cands, &nm);

    assert_eq!(
        t.guard_before_delete(c, &env),
        Guarded::Skip(SkipReason::MountPoint)
    );
    let err = t.execute(c, &env, &ApplyToken::for_tests()).unwrap_err();
    assert_eq!(err.error, DeleteError::ProtectedPath);
    assert!(nm.join("x").exists(), "nothing was removed");
    assert!(t
        .plan_notes(&env, &sel())
        .iter()
        .any(|n| n.contains(DETAIL_MOUNTS_UNREADABLE)));
}

#[test]
fn execute_should_remove_the_tree_by_itself_When_no_guard_ran_first() {
    let f = fixture();
    let nm = stale_node(&f, "app", Some("l"), 30);
    let t = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();
    let cands = t.discover(&env, &sel());

    let freed = t
        .execute(find(&cands, &nm), &env, &ApplyToken::for_tests())
        .unwrap();

    assert!(!nm.exists());
    assert!(
        f.code.join("app/package.json").exists(),
        "only the build dir goes"
    );
    assert!(freed.entries_removed >= 2);
}

#[test]
fn execute_should_succeed_with_nothing_freed_When_the_dir_vanished() {
    let f = fixture();
    let nm = stale_node(&f, "app", Some("l"), 30);
    let t = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();
    let cands = t.discover(&env, &sel());
    fs::remove_dir_all(&nm).unwrap();

    let freed = t
        .execute(find(&cands, &nm), &env, &ApplyToken::for_tests())
        .unwrap();

    assert_eq!(freed.entries_removed, 0);
}

// ------------------------------------------------------------------ parallel apply and accounting

/// Counts the `execute` calls in flight and holds each until `want` of them overlap (or a timeout), so the
/// high-water mark proves both that the pool runs in parallel and that it never exceeds its bound.
struct Counting<'a, T: Target> {
    inner: &'a T,
    want: usize,
    inflight: AtomicUsize,
    high: AtomicUsize,
}

impl<T: Target> Target for Counting<'_, T> {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn selectors(&self) -> &'static [SelectorId] {
        self.inner.selectors()
    }

    fn discover(&self, env: &Env, s: &Selection) -> Vec<Candidate> {
        self.inner.discover(env, s)
    }

    fn guard(&self, c: &Candidate, env: &Env) -> Guarded {
        self.inner.guard(c, env)
    }

    fn guard_before_delete(&self, c: &Candidate, env: &Env) -> Guarded {
        self.inner.guard_before_delete(c, env)
    }

    fn execute(
        &self,
        c: &Candidate,
        env: &Env,
        proof: &ApplyToken,
    ) -> Result<Freed, DeleteFailure> {
        let now = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
        self.high.fetch_max(now, Ordering::SeqCst);
        let t0 = Instant::now();
        while self.inflight.load(Ordering::SeqCst) < self.want
            && t0.elapsed() < Duration::from_secs(3)
        {
            std::thread::sleep(Duration::from_millis(2));
        }
        let r = self.inner.execute(c, env, proof);
        self.inflight.fetch_sub(1, Ordering::SeqCst);
        r
    }
}

#[test]
fn apply_should_run_at_most_four_deletes_at_once_and_remove_all_eight_When_jobs_is_four() {
    let f = fixture();
    let dirs: Vec<PathBuf> = (0..8)
        .map(|i| stale_node(&f, &format!("p{i}"), Some(&format!("lock-{i}")), 30))
        .collect();
    let t = target(&f);
    let counting = Counting {
        inner: &t,
        want: 4,
        inflight: AtomicUsize::new(0),
        high: AtomicUsize::new(0),
    };
    let rig = Rig::new(&f);
    let env = rig.env();
    let ex = Executor::new(&env, vec![&counting as &dyn Target], options(4));

    let report = plan_then_apply(&ex, &env, &sel());

    assert_eq!(counting.high.load(Ordering::SeqCst), 4);
    for d in &dirs {
        assert_eq!(status_at(&report, d), Status::Removed, "{}", d.display());
        assert!(!d.exists());
    }
    assert_eq!(DEFAULT_JOBS, 4);
}

#[test]
fn apply_should_count_a_hardlinked_file_once_When_target_has_debug_deps_foo_linked_to_debug_foo() {
    let f = fixture();
    let proj = f.code.join("crate");
    let deps = proj.join("target/debug/deps");
    fs::create_dir_all(&deps).unwrap();
    fs::write(proj.join("Cargo.toml"), "[package]").unwrap();
    fs::write(deps.join("foo"), vec![9u8; 8 * MIB]).unwrap();
    fs::hard_link(deps.join("foo"), proj.join("target/debug/foo")).unwrap();
    for p in [
        deps.join("foo"),
        deps.clone(),
        proj.join("target/debug"),
        proj.join("target"),
    ] {
        set_age(&p, Duration::from_secs(30 * 86_400));
    }
    let t = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();
    let ex = Executor::new(&env, vec![&t as &dyn Target], options(2));

    let report = plan_then_apply(&ex, &env, &sel());

    let o = report
        .summary
        .outcomes
        .iter()
        .find(|o| o.status == Status::Removed)
        .expect("the target was removed");
    let got = o.measured_bytes.unwrap();
    assert!(
        got >= (8 * MIB) as u64 && got < (8 * MIB + MIB / 2) as u64,
        "measured {got}, one copy of 8 MiB plus directory blocks"
    );
}

#[test]
fn apply_should_stop_picking_up_dirs_and_report_the_abort_When_enospc_hits_the_third_of_eight() {
    let f = fixture();
    let dirs: Vec<PathBuf> = (0..8)
        .map(|i| stale_node(&f, &format!("p{i}"), Some(&format!("lock-{i}")), 30))
        .collect();
    // Each dir holds `x` and the directory itself: two unlinks, so the fifth is the file of dir 3.
    let ops = FaultDirOps::wrap(RealDirOps::confined_to(&f.home))
        .enospc_on_nth_unlinkat(5)
        .build();
    let t = BuildDirTarget::new(roots_for(&f), ops);
    let rig = Rig::new(&f);
    let env = rig.env();
    let ex = Executor::new(&env, vec![&t as &dyn Target], options(1));

    let report = plan_then_apply(&ex, &env, &sel());

    assert_eq!(report.abort, Some(AbortCause::NoSpace));
    assert_eq!(AbortCause::NoSpace.note(), "aborted: ENOSPC");
    let removed = dirs.iter().filter(|d| !d.exists()).count();
    assert_eq!(removed, 2, "{:?}", report.summary.outcomes);
    let aborted = report
        .summary
        .outcomes
        .iter()
        .filter(|o| o.status == Status::Skipped(SkipReason::Aborted))
        .count();
    assert_eq!(aborted, 5);
}

#[test]
fn candidate_ids_should_differ_per_path_When_two_dirs_share_a_lockfile() {
    // The executor and the plan-id key items by id; two duplicates must not collapse into one.
    let f = fixture();
    let [a, b, _] = three_duplicates(&f);
    let t = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();
    let cands = t.discover(&env, &sel());
    let ids: std::collections::HashSet<CandidateId> = cands.iter().map(Candidate::id).collect();
    assert_eq!(ids.len(), 3);
    assert_ne!(find(&cands, &a).id(), find(&cands, &b).id());
}
