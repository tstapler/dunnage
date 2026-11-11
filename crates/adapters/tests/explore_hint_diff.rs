#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    non_snake_case
)]
//! Story 6.4.1 (Task 6.4.1c): the explorer's `kind_hint` against the engine's own discovery over the same
//! directory tree. The plan puts this file in `core/tests`; the engine's discovery (`discover_build_dirs`,
//! `discover_caches`, `discover_worktrees`, the Steam listers) lives in `adapters`, which depends on core, so
//! the test sits here (defect recorded in phase5-findings.md).
//!
//! The explorer side folds the real walker's events into an `ExploreTree` and asks `hint_of_node` for every
//! node. The engine side is the candidate set each kind lists before its guards. Invariants:
//! (1) every `Yes` path is a candidate, (2) every candidate that is a tree node is `Yes` or `Shape`,
//! (3) for build-dirs and caches the `Yes` set equals the candidate set.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use disk_cleanup_adapters::discover_build::{discover_build_dirs, BuildEnv, BuildKinds, BuildOpts};
use disk_cleanup_adapters::discover_cache::{discover_caches, CacheEnv};
use disk_cleanup_adapters::steam::orphans::list_orphans;
use disk_cleanup_adapters::steam::remnants::list_remnants;
use disk_cleanup_adapters::steam::{library_roots, SteamInstall};
use disk_cleanup_adapters::targets::worktree_discover::discover_worktrees;
use disk_cleanup_adapters::RealDirOps;
use disk_cleanup_core::explore::hint::{hint_of_node, kind_hint, shape_of};
use disk_cleanup_core::explore::msg::{
    ClassifyFacts, Hint, NodeEvent, ScanMode, SteamLibraryFacts,
};
use disk_cleanup_core::explore::tree::{ExploreTree, Inside, NodeId};
use disk_cleanup_core::ports::CmdOutput;
use disk_cleanup_core::roots::RootSet;
use disk_cleanup_core::testing::{FakeClock, FakeCommandRunner, FakeProbe};
use disk_cleanup_core::{AppId, Dirs, Identity, Kind, Locator, SelectorId};
use disk_cleanup_scan::classify::ParentFacts;
use disk_cleanup_scan::index::{Backend, CollectRequest, StreamRequest};
use disk_cleanup_scan::walker::WalkerBackend;
use proptest::prelude::*;

// ---------------------------------------------------------------- fixture

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
}

struct Site {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    home: PathBuf,
    code: PathBuf,
    steam_root: PathBuf,
    /// `git worktree list` answers: main repo to registered worktree paths.
    registered: BTreeMap<PathBuf, Vec<PathBuf>>,
}

fn site() -> Site {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let home = base.join("home");
    let code = home.join("code");
    fs::create_dir_all(&code).unwrap();
    let steam_root = home.join("Steam");
    fs::create_dir_all(steam_root.join("steamapps")).unwrap();
    Site {
        _tmp: tmp,
        base,
        home,
        code,
        steam_root,
        registered: BTreeMap::new(),
    }
}

fn mkdir(p: &Path) {
    fs::create_dir_all(p).unwrap();
}

fn touch(p: &Path) {
    mkdir(p.parent().unwrap());
    fs::write(p, b"{}").unwrap();
}

/// One random directory below `dir`. `special` counts the build or `.git` ancestors above it, so worktree
/// shapes are generated only where the cleaner and the hint agree on `Inside` (see the defect note).
fn grow(
    rng: &mut Rng,
    site: &mut Site,
    dir: &Path,
    depth: usize,
    special: bool,
    budget: &mut usize,
) {
    if depth >= 6 || *budget == 0 {
        return;
    }
    for marker in ["package.json", "Cargo.toml"] {
        match rng.below(10) {
            0..=3 => touch(&dir.join(marker)),
            4 => {
                let real = site.base.join(format!("real-{marker}"));
                fs::write(&real, b"{}").unwrap();
                let _ = symlink(&real, dir.join(marker));
            }
            _ => {}
        }
    }
    let names = [
        "node_modules",
        "target",
        "src",
        ".git",
        "foo",
        "app",
        ".claude",
        "agent-9",
    ];
    for _ in 0..=rng.below(3) {
        if *budget == 0 {
            return;
        }
        let name = names[rng.below(names.len())];
        let child = dir.join(name);
        if name == ".claude" {
            if special {
                continue;
            }
            for k in 1..=2 {
                let wt = child.join("worktrees").join(format!("agent-{k}"));
                mkdir(&wt);
                *budget = budget.saturating_sub(1);
                if rng.chance(50) {
                    site.registered
                        .entry(dir.to_path_buf())
                        .or_default()
                        .push(wt);
                }
            }
            mkdir(&child.join("agent-3"));
            continue;
        }
        mkdir(&child);
        *budget = budget.saturating_sub(1);
        let inner_special = special || matches!(name, "node_modules" | "target" | ".git");
        grow(rng, site, &child, depth + 1, inner_special, budget);
    }
}

fn vdf_escape(p: &Path) -> String {
    p.to_str().unwrap().replace('\\', "\\\\")
}

fn write_folders(site: &Site, libs: &[PathBuf]) {
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
    fs::write(site.steam_root.join("steamapps/libraryfolders.vdf"), s).unwrap();
}

fn manifest(lib: &Path, appid: u32, installdir: &str) {
    let body = format!(
        "\"AppState\"\n{{\n\t\"appid\"\t\t\"{appid}\"\n\t\"name\"\t\t\"Game {appid}\"\n\t\"installdir\"\t\t\"{installdir}\"\n\t\"StateFlags\"\t\t\"4\"\n}}\n"
    );
    fs::write(lib.join(format!("steamapps/appmanifest_{appid}.acf")), body).unwrap();
}

/// Libraries `Steam` and `lib2`, game 620 (`Portal 2`) installed in the first, 730 in the second, and a
/// random mix of prefixes, shader caches and remnants.
fn steam_fixture(rng: &mut Rng, site: &Site) {
    let lib2 = site.home.join("lib2");
    mkdir(&lib2.join("steamapps"));
    let libs = [site.steam_root.clone(), lib2.clone()];
    write_folders(site, &libs);
    manifest(&libs[0], 620, "Portal 2");
    manifest(&lib2, 730, "CS");
    for lib in &libs {
        for sub in ["compatdata", "shadercache"] {
            for id in ["620", "730", "99999", "12a", "3000000000"] {
                if rng.chance(60) {
                    mkdir(&lib.join("steamapps").join(sub).join(id).join("pfx"));
                }
            }
        }
        for name in [
            "Portal 2",
            "portal 2",
            "CS",
            "OldGame",
            ".hidden",
            "Other Game",
        ] {
            if rng.chance(60) {
                mkdir(&lib.join("steamapps/common").join(name));
            }
        }
    }
}

fn cache_fixture(rng: &mut Rng, site: &Site) {
    for rel in [
        ".npm/_cacache",
        ".npm/_cacache2",
        ".gradle/caches",
        ".cargo/registry/cache",
        ".cargo/registry/src",
        ".cache/ms-playwright",
        ".cache/go-build",
    ] {
        match rng.below(6) {
            0..=2 => mkdir(&site.home.join(rel).join("x")),
            3 => {
                let target = site.base.join("elsewhere").join(rel.replace('/', "_"));
                mkdir(&target);
                let link = site.home.join(rel);
                mkdir(link.parent().unwrap());
                symlink(&target, link).unwrap();
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------- the two sides

struct World {
    tree: ExploreTree,
    roots: RootSet,
    facts: ClassifyFacts,
    /// The engine's candidates before its guards, by kind.
    build: BTreeSet<PathBuf>,
    cache: BTreeSet<PathBuf>,
    worktree: BTreeSet<PathBuf>,
    steam: BTreeSet<PathBuf>,
}

impl World {
    fn all(&self) -> BTreeSet<PathBuf> {
        [&self.build, &self.cache, &self.worktree, &self.steam]
            .into_iter()
            .flatten()
            .cloned()
            .collect()
    }
}

fn canon(p: &Path) -> std::io::Result<(PathBuf, Identity)> {
    let c = p.canonicalize()?;
    let m = fs::metadata(&c)?;
    Ok((c, Identity::of(&m)))
}

fn porcelain(main: &Path, wts: &[PathBuf]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rec = |p: &Path| {
        out.extend_from_slice(b"worktree ");
        out.extend_from_slice(p.to_str().unwrap().as_bytes());
        out.extend_from_slice(b"\0HEAD 0123456789abcdef0123456789abcdef01234567\0detached\0\0");
    };
    rec(main);
    for w in wts {
        rec(w);
    }
    out
}

fn walk(roots: &[PathBuf]) -> ExploreTree {
    let req = StreamRequest::new(
        CollectRequest {
            roots,
            cross_fs: false,
            deadline: None,
            threads: 2,
        },
        Arc::new(AtomicBool::new(false)),
    );
    let events = Mutex::new(Vec::new());
    WalkerBackend::default()
        .collect_streaming(&req, &|e| events.lock().unwrap().extend(e))
        .unwrap();
    let mut tree = ExploreTree::new();
    let events: Vec<NodeEvent> = events
        .into_inner()
        .unwrap()
        .into_iter()
        .filter_map(NodeEvent::from_dir_event)
        .collect();
    tree.apply_batch(0, ScanMode::Fresh, events).unwrap();
    tree.finish(false);
    tree
}

fn candidates_of(items: impl IntoIterator<Item = PathBuf>) -> BTreeSet<PathBuf> {
    items.into_iter().collect()
}

fn world(site: &Site, with_steam: bool, explore: &[PathBuf]) -> World {
    let dirs = Dirs {
        home: site.home.clone(),
        state: site.home.join(".local/state"),
        cache: site.home.join(".cache"),
        config: site.home.join(".config"),
    };
    let mut roots = RootSet::resolve(
        &dirs,
        std::slice::from_ref(&site.code),
        None,
        &site.home,
        &canon,
    )
    .unwrap();
    if with_steam {
        roots = roots.with_steam_libraries(library_roots(&site.steam_root));
    }

    // build-dirs: the scan report's build roots, wrapped in items whatever their verdict
    let ops = RealDirOps::confined_to(&site.home);
    let (probe, clock) = (FakeProbe::new(), FakeClock::at(1_900_000_000));
    let env = BuildEnv {
        ops: &ops,
        roots: &roots,
        probe: &probe,
        clock: &clock,
    };
    let opts = BuildOpts {
        min_age: std::time::Duration::from_hours(1),
        kinds: BuildKinds::ALL,
        max_duration: None,
        rescan: false,
        walker_cache: None,
    };
    let build = candidates_of(
        discover_build_dirs(&env, &opts)
            .unwrap()
            .items
            .iter()
            .filter_map(|i| match i.candidate.locator() {
                Locator::Path(p) => Some(p.clone()),
                _ => None,
            }),
    );

    // caches: the table plus the path `go env GOCACHE` reports; the facts hold what the RootSet then allows
    let runner = FakeCommandRunner::new();
    runner.on(
        &["go", "env", "GOCACHE"],
        Ok(CmdOutput {
            stdout: format!("{}\n", site.home.join(".cache/go-build").display()).into_bytes(),
            stderr: Vec::new(),
        }),
    );
    runner.absent("pip").absent("pip3");
    let cenv = CacheEnv {
        fs_root: &site.base.join("fsroot"),
        ops: &ops,
        runner: &runner,
        probe: &probe,
    };
    let cache = candidates_of(
        discover_caches(&cenv, &mut roots)
            .items
            .iter()
            .filter_map(|i| match i.candidate.locator() {
                Locator::Path(p) => Some(p.clone()),
                _ => None,
            }),
    );
    let cache_paths = roots
        .allowed(Kind::Cache)
        .into_iter()
        .filter_map(|t| match t {
            disk_cleanup_core::AllowedTarget::Exact(p) => Some(p),
            disk_cleanup_core::AllowedTarget::Within { .. }
            | disk_cleanup_core::AllowedTarget::BelowScanRoot(_) => None,
        })
        .collect();

    // worktrees: one `git worktree list` per main repo, as the engine does
    let git = FakeCommandRunner::new();
    let mains: Vec<PathBuf> = site.registered.keys().cloned().collect();
    for (main, wts) in &site.registered {
        git.on(
            &[
                "git",
                "-C",
                main.to_str().unwrap(),
                "worktree",
                "list",
                "--porcelain",
                "-z",
            ],
            Ok(CmdOutput {
                stdout: porcelain(main, wts),
                stderr: Vec::new(),
            }),
        );
    }
    let worktree = candidates_of(
        discover_worktrees(&git, &mains)
            .sites
            .into_iter()
            .map(|s| s.entry.path),
    );

    // steam: the engine's listers over the installed set, and the facts a loader would build from it
    let (steam, steam_facts) = if with_steam {
        match SteamInstall::load(&site.steam_root) {
            Ok(install) => {
                let listed = if install.check_installed_set().is_ok() {
                    list_orphans(&install)
                        .into_iter()
                        .map(|o| o.path)
                        .chain(list_remnants(&install).into_iter().map(|r| r.path))
                        .collect()
                } else {
                    BTreeSet::new()
                };
                let libs = install
                    .libraries
                    .iter()
                    .map(|l| SteamLibraryFacts {
                        root: l.clone(),
                        mounted: l.join("steamapps").is_dir(),
                        installed: install
                            .installed
                            .iter()
                            .filter(|a| &a.library == l)
                            .map(|a| AppId(u64::from(a.appid)))
                            .collect(),
                        install_dirs: install
                            .installed
                            .iter()
                            .filter(|a| &a.library == l)
                            .map(|a| OsString::from(&a.installdir))
                            .collect(),
                    })
                    .collect();
                (listed, Some(libs))
            }
            Err(_) => (BTreeSet::new(), None),
        }
    } else {
        (BTreeSet::new(), None)
    };

    World {
        tree: walk(explore),
        roots,
        facts: ClassifyFacts {
            cache_paths,
            steam: steam_facts,
        },
        build,
        cache,
        worktree,
        steam,
    }
}

// ---------------------------------------------------------------- the check

type HintFn = dyn Fn(&ExploreTree, NodeId, &RootSet, &ClassifyFacts) -> Hint;

fn real(t: &ExploreTree, id: NodeId, roots: &RootSet, facts: &ClassifyFacts) -> Hint {
    hint_of_node(t, id, roots, facts)
}

/// The inputs `hint_of_node` derives from the tree, for the mutants that tamper with one of them.
fn parts(t: &ExploreTree, id: NodeId) -> (PathBuf, ParentFacts, Inside) {
    let parent = t
        .parent(id)
        .map_or_else(ParentFacts::default, |p| ParentFacts {
            has_package_json: t.has_package_json(p),
            has_cargo_toml: t.has_cargo_toml(p),
        });
    (t.path_of(id), parent, t.inside_special(id))
}

fn hints(w: &World, f: &HintFn) -> BTreeMap<PathBuf, Hint> {
    let t = &w.tree;
    t.iter()
        .map(|id: NodeId| (t.path_of(id), f(t, id, &w.roots, &w.facts)))
        .collect()
}

/// The violations of the three invariants; empty when the hint and the engine agree.
fn violations(w: &World, f: &HintFn) -> Vec<String> {
    let h = hints(w, f);
    let all = w.all();
    let mut bad = Vec::new();
    for (p, hint) in &h {
        if matches!(hint, Hint::Yes { .. }) && !all.contains(p) {
            bad.push(format!(
                "(1) Yes but the engine lists nothing: {}",
                p.display()
            ));
        }
    }
    for c in &all {
        match h.get(c) {
            Some(Hint::Yes { .. } | Hint::Shape { .. }) | None => {}
            Some(other) => bad.push(format!(
                "(2) candidate hidden as {other:?}: {}",
                c.display()
            )),
        }
    }
    // (3) build-dirs and caches: Yes is exactly the candidate set, except where the tree cannot know
    // (a tree root has no parent flags, a node below an unknown ancestor has no `inside`), which is Shape
    let t = &w.tree;
    let unknowable: BTreeSet<PathBuf> = t
        .iter()
        .filter(|&id| t.parent(id).is_none() || t.inside_special(id) == Inside::Unknown)
        .map(|id| t.path_of(id))
        .collect();
    let exact: BTreeSet<&PathBuf> = w.build.iter().chain(&w.cache).collect();
    for (p, hint) in &h {
        let yes_exact = matches!(
            hint,
            Hint::Yes {
                selector: SelectorId::BuildDirs | SelectorId::Caches,
                ..
            }
        );
        let listed = exact.contains(p);
        let should_be_yes = listed && !unknowable.contains(p);
        if yes_exact != listed && (yes_exact || should_be_yes) {
            bad.push(format!(
                "(3) Yes set differs from the candidate set at {}",
                p.display()
            ));
        }
    }
    bad
}

// ---------------------------------------------------------------- the fixed corpus

fn fixed_site() -> Site {
    let mut s = site();
    let c = s.code.clone();
    // plain build dirs
    touch(&c.join("a/package.json"));
    mkdir(&c.join("a/node_modules/dep"));
    touch(&c.join("r/Cargo.toml"));
    mkdir(&c.join("r/target/debug"));
    // look-alikes: no sibling file
    mkdir(&c.join("nopkg/node_modules"));
    mkdir(&c.join("nocargo/target"));
    // a symlinked package.json: metascope sets the flag only for regular files
    let real_pj = s.base.join("real-package.json");
    fs::write(&real_pj, "{}").unwrap();
    mkdir(&c.join("sym/node_modules"));
    symlink(&real_pj, c.join("sym/package.json")).unwrap();
    // nested look-alikes: inside .git, inside node_modules/foo/, a target inside a target
    touch(&c.join("g/.git/package.json"));
    mkdir(&c.join("g/.git/node_modules"));
    touch(&c.join("n/package.json"));
    touch(&c.join("n/node_modules/foo/package.json"));
    mkdir(&c.join("n/node_modules/foo/node_modules"));
    touch(&c.join("t/Cargo.toml"));
    touch(&c.join("t/target/Cargo.toml"));
    mkdir(&c.join("t/target/target"));
    // worktrees: agent-1 outside .claude/worktrees, agent-2 unregistered, agent-3 registered
    mkdir(&c.join("w/agent-1"));
    mkdir(&c.join("w/.claude/worktrees/agent-2"));
    mkdir(&c.join("w/.claude/worktrees/agent-3"));
    s.registered
        .insert(c.join("w"), vec![c.join("w/.claude/worktrees/agent-3")]);
    // caches: a table path, its look-alike, a symlinked cache root, the relocated GOCACHE
    mkdir(&s.home.join(".npm/_cacache/x"));
    mkdir(&s.home.join(".npm/_cacache2/x"));
    let elsewhere = s.base.join("elsewhere");
    mkdir(&elsewhere);
    mkdir(&s.home.join(".cargo/registry"));
    symlink(&elsewhere, s.home.join(".cargo/registry/cache")).unwrap();
    mkdir(&s.home.join(".cache/go-build/x"));
    // Steam: an installed game, an orphan prefix, an orphan shader cache and a remnant
    let mut rng = Rng::new(7);
    steam_fixture(&mut rng, &s);
    mkdir(&s.steam_root.join("steamapps/compatdata/620/pfx"));
    mkdir(&s.steam_root.join("steamapps/compatdata/99999/pfx"));
    mkdir(&s.steam_root.join("steamapps/shadercache/99999"));
    mkdir(&s.steam_root.join("steamapps/common/Portal 2"));
    mkdir(&s.steam_root.join("steamapps/common/OldGame"));
    s
}

fn hint_at(w: &World, path: &Path) -> Hint {
    hints(w, &real)
        .remove(path)
        .unwrap_or_else(|| panic!("{} is not a tree node", path.display()))
}

#[test]
fn the_hint_should_agree_with_the_engine_on_the_fixed_look_alikes() {
    let s = fixed_site();
    let w = world(&s, true, std::slice::from_ref(&s.home));
    let bad = violations(&w, &real);
    assert!(bad.is_empty(), "{bad:#?}");

    let c = &s.code;
    let yes = |sel, tier| Hint::Yes {
        selector: sel,
        tier,
    };
    let nm = yes(
        SelectorId::BuildDirs,
        disk_cleanup_core::explore::msg::Tier::Destructive,
    );
    assert_eq!(hint_at(&w, &c.join("a/node_modules")), nm);
    assert_eq!(hint_at(&w, &c.join("r/target")), nm);
    let not_shape = Hint::No(disk_cleanup_core::explore::msg::NoReason::NotShape);
    let inside = Hint::No(disk_cleanup_core::explore::msg::NoReason::InsideBuildOrGit);
    for p in [
        "nopkg/node_modules",
        "nocargo/target",
        "sym/node_modules",
        "w/agent-1",
    ] {
        assert_eq!(hint_at(&w, &c.join(p)), not_shape, "{p}");
        assert!(!w.all().contains(&c.join(p)), "{p} is a candidate");
    }
    for p in [
        "g/.git/node_modules",
        "n/node_modules/foo/node_modules",
        "t/target/target",
    ] {
        assert_eq!(hint_at(&w, &c.join(p)), inside, "{p}");
        assert!(!w.all().contains(&c.join(p)), "{p} is a candidate");
    }
    // worktrees: Shape; the unregistered one is no candidate, the registered one is
    let wt = Hint::Shape {
        selector: SelectorId::Worktrees,
        tier: disk_cleanup_core::explore::msg::Tier::Guarded,
    };
    assert_eq!(hint_at(&w, &c.join("w/.claude/worktrees/agent-2")), wt);
    assert_eq!(hint_at(&w, &c.join("w/.claude/worktrees/agent-3")), wt);
    assert!(!w.worktree.contains(&c.join("w/.claude/worktrees/agent-2")));
    assert!(w.worktree.contains(&c.join("w/.claude/worktrees/agent-3")));
    // caches
    let cache = yes(
        SelectorId::Caches,
        disk_cleanup_core::explore::msg::Tier::Safe,
    );
    assert_eq!(hint_at(&w, &s.home.join(".npm/_cacache")), cache);
    assert_eq!(hint_at(&w, &s.home.join(".npm/_cacache2")), not_shape);
    assert_eq!(hint_at(&w, &s.home.join(".cache/go-build")), cache);
    assert!(w.cache.contains(&s.home.join(".cargo/registry/cache")));
    assert!(w
        .tree
        .iter()
        .all(|n| w.tree.path_of(n) != s.home.join(".cargo/registry/cache")));
    // steam
    let prefix = yes(
        SelectorId::Steam,
        disk_cleanup_core::explore::msg::Tier::Destructive,
    );
    let installed = Hint::No(disk_cleanup_core::explore::msg::NoReason::InstalledSteamGame);
    let lib = &s.steam_root.join("steamapps");
    assert_eq!(hint_at(&w, &lib.join("compatdata/99999")), prefix);
    assert_eq!(hint_at(&w, &lib.join("compatdata/620")), installed);
    assert_eq!(hint_at(&w, &lib.join("common/Portal 2")), installed);
    assert_eq!(
        hint_at(&w, &lib.join("common/OldGame")),
        yes(
            SelectorId::Steam,
            disk_cleanup_core::explore::msg::Tier::Guarded
        )
    );
}

#[test]
fn a_relocated_gocache_should_be_yes_only_once_the_facts_hold_it() {
    let s = fixed_site();
    let mut w = world(&s, true, std::slice::from_ref(&s.home));
    let go = s.home.join(".cache/go-build");
    assert!(w.cache.contains(&go));
    w.facts.cache_paths.retain(|p| p != &go);
    assert_eq!(
        hint_at(&w, &go),
        Hint::No(disk_cleanup_core::explore::msg::NoReason::NotShape)
    );
    let bad = violations(&w, &real);
    assert!(
        bad.iter()
            .any(|b| b.starts_with("(2)") && b.contains("go-build")),
        "the engine lists the cache the facts did not hold yet: {bad:?}"
    );
}

#[test]
fn nested_look_alikes_above_the_explore_root_should_be_shape_not_yes() {
    let s = fixed_site();
    // the explorer is started inside the outer node_modules: its ancestors are not in the tree
    let inner = s.code.join("n/node_modules/foo");
    let w = world(&s, true, std::slice::from_ref(&inner));
    let bad = violations(&w, &real);
    assert!(bad.is_empty(), "{bad:#?}");
    assert_eq!(
        hint_at(&w, &inner.join("node_modules")),
        Hint::Shape {
            selector: SelectorId::BuildDirs,
            tier: disk_cleanup_core::explore::msg::Tier::Destructive
        }
    );
    // and a target inside a target, started inside the outer one
    let outer = s.code.join("t/target");
    let w = world(&s, true, std::slice::from_ref(&outer));
    let bad = violations(&w, &real);
    assert!(bad.is_empty(), "{bad:#?}");
    assert!(matches!(
        hint_at(&w, &outer.join("target")),
        Hint::Shape { .. }
    ));
}

#[test]
fn a_path_equal_to_a_scan_root_should_not_be_a_candidate_or_a_yes() {
    let s = fixed_site();
    let w = world(&s, true, std::slice::from_ref(&s.home));
    assert!(w.roots.scan_roots().contains(&s.code));
    assert!(!w.all().contains(&s.code));
    assert!(!matches!(
        hint_at(&w, &s.code),
        Hint::Yes { .. } | Hint::Shape { .. }
    ));
}

// ---------------------------------------------------------------- the mutants

#[test]
fn the_three_mutants_should_fail_the_fixed_corpus() {
    let s = fixed_site();
    let w = world(&s, true, std::slice::from_ref(&s.home));
    assert!(violations(&w, &real).is_empty());

    // dropping the sibling check: the markers of the parent are assumed present
    let no_sibling = |t: &ExploreTree, id: NodeId, r: &RootSet, f: &ClassifyFacts| {
        let (p, _, i) = parts(t, id);
        let both = ParentFacts {
            has_package_json: true,
            has_cargo_toml: true,
        };
        kind_hint(&p, both, i, r, f)
    };
    assert!(
        !violations(&w, &no_sibling).is_empty(),
        "sibling mutant survived"
    );

    // treating Shape as Yes
    let shape_is_yes =
        |t: &ExploreTree, id: NodeId, r: &RootSet, f: &ClassifyFacts| match real(t, id, r, f) {
            Hint::Shape { selector, tier } => Hint::Yes { selector, tier },
            other => other,
        };
    assert!(
        !violations(&w, &shape_is_yes).is_empty(),
        "shape mutant survived"
    );

    // a kind_hint that ignores `inside`
    let ignores_inside = |t: &ExploreTree, id: NodeId, r: &RootSet, f: &ClassifyFacts| {
        let (p, pf, _) = parts(t, id);
        kind_hint(&p, pf, Inside::No, r, f)
    };
    assert!(
        !violations(&w, &ignores_inside).is_empty(),
        "inside mutant survived"
    );

    // shape_of is public and lexical: it never depends on the roots
    assert_eq!(
        shape_of(
            Path::new("/x/node_modules"),
            ParentFacts::default(),
            &ClassifyFacts::default()
        ),
        None
    );
}

// ---------------------------------------------------------------- the generative corpus

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    #[test]
    fn the_hint_should_never_fork_from_the_engine_on_random_trees(seed in any::<u64>()) {
        let mut rng = Rng::new(seed);
        let mut s = site();
        let mut budget = 70;
        let code = s.code.clone();
        grow(&mut rng, &mut s, &code, 0, false, &mut budget);
        cache_fixture(&mut rng, &s);
        let with_steam = rng.chance(70);
        if with_steam {
            steam_fixture(&mut rng, &s);
        }
        let w = world(&s, with_steam, std::slice::from_ref(&s.home));
        let bad = violations(&w, &real);
        let h = hints(&w, &real);
        let yes = h.values().filter(|v| matches!(v, Hint::Yes { .. })).count();
        let shape = h.values().filter(|v| matches!(v, Hint::Shape { .. })).count();
        eprintln!("seed {seed}: {} nodes, {} candidates, {yes} yes, {shape} shape", h.len(), w.all().len());
        prop_assert!(bad.is_empty(), "{:#?}", bad);
    }
}
