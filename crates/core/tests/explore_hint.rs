#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 6.4.1 (Task 6.4.1a): the tri-state hint, the cleaner's own roots and the one classifier.
//! `kind_hint` takes no filesystem argument, so "no I/O" holds by its signature; the tests below run it on
//! paths that exist nowhere.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};

use disk_cleanup_core::explore::hint::{hint_of_node, kind_hint, kind_line, shape_of};
use disk_cleanup_core::explore::msg::{
    ClassifyFacts, Hint, NoReason, NodeEvent, ScanMode, SteamLibraryFacts, Tier, NO_PARENT,
};
use disk_cleanup_core::explore::rows::Badge;
use disk_cleanup_core::explore::tree::{ExploreTree, Inside, NodeId};
use disk_cleanup_core::roots::{CacheTool, RootSet, SteamLibraryRoot};
use disk_cleanup_core::{AppId, Dirs, Identity, Kind, SelectorId};
use disk_cleanup_scan::classify::ParentFacts;
use disk_cleanup_scan::index::DirInfo;

const HOME: &str = "/h";

struct FakeCanon {
    dirs: HashMap<PathBuf, u64>,
    links: HashMap<PathBuf, PathBuf>,
    calls: std::cell::RefCell<Vec<PathBuf>>,
}

impl FakeCanon {
    fn new(dirs: &[&str]) -> FakeCanon {
        FakeCanon {
            dirs: dirs
                .iter()
                .enumerate()
                .map(|(i, d)| (PathBuf::from(d), i as u64 + 1))
                .collect(),
            links: HashMap::new(),
            calls: std::cell::RefCell::new(Vec::new()),
        }
    }

    fn link(mut self, from: &str, to: &str) -> FakeCanon {
        self.links.insert(PathBuf::from(from), PathBuf::from(to));
        self
    }

    fn canon(&self, p: &Path) -> io::Result<(PathBuf, Identity)> {
        self.calls.borrow_mut().push(p.to_path_buf());
        let mut out = PathBuf::from("/");
        for c in p.components() {
            match c {
                Component::Normal(n) => {
                    out.push(n);
                    if let Some(t) = self.links.get(&out) {
                        out = t.clone();
                    }
                }
                Component::ParentDir => {
                    out.pop();
                }
                _ => {}
            }
        }
        let ino = self
            .dirs
            .get(&out)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        Ok((out, Identity { dev: 1, ino: *ino }))
    }
}

fn dirs() -> Dirs {
    Dirs {
        home: HOME.into(),
        state: "/h/.local/state".into(),
        cache: "/h/.cache".into(),
        config: "/h/.config".into(),
    }
}

fn resolve(fs: &FakeCanon, config: Option<&[&str]>) -> RootSet {
    let config: Option<Vec<PathBuf>> = config.map(|c| c.iter().map(PathBuf::from).collect());
    RootSet::resolve(&dirs(), &[], config.as_deref(), Path::new("/h"), &|p| {
        fs.canon(p)
    })
    .unwrap()
}

fn roots_code() -> RootSet {
    let fs = FakeCanon::new(&["/", "/h", "/h/code", "/h/Documents"]);
    resolve(&fs, Some(&["~/code"]))
}

const PJ: ParentFacts = ParentFacts {
    has_package_json: true,
    has_cargo_toml: false,
};
const CT: ParentFacts = ParentFacts {
    has_package_json: false,
    has_cargo_toml: true,
};
const NONE: ParentFacts = ParentFacts {
    has_package_json: false,
    has_cargo_toml: false,
};

fn hint(path: &str, parent: ParentFacts, roots: &RootSet, facts: &ClassifyFacts) -> Hint {
    kind_hint(Path::new(path), parent, Inside::No, roots, facts)
}

fn yes(selector: SelectorId, tier: Tier) -> Hint {
    Hint::Yes { selector, tier }
}

fn shape(selector: SelectorId, tier: Tier) -> Hint {
    Hint::Shape { selector, tier }
}

#[test]
fn build_dirs_should_be_yes_only_when_the_parents_marker_matches_and_the_path_is_inside_the_roots()
{
    let r = roots_code();
    let f = ClassifyFacts::default();
    let nm = yes(SelectorId::BuildDirs, Tier::Destructive);
    assert_eq!(hint("/h/code/app/node_modules", PJ, &r, &f), nm);
    assert_eq!(hint("/h/code/app/target", CT, &r, &f), nm);
    // the sibling check: a node_modules with no package.json, a target with no Cargo.toml, crossed markers
    for (p, pf) in [
        ("/h/code/app/node_modules", NONE),
        ("/h/code/app/node_modules", CT),
        ("/h/code/app/target", NONE),
        ("/h/code/app/target", PJ),
    ] {
        assert_eq!(hint(p, pf, &r, &f), Hint::No(NoReason::NotShape), "{p}");
    }
    assert_eq!(
        hint("/h/Documents/x/node_modules", PJ, &r, &f),
        Hint::No(NoReason::OutsideRoot)
    );
    // a scan root itself is not an item below it
    assert_eq!(hint("/h/code", NONE, &r, &f), Hint::No(NoReason::NotShape));
}

#[test]
fn home_ssh_and_paths_outside_every_root_should_never_be_yes() {
    let r = roots_code();
    let f = ClassifyFacts::default();
    for p in ["/h", "/h/.ssh", "/", "/etc", "/h/code", "/tmp/x/target"] {
        for pf in [NONE, PJ, CT] {
            let got = hint(p, pf, &r, &f);
            assert!(!matches!(got, Hint::Yes { .. }), "{p} {pf:?}: {got:?}");
        }
    }
}

#[test]
fn caches_should_be_yes_only_for_an_exact_path_of_the_facts() {
    let r = roots_code();
    let none = ClassifyFacts::default();
    let facts = ClassifyFacts {
        cache_paths: vec!["/h/.npm/_cacache".into()],
        steam: None,
    };
    let want = yes(SelectorId::Caches, Tier::Safe);
    assert_eq!(hint("/h/.npm/_cacache", NONE, &r, &facts), want);
    // a facts path the cleaner's RootSet does not hold is not claimed
    let foreign = ClassifyFacts {
        cache_paths: vec!["/h/.other/cache".into()],
        steam: None,
    };
    assert_eq!(
        hint("/h/.other/cache", NONE, &r, &foreign),
        Hint::No(NoReason::OutsideRoot)
    );
    assert_eq!(
        hint("/h/.npm/_cacache", NONE, &r, &none),
        Hint::No(NoReason::NotShape)
    );
    // with the cache path really in the RootSet the exact path is Yes, a look-alike is not
    let fs = FakeCanon::new(&["/", "/h", "/h/code"]);
    let mut rs = resolve(&fs, Some(&["~/code"]));
    let facts2 = ClassifyFacts {
        cache_paths: rs.allowed(Kind::Cache).iter().filter_map(exact).collect(),
        steam: None,
    };
    assert!(facts2
        .cache_paths
        .contains(&PathBuf::from("/h/.npm/_cacache")));
    assert_eq!(hint("/h/.npm/_cacache", NONE, &rs, &facts2), want);
    assert_eq!(
        hint("/h/.npm/_cacache2", NONE, &rs, &facts2),
        Hint::No(NoReason::NotShape)
    );
    // a relocated GOCACHE is Yes only once the facts hold it
    rs.add_command_cache(CacheTool::Go, Path::new("/h/.cache/go-build"))
        .unwrap();
    assert_eq!(
        hint("/h/.cache/go-build", NONE, &rs, &facts2),
        Hint::No(NoReason::NotShape)
    );
    let facts3 = ClassifyFacts {
        cache_paths: rs.allowed(Kind::Cache).iter().filter_map(exact).collect(),
        steam: None,
    };
    assert_eq!(hint("/h/.cache/go-build", NONE, &rs, &facts3), want);
    assert_eq!(
        kind_hint(
            Path::new("/h/.cache/go-build"),
            NONE,
            Inside::Yes,
            &rs,
            &facts3
        ),
        Hint::No(NoReason::InsideBuildOrGit)
    );
}

fn exact(t: &disk_cleanup_core::AllowedTarget) -> Option<PathBuf> {
    match t {
        disk_cleanup_core::AllowedTarget::Exact(p) => Some(p.clone()),
        disk_cleanup_core::AllowedTarget::Within { .. }
        | disk_cleanup_core::AllowedTarget::BelowScanRoot(_) => None,
    }
}

#[test]
fn worktrees_should_be_shape_never_yes_and_only_below_dot_claude_worktrees() {
    let r = roots_code();
    let f = ClassifyFacts::default();
    assert_eq!(
        hint("/h/code/app/.claude/worktrees/agent-1", NONE, &r, &f),
        shape(SelectorId::Worktrees, Tier::Guarded)
    );
    for p in [
        "/h/code/app/agent-1",
        "/h/code/app/.claude/agent-1",
        "/h/code/app/.claude/worktrees/agent-",
        "/h/code/app/.claude/worktrees/other",
        "/h/code/app/.claude/worktrees/agent-1/src",
    ] {
        assert_eq!(hint(p, NONE, &r, &f), Hint::No(NoReason::NotShape), "{p}");
    }
    assert_eq!(
        hint("/h/Documents/.claude/worktrees/agent-1", NONE, &r, &f),
        Hint::No(NoReason::OutsideRoot)
    );
}

fn steam_roots(mounted: bool) -> RootSet {
    let fs = FakeCanon::new(&["/", "/h", "/h/code"]);
    resolve(&fs, Some(&["~/code"])).with_steam_libraries(vec![SteamLibraryRoot {
        path: "/lib".into(),
        mounted,
    }])
}

fn steam_facts(installed: &[u64], dirs: &[&str]) -> ClassifyFacts {
    ClassifyFacts {
        cache_paths: Vec::new(),
        steam: Some(vec![SteamLibraryFacts {
            root: "/lib".into(),
            mounted: true,
            installed: installed.iter().map(|&i| AppId(i)).collect(),
            install_dirs: dirs.iter().map(OsString::from).collect(),
        }]),
    }
}

#[test]
fn steam_rows_should_be_yes_for_an_orphan_no_for_an_installed_game_and_shape_when_unknown() {
    let r = steam_roots(true);
    let f = steam_facts(&[620], &["Portal 2"]);
    let prefix = yes(SelectorId::Steam, Tier::Destructive);
    let remnant = yes(SelectorId::Steam, Tier::Guarded);
    assert_eq!(hint("/lib/steamapps/compatdata/999", NONE, &r, &f), prefix);
    assert_eq!(hint("/lib/steamapps/shadercache/999", NONE, &r, &f), prefix);
    assert_eq!(hint("/lib/steamapps/common/OldGame", NONE, &r, &f), remnant);
    for p in [
        "/lib/steamapps/compatdata/620",
        "/lib/steamapps/shadercache/620",
    ] {
        assert_eq!(
            hint(p, NONE, &r, &f),
            Hint::No(NoReason::InstalledSteamGame)
        );
    }
    // installdir is compared case-insensitively, as the engine does
    assert_eq!(
        hint("/lib/steamapps/common/portal 2", NONE, &r, &f),
        Hint::No(NoReason::InstalledSteamGame)
    );
    // a non-Steam shortcut id is never an orphan; a name that is not an id is no prefix
    assert_eq!(
        hint("/lib/steamapps/compatdata/3000000000", NONE, &r, &f),
        Hint::No(NoReason::NotShape)
    );
    assert_eq!(
        hint("/lib/steamapps/compatdata/12a", NONE, &r, &f),
        Hint::No(NoReason::NotShape)
    );
    assert_eq!(
        hint("/lib/steamapps/compatdata/999/pfx", NONE, &r, &f),
        Hint::No(NoReason::NotShape)
    );
    // facts absent or failed: Shape, never Yes
    let absent = ClassifyFacts::default();
    assert_eq!(
        hint("/lib/steamapps/compatdata/999", NONE, &r, &absent),
        shape(SelectorId::Steam, Tier::Destructive)
    );
    // an empty installed set or an unmounted library means the manifests were not read
    for bad in [steam_facts(&[], &[]), {
        let mut g = steam_facts(&[620], &[]);
        g.steam.as_mut().unwrap()[0].mounted = false;
        g
    }] {
        assert_eq!(
            hint("/lib/steamapps/compatdata/999", NONE, &r, &bad),
            shape(SelectorId::Steam, Tier::Destructive)
        );
    }
    // outside a discovered library: OutsideRoot; a library the RootSet holds unmounted bounds nothing
    assert_eq!(
        hint("/other/steamapps/compatdata/999", NONE, &r, &f),
        Hint::No(NoReason::OutsideRoot)
    );
    assert_eq!(
        hint(
            "/lib/steamapps/compatdata/999",
            NONE,
            &steam_roots(false),
            &f
        ),
        Hint::No(NoReason::OutsideRoot)
    );
}

#[test]
fn inside_should_turn_every_shape_into_inside_build_or_git_and_unknown_into_shape_for_build_dirs() {
    let r = steam_roots(true);
    let f = steam_facts(&[620], &[]);
    let cases = [
        ("/h/code/app/node_modules", PJ),
        ("/h/code/app/target", CT),
        ("/h/code/app/.claude/worktrees/agent-1", NONE),
        ("/lib/steamapps/compatdata/999", NONE),
    ];
    for (p, pf) in cases {
        assert_eq!(
            kind_hint(Path::new(p), pf, Inside::Yes, &r, &f),
            Hint::No(NoReason::InsideBuildOrGit),
            "{p}"
        );
    }
    // not a shape: still NotShape
    assert_eq!(
        kind_hint(Path::new("/h/code/app/src"), NONE, Inside::Yes, &r, &f),
        Hint::No(NoReason::NotShape)
    );
    assert_eq!(
        kind_hint(
            Path::new("/h/code/app/node_modules"),
            PJ,
            Inside::Unknown,
            &r,
            &f
        ),
        shape(SelectorId::BuildDirs, Tier::Destructive)
    );
    assert_eq!(
        kind_hint(Path::new("/h/code/app/target"), CT, Inside::Unknown, &r, &f),
        shape(SelectorId::BuildDirs, Tier::Destructive)
    );
    // Unknown says nothing about Steam rows
    assert_eq!(
        kind_hint(
            Path::new("/lib/steamapps/compatdata/999"),
            NONE,
            Inside::Unknown,
            &r,
            &f
        ),
        yes(SelectorId::Steam, Tier::Destructive)
    );
}

#[test]
fn docker_btrfs_and_plain_directories_should_be_not_shape() {
    let r = roots_code();
    let f = ClassifyFacts::default();
    for p in [
        "/var/lib/docker",
        "/h/code/app/src",
        "/h/code/app/node_modules.bak",
    ] {
        assert_eq!(hint(p, PJ, &r, &f), Hint::No(NoReason::NotShape), "{p}");
    }
}

#[test]
fn marking_should_use_the_cleaners_roots_never_the_explore_path_and_the_home_default_should_not_exit_64(
) {
    // The explore PATH is `/` or `/home`, both ancestors of $HOME. It is not an input of `kind_hint` or of
    // `RootSet::resolve`; the recording canon sees only HOME, its ancestors and the configured root.
    let fs = FakeCanon::new(&["/", "/h", "/h/code", "/h/Documents", "/data/x"]);
    let r = resolve(&fs, Some(&["~/code"]));
    let seen = fs.calls.borrow().clone();
    assert!(
        seen.iter()
            .all(|p| ["/h", "/", "/h/code"].contains(&p.to_str().unwrap())),
        "{seen:?}"
    );
    assert_eq!(r.scan_roots(), [PathBuf::from("/h/code")]);
    let f = ClassifyFacts::default();
    for _explore_path in ["/", "/home", "/h"] {
        assert_eq!(
            hint("/h/code/app/node_modules", PJ, &r, &f),
            yes(SelectorId::BuildDirs, Tier::Destructive)
        );
        assert_eq!(
            hint("/h/Documents/x/node_modules", PJ, &r, &f),
            Hint::No(NoReason::OutsideRoot)
        );
    }
    // `explore /data/x`: a real node_modules there has no badge and the Kind line says why
    let shape_kind = shape_of(Path::new("/data/x/node_modules"), PJ, &f);
    assert_eq!(shape_kind, Some(Kind::NodeModules));
    let h = hint("/data/x/node_modules", PJ, &r, &f);
    assert_eq!(h, Hint::No(NoReason::OutsideRoot));
    assert_eq!(h.badge(), Badge::None);
    assert_eq!(
        kind_line(h, shape_kind),
        "build-dirs shape, outside the cleaner's folders: not markable"
    );
    assert_eq!(
        r.scan_roots()
            .iter()
            .filter(|p| p.as_path() == Path::new("/data/x"))
            .count(),
        0
    );
}

#[test]
fn with_no_cleaner_folder_every_row_should_be_outside_root_or_not_shape() {
    let fs = FakeCanon::new(&["/", "/h"]);
    let r = resolve(&fs, None);
    assert!(r.scan_roots().is_empty());
    let f = ClassifyFacts::default();
    assert_eq!(
        hint("/h/code/app/node_modules", PJ, &r, &f),
        Hint::No(NoReason::OutsideRoot)
    );
    assert_eq!(
        hint("/h/code/app/src", PJ, &r, &f),
        Hint::No(NoReason::NotShape)
    );
}

#[test]
fn a_symlinked_cleaner_root_should_be_browsable_by_its_real_path() {
    // ~/code -> /data/code, roots [~/code]: resolve canonicalises once to /data/code
    let fs = FakeCanon::new(&["/", "/h", "/data", "/data/code"]).link("/h/code", "/data/code");
    let r = resolve(&fs, Some(&["~/code"]));
    assert_eq!(r.scan_roots(), [PathBuf::from("/data/code")]);
    let f = ClassifyFacts::default();
    assert_eq!(
        hint("/data/code/app/node_modules", PJ, &r, &f),
        yes(SelectorId::BuildDirs, Tier::Destructive)
    );
    // the pasted spelling through the link is not a root-relative path: no badge
    assert_eq!(
        hint("/h/code/app/node_modules", PJ, &r, &f),
        Hint::No(NoReason::OutsideRoot)
    );
}

#[test]
fn hint_of_node_should_read_the_parents_flags_and_the_ancestors_from_the_tree() {
    // /h/code/{app{package.json}/{node_modules/foo{package.json}/node_modules, src}, .git/node_modules}
    let rows: &[(&str, bool, bool)] = &[
        ("app", true, false),
        ("app/node_modules", false, false),
        ("app/node_modules/foo", true, false),
        ("app/node_modules/foo/node_modules", false, false),
        ("app/src", false, false),
        ("app/.git", true, false),
        ("app/.git/node_modules", false, false),
        ("bare", false, false),
        ("bare/node_modules", false, false),
    ];
    let t = tree("/h/code", rows);
    let r = roots_code();
    let f = ClassifyFacts::default();
    let at = |rel: &str| find(&t, &format!("/h/code/{rel}"));
    let h = |rel: &str| hint_of_node(&t, at(rel), &r, &f);
    assert_eq!(
        h("app/node_modules"),
        yes(SelectorId::BuildDirs, Tier::Destructive)
    );
    assert_eq!(
        h("app/node_modules/foo/node_modules"),
        Hint::No(NoReason::InsideBuildOrGit)
    );
    assert_eq!(
        h("app/.git/node_modules"),
        Hint::No(NoReason::InsideBuildOrGit)
    );
    assert_eq!(h("bare/node_modules"), Hint::No(NoReason::NotShape));
    assert_eq!(h("app/src"), Hint::No(NoReason::NotShape));
}

#[test]
fn hint_of_node_should_be_shape_when_a_build_dir_sits_under_a_scan_root_named_like_one() {
    // the scan root itself is `/h/code/node_modules`: its ancestors above the root are not in the tree
    let t = tree(
        "/h/code/node_modules",
        &[("x", true, false), ("x/node_modules", false, false)],
    );
    let r = roots_code();
    let id = find(&t, "/h/code/node_modules/x/node_modules");
    assert_eq!(
        hint_of_node(&t, id, &r, &ClassifyFacts::default()),
        shape(SelectorId::BuildDirs, Tier::Destructive)
    );
}

fn tree(root: &str, rows: &[(&str, bool, bool)]) -> ExploreTree {
    let mut ids: HashMap<&str, u64> = HashMap::new();
    let mut events = vec![NodeEvent::Dir {
        id: 0,
        parent: NO_PARENT,
        name: root.into(),
        dev: 1,
        info: DirInfo::default(),
        subdirs: rows.iter().filter(|(p, ..)| !p.contains('/')).count() as u32,
        from_cache: false,
        painted: false,
    }];
    for (i, (p, ..)) in rows.iter().enumerate() {
        ids.insert(p, i as u64 + 1);
    }
    for (i, (p, pkg, cargo)) in rows.iter().enumerate() {
        let (parent, name) = match p.rsplit_once('/') {
            Some((pp, n)) => (ids[pp], n),
            None => (0, *p),
        };
        let prefix = format!("{p}/");
        let kids = rows
            .iter()
            .filter(|(q, ..)| q.strip_prefix(&prefix).is_some_and(|r| !r.contains('/')))
            .count() as u32;
        events.push(NodeEvent::Dir {
            id: i as u64 + 1,
            parent,
            name: name.into(),
            dev: 1,
            info: DirInfo {
                has_package_json: *pkg,
                has_cargo_toml: *cargo,
                ..DirInfo::default()
            },
            subdirs: kids,
            from_cache: false,
            painted: false,
        });
    }
    let mut t = ExploreTree::new();
    t.apply_batch(0, ScanMode::Fresh, events).unwrap();
    t.finish(false);
    t
}

fn find(t: &ExploreTree, path: &str) -> NodeId {
    t.iter()
        .find(|&n| t.path_of(n) == Path::new(path))
        .unwrap_or_else(|| panic!("no node {path}"))
}

#[test]
fn the_hint_should_decide_build_dirs_only_through_classify_dir() {
    let src = include_str!("../src/explore/hint.rs");
    assert!(
        src.contains("classify_dir("),
        "hint.rs must call classify_dir"
    );
    for lit in [
        "\"node_modules\"",
        "\"target\"",
        "b\"node_modules\"",
        "b\"target\"",
    ] {
        assert!(!src.contains(lit), "hint.rs holds a second table: {lit}");
    }
}

#[test]
fn kind_lines_should_follow_the_plan_wording() {
    let d = yes(SelectorId::BuildDirs, Tier::Destructive);
    assert_eq!(
        kind_line(d, None),
        "build-dirs (destructive tier): the cleaner lists it"
    );
    assert_eq!(
        kind_line(shape(SelectorId::Worktrees, Tier::Guarded), None),
        "worktrees (guarded): possible candidate, p checks"
    );
    assert_eq!(
        kind_line(Hint::No(NoReason::NotShape), None),
        "not a cleaner item: deleted here as a plain path"
    );
    assert_eq!(d.badge(), Badge::Candidate);
    assert_eq!(
        shape(SelectorId::Worktrees, Tier::Guarded).badge(),
        Badge::Possible
    );
}

#[test]
fn a_tree_root_named_like_a_build_dir_should_be_shape_because_its_marker_files_are_unknown() {
    let t = tree("/h/code/app/node_modules", &[("x", false, false)]);
    let r = roots_code();
    let root = t.roots().next().unwrap();
    assert_eq!(
        hint_of_node(&t, root, &r, &ClassifyFacts::default()),
        shape(SelectorId::BuildDirs, Tier::Destructive)
    );
    // a root that is not build-dir shaped stays a plain path
    let t = tree("/h/code/app", &[]);
    let root = t.roots().next().unwrap();
    assert_eq!(
        hint_of_node(&t, root, &r, &ClassifyFacts::default()),
        Hint::No(NoReason::NotShape)
    );
}
