#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! The explorer's hints for the cargo-aware kinds: a part of a cargo target (`target/<profile>/incremental`,
//! `deps`, ...) is a possible `build-dirs` item, an unpacked crate or git checkout below `CARGO_HOME` a possible
//! `cargo-home` item. The explorer has no ages and no `CACHEDIR.TAG`, so neither is ever a certain one.

use std::collections::HashMap;
use std::io;
use std::path::{Component, Path, PathBuf};

use dunnage_core::explore::hint::{hint_of_node, kind_hint, shape_of};
use dunnage_core::explore::msg::{
    ClassifyFacts, Hint, NoReason, NodeEvent, ScanMode, Tier, NO_PARENT,
};
use dunnage_core::explore::tree::{ExploreTree, Inside, NodeId};
use dunnage_core::roots::RootSet;
use dunnage_core::{Dirs, Identity, Kind, SelectorId};
use dunnage_scan::classify::ParentFacts;
use dunnage_scan::index::DirInfo;

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

    #[allow(dead_code)]
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

fn roots() -> RootSet {
    let fs = FakeCanon::new(&["/", "/h", "/h/code"]);
    RootSet::resolve(
        &dirs(),
        &[],
        Some(&[PathBuf::from("~/code")]),
        Path::new("/h"),
        &|p| fs.canon(p),
    )
    .unwrap()
}

const NONE: ParentFacts = ParentFacts {
    has_package_json: false,
    has_cargo_toml: false,
};

fn part() -> Hint {
    Hint::Shape {
        selector: SelectorId::BuildDirs,
        tier: Tier::Destructive,
    }
}

fn rows() -> Vec<(&'static str, bool, bool)> {
    vec![
        ("app", false, true),
        ("app/src", false, false),
        ("app/target", false, false),
        ("app/target/debug", false, false),
        ("app/target/debug/incremental", false, false),
        ("app/target/debug/.fingerprint", false, false),
        ("app/target/debug/deps", false, false),
        ("app/target/debug/build", false, false),
        ("app/target/debug/examples", false, false),
        ("app/target/debug/other", false, false),
        ("app/target/x86_64-unknown-linux-gnu", false, false),
        ("app/target/x86_64-unknown-linux-gnu/release", false, false),
        (
            "app/target/x86_64-unknown-linux-gnu/release/deps",
            false,
            false,
        ),
        ("app/src/debug", false, false),
        ("app/src/debug/deps", false, false),
        ("bare", false, false),
        ("bare/target", false, false),
        ("bare/target/debug", false, false),
        ("bare/target/debug/deps", false, false),
        ("web", true, false),
        ("web/node_modules", false, false),
        ("web/node_modules/foo", false, true),
        ("web/node_modules/foo/target", false, false),
        ("web/node_modules/foo/target/debug", false, false),
        ("web/node_modules/foo/target/debug/deps", false, false),
    ]
}

#[test]
fn a_part_of_a_cargo_target_should_be_a_possible_build_dirs_item() {
    let t = tree("/h/code", &rows());
    let r = roots();
    let f = ClassifyFacts::default();
    let h = |rel: &str| hint_of_node(&t, find(&t, &format!("/h/code/{rel}")), &r, &f);
    for rel in [
        "app/target/debug/incremental",
        "app/target/debug/.fingerprint",
        "app/target/debug/deps",
        "app/target/debug/build",
        "app/target/debug/examples",
        "app/target/x86_64-unknown-linux-gnu/release/deps",
    ] {
        assert_eq!(h(rel), part(), "{rel}");
    }
}

#[test]
fn only_the_part_names_below_a_real_target_should_be_possible_items() {
    let t = tree("/h/code", &rows());
    let r = roots();
    let f = ClassifyFacts::default();
    let h = |rel: &str| hint_of_node(&t, find(&t, &format!("/h/code/{rel}")), &r, &f);
    // the target itself is unchanged: a certain item
    assert_eq!(
        h("app/target"),
        Hint::Yes {
            selector: SelectorId::BuildDirs,
            tier: Tier::Destructive
        }
    );
    // a profile, an unknown name, a part-named dir outside a target, and a target with no Cargo.toml
    assert_eq!(h("app/target/debug"), Hint::No(NoReason::NotShape));
    assert_eq!(h("app/target/debug/other"), Hint::No(NoReason::NotShape));
    assert_eq!(h("app/src/debug/deps"), Hint::No(NoReason::NotShape));
    assert_eq!(h("bare/target/debug/deps"), Hint::No(NoReason::NotShape));
}

#[test]
fn a_part_of_a_target_inside_node_modules_should_not_be_a_listed_item() {
    let t = tree("/h/code", &rows());
    let r = roots();
    let f = ClassifyFacts::default();
    assert_eq!(
        hint_of_node(
            &t,
            find(&t, "/h/code/web/node_modules/foo/target/debug/deps"),
            &r,
            &f
        ),
        Hint::No(NoReason::InsideBuildOrGit)
    );
}

#[test]
fn a_part_outside_the_cleaners_roots_should_say_so() {
    let t = tree(
        "/h/other",
        &[
            ("app", false, true),
            ("app/target", false, false),
            ("app/target/debug", false, false),
            ("app/target/debug/deps", false, false),
        ],
    );
    assert_eq!(
        hint_of_node(
            &t,
            find(&t, "/h/other/app/target/debug/deps"),
            &roots(),
            &ClassifyFacts::default()
        ),
        Hint::No(NoReason::OutsideRoot)
    );
}

fn cargo_home(path: &str, roots: &RootSet) -> Hint {
    kind_hint(
        Path::new(path),
        NONE,
        Inside::No,
        roots,
        &ClassifyFacts::default(),
    )
}

#[test]
fn a_crate_or_checkout_below_cargo_home_should_be_a_possible_cargo_home_item() {
    let r = roots();
    let want = Hint::Shape {
        selector: SelectorId::CargoHome,
        tier: Tier::Guarded,
    };
    for p in [
        "/h/.cargo/registry/src/index.crates.io-6f17d22bba15001f/serde-1.0.200",
        "/h/.cargo/git/checkouts/repo-1a2b3c/abcdef1",
    ] {
        assert_eq!(cargo_home(p, &r), want, "{p}");
        assert_eq!(
            shape_of(Path::new(p), NONE, &ClassifyFacts::default()),
            Some(Kind::CargoHome),
            "{p}"
        );
    }
    for p in [
        "/h/.cargo/registry/src",
        "/h/.cargo/registry/src/index.crates.io-6f17d22bba15001f",
        "/h/.cargo/registry/cache/idx/serde-1.0.200",
        "/h/.cargo/git/db/repo-1a2b3c",
        "/h/.cargo/bin/cargo",
    ] {
        assert_eq!(cargo_home(p, &r), Hint::No(NoReason::NotShape), "{p}");
    }
}

#[test]
fn cargo_home_hints_should_follow_cargo_home_and_refuse_other_places() {
    let moved = roots().with_cargo_home(Some(Path::new("/h/dev/cargo")));
    let shape = Hint::Shape {
        selector: SelectorId::CargoHome,
        tier: Tier::Guarded,
    };
    assert_eq!(
        cargo_home("/h/dev/cargo/registry/src/i/c-1.0.0", &moved),
        shape
    );
    assert_eq!(
        cargo_home("/h/.cargo/registry/src/i/c-1.0.0", &moved),
        Hint::No(NoReason::OutsideRoot)
    );
    assert_eq!(
        cargo_home("/h/elsewhere/registry/src/i/c-1.0.0", &roots()),
        Hint::No(NoReason::OutsideRoot)
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
