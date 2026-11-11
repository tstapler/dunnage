//! `RootSet` (Story 1.1.5): what may be touched, defined once per kind. Every out-of-root guard and test uses
//! this as its oracle. Pure: the filesystem is reached only through the injected `canon` function.
//!
//! Scan roots (build dirs and worktrees) come from `--root` flags, else `config.toml`, else the defaults
//! `$HOME/code`, `$HOME/dotfiles` and `$HOME/Programming` (existing ones only). Caches are exact table paths.
//! Steam is bounded by its mounted libraries. Docker and btrfs have no filesystem root.

use std::io;
use std::path::{Path, PathBuf};

use crate::audit_types::RunRoots;
use crate::delete_types::{AllowedTarget, NamePattern};
use crate::dirs::Dirs;
use crate::ids::Identity;
use crate::kind::Kind;
use crate::outcome::SkipReason;
use crate::reasons::FatalError;

/// Directories scanned when neither `--root` nor `config.toml` names any, relative to `$HOME`.
pub const DEFAULT_SCAN_ROOTS: [&str; 3] = ["code", "dotfiles", "Programming"];

/// The path caches, relative to `$HOME`. Each is an `Exact` target.
pub const CACHE_PATHS: [&str; 5] = [
    ".npm/_cacache",
    ".gradle/caches",
    ".cache/ms-playwright",
    ".cargo/registry/cache",
    ".cargo/registry/src",
];

/// `realpath` plus `stat`: the canonical path and the `(dev, ino)` of the directory it names.
pub type Canon<'a> = &'a dyn Fn(&Path) -> io::Result<(PathBuf, Identity)>;

/// Why roots could not be resolved. `HomeOrAncestor` is a usage error (exit 64); an unreadable root is the
/// run-level `FatalError::RootsUnreadable` (exit 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootError {
    HomeOrAncestor { path: PathBuf },
    Unreadable { path: PathBuf, errno: i32 },
}

impl RootError {
    pub fn exit_code(&self) -> i32 {
        match self {
            RootError::HomeOrAncestor { .. } => 64,
            RootError::Unreadable { path, errno } => FatalError::RootsUnreadable {
                path: path.clone(),
                errno: *errno,
            }
            .exit_code(),
        }
    }

    pub fn text(&self) -> String {
        match self {
            RootError::HomeOrAncestor { path } => format!(
                "error: root is HOME or an ancestor of HOME: {}. Nothing scanned. Pass a directory below your home, for example --root ~/code.",
                path.display()
            ),
            RootError::Unreadable { path, errno } => FatalError::RootsUnreadable {
                path: path.clone(),
                errno: *errno,
            }
            .text(),
        }
    }
}

impl std::fmt::Display for RootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text())
    }
}

impl std::error::Error for RootError {}

/// A Steam library root and whether its volume is mounted (an unmounted one bounds nothing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteamLibraryRoot {
    pub path: PathBuf,
    pub mounted: bool,
}

/// The tools whose cache directory is asked of the tool itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheTool {
    /// `go env GOCACHE`, expected basename `go-build`.
    Go,
    /// `pip cache dir`, expected basename `pip`.
    Pip,
}

impl CacheTool {
    pub fn basename(self) -> &'static str {
        match self {
            CacheTool::Go => "go-build",
            CacheTool::Pip => "pip",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootSet {
    home: PathBuf,
    scan_roots: Vec<PathBuf>,
    cache_paths: Vec<PathBuf>,
    steam_libraries: Vec<SteamLibraryRoot>,
}

/// Absolute, no `.` or `..`, no empty component, no trailing `/` (except `/` itself). The string-level rule of
/// Story 0.1.3 C3: `starts_with` is never applied to anything else.
pub(crate) fn lexically_clean(p: &Path) -> bool {
    let Some(s) = p.to_str() else { return false };
    let Some(rest) = s.strip_prefix('/') else {
        return false;
    };
    rest.is_empty()
        || rest
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..")
}

/// `path` is a strict descendant of `root` (the root itself is not). Lexical; the caller has checked
/// [`lexically_clean`]. The one definition of "strictly below a root" for [`RootSet::admits`] and the
/// explorer's mark rule M1.
pub fn strictly_below(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root)
        .is_ok_and(|rel| !rel.as_os_str().is_empty())
}

fn expand(raw: &Path, home: &Path, cwd: &Path) -> PathBuf {
    if let Ok(rest) = raw.strip_prefix("~") {
        return home.join(rest);
    }
    if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    }
}

/// `rel` is non-empty and below the root. Steam patterns name exactly one component below their root.
fn pattern_matches(p: NamePattern, rel: &Path) -> bool {
    let Some(name) = rel.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let one_below = rel.iter().count() == 1;
    match p {
        NamePattern::NodeModules => name == "node_modules",
        NamePattern::CargoTarget => name == "target",
        NamePattern::AgentWorktree => name.strip_prefix("agent-").is_some_and(|s| !s.is_empty()),
        NamePattern::SteamAppId => one_below && name.bytes().all(|b| b.is_ascii_digit()),
        NamePattern::SteamInstallDir => one_below && !name.starts_with('.'),
    }
}

impl RootSet {
    /// Canonicalises `Dirs.home` and every root exactly once. A relative root is resolved against `cwd`, a
    /// leading `~` against `Dirs.home` only. "Is HOME" and "is an ancestor of HOME" are decided by `Identity`
    /// of the canonical directories (never by `Path` equality alone): `/var/home/u` and `/home/u` are one
    /// directory.
    ///
    /// `flag_roots` (repeatable `--root`) replace `config_roots`, which replace the defaults. An explicit root
    /// that cannot be canonicalised is `Unreadable`; a default that cannot is skipped.
    pub fn resolve(
        dirs: &Dirs,
        flag_roots: &[PathBuf],
        config_roots: Option<&[PathBuf]>,
        cwd: &Path,
        canon: Canon<'_>,
    ) -> Result<RootSet, RootError> {
        let (home, home_id) = canon(&dirs.home).map_err(|e| RootError::Unreadable {
            path: dirs.home.clone(),
            errno: e.raw_os_error().unwrap_or(5),
        })?;
        let mut forbidden = vec![home_id];
        let mut forbidden_paths = vec![home.clone()];
        let mut up = home.parent();
        while let Some(a) = up {
            if let Ok((_, id)) = canon(a) {
                forbidden.push(id);
            }
            forbidden_paths.push(a.to_path_buf());
            up = a.parent();
        }
        let is_forbidden = |p: &Path, id: Identity| {
            forbidden.contains(&id) || forbidden_paths.iter().any(|f| f == p)
        };

        let explicit: Option<Vec<PathBuf>> = if flag_roots.is_empty() {
            config_roots.map(<[PathBuf]>::to_vec)
        } else {
            Some(flag_roots.to_vec())
        };
        let mut scan_roots: Vec<PathBuf> = Vec::new();
        let mut push = |p: PathBuf| {
            if !scan_roots.contains(&p) {
                scan_roots.push(p);
            }
        };
        if let Some(roots) = explicit {
            for raw in roots {
                let wanted = expand(&raw, &home, cwd);
                let (path, id) = canon(&wanted).map_err(|e| RootError::Unreadable {
                    path: wanted.clone(),
                    errno: e.raw_os_error().unwrap_or(5),
                })?;
                if is_forbidden(&path, id) {
                    return Err(RootError::HomeOrAncestor { path: wanted });
                }
                push(path);
            }
        } else {
            for name in DEFAULT_SCAN_ROOTS {
                if let Ok((path, id)) = canon(&home.join(name)) {
                    if !is_forbidden(&path, id) {
                        push(path);
                    }
                }
            }
        }
        let cache_paths = CACHE_PATHS.iter().map(|p| home.join(p)).collect();
        Ok(RootSet {
            home,
            scan_roots,
            cache_paths,
            steam_libraries: Vec::new(),
        })
    }

    #[must_use]
    pub fn with_steam_libraries(mut self, libraries: Vec<SteamLibraryRoot>) -> RootSet {
        self.steam_libraries = libraries;
        self
    }

    /// Adds the cache directory a tool reported (`go env GOCACHE`, `pip cache dir`). It must be a strict
    /// descendant of `$HOME`, lexically clean, not an ancestor of a table target, and end in the tool's
    /// expected basename; otherwise `Skip(OutsideRoot)`.
    pub fn add_command_cache(
        &mut self,
        tool: CacheTool,
        reported: &Path,
    ) -> Result<(), SkipReason> {
        let ok = lexically_clean(reported)
            && reported != self.home
            && reported.starts_with(&self.home)
            && reported.file_name().is_some_and(|n| n == tool.basename())
            && !self.cache_paths.iter().any(|t| t.starts_with(reported));
        if !ok {
            return Err(SkipReason::OutsideRoot);
        }
        if !self.cache_paths.iter().any(|p| p == reported) {
            self.cache_paths.push(reported.to_path_buf());
        }
        Ok(())
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    /// The canonical scan roots, in resolution order.
    pub fn scan_roots(&self) -> &[PathBuf] {
        &self.scan_roots
    }

    pub fn is_scan_root(&self, p: &Path) -> bool {
        self.scan_roots.iter().any(|r| r == p)
    }

    /// Where a delete of `kind` may land. Docker and btrfs kinds have no filesystem root: empty.
    pub fn allowed(&self, kind: Kind) -> Vec<AllowedTarget> {
        let within = |roots: &[PathBuf], name| {
            roots
                .iter()
                .map(|r| AllowedTarget::Within {
                    root: r.clone(),
                    name,
                })
                .collect()
        };
        let steam = |sub: &str, name| -> Vec<AllowedTarget> {
            self.steam_libraries
                .iter()
                .filter(|l| l.mounted)
                .map(|l| AllowedTarget::Within {
                    root: l.path.join("steamapps").join(sub),
                    name,
                })
                .collect()
        };
        match kind {
            Kind::Cache => self
                .cache_paths
                .iter()
                .cloned()
                .map(AllowedTarget::Exact)
                .collect(),
            Kind::NodeModules => within(&self.scan_roots, NamePattern::NodeModules),
            Kind::CargoTarget => within(&self.scan_roots, NamePattern::CargoTarget),
            Kind::Worktree => within(&self.scan_roots, NamePattern::AgentWorktree),
            Kind::SteamPrefix => steam("compatdata", NamePattern::SteamAppId),
            Kind::SteamShaderCache => steam("shadercache", NamePattern::SteamAppId),
            Kind::SteamRemnant => steam("common", NamePattern::SteamInstallDir),
            Kind::DockerImage
            | Kind::DockerBuildCache
            | Kind::DockerVolume
            | Kind::BtrfsBalance
            | Kind::BtrfsTrim
            // The explorer's marks are checked by `admits` (any path strictly below a scan root); the
            // frozen `NamePattern` has no "any name" pattern, so the apply path of Story 6.6.3 adds its own.
            | Kind::ExplorerSelection => Vec::new(),
            // The test-only target names its own `Exact` paths per item.
            #[cfg(feature = "test-support")]
            Kind::FixtureStale => Vec::new(),
        }
    }

    /// The out-of-root oracle: lexical, no filesystem access. `true` only for a lexically clean absolute
    /// path that is an exact target of `kind` or a strict descendant of one of its roots whose name matches
    /// the kind's pattern. A root itself is not admitted.
    pub fn admits(&self, kind: Kind, path: &Path) -> bool {
        if !lexically_clean(path) {
            return false;
        }
        if kind == Kind::ExplorerSelection {
            return self.scan_roots.iter().any(|r| strictly_below(r, path));
        }
        self.allowed(kind).iter().any(|t| match t {
            AllowedTarget::Exact(p) => p == path,
            AllowedTarget::Within { root, name } => {
                strictly_below(root, path)
                    && path
                        .strip_prefix(root)
                        .is_ok_and(|rel| pattern_matches(*name, rel))
            }
        })
    }

    /// The roots as the audit `run_start` records them.
    pub fn run_roots(&self) -> RunRoots {
        RunRoots {
            scan_roots: self.scan_roots.clone(),
            cache_paths: self.cache_paths.clone(),
            worktree_roots: self.scan_roots.clone(),
            steam_libraries: self
                .steam_libraries
                .iter()
                .filter(|l| l.mounted)
                .map(|l| l.path.clone())
                .collect(),
        }
    }
}
