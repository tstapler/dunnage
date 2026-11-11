#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    non_snake_case
)]
//! The `steam` target (Story 4.2.2): orphan prefixes, shader caches and uninstalled-game remnants over
//! fixture libraries in tempdirs. Nothing here reads or writes a real Steam directory; the probe is fake.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use disk_cleanup_adapters::steam::{library_roots, target::SteamTarget};
use disk_cleanup_adapters::RealDirOps;
use disk_cleanup_core::audit_types::{AuditRecord, RunRoots};
use disk_cleanup_core::executor::{Executor, Mode, RunMeta, RunOptions, RunReport};
use disk_cleanup_core::ports::Env;
use disk_cleanup_core::roots::RootSet;
use disk_cleanup_core::selector::{Discard, KindSelector, Selection};
use disk_cleanup_core::target::{Guarded, KindGate, Target};
use disk_cleanup_core::testing::{
    FakeAuditSink, FakeClock, FakeCommandRunner, FakeFs, FakePlanStore, FakeProbe, FakeRunGuard,
    FakeSpaceMeter,
};
use disk_cleanup_core::{
    AppId, ApplyToken, BuildInfo, Candidate, DeleteError, Dirs, Identity, Kind, KindFacts, Locator,
    SelectorId, SkipReason, Status, SteamFacts, Tri,
};

const DAY: Duration = Duration::from_hours(24);

struct Steam {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    home: PathBuf,
    root: PathBuf,
    libs: Vec<PathBuf>,
}

fn vdf_escape(p: &Path) -> String {
    p.to_str().unwrap().replace('\\', "\\\\")
}

fn write_folders(root: &Path, libs: &[PathBuf]) {
    let mut s = String::from("\"libraryfolders\"\n{\n");
    for (i, l) in libs.iter().enumerate() {
        write!(
            s,
            "\t\"{i}\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t\t\"label\"\t\t\"\"\n\t\t\"apps\"\n\t\t{{\n\t\t}}\n\t}}\n",
            vdf_escape(l)
        )
        .unwrap();
    }
    s.push_str("}\n");
    fs::write(root.join("steamapps/libraryfolders.vdf"), s).unwrap();
}

/// A fake HOME with `~/.local/share/Steam` (library 0) and `extra` more libraries beside it.
fn steam(extra: &[&str]) -> Steam {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let home = base.join("home");
    fs::create_dir_all(home.join("code")).unwrap();
    let root = home.join(".local/share/Steam");
    let mut libs = vec![root.clone()];
    libs.extend(extra.iter().map(|n| base.join(n)));
    for l in &libs {
        fs::create_dir_all(l.join("steamapps")).unwrap();
    }
    write_folders(&root, &libs);
    Steam {
        _tmp: tmp,
        base,
        home,
        root,
        libs,
    }
}

fn manifest(lib: &Path, appid: u32, installdir: &str) {
    let body = format!(
        "\"AppState\"\n{{\n\t\"appid\"\t\t\"{appid}\"\n\t\"name\"\t\t\"Game {appid}\"\n\t\"installdir\"\t\t\"{installdir}\"\n\t\"StateFlags\"\t\t\"4\"\n}}\n"
    );
    fs::write(lib.join(format!("steamapps/appmanifest_{appid}.acf")), body).unwrap();
}

/// `<lib>/steamapps/<sub>/<name>/f` of `bytes`, file and directory aged `days`.
fn entry(lib: &Path, sub: &str, name: &str, bytes: usize, days: u64) -> PathBuf {
    let dir = lib.join("steamapps").join(sub).join(name);
    fs::create_dir_all(&dir).unwrap();
    let f = dir.join("f");
    fs::write(&f, vec![7u8; bytes]).unwrap();
    age(&f, days);
    age(&dir, days);
    dir
}

fn age(p: &Path, days: u64) {
    File::open(p)
        .unwrap()
        .set_modified(SystemTime::now() - DAY * u32::try_from(days).unwrap())
        .unwrap();
}

/// Ages the symlink itself (not what it points to).
fn age_link(p: &Path, days: u64) {
    use rustix::fs::{utimensat, AtFlags, Timestamps, CWD};
    use rustix::fs::Timespec;
    let t = Timespec {
        tv_sec: i64::try_from(
            (SystemTime::now() - DAY * u32::try_from(days).unwrap())
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap(),
        tv_nsec: 0,
    };
    utimensat(
        CWD,
        p,
        &Timestamps {
            last_access: t,
            last_modification: t,
        },
        AtFlags::SYMLINK_NOFOLLOW,
    )
    .unwrap();
}

/// Ages `dir`, everything below it and every directory on the way.
fn age_tree(dir: &Path, days: u64) {
    for e in fs::read_dir(dir).unwrap().flatten() {
        if e.file_type().unwrap().is_dir() {
            age_tree(&e.path(), days);
        } else {
            age(&e.path(), days);
        }
    }
    age(dir, days);
}

fn dirs_of(home: &Path) -> Dirs {
    Dirs {
        home: home.to_path_buf(),
        state: home.join(".local/state"),
        cache: home.join(".cache"),
        config: home.join(".config"),
    }
}

fn roots_for(s: &Steam) -> RootSet {
    RootSet::resolve(
        &dirs_of(&s.home),
        std::slice::from_ref(&s.home.join("code")),
        None,
        &s.home,
        &|p: &Path| {
            let c = p.canonicalize()?;
            let m = fs::metadata(&c)?;
            Ok((c, Identity::of(&m)))
        },
    )
    .unwrap()
    .with_steam_libraries(library_roots(&s.root))
}

fn target(s: &Steam) -> SteamTarget<RealDirOps> {
    SteamTarget::new(roots_for(s), RealDirOps::confined_to(&s.base))
}

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
    fn new(s: &Steam) -> Rig {
        Rig::with_probe(s, FakeProbe::new())
    }

    fn with_probe(s: &Steam, probe: FakeProbe) -> Rig {
        Rig {
            fs: FakeFs::new(),
            runner: FakeCommandRunner::new(),
            clock: FakeClock::at(
                i64::try_from(
                    SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap()
                        .as_secs(),
                )
                .unwrap(),
            ),
            probe,
            space: FakeSpaceMeter::new(
                vec![FakeSpaceMeter::plain_snapshot("/", 500, 1000)],
                FakeSpaceMeter::stable(),
            ),
            audit: FakeAuditSink::new(),
            guard: FakeRunGuard::new(),
            plans: FakePlanStore::new(),
            home: s.home.clone(),
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
        KindSelector::of(&[SelectorId::Steam]),
        KindSelector::default(),
    )
}

fn sel_discard() -> Selection {
    let mut s = sel();
    s.steam_saves = Some(Discard::Discard);
    s
}

fn path_of(c: &Candidate) -> &Path {
    match c.locator() {
        Locator::Path(p) | Locator::Steam(_, p) => p,
        Locator::Docker(_) => unreachable!("steam candidates have paths"),
    }
}

fn find<'a>(cs: &'a [Candidate], p: &Path) -> &'a Candidate {
    cs.iter()
        .find(|c| path_of(c) == p)
        .unwrap_or_else(|| panic!("no candidate {}", p.display()))
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
            stamp: None,
            break_glass: None,
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

fn status_at(r: &RunReport, p: &Path) -> Status {
    r.summary
        .outcomes
        .iter()
        .find(|o| o.path.as_deref() == Some(p))
        .unwrap_or_else(|| panic!("no outcome for {}", p.display()))
        .status
        .clone()
}

// ------------------------------------------------------------------ the orphan rule

#[test]
fn discover_should_offer_only_99999_prefix_and_shader_cache_When_installed_set_is_620() {
    let s = steam(&["lib1"]);
    manifest(&s.libs[0], 620, "Portal 2");
    for id in ["620", "99999", "3000000000"] {
        entry(&s.libs[0], "compatdata", id, 100, 60);
    }
    for id in ["620", "99999"] {
        entry(&s.libs[0], "shadercache", id, 100, 60);
    }
    let (t, rig) = (target(&s), Rig::new(&s));
    let env = rig.env();

    let cands = t.discover(&env, &sel());

    let mut got: Vec<(Kind, PathBuf)> = cands
        .iter()
        .filter(|c| c.kind() != Kind::SteamRemnant)
        .map(|c| (c.kind(), path_of(c).to_path_buf()))
        .collect();
    got.sort();
    let mut want = vec![
        (
            Kind::SteamPrefix,
            s.libs[0].join("steamapps/compatdata/99999"),
        ),
        (
            Kind::SteamShaderCache,
            s.libs[0].join("steamapps/shadercache/99999"),
        ),
    ];
    want.sort();
    assert_eq!(got, want);
    let prefix = find(&cands, &s.libs[0].join("steamapps/compatdata/99999"));
    assert_eq!(
        prefix.locator(),
        &Locator::Steam(AppId(99_999), path_of(prefix).to_path_buf())
    );
    assert_eq!(t.guard(prefix, &env), Guarded::Allow);
    assert_eq!(
        t.detail(prefix).as_deref(),
        Some("no appmanifest in 2 libraries")
    );
}

#[test]
fn discover_should_not_list_compatibilitytools_workshop_or_downloading_When_they_sit_beside_the_data(
) {
    let s = steam(&[]);
    manifest(&s.libs[0], 620, "Portal 2");
    entry(&s.libs[0], "compatdata", "99999", 10, 60);
    fs::create_dir_all(s.root.join("compatibilitytools.d/GE-Proton9")).unwrap();
    fs::create_dir_all(s.root.join("steamapps/workshop/content")).unwrap();
    fs::create_dir_all(s.root.join("steamapps/downloading")).unwrap();
    fs::write(s.root.join("steamapps/compatdata/pfx.lock"), "x").unwrap();
    let (t, rig) = (target(&s), Rig::new(&s));
    let env = rig.env();

    let cands = t.discover(&env, &sel());

    assert_eq!(
        cands.len(),
        1,
        "{:?}",
        cands.iter().map(path_of).collect::<Vec<_>>()
    );
}

// ------------------------------------------------------------------ age is the newest file inside

#[test]
fn guard_should_skip_too_recent_When_the_dir_is_90_days_old_but_a_file_inside_is_2_days_old() {
    let s = steam(&[]);
    manifest(&s.libs[0], 620, "Portal 2");
    let prefix = entry(&s.libs[0], "compatdata", "99999", 100, 90);
    let shader = entry(&s.libs[0], "shadercache", "99999", 100, 90);
    let remnant = entry(&s.libs[0], "common", "OldGame", 100, 90);
    for d in [&prefix, &shader, &remnant] {
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::write(d.join("sub/recent"), "x").unwrap();
        age(&d.join("sub/recent"), 2);
        age(&d.join("sub"), 90);
        age(d, 90);
    }
    let (t, rig) = (target(&s), Rig::new(&s));
    let env = rig.env();

    let cands = t.discover(&env, &sel());

    for d in [&prefix, &shader, &remnant] {
        assert_eq!(
            t.guard(find(&cands, d), &env),
            Guarded::Skip(SkipReason::TooRecent),
            "{}",
            d.display()
        );
        assert_eq!(
            t.guard_before_delete(find(&cands, d), &env),
            Guarded::Skip(SkipReason::TooRecent)
        );
    }
}

#[test]
fn guard_should_allow_When_every_file_is_older_than_min_age_even_if_the_dir_mtime_is_today() {
    let s = steam(&[]);
    manifest(&s.libs[0], 620, "Portal 2");
    let prefix = entry(&s.libs[0], "compatdata", "99999", 100, 90);
    File::open(&prefix)
        .unwrap()
        .set_modified(SystemTime::now())
        .unwrap();
    let (t, rig) = (target(&s), Rig::new(&s));
    let env = rig.env();

    let cands = t.discover(&env, &sel());

    assert_eq!(t.guard(find(&cands, &prefix), &env), Guarded::Allow);
}

// ------------------------------------------------------------------ saves

fn prefix_with_save(s: &Steam, id: &str, dir: &str) -> PathBuf {
    let p = entry(&s.libs[0], "compatdata", id, 100, 90);
    let d = p.join("pfx/drive_c/users/steamuser").join(dir);
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("save.dat"), "x").unwrap();
    age_tree(&p, 90);
    p
}

#[test]
fn guard_should_skip_may_hold_saves_naming_the_dir_When_a_save_directory_holds_a_file() {
    let s = steam(&[]);
    manifest(&s.libs[0], 620, "Portal 2");
    let dirs = [
        "Saved Games",
        "Documents",
        "My Documents",
        "AppData/Local/x",
    ];
    let paths: Vec<PathBuf> = dirs
        .iter()
        .enumerate()
        .map(|(i, d)| prefix_with_save(&s, &format!("9000{i}"), d))
        .collect();
    let (t, rig) = (target(&s), Rig::new(&s));
    let env = rig.env();

    let cands = t.discover(&env, &sel());

    for (p, want) in paths
        .iter()
        .zip(["Saved Games", "Documents", "My Documents", "AppData"])
    {
        let c = find(&cands, p);
        assert_eq!(t.guard(c, &env), Guarded::Skip(SkipReason::MayHoldSaves));
        assert_eq!(t.detail(c).as_deref(), Some(want));
    }
}

#[test]
fn guard_should_allow_the_prefix_and_leave_shader_caches_alone_When_steam_saves_discard_or_no_save_files(
) {
    let s = steam(&[]);
    manifest(&s.libs[0], 620, "Portal 2");
    let with_save = prefix_with_save(&s, "90001", "Documents");
    let empty_dirs = entry(&s.libs[0], "compatdata", "90002", 100, 90);
    fs::create_dir_all(empty_dirs.join("pfx/drive_c/users/steamuser/Documents/sub")).unwrap();
    age_tree(&empty_dirs, 90);
    let shader = entry(&s.libs[0], "shadercache", "90001", 100, 90);
    let (t, rig) = (target(&s), Rig::new(&s));
    let env = rig.env();

    let cands = t.discover(&env, &sel());
    assert_eq!(t.guard(find(&cands, &empty_dirs), &env), Guarded::Allow);
    assert_eq!(t.guard(find(&cands, &shader), &env), Guarded::Allow);
    assert_eq!(
        t.guard(find(&cands, &with_save), &env),
        Guarded::Skip(SkipReason::MayHoldSaves)
    );

    let cands = t.discover(&env, &sel_discard());
    assert_eq!(t.guard(find(&cands, &with_save), &env), Guarded::Allow);
    assert_eq!(
        t.guard_before_delete(find(&cands, &with_save), &env),
        Guarded::Allow
    );
}

// ------------------------------------------------------------------ removal through the engine

#[test]
fn apply_should_remove_the_orphan_prefix_and_audit_kind_steam_prefix_When_the_plan_id_matches() {
    let s = steam(&["lib1"]);
    manifest(&s.libs[0], 620, "Portal 2");
    let orphan = entry(&s.libs[1], "compatdata", "99999", 5 << 20, 90);
    let kept = entry(&s.libs[0], "compatdata", "620", 1000, 90);
    let t = target(&s);
    let rig = Rig::new(&s);
    let env = rig.env();
    let ex = Executor::new(&env, vec![&t as &dyn Target], options());

    let r = plan_then_apply(&ex, &env, &sel());

    assert_eq!(status_at(&r, &orphan), Status::Removed);
    assert!(!orphan.exists());
    assert!(kept.is_dir());
    let logged: Vec<_> = rig
        .audit
        .records()
        .into_iter()
        .filter_map(|r| match r {
            AuditRecord::Result(i) => Some(i.0),
            _ => None,
        })
        .collect();
    assert!(logged
        .iter()
        .any(|o| o.kind == Kind::SteamPrefix && o.status == Status::Removed));
    assert!(r
        .summary
        .outcomes
        .iter()
        .any(|o| o.measured_bytes.unwrap_or(0) >= (5 << 20)));
}

#[test]
fn apply_should_skip_symlink_and_leave_the_target_alone_When_the_prefix_dir_is_a_symlink() {
    let s = steam(&[]);
    manifest(&s.libs[0], 620, "Portal 2");
    let victim = s.base.join("victim");
    fs::create_dir_all(&victim).unwrap();
    fs::write(victim.join("precious"), "keep me").unwrap();
    age(&victim.join("precious"), 90);
    let link = s.libs[0].join("steamapps/compatdata/99999");
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    symlink(&victim, &link).unwrap();
    let remnant_link = s.libs[0].join("steamapps/common/LinkGame");
    fs::create_dir_all(remnant_link.parent().unwrap()).unwrap();
    symlink(&victim, &remnant_link).unwrap();
    age_link(&link, 90);
    age_link(&remnant_link, 90);
    let t = target(&s);
    let rig = Rig::new(&s);
    let env = rig.env();
    let ex = Executor::new(&env, vec![&t as &dyn Target], options());

    let cands = t.discover(&env, &sel());
    assert_eq!(
        t.guard(find(&cands, &link), &env),
        Guarded::Skip(SkipReason::Symlink)
    );
    assert_eq!(
        t.guard(find(&cands, &remnant_link), &env),
        Guarded::Skip(SkipReason::Symlink)
    );
    let r = plan_then_apply(&ex, &env, &sel());

    assert_eq!(status_at(&r, &link), Status::Skipped(SkipReason::Symlink));
    assert_eq!(
        fs::read_to_string(victim.join("precious")).unwrap(),
        "keep me"
    );
    assert!(link.symlink_metadata().is_ok());
}

// ------------------------------------------------------------------ remnants

fn remnant_fixture() -> Steam {
    let s = steam(&["lib1"]);
    manifest(&s.libs[0], 620, "Portal 2");
    manifest(&s.libs[1], 1_091_500, "MovedGame");
    entry(&s.libs[0], "common", "Portal 2", 100, 90);
    entry(&s.libs[0], "common", "MovedGame", 100, 90);
    entry(&s.libs[0], "common", "OldGame", 100, 90);
    fs::write(s.libs[0].join("steamapps/common/readme.txt"), "x").unwrap();
    s
}

#[test]
fn discover_should_offer_only_OldGame_When_the_other_two_are_named_by_manifests_in_either_library()
{
    let s = remnant_fixture();
    let (t, rig) = (target(&s), Rig::new(&s));
    let env = rig.env();

    let cands = t.discover(&env, &sel());

    let remnants: Vec<&Candidate> = cands
        .iter()
        .filter(|c| c.kind() == Kind::SteamRemnant)
        .collect();
    assert_eq!(remnants.len(), 1);
    assert_eq!(
        path_of(remnants[0]),
        s.libs[0].join("steamapps/common/OldGame")
    );
    assert_eq!(
        remnants[0].facts(),
        &KindFacts::Steam(SteamFacts::Remnant {
            install_dir: "OldGame".into()
        })
    );
    assert_eq!(t.guard(remnants[0], &env), Guarded::Allow);
}

#[test]
fn discover_should_match_installdir_case_insensitively_When_the_folder_differs_in_case() {
    let s = steam(&[]);
    manifest(&s.libs[0], 620, "Portal 2");
    entry(&s.libs[0], "common", "PORTAL 2", 100, 90);
    let (t, rig) = (target(&s), Rig::new(&s));
    let env = rig.env();

    assert!(t.discover(&env, &sel()).is_empty());
}

#[test]
fn gate_should_be_library_unmounted_for_the_whole_kind_When_any_listed_library_is_missing() {
    let s = remnant_fixture();
    fs::remove_dir_all(&s.libs[1]).unwrap();
    let (t, rig) = (target(&s), Rig::new(&s));
    let env = rig.env();

    let gate = t.gate(&env);

    assert_eq!(gate, KindGate::Unavailable(SkipReason::LibraryUnmounted));
    let detail = t.unavailable_detail(&env);
    assert_eq!(detail.as_deref(), s.libs[1].to_str());
    assert_eq!(
        gate.skipped_line("steam", detail.as_deref(), false)
            .unwrap(),
        format!(
            "steam: SKIPPED (Steam library {} not mounted)",
            s.libs[1].display()
        )
    );
}

#[test]
fn gate_should_be_library_unmounted_When_a_mount_point_is_left_behind_without_steamapps() {
    let s = remnant_fixture();
    fs::remove_dir_all(s.libs[1].join("steamapps")).unwrap();
    let (t, rig) = (target(&s), Rig::new(&s));

    assert_eq!(
        t.gate(&rig.env()),
        KindGate::Unavailable(SkipReason::LibraryUnmounted)
    );
}

#[test]
fn gate_should_be_steam_running_When_steam_runs_or_downloads_are_pending() {
    let s = remnant_fixture();
    let t = target(&s);
    let running = Rig::with_probe(&s, FakeProbe::new().process("steam", Tri::Running));
    assert_eq!(
        t.gate(&running.env()),
        KindGate::Unavailable(SkipReason::SteamRunning)
    );

    let unknown = Rig::with_probe(&s, FakeProbe::new().process("steam", Tri::Unknown));
    assert_eq!(
        t.gate(&unknown.env()),
        KindGate::Unavailable(SkipReason::SteamRunning)
    );

    let rig = Rig::new(&s);
    assert_eq!(t.gate(&rig.env()), KindGate::Available);
    fs::create_dir_all(s.libs[0].join("steamapps/downloading/620")).unwrap();
    assert_eq!(
        t.gate(&rig.env()),
        KindGate::Unavailable(SkipReason::SteamRunning)
    );
}

#[test]
fn gate_should_fail_closed_When_the_installed_set_is_empty_but_folders_have_entries() {
    let s = steam(&[]);
    entry(&s.libs[0], "compatdata", "620", 100, 90);
    entry(&s.libs[0], "common", "Portal 2", 100, 90);
    let (t, rig) = (target(&s), Rig::new(&s));

    assert_eq!(
        t.gate(&rig.env()),
        KindGate::Unavailable(SkipReason::LibraryUnmounted)
    );
}

#[test]
fn gate_should_be_unrecognised_output_When_the_library_file_is_truncated() {
    let s = remnant_fixture();
    fs::write(
        s.root.join("steamapps/libraryfolders.vdf"),
        "\"libraryfolders\"\n{\n\t\"0\"\n",
    )
    .unwrap();
    let (t, rig) = (target(&s), Rig::new(&s));
    let env = rig.env();

    let gate = t.gate(&env);

    assert_eq!(gate, KindGate::Unavailable(SkipReason::UnrecognisedOutput));
    assert_eq!(
        gate.skipped_line("steam", t.unavailable_detail(&env).as_deref(), false)
            .unwrap(),
        "steam: SKIPPED (unrecognised steam output; nothing listed)"
    );
}

#[test]
fn gate_should_be_available_and_discover_nothing_When_steam_is_not_installed() {
    let s = steam(&[]);
    fs::remove_dir_all(&s.root).unwrap();
    let (t, rig) = (target(&s), Rig::new(&s));
    let env = rig.env();

    assert_eq!(t.gate(&env), KindGate::Available);
    assert!(t.discover(&env, &sel()).is_empty());
}

#[test]
fn apply_should_remove_the_remnant_and_audit_kind_steam_remnant_When_the_plan_id_matches() {
    let s = remnant_fixture();
    let old = s.libs[0].join("steamapps/common/OldGame");
    let t = target(&s);
    let rig = Rig::new(&s);
    let env = rig.env();
    let ex = Executor::new(&env, vec![&t as &dyn Target], options());

    let r = plan_then_apply(&ex, &env, &sel());

    assert_eq!(status_at(&r, &old), Status::Removed);
    assert!(!old.exists());
    assert!(s.libs[0].join("steamapps/common/Portal 2/f").is_file());
    assert!(s.libs[0].join("steamapps/common/MovedGame/f").is_file());
    assert!(s.libs[0].join("steamapps/common/readme.txt").is_file());
    assert!(rig.audit.records().into_iter().any(|r| matches!(
        r,
        AuditRecord::Result(i) if i.0.kind == Kind::SteamRemnant && i.0.status == Status::Removed
    )));
}

#[test]
fn execute_should_refuse_with_outside_scope_When_the_installdir_is_dot_dot_hidden_or_has_a_slash() {
    let s = remnant_fixture();
    let (t, rig) = (target(&s), Rig::new(&s));
    let env = rig.env();
    let common = s.libs[0].join("steamapps/common");
    let hidden = common.join(".hidden");
    fs::create_dir_all(&hidden).unwrap();
    let nested = common.join("a/b");
    fs::create_dir_all(&nested).unwrap();
    for forged in [
        common.join(".."),
        hidden.clone(),
        nested.clone(),
        PathBuf::from(format!("{}/a/b", common.display())),
    ] {
        let c = Candidate::new(
            Locator::Path(forged.clone()),
            KindFacts::Steam(SteamFacts::Remnant {
                install_dir: forged
                    .file_name()
                    .map_or(String::new(), |n| n.to_string_lossy().into_owned()),
            }),
            disk_cleanup_core::AllocatedBytes(0),
            None,
            Identity { dev: 1, ino: 1 },
        );
        let err = t
            .execute(&c, &env, &ApplyToken::for_tests())
            .expect_err(&format!("{} must be refused", forged.display()));
        assert_eq!(err.error, DeleteError::OutsideScope, "{}", forged.display());
    }
    assert!(hidden.is_dir() && nested.is_dir());
}

// ------------------------------------------------------------------ execute-time re-check

#[test]
fn guard_before_delete_should_skip_steam_running_and_execute_remove_nothing_When_steam_starts_after_the_plan(
) {
    let s = remnant_fixture();
    let orphan = entry(&s.libs[0], "compatdata", "99999", 100, 90);
    let t = target(&s);
    let calm = Rig::new(&s);
    let cands = t.discover(&calm.env(), &sel());
    let running = Rig::with_probe(&s, FakeProbe::new().process("steam", Tri::Running));
    let env = running.env();

    for c in &cands {
        assert_eq!(
            t.guard_before_delete(c, &env),
            Guarded::Skip(SkipReason::SteamRunning)
        );
        let e = t.execute(c, &env, &ApplyToken::for_tests()).unwrap_err();
        assert_eq!(e.error, DeleteError::Refused);
    }
    assert!(orphan.is_dir());
    assert!(s.libs[0].join("steamapps/common/OldGame/f").is_file());
}

#[test]
fn guard_before_delete_should_skip_library_unmounted_When_a_library_disappears_after_the_plan() {
    let s = remnant_fixture();
    let t = target(&s);
    let rig = Rig::new(&s);
    let env = rig.env();
    let cands = t.discover(&env, &sel());
    assert!(!cands.is_empty());
    fs::remove_dir_all(&s.libs[1]).unwrap();

    for c in &cands {
        assert_eq!(
            t.guard_before_delete(c, &env),
            Guarded::Skip(SkipReason::LibraryUnmounted)
        );
    }
    assert!(s.libs[0].join("steamapps/common/OldGame/f").is_file());
}

#[test]
fn guard_before_delete_should_skip_When_a_manifest_names_the_item_after_the_plan() {
    let s = remnant_fixture();
    let orphan = entry(&s.libs[0], "compatdata", "99999", 100, 90);
    let t = target(&s);
    let rig = Rig::new(&s);
    let env = rig.env();
    let cands = t.discover(&env, &sel());
    manifest(&s.libs[1], 99_999, "OldGame");

    let prefix = find(&cands, &orphan);
    let remnant = find(&cands, &s.libs[0].join("steamapps/common/OldGame"));

    assert_eq!(
        t.guard_before_delete(prefix, &env),
        Guarded::Skip(SkipReason::IdentityChanged)
    );
    assert_eq!(
        t.guard_before_delete(remnant, &env),
        Guarded::Skip(SkipReason::IdentityChanged)
    );
    assert!(t.execute(prefix, &env, &ApplyToken::for_tests()).is_err());
    assert!(orphan.is_dir());
}

#[test]
fn guard_before_delete_should_skip_identity_changed_When_the_dir_was_replaced_after_the_plan() {
    let s = steam(&[]);
    manifest(&s.libs[0], 620, "Portal 2");
    let orphan = entry(&s.libs[0], "compatdata", "99999", 100, 90);
    let t = target(&s);
    let rig = Rig::new(&s);
    let env = rig.env();
    let cands = t.discover(&env, &sel());
    fs::remove_dir_all(&orphan).unwrap();
    entry(&s.libs[0], "compatdata", "99999", 100, 90);

    assert_eq!(
        t.guard_before_delete(find(&cands, &orphan), &env),
        Guarded::Skip(SkipReason::IdentityChanged)
    );
}

#[test]
fn execute_should_succeed_with_nothing_freed_When_the_item_is_already_gone() {
    let s = steam(&[]);
    manifest(&s.libs[0], 620, "Portal 2");
    let orphan = entry(&s.libs[0], "compatdata", "99999", 100, 90);
    let t = target(&s);
    let rig = Rig::new(&s);
    let env = rig.env();
    let cands = t.discover(&env, &sel());
    fs::remove_dir_all(&orphan).unwrap();

    let freed = t
        .execute(find(&cands, &orphan), &env, &ApplyToken::for_tests())
        .unwrap();

    assert_eq!(freed.entries_removed, 0);
}

#[test]
fn discover_should_note_the_symlink_and_list_nothing_When_compatdata_itself_is_a_link() {
    let s = steam(&[]);
    manifest(&s.libs[0], 620, "Portal 2");
    let elsewhere = s.base.join("elsewhere");
    fs::create_dir_all(elsewhere.join("99999")).unwrap();
    age(&elsewhere.join("99999"), 90);
    symlink(&elsewhere, s.libs[0].join("steamapps/compatdata")).unwrap();
    let (t, rig) = (target(&s), Rig::new(&s));
    let env = rig.env();

    let cands = t.discover(&env, &sel());

    assert!(cands.is_empty());
    let notes = t.plan_notes(&env, &sel());
    assert!(
        notes.iter().any(|n| n.contains("is a symlink")),
        "{notes:?}"
    );
    assert!(elsewhere.join("99999").is_dir());
}

#[test]
fn selectors_should_be_steam_only() {
    let s = steam(&[]);
    assert_eq!(target(&s).selectors(), &[SelectorId::Steam]);
    assert_eq!(target(&s).name(), "steam");
}
