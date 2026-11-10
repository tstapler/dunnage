//! Cache discovery for the read-only report (Story 1.3.2): the static table of `core::cache_targets` plus the
//! caches Go and pip report for themselves, each through the common path guard and sized by the one tree-walk
//! rule (`treestat::measure`), never by a second `du`-style walk.
//!
//! A path a tool reports is accepted only when `RootSet::add_command_cache` admits it as an `Exact` target;
//! a refused one is a `Skip(OutsideRoot)` candidate that is never walked.

use std::path::{Path, PathBuf};

use disk_cleanup_core::cache_targets::{cache_table, CacheRow, CacheSource};
use disk_cleanup_core::guard::{decide_path_common, Facts, GuardDecision};
use disk_cleanup_core::ports::{Cmd, CommandRunner, SystemProbe};
use disk_cleanup_core::roots::{CacheTool, RootSet};
use disk_cleanup_core::{
    AllocatedBytes, Allowed, CacheFacts, Candidate, Identity, Kind, KindFacts, Locator, RootDev,
    SkipReason,
};

use crate::dirops::{open_verified_root, DirOps};
use crate::discover_build::symlink_between;
use crate::mtime_probe::unix_secs;
use crate::treestat::{measure, MeasureOpts};

/// How the location of a cache was learned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// A fixed path of the table.
    Table,
    /// Asked of the tool (`go env GOCACHE`, `pip cache dir`).
    Command,
}

#[derive(Debug)]
pub enum CacheVerdict {
    Will(Allowed),
    Skip(SkipReason),
}

#[derive(Debug)]
pub struct CacheItem {
    pub candidate: Candidate,
    pub verdict: CacheVerdict,
    pub via: Via,
}

#[derive(Debug, Default)]
pub struct CacheDiscovery {
    pub items: Vec<CacheItem>,
    /// Tool-owned caches whose tool is not installed or gave no usable answer.
    pub missing_tools: Vec<&'static str>,
    pub mounts_unreadable: bool,
}

pub struct CacheEnv<'a, D> {
    /// Absolute table rows (apt) are looked up below this directory: `/` in production, a tempdir in tests.
    pub fs_root: &'a Path,
    pub ops: &'a D,
    pub runner: &'a dyn CommandRunner,
    pub probe: &'a dyn SystemProbe,
}

fn tool_program(t: CacheTool) -> &'static str {
    match t {
        CacheTool::Go => "go",
        CacheTool::Pip => "pip",
    }
}

fn tool_cmd(t: CacheTool, program: &str) -> Cmd {
    match t {
        CacheTool::Go => Cmd::new(program, ["env", "GOCACHE"], false),
        CacheTool::Pip => Cmd::new(program, ["cache", "dir"], false),
    }
}

/// The path the tool reported: its first non-empty stdout line, trimmed.
fn ask_tool(runner: &dyn CommandRunner, t: CacheTool) -> Option<PathBuf> {
    let program = match t {
        CacheTool::Go => "go",
        CacheTool::Pip if runner.exists("pip") => "pip",
        CacheTool::Pip => "pip3",
    };
    if !runner.exists(program) {
        return None;
    }
    let out = runner.run(&tool_cmd(t, program)).ok()?;
    let line = out
        .stdout_lossy()
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())?
        .to_string();
    Some(PathBuf::from(line))
}

struct Located {
    path: PathBuf,
    via: Via,
    /// A tool answer `RootSet` refused.
    refused: bool,
}

pub fn discover_caches<D: DirOps>(env: &CacheEnv<'_, D>, roots: &mut RootSet) -> CacheDiscovery {
    let platform = env.probe.platform();
    let mut out = CacheDiscovery::default();
    let mounts = match env.probe.mounts() {
        Ok(m) => m.into_iter().map(|e| e.mount_point).collect(),
        Err(_) => {
            out.mounts_unreadable = true;
            Vec::new()
        }
    };
    let root_dev = std::fs::metadata(roots.home()).map(|m| std::os::unix::fs::MetadataExt::dev(&m));
    for row in cache_table(platform) {
        let loc = match locate(env, roots, &row) {
            Ok(l) => l,
            Err(tool) => {
                out.missing_tools.push(tool);
                continue;
            }
        };
        let ctx = Ctx {
            env,
            roots,
            mounts: &mounts,
            root_dev: root_dev.as_ref().map(|d| RootDev(*d)).ok(),
        };
        if let Some(item) = ctx.evaluate(&row, &loc) {
            out.items.push(item);
        }
    }
    out
}

/// `Err(tool)`: a tool-owned cache with no answer.
fn locate<D: DirOps>(
    env: &CacheEnv<'_, D>,
    roots: &mut RootSet,
    row: &CacheRow,
) -> Result<Located, &'static str> {
    match row.source {
        CacheSource::Home(rel) => Ok(Located {
            path: roots.home().join(rel),
            via: Via::Table,
            refused: false,
        }),
        CacheSource::Absolute(p) => Ok(Located {
            path: env.fs_root.join(p.trim_start_matches('/')),
            via: Via::Table,
            refused: false,
        }),
        CacheSource::Tool(t) => {
            let reported = ask_tool(env.runner, t).ok_or(tool_program(t))?;
            let refused = roots.add_command_cache(t, &reported).is_err();
            Ok(Located {
                path: reported,
                via: Via::Command,
                refused,
            })
        }
    }
}

struct Ctx<'a, D> {
    env: &'a CacheEnv<'a, D>,
    roots: &'a RootSet,
    mounts: &'a [PathBuf],
    root_dev: Option<RootDev>,
}

const NO_IDENT: Identity = Identity { dev: 0, ino: 0 };

impl<D: DirOps> Ctx<'_, D> {
    fn item(
        row: &CacheRow,
        loc: &Located,
        ident: Identity,
        size: (u64, Option<i64>),
        verdict: CacheVerdict,
    ) -> CacheItem {
        let facts = KindFacts::Cache(CacheFacts {
            label: row.label.to_string(),
            apt: row.is_apt(),
        });
        CacheItem {
            candidate: Candidate::new(
                Locator::Path(loc.path.clone()),
                facts,
                AllocatedBytes(size.0),
                size.1,
                ident,
            ),
            verdict,
            via: loc.via,
        }
    }

    fn skipped(row: &CacheRow, loc: &Located, ident: Identity, r: SkipReason) -> CacheItem {
        Self::item(row, loc, ident, (0, None), CacheVerdict::Skip(r))
    }

    /// `None` when the cache directory does not exist (nothing to list).
    fn evaluate(&self, row: &CacheRow, loc: &Located) -> Option<CacheItem> {
        if loc.refused {
            return Some(Self::skipped(row, loc, NO_IDENT, SkipReason::OutsideRoot));
        }
        let md = std::fs::symlink_metadata(&loc.path).ok()?;
        let ident = Identity::of(&md);
        if row.needs_root && self.env.probe.euid() != 0 {
            return Some(Self::skipped(row, loc, ident, SkipReason::NeedsRoot));
        }
        let facts = Facts {
            kind: Kind::Cache,
            path: loc.path.clone(),
            ident,
            is_symlink: md.file_type().is_symlink()
                || symlink_between(self.roots.home(), &loc.path),
            root_dev: self.root_dev.unwrap_or(RootDev(ident.dev)),
            is_mount_point: self.mounts.iter().any(|m| m == &loc.path),
        };
        match decide_path_common(&facts, self.roots) {
            GuardDecision::Skip(r) => Some(Self::skipped(row, loc, ident, r)),
            GuardDecision::Allow(a) => {
                let Some(size) = self.size_of(&loc.path, ident, facts.root_dev) else {
                    return Some(Self::skipped(row, loc, ident, SkipReason::Gone));
                };
                Some(Self::item(row, loc, ident, size, CacheVerdict::Will(a)))
            }
        }
    }

    fn size_of(
        &self,
        path: &Path,
        ident: Identity,
        root_dev: RootDev,
    ) -> Option<(u64, Option<i64>)> {
        let root = open_verified_root(self.env.ops, path, ident).ok()?;
        let s = measure(self.env.ops, root.dir(), root_dev, MeasureOpts::default());
        Some((s.allocated.0, s.newest_mtime.map(unix_secs)))
    }
}
