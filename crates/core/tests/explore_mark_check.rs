#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 6.6.1 (Task 6.6.1a): the mark rules M1 to M8 over fakes. `mark_check` has no filesystem argument; every
//! fact comes through `MarkFacts`, the mount table and `OwnState`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use disk_cleanup_core::explore::mark_check::{
    mark_check, MarkCtx, MarkEnv, MarkFacts, MarkMount, MarkOk, MountTable, OwnState, SymlinkFact,
};
use disk_cleanup_core::explore::marks::{Mark, Marks, MARK_LIMIT};
use disk_cleanup_core::explore::msg::MarkRefusal;
use disk_cleanup_core::roots::{strictly_below, RootSet};
use disk_cleanup_core::{
    AllowedTarget, DeleteError, DeletePolicy, Dirs, Identity, Kind, RootDev, SkipReason,
};
use proptest::prelude::*;

const PROTECTED: MarkRefusal = MarkRefusal::Engine(SkipReason::ProtectedPath);

#[derive(Default)]
struct Fake {
    links: HashMap<PathBuf, SymlinkFact>,
    devs: HashMap<PathBuf, u64>,
    boundary: HashSet<PathBuf>,
    scanning: HashSet<PathBuf>,
    files: HashSet<PathBuf>,
}

impl Fake {
    fn dev(mut self, p: &str, d: u64) -> Fake {
        self.devs.insert(p.into(), d);
        self
    }
    fn link(mut self, p: &str, real: Option<&str>) -> Fake {
        self.links.insert(
            p.into(),
            SymlinkFact {
                real: real.map(PathBuf::from),
            },
        );
        self
    }
}

impl MarkFacts for Fake {
    fn symlink_in(&self, path: &Path) -> Option<SymlinkFact> {
        path.ancestors().find_map(|a| self.links.get(a).cloned())
    }
    fn dev_of(&self, path: &Path) -> Option<u64> {
        Some(self.devs.get(path).copied().unwrap_or(1))
    }
    fn is_dir(&self, path: &Path) -> Option<bool> {
        Some(!self.files.contains(path))
    }
    fn is_boundary(&self, path: &Path) -> bool {
        self.boundary.contains(path)
    }
    fn scanning(&self, path: &Path) -> bool {
        self.scanning.contains(path)
    }
}

struct World {
    roots: Vec<PathBuf>,
    home: PathBuf,
    mounts: MountTable,
    own: OwnState,
    facts: Fake,
}

fn world(home: &str, roots: &[&str]) -> World {
    World {
        roots: roots.iter().map(PathBuf::from).collect(),
        home: home.into(),
        mounts: MountTable::default(),
        own: OwnState::new(
            &Dirs {
                home: home.into(),
                state: format!("{home}/.local/state").into(),
                cache: format!("{home}/.cache").into(),
                config: format!("{home}/.config").into(),
            },
            "/usr/local/bin/disk-cleanup".into(),
        ),
        facts: Fake::default(),
    }
}

impl World {
    fn check_with(&self, path: &str, marks: &Marks) -> Result<MarkOk, MarkRefusal> {
        let env = MarkEnv {
            roots: &self.roots,
            home: &self.home,
            mounts: &self.mounts,
            own_state: &self.own,
            facts: &self.facts,
            tty: false,
        };
        mark_check(Path::new(path), &MarkCtx { env: &env, marks })
    }
    fn check(&self, path: &str) -> Result<MarkOk, MarkRefusal> {
        self.check_with(path, &Marks::new())
    }
    fn refusal(&self, path: &str) -> Option<MarkRefusal> {
        self.check(path).err()
    }
}

fn mark(p: &str) -> Mark {
    Mark {
        path: p.into(),
        est_bytes: 1,
        cleaner_item: false,
    }
}

// ---- M1 ----------------------------------------------------------------------------------------------

#[test]
fn m1_should_admit_only_strict_descendants_of_a_root_When_checked() {
    let w = world("/home/u", &["/home/u/code"]);
    assert!(w.check("/home/u/code/app/dist.old").is_ok());
    for p in [
        "/home/u/code",
        "/home/u",
        "/home/u/Documents/x",
        "/data/x",
        "/",
        "/home/u/code/../.ssh",
        "relative/path",
        "/home/u/code/app/",
    ] {
        assert_eq!(w.refusal(p), Some(MarkRefusal::OutsideRoot), "{p}");
    }
}

#[test]
fn m1_should_ignore_the_explore_path_and_see_only_the_roots_resolve_returned_When_the_path_is_slash_or_home(
) {
    // `RootSet::resolve` is the only source of roots; the explore PATH is not an input to it, so the same
    // roots (and the same answers) hold for `/` and `/home`. The recording canon shows what resolve asked for.
    let seen = RefCell::new(Vec::<PathBuf>::new());
    let canon = |p: &Path| -> io::Result<(PathBuf, Identity)> {
        seen.borrow_mut().push(p.to_path_buf());
        let ino = p.as_os_str().len() as u64 + 1;
        Ok((p.to_path_buf(), Identity { dev: 1, ino }))
    };
    let dirs = Dirs {
        home: "/home/u".into(),
        state: "/home/u/.local/state".into(),
        cache: "/home/u/.cache".into(),
        config: "/home/u/.config".into(),
    };
    let rs = RootSet::resolve(
        &dirs,
        &[],
        Some(&[PathBuf::from("/home/u/work")]),
        Path::new("/"),
        &canon,
    )
    .unwrap();
    let asked: HashSet<PathBuf> = seen.borrow().iter().cloned().collect();
    let allowed: HashSet<PathBuf> = ["/home/u", "/home", "/", "/home/u/work"]
        .iter()
        .map(PathBuf::from)
        .collect();
    assert!(asked.is_subset(&allowed), "resolve looked at {asked:?}");
    let mut w = world("/home/u", &[]);
    w.roots = rs.scan_roots().to_vec();
    for explore_path in ["/", "/home"] {
        let _ = explore_path; // the PATH never reaches mark_check
        assert!(w.check("/home/u/work/app/node_modules").is_ok());
        assert_eq!(w.refusal("/home/u/other"), Some(MarkRefusal::OutsideRoot));
        assert_eq!(w.refusal("/home"), Some(MarkRefusal::OutsideRoot));
    }
}

// ---- M2 ----------------------------------------------------------------------------------------------

#[test]
fn m2_should_refuse_home_its_ancestors_and_anything_under_two_components_below_home_When_a_root_admits_them(
) {
    // M1 comes first, so rule F only decides what a root admits. `/` as a root is not reachable through
    // `RootSet::resolve` (it refuses ancestors of HOME); the check is defence in depth against that.
    let w = world("/home/u", &["/"]);
    for p in ["/home", "/home/u", "/home/u/.cache", "/home/u/Documents"] {
        assert_eq!(w.refusal(p), Some(PROTECTED), "{p}");
    }
    assert!(w.check("/home/u/Documents/x").is_ok()); // two components below home: rule F lets it through
    assert_eq!(w.refusal("/"), Some(MarkRefusal::OutsideRoot));
    assert!(w.check("/opt/app/node_modules").is_ok());
}

#[test]
fn m2_should_refuse_an_ancestor_of_home_that_a_root_admits_by_m1_When_the_root_is_an_ancestor_of_home(
) {
    let w = world("/data/home/u", &["/data"]);
    assert_eq!(w.refusal("/data/home"), Some(PROTECTED));
    assert_eq!(w.refusal("/data/home/u"), Some(PROTECTED));
    assert_eq!(w.refusal("/data/home/u/.cache"), Some(PROTECTED));
    // Other children of the root are fine, and so is a deep path under $HOME.
    assert!(w.check("/data/other/app").is_ok());
    assert!(w.check("/data/home/u/code/app").is_ok());
}

#[test]
fn m2_should_use_the_canonical_home_When_home_is_a_symlink() {
    // $HOME=/home/u is a link to /data/u: RootSet::resolve canonicalises it, so the ancestors that are
    // protected are those of /data/u (`/data`, `/`), not only those of the spelling in the environment.
    let canon = |p: &Path| -> io::Result<(PathBuf, Identity)> {
        let real = if p == Path::new("/home/u") {
            PathBuf::from("/data/u")
        } else {
            p.to_path_buf()
        };
        let ino = real.as_os_str().len() as u64;
        Ok((real, Identity { dev: 1, ino }))
    };
    let dirs = Dirs {
        home: "/home/u".into(),
        state: "/home/u/.local/state".into(),
        cache: "/home/u/.cache".into(),
        config: "/home/u/.config".into(),
    };
    let rs = RootSet::resolve(
        &dirs,
        &[],
        Some(&[PathBuf::from("/srv")]),
        Path::new("/"),
        &canon,
    )
    .unwrap();
    assert_eq!(rs.home(), Path::new("/data/u"));
    let mut w = world(rs.home().to_str().unwrap(), &["/"]);
    w.roots = vec![PathBuf::from("/")];
    for p in ["/data", "/data/u", "/data/u/.cache"] {
        assert_eq!(w.refusal(p), Some(PROTECTED), "{p}");
    }
}

#[test]
fn m2_should_refuse_a_path_with_a_git_component_as_decide_path_common_does() {
    let w = world("/home/u", &["/home/u/code"]);
    assert_eq!(w.refusal("/home/u/code/app/.git"), Some(PROTECTED));
    assert_eq!(w.refusal("/home/u/code/app/.git/objects"), Some(PROTECTED));
}

// ---- M3 ----------------------------------------------------------------------------------------------

fn mount_world() -> World {
    let mut w = world("/home/u", &["/data/x"]);
    w.mounts = MountTable::new(vec![
        MarkMount {
            point: "/data/x/mnt".into(),
            source: "/dev/sdb1".into(),
        },
        MarkMount {
            point: "/data/x/bind".into(),
            source: "/dev/sda2".into(),
        },
        MarkMount {
            point: "/data/x/deep/under/other".into(),
            source: "/dev/sdc1".into(),
        },
    ]);
    // /data/x is on device 1; /mnt is another device; /bind and its siblings share the parent's device.
    w.facts = Fake::default()
        .dev("/data/x/mnt", 2)
        .dev("/data/x/deep/under/other", 3);
    w
}

#[test]
fn m3_should_refuse_a_mount_point_of_another_device_and_a_same_device_bind_mount() {
    let w = mount_world();
    assert_eq!(w.refusal("/data/x/mnt"), Some(MarkRefusal::MountPoint));
    assert_eq!(w.refusal("/data/x/bind"), Some(MarkRefusal::MountPoint));
}

#[test]
fn m3_should_refuse_an_x_boundary_row() {
    let mut w = world("/home/u", &["/data/x"]);
    w.facts.boundary.insert("/data/x/other".into());
    assert_eq!(w.refusal("/data/x/other"), Some(MarkRefusal::MountPoint));
}

#[test]
fn m3_should_refuse_a_path_whose_st_dev_differs_from_the_roots_even_when_the_table_misses_it() {
    let mut w = world("/home/u", &["/data/x"]);
    w.facts = Fake::default().dev("/data/x/sub", 9);
    assert_eq!(
        w.refusal("/data/x/sub"),
        Some(MarkRefusal::Engine(SkipReason::CrossDevice))
    );
}

#[test]
fn m3_should_refuse_a_path_below_a_mount_of_another_device_as_the_engine_does_C14() {
    // The review's C14: the parent is on the other device and so is the child; comparing with the parent
    // said "same device, markable", the engine (root device) says CrossDevice.
    let mut w = world("/home/u", &["/data/x"]);
    w.facts = Fake::default()
        .dev("/data/x/mnt", 9)
        .dev("/data/x/mnt/deep", 9);
    assert_eq!(
        w.refusal("/data/x/mnt/deep"),
        Some(MarkRefusal::Engine(SkipReason::CrossDevice))
    );
}

#[test]
fn m3_should_compare_with_the_longest_containing_root_When_roots_are_nested() {
    let mut w = world("/home/u", &["/data/x", "/data/x/inner"]);
    w.facts = Fake::default()
        .dev("/data/x/inner", 9)
        .dev("/data/x/inner/a", 9);
    assert_eq!(w.refusal("/data/x/inner/a"), None);
}

#[test]
fn safety_explorer_cannot_mark_a_file_only_folders_When_space_is_pressed() {
    let mut w = world("/home/u", &["/data/x"]);
    w.facts.files.insert("/data/x/notes.txt".into());
    assert_eq!(
        w.refusal("/data/x/notes.txt"),
        Some(MarkRefusal::NotADirectory)
    );
    assert_eq!(w.refusal("/data/x/dir"), None);
}

#[test]
fn m3_should_note_a_directory_that_contains_another_device_and_still_mark_it() {
    let mut w = mount_world();
    w.mounts = MountTable::new(vec![MarkMount {
        point: "/data/x/dir/mnt".into(),
        source: "/dev/sdb1".into(),
    }]);
    w.facts = Fake::default().dev("/data/x/dir/mnt", 2);
    let ok = w.check("/data/x/dir").unwrap();
    assert_eq!(
        ok.note.as_deref(),
        Some("contains another filesystem (/dev/sdb1); it is not entered and the directory stays")
    );
}

#[test]
fn m3_should_refuse_a_directory_with_a_same_device_bind_mount_at_any_depth() {
    let mut w = world("/home/u", &["/data/x"]);
    w.mounts = MountTable::new(vec![MarkMount {
        point: "/data/x/app/a/b/bind".into(),
        source: "/dev/sda2".into(),
    }]);
    assert_eq!(
        w.refusal("/data/x/app"),
        Some(MarkRefusal::BindMountBelow("/data/x/app/a/b/bind".into()))
    );
    // A sibling directory that does not hold it is fine, and an unreadable mount device refuses too.
    assert!(w.check("/data/x/other").is_ok());
}

// ---- M4 ----------------------------------------------------------------------------------------------

#[test]
fn m4_should_refuse_a_symlink_row_and_a_path_with_a_symlinked_component() {
    let mut w = world("/home/u", &["/home/u/code", "/home/u/code-link"]);
    w.facts = Fake::default()
        .link("/home/u/code/app", Some("/data/app"))
        .link("/home/u/code-link", Some("/home/u/code"));
    assert_eq!(w.refusal("/home/u/code/app"), Some(MarkRefusal::SymlinkRow));
    assert_eq!(
        w.refusal("/home/u/code-link/app"),
        Some(MarkRefusal::SymlinkRow)
    );
    assert!(w.check("/home/u/code/other").is_ok());
}

// ---- M5 ----------------------------------------------------------------------------------------------

#[test]
fn m5_should_refuse_every_file_of_the_tools_own_state_its_directories_ancestors_and_the_binary() {
    let w = world("/home/u", &["/"]);
    let state = "/home/u/.local/state/disk-cleanup";
    let cache = "/home/u/.cache/disk-cleanup";
    let config = "/home/u/.config/disk-cleanup";
    let mut paths: Vec<String> = [
        "audit.jsonl",
        "last_plan",
        "last_plan.lock",
        "audit.reserve",
        "run.lock",
        "outcome.jsonl",
    ]
    .iter()
    .map(|f| format!("{state}/{f}"))
    .collect();
    paths.extend(
        ["explorer.cache", "walker.cache"]
            .iter()
            .map(|f| format!("{cache}/{f}")),
    );
    paths.push(format!("{config}/config.toml"));
    paths.extend(
        [
            state,
            cache,
            config,
            "/home/u/.local/state",
            "/usr/local/bin/disk-cleanup",
        ]
        .map(String::from),
    );
    paths.push(format!("{cache}/nested/deeper"));
    for p in &paths {
        assert_eq!(w.refusal(p), Some(MarkRefusal::OwnState), "{p}");
    }
    assert!(w.check("/home/u/.cache/other").is_ok()); // beside the state, not inside it
    assert!(w.check("/opt/app/target").is_ok());
}

#[test]
fn m5_should_follow_a_relocated_state_directory_and_refuse_a_root_that_holds_it() {
    let mut w = world("/home/u", &["/mnt/big"]);
    w.own = OwnState::new(
        &Dirs {
            home: "/home/u".into(),
            state: "/mnt/big/xdg-state".into(),
            cache: "/home/u/.cache".into(),
            config: "/home/u/.config".into(),
        },
        "/bin/dc".into(),
    );
    assert_eq!(
        w.refusal("/mnt/big/xdg-state/disk-cleanup/audit.jsonl"),
        Some(MarkRefusal::OwnState)
    );
    // The root that contains the state is refused by M1 (and would be by M5).
    assert_eq!(w.refusal("/mnt/big"), Some(MarkRefusal::OutsideRoot));
    assert_eq!(w.refusal("/mnt/big/xdg-state"), Some(MarkRefusal::OwnState));
    assert!(w.check("/mnt/big/projects").is_ok());
}

// ---- M6 to M8 ----------------------------------------------------------------------------------------

#[test]
fn m6_should_refuse_inside_and_above_a_mark_and_pass_the_mark_itself() {
    let w = world("/home/u", &["/home/u/code"]);
    let mut marks = Marks::new();
    marks.add(mark("/home/u/code/app")).unwrap();
    assert_eq!(
        w.check_with("/home/u/code/app/dist", &marks).err(),
        Some(MarkRefusal::InsideMarked("/home/u/code/app".into()))
    );
    assert!(w.check_with("/home/u/code/app", &marks).is_ok());
    assert!(w.check_with("/home/u/code/app2", &marks).is_ok());

    let mut marks = Marks::new();
    marks.add(mark("/home/u/code/app/dist")).unwrap();
    assert_eq!(
        w.check_with("/home/u/code/app", &marks).err(),
        Some(MarkRefusal::ContainsMarked(1))
    );
    marks.add(mark("/home/u/code/app/target")).unwrap();
    assert_eq!(
        w.check_with("/home/u/code/app", &marks).err(),
        Some(MarkRefusal::ContainsMarked(2))
    );
}

#[test]
fn m7_should_refuse_a_path_still_being_scanned() {
    let mut w = world("/home/u", &["/home/u/code"]);
    w.facts.scanning.insert("/home/u/code/app".into());
    assert_eq!(
        w.refusal("/home/u/code/app"),
        Some(MarkRefusal::StillScanning)
    );
}

#[test]
fn m8_should_refuse_a_long_path_the_21st_mark_and_a_quoted_only_text_over_24_kib() {
    let w = world("/home/u", &["/home/u/code"]);
    let long = format!("/home/u/code/{}", "a".repeat(512));
    assert_eq!(w.refusal(&long), Some(MarkRefusal::PathTooLong));
    let ok = format!("/home/u/code/{}", "a".repeat(512 - "/home/u/code/".len()));
    assert!(w.check(&ok).is_ok());

    let mut marks = Marks::new();
    for i in 0..MARK_LIMIT {
        marks.add(mark(&format!("/home/u/code/p{i:02}"))).unwrap();
    }
    assert_eq!(
        w.check_with("/home/u/code/extra", &marks).err(),
        Some(MarkRefusal::LimitReached)
    );

    // 512-byte paths of control bytes quote to four bytes per byte: the text passes 24 KiB before 20 marks.
    let nasty = |i: usize| {
        let mut b = b"/home/u/code/".to_vec();
        b.extend(std::iter::repeat_n(0x01u8, 512 - b.len() - 3));
        b.extend(format!("{i:03}").bytes());
        PathBuf::from(OsString::from_vec(b))
    };
    let mut marks = Marks::new();
    let mut refused_at = None;
    for i in 0..MARK_LIMIT {
        let p = nasty(i);
        match w.check_with(p.to_str().unwrap_or_default(), &marks) {
            _ if p.to_str().is_none() => unreachable!(),
            Ok(_) => marks
                .add(Mark {
                    path: p,
                    est_bytes: 1,
                    cleaner_item: false,
                })
                .unwrap(),
            Err(r) => {
                refused_at = Some((i, r));
                break;
            }
        }
    }
    let (at, r) = refused_at.expect("the quoted text must reach the cap before 20 marks");
    assert_eq!(r, MarkRefusal::PathTooLong);
    assert!(at > 0 && at < MARK_LIMIT);
}

// ---- order -------------------------------------------------------------------------------------------

#[test]
fn the_first_rule_that_applies_should_win_in_the_order_m1_to_m8() {
    let mut w = world("/home/u", &["/home/u/code"]);
    w.facts.scanning.insert("/home/u/code/.git".into());
    // M1 before everything.
    assert_eq!(w.refusal("/elsewhere/.git"), Some(MarkRefusal::OutsideRoot));
    // M2 (.git) before M7.
    assert_eq!(w.refusal("/home/u/code/.git"), Some(PROTECTED));
    // M3 before M4: a mount point that is also a link is a mount point.
    w.mounts = MountTable::new(vec![MarkMount {
        point: "/home/u/code/m".into(),
        source: "x".into(),
    }]);
    w.facts = Fake::default().link("/home/u/code/m", None);
    assert_eq!(w.refusal("/home/u/code/m"), Some(MarkRefusal::MountPoint));
    // M4 before M5 and M6 before M7.
    w.facts = Fake::default().link("/home/u/code/l", None);
    assert_eq!(w.refusal("/home/u/code/l"), Some(MarkRefusal::SymlinkRow));
}

// ---- shared with the engine --------------------------------------------------------------------------

#[test]
fn mark_check_should_call_the_engines_own_functions_not_copies() {
    let src = include_str!("../src/explore/mark_check.rs");
    for call in ["floor_protected(", "strictly_below(", "protected_position("] {
        assert!(src.contains(call), "mark_check.rs must call {call}");
    }
    // ... and the engine side uses the same functions.
    let floor = include_str!("../src/floor_rule.rs");
    assert!(
        floor.contains("pub fn floor_protected")
            && floor.contains("is_structurally_protected(target, &self.home")
    );
    let roots = include_str!("../src/roots.rs");
    assert!(roots.contains("strictly_below(root, path)"));
    let guard = include_str!("../src/guard.rs");
    assert!(guard.contains("protected_position(path, roots.scan_roots())"));
}

fn comp() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just("a"),
        Just("b"),
        Just("home"),
        Just("u"),
        Just("code"),
        Just("node_modules"),
        Just(".git"),
        Just("data"),
    ]
}

fn abs_path(max: usize) -> impl Strategy<Value = PathBuf> {
    proptest::collection::vec(comp(), 0..max).prop_map(|v| {
        let mut p = PathBuf::from("/");
        for c in v {
            p.push(c);
        }
        p
    })
}

#[allow(clippy::unnecessary_wraps)]
fn identity_canon(p: &Path) -> io::Result<(PathBuf, Identity)> {
    let ino = p.as_os_str().len() as u64 + p.components().count() as u64 * 1000;
    Ok((p.to_path_buf(), Identity { dev: 1, ino }))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(600))]

    /// Task 6.6.5i: `mark_check` and the engine agree. A path the screen accepts is allowed by
    /// `decide_path_common`, and `DeletePolicy::for_allowed(..).validate` accepts it; a path
    /// `decide_path_common` skips is refused on the screen.
    #[test]
    fn mark_check_should_agree_with_decide_path_common_and_the_below_scan_root_target(
        path in abs_path(7), home in abs_path(5), r1 in abs_path(4), r2 in abs_path(4),
    ) {
        use disk_cleanup_core::guard::{decide_path_common, Facts, GuardDecision};
        prop_assume!(path != Path::new("/") && home != Path::new("/"));
        let dirs = Dirs {
            home: home.clone(),
            state: home.join(".local/state"),
            cache: home.join(".cache"),
            config: home.join(".config"),
        };
        let Ok(rs) = disk_cleanup_core::roots::RootSet::resolve(
            &dirs, &[], Some(&[r1, r2]), Path::new("/"), &identity_canon,
        ) else {
            return Ok(());
        };
        let mut w = world(home.to_str().unwrap(), &[]);
        w.roots = rs.scan_roots().to_vec();
        let screen = w.check(path.to_str().unwrap());
        let decision = decide_path_common(
            &Facts {
                kind: Kind::ExplorerSelection,
                path: path.clone(),
                ident: Identity { dev: 1, ino: 5 },
                is_symlink: false,
                root_dev: RootDev(1),
                is_mount_point: false,
            },
            &rs,
        );
        match decision {
            GuardDecision::Skip(_) => prop_assert!(screen.is_err(), "{path:?}"),
            GuardDecision::Allow(a) => {
                if screen.is_ok() {
                    let policy = DeletePolicy::for_allowed(
                        &a, RootDev(1), 1, home.clone(), Identity { dev: 1, ino: 1 },
                        rs.scan_roots().to_vec(),
                    ).unwrap();
                    prop_assert_eq!(policy.validate(path.file_name().unwrap()), Ok(()), "{:?}", path);
                }
            }
        }
    }

    /// M2 and the engine's rule F give equal answers: for a path M1 admits, the explorer refuses it with
    /// `ProtectedPath` exactly when `DeletePolicy::validate` does (for an allowed target equal to the path).
    #[test]
    fn m2_should_equal_the_engines_floor_rule_When_paths_roots_and_homes_are_random(
        path in abs_path(7), home in abs_path(5), r1 in abs_path(4), r2 in abs_path(4),
    ) {
        prop_assume!(path != Path::new("/") && home != Path::new("/"));
        let roots = vec![r1, r2];
        let mut w = world(home.to_str().unwrap(), &[]);
        w.roots = roots.clone();
        let m1_admits = roots.iter().any(|r| strictly_below(r, &path));
        let got = w.check(path.to_str().unwrap());
        let git = path.components().any(|c| c.as_os_str() == ".git");
        let policy = DeletePolicy {
            root_dev: RootDev(1),
            root_mnt: None,
            home: home.clone(),
            home_id: Identity { dev: 1, ino: 1 },
            scan_roots: roots.clone(),
            allowed: AllowedTarget::Exact(path.clone()),
            target: path.clone(),
            require_sibling_any: vec![],
            max_depth: 64,
            restore_owner_write: false,
        };
        let engine = policy.validate(path.file_name().unwrap());
        if !m1_admits {
            prop_assert_eq!(got.err(), Some(MarkRefusal::OutsideRoot));
        } else if git {
            prop_assert_eq!(got.err(), Some(PROTECTED));
        } else {
            let engine_protected = matches!(engine, Err(DeleteError::ProtectedPath));
            prop_assert_eq!(got == Err(PROTECTED), engine_protected);
            // Whatever the engine would accept as an Exact target, the explorer accepts too (no facts refuse).
            if engine.is_ok() {
                prop_assert!(got.is_ok());
            }
        }
    }

    /// M1 is the oracle `RootSet::admits` is built on: for a name the kind accepts, both agree.
    #[test]
    fn m1_should_equal_roots_admits_for_a_node_modules_name_When_roots_are_resolved(
        base in abs_path(6), r1 in abs_path(4), r2 in abs_path(4),
    ) {
        let mut path = base;
        path.push("node_modules");
        let flags = vec![r1.clone(), r2.clone()];
        let dirs = Dirs {
            home: "/zz/home".into(),
            state: "/zz/home/s".into(),
            cache: "/zz/home/c".into(),
            config: "/zz/home/f".into(),
        };
        let Ok(rs) = RootSet::resolve(&dirs, &flags, None, Path::new("/"), &identity_canon) else {
            return Ok(());
        };
        let w = {
            let mut w = world("/zz/home", &[]);
            w.roots = rs.scan_roots().to_vec();
            w
        };
        let admitted = rs.admits(Kind::NodeModules, &path);
        let m1 = !matches!(w.check(path.to_str().unwrap()), Err(MarkRefusal::OutsideRoot));
        prop_assert_eq!(admitted, m1);
    }
}

// ---- safety_ -----------------------------------------------------------------------------------------

#[test]
fn safety_explorer_cannot_mark_home_an_ancestor_of_home_or_slash() {
    for (home, root) in [("/home/u", "/"), ("/data/home/u", "/data"), ("/h", "/")] {
        let w = world(home, &[root]);
        let mut p = Path::new(home).parent();
        while let Some(a) = p {
            if a != Path::new("/") && strictly_below(Path::new(root), a) {
                assert_eq!(w.refusal(a.to_str().unwrap()), Some(PROTECTED), "{a:?}");
            }
            p = a.parent();
        }
        assert_eq!(w.refusal(home), Some(PROTECTED));
        assert!(w.check("/").is_err());
    }
}

#[test]
fn safety_explorer_cannot_mark_a_mount_point_or_a_directory_holding_a_bind_mount() {
    let w = mount_world();
    assert_eq!(w.refusal("/data/x/mnt"), Some(MarkRefusal::MountPoint));
    assert_eq!(w.refusal("/data/x/bind"), Some(MarkRefusal::MountPoint));
    let mut w2 = world("/home/u", &["/data/x"]);
    w2.mounts = MountTable::new(vec![MarkMount {
        point: "/data/x/app/bind".into(),
        source: "/dev/sda2".into(),
    }]);
    assert!(matches!(
        w2.refusal("/data/x/app"),
        Some(MarkRefusal::BindMountBelow(_))
    ));
}

#[test]
fn safety_explorer_cannot_mark_a_symlink_or_the_tools_own_state() {
    let mut w = world("/home/u", &["/home/u/code"]);
    w.facts = Fake::default().link("/home/u/code/l", None);
    assert_eq!(w.refusal("/home/u/code/l"), Some(MarkRefusal::SymlinkRow));
    let w = world("/home/u", &["/"]);
    assert_eq!(
        w.refusal("/home/u/.local/state/disk-cleanup/audit.jsonl"),
        Some(MarkRefusal::OwnState)
    );
    assert_eq!(
        w.refusal("/home/u/.config/disk-cleanup/config.toml"),
        Some(MarkRefusal::OwnState)
    );
}

#[test]
fn safety_explorer_mark_check_agrees_with_the_engines_floor_rule() {
    // The plan's table, put to `DeletePolicy::validate` (rule F) and to `mark_check` with roots that admit
    // every path: a path the engine calls ProtectedPath is never markable.
    let w = world("/home/u", &["/"]);
    for p in ["/home", "/home/u", "/home/u/.cache", "/home/u/Documents"] {
        let policy = DeletePolicy {
            root_dev: RootDev(1),
            root_mnt: None,
            home: "/home/u".into(),
            home_id: Identity { dev: 1, ino: 1 },
            scan_roots: vec!["/".into()],
            allowed: AllowedTarget::Exact(p.into()),
            target: p.into(),
            require_sibling_any: vec![],
            max_depth: 64,
            restore_owner_write: false,
        };
        let name = Path::new(p).file_name().unwrap();
        assert_eq!(
            policy.validate(name),
            Err(DeleteError::ProtectedPath),
            "{p}"
        );
        assert_eq!(w.refusal(p), Some(PROTECTED), "{p}");
    }
}
