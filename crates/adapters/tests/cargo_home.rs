#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    non_snake_case
)]
//! The `cargo-home` kind: unpacked crates (`registry/src/<index>/<crate>-<version>`) and git checkouts
//! (`git/checkouts/<repo>/<rev>`) below `CARGO_HOME` that nobody has touched for the age floor. Real fixtures
//! in tempdirs below a fake `$HOME`, fake ports; nothing here touches the real `~/.cargo`.

mod common;
use common::discovery::*;

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::Duration;

use dunnage_adapters::targets::cargo_home::CargoHomeTarget;
use dunnage_adapters::RealDirOps;
use dunnage_core::audit_types::RunRoots;
use dunnage_core::executor::{Executor, Mode, RunMeta, RunOptions, RunReport};
use dunnage_core::plan::Planned;
use dunnage_core::ports::{Env, RunState};
use dunnage_core::roots::RootSet;
use dunnage_core::selector::{KindSelector, Selection};
use dunnage_core::target::{Guarded, Target};
use dunnage_core::testing::{
    FakeAuditSink, FakeClock, FakeCommandRunner, FakeFs, FakePlanStore, FakeProbe, FakeRunGuard,
    FakeSpaceMeter,
};
use dunnage_core::{
    ApplyToken, BuildInfo, Candidate, CargoHomeArea, DeleteError, Kind, KindFacts, Locator,
    SelectorId, SkipReason, Status,
};

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
        KindSelector::of(&[SelectorId::CargoHome]),
        KindSelector::default(),
    )
}

fn target_with(f: &Fixture, roots: RootSet) -> CargoHomeTarget<RealDirOps> {
    CargoHomeTarget::new(roots, RealDirOps::confined_to(&f.home))
}

fn target(f: &Fixture) -> CargoHomeTarget<RealDirOps> {
    target_with(f, roots_for(f))
}

fn options() -> RunOptions {
    RunOptions {
        jobs: 2,
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

fn days(n: u64) -> Duration {
    Duration::from_hours(n * 24)
}

fn age_tree(dir: &Path, age: Duration) {
    for e in fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if e.file_type().unwrap().is_dir() {
            age_tree(&p, age);
        } else if e.file_type().unwrap().is_file() {
            set_age(&p, age);
        }
    }
    set_age(dir, age);
}

const INDEX: &str = "index.crates.io-6f17d22bba15001f";

/// `<ch>/registry/src/<INDEX>/<name>` with a manifest and a source file, `age` old.
fn crate_dir(ch: &Path, name: &str, age: Duration) -> PathBuf {
    let d = ch.join("registry/src").join(INDEX).join(name);
    fs::create_dir_all(d.join("src")).unwrap();
    fs::write(d.join("Cargo.toml"), "[package]\n").unwrap();
    fs::write(d.join("src/lib.rs"), vec![b'x'; 32 * 1024]).unwrap();
    age_tree(&d, age);
    d
}

fn checkout(ch: &Path, repo: &str, rev: &str, age: Duration) -> PathBuf {
    let d = ch.join("git/checkouts").join(repo).join(rev);
    fs::create_dir_all(d.join("src")).unwrap();
    fs::write(d.join("src/lib.rs"), vec![b'y'; 32 * 1024]).unwrap();
    age_tree(&d, age);
    d
}

/// The parts of CARGO_HOME the kind must never touch, with an old mtime so that only the rules protect them.
fn neighbours(ch: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    for rel in [
        "registry/cache/index.crates.io-6f17d22bba15001f/serde-1.0.200.crate",
        "registry/index/index.crates.io-6f17d22bba15001f/.cache/se/rd/serde",
        "git/db/repo-abc/HEAD",
        "bin/cargo",
        "credentials.toml",
        "config.toml",
    ] {
        let p = ch.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, "keep").unwrap();
        v.push(p);
    }
    for d in ["registry", "git", "bin"] {
        age_tree(&ch.join(d), days(90));
    }
    v
}

fn paths_of(cands: &[Candidate]) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = cands
        .iter()
        .filter_map(|c| match c.locator() {
            Locator::Path(p) => Some(p.clone()),
            _ => None,
        })
        .collect();
    v.sort();
    v
}

fn find<'a>(cands: &'a [Candidate], p: &Path) -> &'a Candidate {
    cands
        .iter()
        .find(|c| matches!(c.locator(), Locator::Path(q) if q == p))
        .unwrap_or_else(|| panic!("no candidate {}", p.display()))
}

#[test]
fn discover_should_list_old_crates_and_checkouts_and_nothing_else_below_cargo_home() {
    let f = fixture();
    let ch = f.home.join(".cargo");
    let neigh = neighbours(&ch);
    let old_crate = crate_dir(&ch, "serde-1.0.200", days(60));
    let young_crate = crate_dir(&ch, "tokio-1.40.0", days(2));
    let old_git = checkout(&ch, "repo-1a2b3c", "abcdef1", days(60));
    let hidden = ch.join("registry/src").join(INDEX).join(".cargo-ok");
    fs::write(&hidden, "").unwrap();
    let t = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();

    let cands = t.discover(&env, &sel());

    let mut want = vec![old_crate.clone(), old_git.clone()];
    want.sort();
    assert_eq!(paths_of(&cands), want, "young {}", young_crate.display());
    let c = find(&cands, &old_crate);
    assert_eq!(c.kind(), Kind::CargoHome);
    assert_eq!(c.selector(), SelectorId::CargoHome);
    assert!(c.est_bytes().0 >= 32 * 1024);
    match c.facts() {
        KindFacts::CargoHome(h) => {
            assert_eq!(h.area, CargoHomeArea::RegistrySrc);
            assert_eq!(h.source, INDEX);
            assert!(h.age_secs >= 59 * 86_400);
        }
        other => panic!("{other:?}"),
    }
    match find(&cands, &old_git).facts() {
        KindFacts::CargoHome(h) => {
            assert_eq!(h.area, CargoHomeArea::GitCheckout);
            assert_eq!(h.source, "repo-1a2b3c");
        }
        other => panic!("{other:?}"),
    }
    for p in neigh {
        assert!(p.exists(), "{}", p.display());
    }
    assert!(hidden.exists());
}

#[test]
fn discover_should_follow_min_age_and_never_go_below_one_day() {
    let f = fixture();
    let ch = f.home.join(".cargo");
    let a = crate_dir(&ch, "a-1.0.0", days(10));
    let hours = crate_dir(&ch, "b-1.0.0", Duration::from_hours(3));
    let t = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();
    let mut s = sel();

    // default 30 days: neither
    assert!(paths_of(&t.discover(&env, &s)).is_empty());
    s.min_age_secs = Some(7 * 86_400);
    assert_eq!(paths_of(&t.discover(&env, &s)), vec![a.clone()]);
    s.min_age_secs = Some(0);
    assert_eq!(
        paths_of(&t.discover(&env, &s)),
        vec![a],
        "3h old is under the 24h floor"
    );
    assert!(hours.exists());
}

#[test]
fn discover_should_follow_cargo_home_when_it_is_set_below_home() {
    let f = fixture();
    let moved = f.home.join("dev/cargo");
    let in_moved = crate_dir(&moved, "serde-1.0.200", days(60));
    let in_default = crate_dir(&f.home.join(".cargo"), "serde-1.0.200", days(60));
    let roots = roots_for(&f).with_cargo_home(Some(&moved));
    let t = target_with(&f, roots);
    let rig = Rig::new(&f);
    let env = rig.env();

    let cands = t.discover(&env, &sel());

    assert_eq!(paths_of(&cands), vec![in_moved]);
    assert!(in_default.exists());
}

#[test]
fn discover_should_list_nothing_and_say_why_When_cargo_home_is_outside_home() {
    let f = fixture();
    crate_dir(&f.home.join(".cargo"), "serde-1.0.200", days(60));
    let roots = roots_for(&f).with_cargo_home(Some(Path::new("/opt/cargo")));
    let t = target_with(&f, roots);
    let rig = Rig::new(&f);
    let env = rig.env();

    assert!(t.discover(&env, &sel()).is_empty());
    let notes = t.plan_notes(&env, &sel());
    assert!(notes.iter().any(|n| n.contains("CARGO_HOME")), "{notes:?}");
}

#[test]
fn discover_should_not_follow_symlinks_at_any_level() {
    let f = fixture();
    let real = f.home.join("elsewhere");
    let ch_real = real.join("cargo-real");
    let victim = crate_dir(&ch_real, "serde-1.0.200", days(60));
    // ~/.cargo is a link
    symlink(&ch_real, f.home.join(".cargo")).unwrap();
    let t = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();
    assert!(t.discover(&env, &sel()).is_empty(), "followed ~/.cargo");
    assert!(victim.exists());
    fs::remove_file(f.home.join(".cargo")).unwrap();

    // an index dir and an entry are links
    let ch = f.home.join(".cargo");
    let src = ch.join("registry/src");
    fs::create_dir_all(&src).unwrap();
    symlink(
        ch_real.join("registry/src").join(INDEX),
        src.join("linked-index"),
    )
    .unwrap();
    let idx = src.join(INDEX);
    fs::create_dir_all(&idx).unwrap();
    symlink(&victim, idx.join("linked-crate-1.0.0")).unwrap();
    let cands = t.discover(&env, &sel());
    assert!(paths_of(&cands).is_empty(), "{:?}", paths_of(&cands));
    assert!(victim.join("Cargo.toml").exists());
}

#[test]
fn safety_cargo_home_running_build_never_deleted() {
    for state in [RunState::Running, RunState::Unknown] {
        let f = fixture();
        let ch = f.home.join(".cargo");
        let busy = crate_dir(&ch, "busy-1.0.0", days(60));
        let probe = FakeProbe::new().run_state_of(&busy, state);
        let t = target(&f);
        let rig = Rig::with_probe(&f, probe);
        let env = rig.env();
        let cands = t.discover(&env, &sel());
        let c = find(&cands, &busy);
        let want = Guarded::Skip(match state {
            RunState::Running => SkipReason::RunningBuild,
            _ => SkipReason::RunStateUnknown,
        });

        assert_eq!(t.guard(c, &env), want);
        assert_eq!(t.guard_before_delete(c, &env), want);
        let err = t.execute(c, &env, &ApplyToken::for_tests()).unwrap_err();
        assert_eq!(err.error, DeleteError::Refused);
        assert!(busy.join("Cargo.toml").exists());
    }
}

#[test]
fn guard_before_delete_should_skip_too_recent_When_a_file_is_written_after_planning() {
    let f = fixture();
    let ch = f.home.join(".cargo");
    let d = crate_dir(&ch, "serde-1.0.200", days(60));
    let t = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();
    let cands = t.discover(&env, &sel());
    let c = find(&cands, &d);
    assert_eq!(t.guard(c, &env), Guarded::Allow);

    fs::write(d.join("src/new.rs"), "now").unwrap();

    assert_eq!(
        t.guard_before_delete(c, &env),
        Guarded::Skip(SkipReason::TooRecent)
    );
    assert!(t.execute(c, &env, &ApplyToken::for_tests()).is_err());
    assert!(d.join("src/new.rs").exists());
}

#[test]
fn guard_before_delete_should_skip_with_identity_changed_When_the_entry_was_recreated() {
    let f = fixture();
    let ch = f.home.join(".cargo");
    let d = crate_dir(&ch, "serde-1.0.200", days(60));
    let t = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();
    let cands = t.discover(&env, &sel());
    let c = find(&cands, &d);

    fs::remove_dir_all(&d).unwrap();
    let again = crate_dir(&ch, "serde-1.0.200", days(60));
    assert_eq!(again, d);

    assert_eq!(
        t.guard_before_delete(c, &env),
        Guarded::Skip(SkipReason::IdentityChanged)
    );
    assert_eq!(
        t.execute(c, &env, &ApplyToken::for_tests())
            .unwrap_err()
            .error,
        DeleteError::IdentityMismatch
    );
    assert!(d.join("Cargo.toml").exists());
}

#[test]
fn safety_cargo_home_symlink_swap_leaves_target_alone() {
    let f = fixture();
    let ch = f.home.join(".cargo");
    let d = crate_dir(&ch, "serde-1.0.200", days(60));
    let elsewhere = f.home.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::write(elsewhere.join("precious"), "p").unwrap();
    let t = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();
    let cands = t.discover(&env, &sel());
    let c = find(&cands, &d);

    fs::remove_dir_all(&d).unwrap();
    symlink(&elsewhere, &d).unwrap();

    assert_eq!(
        t.guard_before_delete(c, &env),
        Guarded::Skip(SkipReason::Symlink)
    );
    assert_eq!(
        t.execute(c, &env, &ApplyToken::for_tests())
            .unwrap_err()
            .error,
        DeleteError::Symlink
    );
    assert!(elsewhere.join("precious").exists());

    // the index directory swapped for a link
    let ch2 = f.home.join("other-ch");
    let d2 = crate_dir(&ch2, "x-1.0.0", days(60));
    let _ = d2;
    let idx = ch.join("registry/src").join(INDEX);
    fs::remove_file(&d).unwrap();
    fs::remove_dir(&idx).unwrap();
    symlink(ch2.join("registry/src").join(INDEX), &idx).unwrap();
    let t2 = target(&f);
    let c2 = Candidate::new(
        Locator::Path(idx.join("x-1.0.0")),
        c.facts().clone(),
        c.est_bytes(),
        c.newest_mtime(),
        dunnage_core::Identity::of(
            &fs::metadata(ch2.join("registry/src").join(INDEX).join("x-1.0.0")).unwrap(),
        ),
    );
    let _ = t2.discover(&env, &sel());
    assert!(t2.execute(&c2, &env, &ApplyToken::for_tests()).is_err());
    assert!(ch2
        .join("registry/src")
        .join(INDEX)
        .join("x-1.0.0/Cargo.toml")
        .exists());
}

#[test]
fn execute_should_succeed_with_nothing_freed_When_the_entry_vanished() {
    let f = fixture();
    let ch = f.home.join(".cargo");
    let d = crate_dir(&ch, "serde-1.0.200", days(60));
    let t = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();
    let cands = t.discover(&env, &sel());
    fs::remove_dir_all(&d).unwrap();

    let freed = t
        .execute(find(&cands, &d), &env, &ApplyToken::for_tests())
        .unwrap();

    assert_eq!(freed.entries_removed, 0);
}

#[test]
fn apply_should_remove_only_the_old_entries_and_log_the_kind() {
    let f = fixture();
    let ch = f.home.join(".cargo");
    let neigh = neighbours(&ch);
    let old_crate = crate_dir(&ch, "serde-1.0.200", days(60));
    let old_git = checkout(&ch, "repo-1a2b3c", "abcdef1", days(60));
    let young = crate_dir(&ch, "tokio-1.40.0", days(2));
    let t = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();
    let ex = Executor::new(&env, vec![&t as &dyn Target], options());

    let report = plan_then_apply(&ex, &env, &sel());

    assert!(!old_crate.exists() && !old_git.exists());
    assert!(young.join("Cargo.toml").exists());
    for p in neigh {
        assert!(p.exists(), "{}", p.display());
    }
    // the index and repo directories stay, only their children go
    assert!(ch.join("registry/src").join(INDEX).is_dir());
    assert!(ch.join("git/checkouts/repo-1a2b3c").is_dir());
    let removed: Vec<_> = report
        .summary
        .outcomes
        .iter()
        .filter(|o| o.status == Status::Removed)
        .collect();
    assert_eq!(removed.len(), 2, "{:?}", report.summary.outcomes);
    assert!(removed.iter().all(|o| o.kind == Kind::CargoHome));
    assert!(removed
        .iter()
        .all(|o| o.measured_bytes.unwrap() >= 32 * 1024));
    let logged: Vec<String> = rig
        .audit
        .records()
        .iter()
        .filter_map(|r| serde_json::to_value(r).ok())
        .filter_map(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_string))
        .collect();
    assert!(logged.iter().any(|k| k == "cargo-home"), "{logged:?}");
}

#[test]
fn a_dry_run_should_plan_every_old_entry_and_delete_nothing() {
    let f = fixture();
    let ch = f.home.join(".cargo");
    let d = crate_dir(&ch, "serde-1.0.200", days(60));
    let t = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();
    let ex = Executor::new(&env, vec![&t as &dyn Target], options());

    let planned = ex.run(&sel(), &Mode::Plan, None).unwrap();

    assert_eq!(planned.plan.items.len(), 1);
    assert_eq!(planned.plan.items[0].planned, Planned::Will);
    assert!(d.join("Cargo.toml").exists());
}

#[test]
fn cargo_home_should_not_need_a_plan_id() {
    assert!(!sel().needs_plan_id());
}
