#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::path::{Component, Path, PathBuf};

use dunnage_core::roots::{CacheTool, RootError, RootSet, SteamLibraryRoot};
use dunnage_core::{AllowedTarget, Dirs, Identity, Kind, NamePattern, SkipReason};

/// A tiny filesystem for `canon`: directories with inode numbers, plus symlinks. Paths are normalised
/// lexically first (as `realpath` does for `.` and `..`), then symlinks are followed.
struct FakeCanon {
    dirs: HashMap<PathBuf, u64>,
    links: HashMap<PathBuf, PathBuf>,
    calls: RefCell<Vec<PathBuf>>,
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
            calls: RefCell::new(Vec::new()),
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

fn dirs(home: &str) -> Dirs {
    Dirs {
        home: home.into(),
        state: format!("{home}/.local/state").into(),
        cache: format!("{home}/.cache").into(),
        config: format!("{home}/.config").into(),
    }
}

fn resolve(
    fs: &FakeCanon,
    home: &str,
    flags: &[&str],
    config: Option<&[&str]>,
    cwd: &str,
) -> Result<RootSet, RootError> {
    let flags: Vec<PathBuf> = flags.iter().map(PathBuf::from).collect();
    let config: Option<Vec<PathBuf>> = config.map(|c| c.iter().map(PathBuf::from).collect());
    RootSet::resolve(
        &dirs(home),
        &flags,
        config.as_deref(),
        Path::new(cwd),
        &|p| fs.canon(p),
    )
}

fn roots(rs: &RootSet) -> Vec<&str> {
    rs.scan_roots()
        .iter()
        .map(|p| p.to_str().unwrap())
        .collect()
}

#[test]
fn root_defaults_and_precedence_should_be_flag_over_config_over_defaults_When_resolved() {
    let fs = FakeCanon::new(&["/", "/h", "/h/code", "/h/dotfiles", "/h/work", "/h/x"]);
    // Defaults: only existing ones; /h/Programming is missing and is skipped silently.
    let rs = resolve(&fs, "/h", &[], None, "/h").unwrap();
    assert_eq!(roots(&rs), ["/h/code", "/h/dotfiles"]);
    // Config replaces the defaults.
    let rs = resolve(&fs, "/h", &[], Some(&["/h/work"]), "/h").unwrap();
    assert_eq!(roots(&rs), ["/h/work"]);
    // The flag replaces the config; repeatable; duplicates collapse after canonicalisation.
    let rs = resolve(&fs, "/h", &["/h/x", "/h/x/../x"], Some(&["/h/work"]), "/h").unwrap();
    assert_eq!(roots(&rs), ["/h/x"]);
    // `~` is expanded from Dirs.home only; a relative root is taken against the working directory.
    let rs = resolve(&fs, "/h", &["~/work", "x"], None, "/h").unwrap();
    assert_eq!(roots(&rs), ["/h/work", "/h/x"]);
    // Home and each root are canonicalised once (ancestors of home are only stat-ed for their identity).
    let fs = FakeCanon::new(&["/", "/h", "/h/code", "/h/dotfiles"]);
    resolve(&fs, "/h", &["/h/code"], None, "/h").unwrap();
    let calls = fs.calls.borrow();
    assert_eq!(
        calls
            .iter()
            .filter(|c| c.as_path() == Path::new("/h"))
            .count(),
        1
    );
    assert_eq!(
        calls
            .iter()
            .filter(|c| c.as_path() == Path::new("/h/code"))
            .count(),
        1
    );
}

#[test]
fn root_resolution_should_exit_64_When_root_is_home_ancestor_symlink_to_home_or_dot() {
    let fs = FakeCanon::new(&[
        "/",
        "/h",
        "/h/code",
        "/home",
        "/var",
        "/var/home",
        "/var/home/u",
    ])
    .link("/h/link-to-home", "/h")
    .link("/home", "/var/home");
    for root in [".", "code/..", "/h/link-to-home", "/", "/h", "~"] {
        let e = resolve(&fs, "/h", &[root], None, "/h").unwrap_err();
        assert_eq!(e.exit_code(), 64, "{root}");
        assert!(
            e.text().contains("root is HOME or an ancestor"),
            "{root}: {}",
            e.text()
        );
    }
    // An ancestor of HOME other than `/`.
    let fs2 = FakeCanon::new(&["/", "/h", "/h/u", "/h/u/code"]);
    assert_eq!(
        resolve(&fs2, "/h/u", &["/h"], None, "/h/u")
            .unwrap_err()
            .exit_code(),
        64
    );
    // `/home -> /var/home`: HOME=/home/u and --root /var/home/u are one directory.
    let e = resolve(&fs, "/home/u", &["/var/home/u"], None, "/home/u").unwrap_err();
    assert_eq!(e.exit_code(), 64);
    // Config roots are held to the same rule.
    assert_eq!(
        resolve(&fs, "/h", &[], Some(&["/h"]), "/h")
            .unwrap_err()
            .exit_code(),
        64
    );
    // A missing or unreadable root is exit 2 with the full line.
    let e = resolve(&fs, "/h", &["/h/does-not-exist"], None, "/h").unwrap_err();
    assert!(matches!(
        &e,
        RootError::Unreadable { path, .. } if path == std::path::Path::new("/h/does-not-exist")
    ));
    assert_eq!(e.exit_code(), 2);
    assert!(e
        .text()
        .starts_with("error: root /h/does-not-exist does not exist or is unreadable ("));
    assert!(e.text().contains("Nothing scanned."));
}

fn set() -> RootSet {
    let fs = FakeCanon::new(&[
        "/",
        "/h",
        "/h/code",
        "/h/dotfiles",
        "/h/Programming",
        "/lib",
    ]);
    resolve(&fs, "/h", &[], None, "/h")
        .unwrap()
        .with_steam_libraries(vec![
            SteamLibraryRoot {
                path: "/lib".into(),
                mounted: true,
            },
            SteamLibraryRoot {
                path: "/gone".into(),
                mounted: false,
            },
        ])
}

#[test]
fn admits_should_be_false_for_sibling_documents_and_tmp_sentinels_and_true_inside_bound_When_each_kind_asked(
) {
    let rs = set();
    let p = Path::new;
    // Sentinels outside every RootSet, asked for every kind with a filesystem root.
    for kind in [
        Kind::NodeModules,
        Kind::CargoTarget,
        Kind::Worktree,
        Kind::Cache,
    ] {
        for out in [
            "/h/sibling/node_modules",
            "/h/Documents/x/node_modules",
            "/h/Documents/x",
            "/tmp/elsewhere",
            "/tmp/elsewhere/node_modules",
            "/h/codeX/app/node_modules",
        ] {
            assert!(!rs.admits(kind, p(out)), "{kind:?} {out}");
        }
    }
    // Inside the bound, by pattern.
    assert!(rs.admits(Kind::NodeModules, p("/h/code/app/node_modules")));
    assert!(rs.admits(Kind::NodeModules, p("/h/Programming/a/b/node_modules")));
    assert!(rs.admits(Kind::CargoTarget, p("/h/code/app/target")));
    assert!(rs.admits(
        Kind::Worktree,
        p("/h/code/app/.claude/worktrees/agent-1a2b")
    ));
    assert!(rs.admits(Kind::Cache, p("/h/.npm/_cacache")));
    assert!(rs.admits(Kind::SteamPrefix, p("/lib/steamapps/compatdata/570")));
    assert!(rs.admits(Kind::SteamShaderCache, p("/lib/steamapps/shadercache/570")));
    assert!(rs.admits(Kind::SteamRemnant, p("/lib/steamapps/common/Old Game")));
    // Wrong name for the kind, the root itself, an unmounted library, a cache subpath.
    assert!(!rs.admits(Kind::NodeModules, p("/h/code/app/target")));
    assert!(!rs.admits(Kind::NodeModules, p("/h/code")));
    assert!(!rs.admits(Kind::Worktree, p("/h/code/app")));
    assert!(!rs.admits(Kind::Worktree, p("/h/code/app/.claude/worktrees/agent-")));
    assert!(!rs.admits(Kind::Cache, p("/h/.npm/_cacache/sub")));
    assert!(!rs.admits(Kind::SteamPrefix, p("/gone/steamapps/compatdata/570")));
    assert!(!rs.admits(Kind::SteamPrefix, p("/lib/steamapps/compatdata/570/pfx")));
    assert!(!rs.admits(Kind::SteamPrefix, p("/lib/steamapps/compatdata/abc")));
    assert!(!rs.admits(Kind::SteamRemnant, p("/lib/steamapps/common/.hidden")));
    // Never lexically unclean, even when starts_with would pass.
    for dirty in [
        "/h/code/../.ssh/node_modules",
        "/h/code/./app/node_modules",
        "/h/code//app/node_modules",
        "/h/code/app/node_modules/",
        "h/code/app/node_modules",
    ] {
        assert!(!rs.admits(Kind::NodeModules, p(dirty)), "{dirty}");
    }
    // Docker and btrfs have no filesystem root.
    for kind in [
        Kind::DockerImage,
        Kind::DockerBuildCache,
        Kind::DockerVolume,
        Kind::BtrfsBalance,
        Kind::BtrfsTrim,
    ] {
        assert!(rs.allowed(kind).is_empty());
        assert!(!rs.admits(kind, p("/h/code/app/node_modules")));
    }
}

#[test]
fn allowed_should_return_exact_cache_paths_and_within_roots_with_the_name_patterns() {
    let rs = set();
    let caches = rs.allowed(Kind::Cache);
    for rel in [
        "/h/.npm/_cacache",
        "/h/.gradle/caches",
        "/h/.cache/ms-playwright",
        "/h/.cargo/registry/cache",
        "/h/.cargo/registry/src",
    ] {
        assert!(caches.contains(&AllowedTarget::Exact(rel.into())), "{rel}");
    }
    assert_eq!(rs.allowed(Kind::NodeModules).len(), 3);
    assert_eq!(
        rs.allowed(Kind::Worktree)[0],
        AllowedTarget::Within {
            root: "/h/code".into(),
            name: NamePattern::AgentWorktree
        }
    );
    assert_eq!(
        rs.allowed(Kind::SteamRemnant),
        vec![AllowedTarget::Within {
            root: "/lib/steamapps/common".into(),
            name: NamePattern::SteamInstallDir
        }]
    );
}

#[test]
fn command_derived_caches_should_be_admitted_only_inside_home_with_the_expected_basename() {
    let mut rs = set();
    assert_eq!(
        rs.add_command_cache(CacheTool::Go, Path::new("/h/.cache/go-build")),
        Ok(())
    );
    assert_eq!(
        rs.add_command_cache(CacheTool::Pip, Path::new("/h/.cache/pip")),
        Ok(())
    );
    assert!(rs.admits(Kind::Cache, Path::new("/h/.cache/go-build")));
    for (tool, bad) in [
        (CacheTool::Go, "/tmp/go-build"),
        (CacheTool::Go, "/h"),
        (CacheTool::Go, "/h/.cache"),
        (CacheTool::Go, "/h/.cache/other"),
        (CacheTool::Go, "/h/.cache/go-build/"),
        (CacheTool::Go, "/h/../x/go-build"),
        (CacheTool::Pip, "/h/.cache/go-build"),
        (CacheTool::Pip, "/h/.cargo/registry"),
    ] {
        assert_eq!(
            rs.add_command_cache(tool, Path::new(bad)),
            Err(SkipReason::OutsideRoot),
            "{bad}"
        );
    }
}

#[test]
fn worktree_scan_should_include_programming_dir_and_kinds_output_should_say_so_When_it_exists() {
    let rs = set();
    assert!(roots(&rs).contains(&"/h/Programming"));
    assert!(rs.admits(
        Kind::Worktree,
        Path::new("/h/Programming/app/.claude/worktrees/agent-1")
    ));
    // The `kinds` rendering of the widening is Story 1.2.4's; the audit record already carries the roots.
    assert!(rs
        .run_roots()
        .worktree_roots
        .contains(&PathBuf::from("/h/Programming")));
}
