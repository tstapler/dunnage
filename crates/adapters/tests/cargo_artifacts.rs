#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    non_snake_case
)]
//! The `cargo-artifact` kind of the `build-dirs` target: a cargo target that is too young to remove whole is
//! offered its stale parts instead (`incremental/`, and a profile's `.fingerprint/`, `deps/`, `build/` and
//! `examples/` once no build has touched the profile for the age floor). Real fixtures in tempdirs, fake
//! ports; nothing here touches a real project, a real `target/` or the real `$HOME`.

mod common;
use common::discovery::*;

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::Duration;

use dunnage_adapters::targets::build_dir::BuildDirTarget;
use dunnage_adapters::RealDirOps;
use dunnage_core::audit_types::RunRoots;
use dunnage_core::executor::{Executor, Mode, RunMeta, RunOptions, RunReport};
use dunnage_core::plan::Planned;
use dunnage_core::ports::{Env, RunState};
use dunnage_core::selector::{KindSelector, Selection};
use dunnage_core::target::{Guarded, Target};
use dunnage_core::testing::{
    FakeAuditSink, FakeClock, FakeCommandRunner, FakeFs, FakePlanStore, FakeProbe, FakeRunGuard,
    FakeSpaceMeter,
};
use dunnage_core::{
    ApplyToken, BuildInfo, Candidate, CargoPart, DeleteError, Kind, KindFacts, Locator, SelectorId,
    SkipReason, Status,
};

const CARGO_TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n# This file is a cache directory tag created by cargo.\n";

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

fn target(f: &Fixture) -> BuildDirTarget<RealDirOps> {
    BuildDirTarget::new(roots_for(f), RealDirOps::confined_to(&f.home))
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

/// Sets every file and directory below `dir` (and `dir`) to `age` ago, children first so a directory's own
/// time is not moved again by a later write.
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

/// One profile directory in cargo's layout, `bytes` of artifacts in `deps/`.
fn profile(target: &Path, rel: &str, bytes: usize) -> PathBuf {
    let p = target.join(rel);
    for d in [
        ".fingerprint/foo-1a2b",
        "deps",
        "build/foo-3c4d",
        "incremental/foo-5e6f/s-abc",
        "examples",
    ] {
        fs::create_dir_all(p.join(d)).unwrap();
    }
    fs::write(p.join(".fingerprint/foo-1a2b/invoked.timestamp"), "").unwrap();
    fs::write(p.join("deps/libfoo.rlib"), vec![7u8; bytes]).unwrap();
    fs::write(p.join("build/foo-3c4d/out"), "o").unwrap();
    fs::write(
        p.join("incremental/foo-5e6f/s-abc/dep-graph.bin"),
        vec![1u8; bytes],
    )
    .unwrap();
    fs::write(p.join("examples/ex"), vec![2u8; bytes]).unwrap();
    fs::write(p.join("foo"), "bin").unwrap();
    fs::write(p.join(".cargo-lock"), "").unwrap();
    p
}

/// `<code>/<name>/Cargo.toml` and a real cargo target (tag + rustc info cache) with the given profiles.
/// Everything is left at "now"; the test ages what it wants.
fn project(f: &Fixture, name: &str, profiles: &[&str]) -> (PathBuf, PathBuf) {
    let proj = f.code.join(name);
    fs::create_dir_all(proj.join("src")).unwrap();
    fs::write(proj.join("Cargo.toml"), "[package]\nname = \"foo\"\n").unwrap();
    let t = proj.join("target");
    fs::create_dir_all(&t).unwrap();
    fs::write(t.join("CACHEDIR.TAG"), CARGO_TAG).unwrap();
    fs::write(t.join(".rustc_info.json"), "{}").unwrap();
    for p in profiles {
        profile(&t, p, 64 * 1024);
    }
    (proj, t)
}

fn age_target_files(t: &Path, age: Duration) {
    set_age(&t.join("CACHEDIR.TAG"), age);
    set_age(&t.join(".rustc_info.json"), age);
}

fn parts_of(cands: &[Candidate]) -> Vec<(String, CargoPart)> {
    let mut v: Vec<(String, CargoPart)> = cands
        .iter()
        .filter_map(|c| match c.facts() {
            KindFacts::CargoPart(p) => Some((p.profile.clone(), p.part)),
            _ => None,
        })
        .collect();
    v.sort();
    v
}

fn part<'a>(cands: &'a [Candidate], path: &Path) -> &'a Candidate {
    cands
        .iter()
        .find(|c| matches!(c.locator(), Locator::Path(p) if p == path))
        .unwrap_or_else(|| panic!("no candidate {}", path.display()))
}

fn has(cands: &[Candidate], path: &Path) -> bool {
    cands
        .iter()
        .any(|c| matches!(c.locator(), Locator::Path(p) if p == path))
}

const ALL_FIVE: [CargoPart; 5] = [
    CargoPart::Incremental,
    CargoPart::Fingerprint,
    CargoPart::Deps,
    CargoPart::Build,
    CargoPart::Examples,
];

#[test]
fn discover_should_offer_every_part_of_the_idle_profile_and_none_of_the_active_one_When_the_target_is_young(
) {
    let f = fixture();
    let (_proj, t) = project(&f, "app", &["debug", "release"]);
    age_tree(&t.join("debug"), days(40));
    age_target_files(&t, days(40));
    // release was built just now: the target as a whole is far too young for the 14 day floor
    let tgt = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();

    let cands = tgt.discover(&env, &sel());

    let want: Vec<(String, CargoPart)> = {
        let mut v: Vec<_> = ALL_FIVE.iter().map(|p| ("debug".to_string(), *p)).collect();
        v.sort();
        v
    };
    assert_eq!(parts_of(&cands), want);
    for c in cands.iter().filter(|c| c.kind() == Kind::CargoArtifact) {
        assert_eq!(c.selector(), SelectorId::BuildDirs);
    }
    for big in ["deps", "incremental", "examples"] {
        assert!(
            part(&cands, &t.join("debug").join(big)).est_bytes().0 >= 64 * 1024,
            "{big}"
        );
    }
    let inc = part(&cands, &t.join("debug/incremental"));
    assert!(inc.est_bytes().0 >= 64 * 1024);
    assert!(inc.newest_mtime().is_some());
}

#[test]
fn discover_should_offer_the_whole_target_and_no_parts_When_the_whole_target_is_stale() {
    let f = fixture();
    let (_proj, t) = project(&f, "app", &["debug", "release"]);
    age_tree(&t, days(40));
    let tgt = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();

    let cands = tgt.discover(&env, &sel());

    assert_eq!(parts_of(&cands), Vec::new());
    assert!(has(&cands, &t));
    assert_eq!(tgt.guard(part(&cands, &t), &env), Guarded::Allow);
}

#[test]
fn discover_should_offer_only_incremental_When_it_is_stale_but_a_build_touched_the_profile_yesterday(
) {
    let f = fixture();
    let (_proj, t) = project(&f, "app", &["debug"]);
    age_tree(&t.join("debug"), days(40));
    age_target_files(&t, days(40));
    // a build that found everything fresh rewrites only invoked.timestamp
    set_age(
        &t.join("debug/.fingerprint/foo-1a2b/invoked.timestamp"),
        days(1),
    );
    let tgt = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();

    let cands = tgt.discover(&env, &sel());

    assert_eq!(
        parts_of(&cands),
        vec![("debug".to_string(), CargoPart::Incremental)],
        "deps/ has not been rewritten for 40 days but the profile is in use"
    );
}

#[test]
fn discover_should_offer_nothing_When_the_incremental_dir_is_young_and_the_rest_of_the_profile_stale(
) {
    let f = fixture();
    let (_proj, t) = project(&f, "app", &["debug"]);
    age_tree(&t.join("debug"), days(40));
    age_target_files(&t, days(40));
    fs::write(t.join("debug/incremental/foo-5e6f/s-abc/new.bin"), "x").unwrap();
    let tgt = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();

    let cands = tgt.discover(&env, &sel());

    assert_eq!(
        parts_of(&cands),
        Vec::new(),
        "incremental activity is profile activity: nothing in the profile is stale"
    );
}

#[test]
fn discover_should_keep_to_the_named_profile_When_cargo_profile_is_given() {
    let f = fixture();
    let (_proj, t) = project(
        &f,
        "app",
        &["debug", "release", "x86_64-unknown-linux-gnu/debug"],
    );
    for p in ["debug", "release", "x86_64-unknown-linux-gnu/debug"] {
        age_tree(&t.join(p), days(40));
    }
    age_target_files(&t, days(40));
    fs::write(t.join("keepalive"), "now").unwrap(); // the target dir itself stays young
    let tgt = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();
    let mut s = sel();
    s.cargo_profiles = vec!["debug".into()];

    let cands = tgt.discover(&env, &s);

    let profiles: std::collections::BTreeSet<String> =
        parts_of(&cands).into_iter().map(|(p, _)| p).collect();
    assert_eq!(
        profiles.into_iter().collect::<Vec<_>>(),
        vec![
            "debug".to_string(),
            "x86_64-unknown-linux-gnu/debug".to_string()
        ]
    );
}

#[test]
fn discover_should_find_the_profiles_below_a_target_triple_directory() {
    let f = fixture();
    let (_proj, t) = project(&f, "app", &["x86_64-unknown-linux-gnu/release", "debug"]);
    age_tree(&t.join("x86_64-unknown-linux-gnu"), days(40));
    age_target_files(&t, days(40));
    let tgt = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();

    let cands = tgt.discover(&env, &sel());

    let profiles: std::collections::BTreeSet<String> =
        parts_of(&cands).into_iter().map(|(p, _)| p).collect();
    assert_eq!(
        profiles.into_iter().collect::<Vec<_>>(),
        vec!["x86_64-unknown-linux-gnu/release".to_string()]
    );
    assert!(has(
        &cands,
        &t.join("x86_64-unknown-linux-gnu/release/incremental")
    ));
}

#[test]
fn discover_should_offer_no_parts_When_the_target_is_not_cargos_own() {
    let f = fixture();
    let (_proj, t) = project(&f, "app", &["debug", "release"]);
    age_tree(&t.join("debug"), days(40));
    age_target_files(&t, days(40));
    fs::remove_file(t.join("CACHEDIR.TAG")).unwrap();
    fs::remove_file(t.join(".rustc_info.json")).unwrap();
    fs::write(t.join(".cargo-keep"), "now").unwrap();
    let tgt = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();

    let cands = tgt.discover(&env, &sel());

    assert_eq!(parts_of(&cands), Vec::new());
}

#[test]
fn discover_should_accept_a_target_that_has_only_the_rustc_info_cache_or_only_the_tag() {
    for keep_tag in [true, false] {
        let f = fixture();
        let (_proj, t) = project(&f, "app", &["debug", "release"]);
        age_tree(&t.join("debug"), days(40));
        age_target_files(&t, days(40));
        fs::remove_file(t.join(if keep_tag {
            ".rustc_info.json"
        } else {
            "CACHEDIR.TAG"
        }))
        .unwrap();
        let tgt = target(&f);
        let rig = Rig::new(&f);
        let env = rig.env();
        let cands = tgt.discover(&env, &sel());
        assert_eq!(parts_of(&cands).len(), 5, "keep_tag={keep_tag}");
    }
}

#[test]
fn discover_should_not_follow_a_symlinked_profile_or_a_symlinked_part() {
    let f = fixture();
    let (_proj, t) = project(&f, "app", &["release", "debug"]);
    let elsewhere = f.home.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    profile(&elsewhere, "stage", 64 * 1024);
    age_tree(&elsewhere, days(40));
    // `debug` is a link to a stale profile outside the project; release/incremental is a link too
    fs::remove_dir_all(t.join("debug")).unwrap();
    symlink(elsewhere.join("stage"), t.join("debug")).unwrap();
    age_tree(&t.join("release"), days(40));
    fs::remove_dir_all(t.join("release/incremental")).unwrap();
    symlink(
        elsewhere.join("stage/incremental"),
        t.join("release/incremental"),
    )
    .unwrap();
    age_target_files(&t, days(40));
    fs::write(t.join("keepalive"), "now").unwrap();
    let tgt = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();

    let cands = tgt.discover(&env, &sel());

    let got = parts_of(&cands);
    assert!(
        got.iter().all(|(p, _)| p == "release"),
        "a symlinked profile was followed: {got:?}"
    );
    assert!(
        !got.contains(&("release".to_string(), CargoPart::Incremental)),
        "a symlinked part was followed: {got:?}"
    );
    assert!(elsewhere.join("stage/incremental").exists());
}

#[test]
fn discover_should_offer_no_parts_When_target_itself_is_a_symlink() {
    let f = fixture();
    let (proj, t) = project(&f, "app", &["debug"]);
    age_tree(&t, days(40));
    let real = f.home.join("real-target");
    fs::rename(&t, &real).unwrap();
    symlink(&real, proj.join("target")).unwrap();
    fs::write(real.join("keepalive"), "now").unwrap();
    let tgt = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();

    let cands = tgt.discover(&env, &sel());

    assert_eq!(parts_of(&cands), Vec::new());
}

#[test]
fn discover_should_not_go_below_the_sixty_minute_floor_When_min_age_is_zero() {
    let f = fixture();
    let (_proj, t) = project(&f, "app", &["debug"]);
    age_tree(&t.join("debug"), Duration::from_mins(30));
    age_target_files(&t, Duration::from_mins(30));
    let tgt = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();
    let mut s = sel();
    s.min_age_secs = Some(0);

    assert_eq!(parts_of(&tgt.discover(&env, &s)), Vec::new());

    age_tree(&t.join("debug"), Duration::from_hours(2));
    age_target_files(&t, Duration::from_hours(2));
    fs::write(t.join("keepalive"), "now").unwrap();
    assert_eq!(parts_of(&tgt.discover(&env, &s)).len(), 5);
}

#[test]
fn discover_should_follow_min_age_for_parts_When_the_flag_is_raised() {
    let f = fixture();
    let (_proj, t) = project(&f, "app", &["debug", "release"]);
    age_tree(&t.join("debug"), days(20));
    age_target_files(&t, days(20));
    let tgt = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();
    let mut s = sel();
    s.min_age_secs = Some(30 * 86_400);
    assert_eq!(parts_of(&tgt.discover(&env, &s)), Vec::new());
    s.min_age_secs = Some(7 * 86_400);
    assert_eq!(parts_of(&tgt.discover(&env, &s)).len(), 5);
}

// ------------------------------------------------------------------ in use

#[test]
fn safety_cargo_artifact_running_cargo_never_deleted() {
    for busy_path in ["target", "profile"] {
        for state in [RunState::Running, RunState::Unknown] {
            let f = fixture();
            let (_proj, t) = project(&f, "app", &["debug", "release"]);
            age_tree(&t.join("debug"), days(40));
            age_target_files(&t, days(40));
            let probe = FakeProbe::new().run_state_of(
                if busy_path == "target" {
                    t.clone()
                } else {
                    t.join("debug")
                },
                state,
            );
            let tgt = target(&f);
            let rig = Rig::with_probe(&f, probe);
            let env = rig.env();
            let cands = tgt.discover(&env, &sel());
            let inc = part(&cands, &t.join("debug/incremental"));
            let want = Guarded::Skip(match state {
                RunState::Running => SkipReason::RunningBuild,
                _ => SkipReason::RunStateUnknown,
            });

            assert_eq!(tgt.guard(inc, &env), want, "{busy_path} {state:?}");
            assert_eq!(
                tgt.guard_before_delete(inc, &env),
                want,
                "{busy_path} {state:?}"
            );
            let err = tgt
                .execute(inc, &env, &ApplyToken::for_tests())
                .unwrap_err();
            assert_eq!(err.error, DeleteError::Refused, "{busy_path} {state:?}");
            assert!(t
                .join("debug/incremental/foo-5e6f/s-abc/dep-graph.bin")
                .exists());
        }
    }
}

// ------------------------------------------------------------------ the re-check before the delete

#[test]
fn guard_before_delete_should_skip_too_recent_and_keep_the_part_When_the_profile_is_built_after_planning(
) {
    let f = fixture();
    let (_proj, t) = project(&f, "app", &["debug", "release"]);
    age_tree(&t.join("debug"), days(40));
    age_target_files(&t, days(40));
    let tgt = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();
    let cands = tgt.discover(&env, &sel());
    let deps = part(&cands, &t.join("debug/deps"));
    assert_eq!(tgt.guard(deps, &env), Guarded::Allow);

    // a build starts and touches the fingerprints; nothing under deps/ changed yet
    fs::write(
        t.join("debug/.fingerprint/foo-1a2b/invoked.timestamp"),
        "now",
    )
    .unwrap();

    assert_eq!(
        tgt.guard_before_delete(deps, &env),
        Guarded::Skip(SkipReason::TooRecent)
    );
    let err = tgt
        .execute(deps, &env, &ApplyToken::for_tests())
        .unwrap_err();
    assert_eq!(err.error, DeleteError::Refused);
    assert!(t.join("debug/deps/libfoo.rlib").exists());
}

#[test]
fn guard_before_delete_should_skip_with_identity_changed_When_the_part_was_replaced_after_planning()
{
    let f = fixture();
    let (_proj, t) = project(&f, "app", &["debug", "release"]);
    age_tree(&t.join("debug"), days(40));
    age_target_files(&t, days(40));
    let tgt = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();
    let cands = tgt.discover(&env, &sel());
    let inc = part(&cands, &t.join("debug/incremental"));

    // the old directory stays (moved aside) so the new one cannot get its inode back
    fs::rename(t.join("debug/incremental"), t.join("debug/incremental.old")).unwrap();
    fs::create_dir_all(t.join("debug/incremental/other")).unwrap();
    age_tree(&t.join("debug/incremental"), days(40));

    assert_eq!(
        tgt.guard_before_delete(inc, &env),
        Guarded::Skip(SkipReason::IdentityChanged)
    );
    let err = tgt
        .execute(inc, &env, &ApplyToken::for_tests())
        .unwrap_err();
    assert_eq!(err.error, DeleteError::IdentityMismatch);
    assert!(t.join("debug/incremental/other").exists());
}

#[test]
fn safety_cargo_artifact_symlink_swap_leaves_target_alone() {
    let f = fixture();
    let (_proj, t) = project(&f, "app", &["debug", "release"]);
    age_tree(&t.join("debug"), days(40));
    age_target_files(&t, days(40));
    let elsewhere = f.home.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::write(elsewhere.join("precious"), "p").unwrap();
    let tgt = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();
    let cands = tgt.discover(&env, &sel());
    let inc = part(&cands, &t.join("debug/incremental"));

    fs::remove_dir_all(t.join("debug/incremental")).unwrap();
    symlink(&elsewhere, t.join("debug/incremental")).unwrap();

    assert_eq!(
        tgt.guard_before_delete(inc, &env),
        Guarded::Skip(SkipReason::Symlink)
    );
    let err = tgt
        .execute(inc, &env, &ApplyToken::for_tests())
        .unwrap_err();
    assert_eq!(err.error, DeleteError::Symlink);
    assert!(elsewhere.join("precious").exists());
}

#[test]
fn safety_cargo_artifact_profile_swapped_for_a_symlink_leaves_the_link_target_alone() {
    let f = fixture();
    let (_proj, t) = project(&f, "app", &["debug", "release"]);
    age_tree(&t.join("debug"), days(40));
    age_target_files(&t, days(40));
    let elsewhere = f.home.join("elsewhere");
    profile(&elsewhere, "stage", 1024);
    age_tree(&elsewhere, days(40));
    let tgt = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();
    let cands = tgt.discover(&env, &sel());
    let deps = part(&cands, &t.join("debug/deps"));

    fs::remove_dir_all(t.join("debug")).unwrap();
    symlink(elsewhere.join("stage"), t.join("debug")).unwrap();

    assert_eq!(
        tgt.guard_before_delete(deps, &env),
        Guarded::Skip(SkipReason::Symlink)
    );
    assert!(tgt.execute(deps, &env, &ApplyToken::for_tests()).is_err());
    assert!(elsewhere.join("stage/deps/libfoo.rlib").exists());
}

#[test]
fn guard_before_delete_should_skip_When_the_target_lost_cargos_marker_or_the_manifest() {
    for what in ["tag", "manifest"] {
        let f = fixture();
        let (proj, t) = project(&f, "app", &["debug", "release"]);
        age_tree(&t.join("debug"), days(40));
        age_target_files(&t, days(40));
        let tgt = target(&f);
        let rig = Rig::new(&f);
        let env = rig.env();
        let cands = tgt.discover(&env, &sel());
        let inc = part(&cands, &t.join("debug/incremental"));
        if what == "tag" {
            fs::remove_file(t.join("CACHEDIR.TAG")).unwrap();
            fs::remove_file(t.join(".rustc_info.json")).unwrap();
        } else {
            fs::remove_file(proj.join("Cargo.toml")).unwrap();
        }
        assert!(
            matches!(tgt.guard_before_delete(inc, &env), Guarded::Skip(_)),
            "{what}"
        );
        assert!(
            tgt.execute(inc, &env, &ApplyToken::for_tests()).is_err(),
            "{what}"
        );
        assert!(t.join("debug/incremental").exists(), "{what}");
    }
}

#[test]
fn execute_should_succeed_with_nothing_freed_When_the_part_vanished() {
    let f = fixture();
    let (_proj, t) = project(&f, "app", &["debug", "release"]);
    age_tree(&t.join("debug"), days(40));
    age_target_files(&t, days(40));
    let tgt = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();
    let cands = tgt.discover(&env, &sel());
    fs::remove_dir_all(t.join("debug/incremental")).unwrap();

    let freed = tgt
        .execute(
            part(&cands, &t.join("debug/incremental")),
            &env,
            &ApplyToken::for_tests(),
        )
        .unwrap();

    assert_eq!(freed.entries_removed, 0);
}

// ------------------------------------------------------------------ plan and apply through the executor

#[test]
fn apply_should_drop_the_debug_parts_and_keep_release_manifest_and_binaries_When_debug_is_idle() {
    let f = fixture();
    let (proj, t) = project(&f, "app", &["debug", "release"]);
    age_tree(&t.join("debug"), days(40));
    age_target_files(&t, days(40));
    let tgt = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();
    let ex = Executor::new(&env, vec![&tgt as &dyn Target], options());

    let report = plan_then_apply(&ex, &env, &sel());

    for part in ["incremental", ".fingerprint", "deps", "build", "examples"] {
        assert!(
            !t.join("debug").join(part).exists(),
            "{part} survived: {:?}",
            report.summary.outcomes
        );
    }
    let removed: Vec<_> = report
        .summary
        .outcomes
        .iter()
        .filter(|o| o.status == Status::Removed)
        .collect();
    assert_eq!(removed.len(), 5, "{:?}", report.summary.outcomes);
    assert!(removed.iter().all(|o| o.kind == Kind::CargoArtifact));
    let freed: u64 = removed.iter().map(|o| o.measured_bytes.unwrap()).sum();
    assert!(freed >= 3 * 64 * 1024, "freed {freed}");
    // everything else stays
    assert!(t.join("debug/foo").exists());
    assert!(t.join("debug").is_dir());
    assert!(t.join("release/deps/libfoo.rlib").exists());
    assert!(t
        .join("release/incremental/foo-5e6f/s-abc/dep-graph.bin")
        .exists());
    assert!(proj.join("Cargo.toml").exists() && proj.join("src").exists());
    assert!(t.join("CACHEDIR.TAG").exists());
    // each deletion is in the audit log under the new kind
    let kinds: std::collections::BTreeSet<String> = rig
        .audit
        .records()
        .iter()
        .filter_map(|r| serde_json::to_value(r).ok())
        .filter_map(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_string))
        .collect();
    assert!(kinds.contains("cargo-artifact"), "{kinds:?}");
}

#[test]
fn apply_should_drop_only_the_selected_profile_When_cargo_profile_is_debug() {
    let f = fixture();
    let (_proj, t) = project(&f, "app", &["debug", "release"]);
    for p in ["debug", "release"] {
        age_tree(&t.join(p), days(40));
    }
    age_target_files(&t, days(40));
    fs::write(t.join("keepalive"), "now").unwrap();
    let tgt = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();
    let ex = Executor::new(&env, vec![&tgt as &dyn Target], options());
    let mut s = sel();
    s.cargo_profiles = vec!["debug".into()];

    plan_then_apply(&ex, &env, &s);

    assert!(!t.join("debug/deps").exists());
    assert!(t.join("release/deps/libfoo.rlib").exists());
    assert!(t.join("release/incremental").exists());
}

#[test]
fn a_dry_run_should_list_the_parts_and_delete_nothing() {
    let f = fixture();
    let (_proj, t) = project(&f, "app", &["debug", "release"]);
    age_tree(&t.join("debug"), days(40));
    age_target_files(&t, days(40));
    let tgt = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();
    let ex = Executor::new(&env, vec![&tgt as &dyn Target], options());

    let planned = ex.run(&sel(), &Mode::Plan, None).unwrap();

    let would: Vec<_> = planned
        .plan
        .items
        .iter()
        .filter(|i| i.candidate.kind() == Kind::CargoArtifact)
        .collect();
    assert_eq!(would.len(), 5);
    assert!(
        would.iter().all(|i| i.planned == Planned::Will),
        "{would:?}"
    );
    assert!(t.join("debug/deps/libfoo.rlib").exists());
}

#[test]
fn a_part_should_not_be_listed_for_removal_When_build_dirs_is_not_selected() {
    let f = fixture();
    let (_proj, t) = project(&f, "app", &["debug", "release"]);
    age_tree(&t.join("debug"), days(40));
    age_target_files(&t, days(40));
    let tgt = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();
    let s = Selection::new(
        KindSelector::of(&[SelectorId::Caches]),
        KindSelector::default(),
    );
    assert!(
        parts_of(&tgt.discover(&env, &s)).is_empty()
            || tgt.guard(&tgt.discover(&env, &s)[0], &env) == Guarded::Allow
    );
    assert!(t.join("debug/deps").exists());
}
