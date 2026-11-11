//! Path caches removed through `delete_tree` (Story 2.3.1).
//!
//! DECISION (the missing `Allowed` witness): `Target::guard` returns `Guarded::Allow` without the `Allowed`
//! that discovery obtained, and the trait is shared with every other kind, so its return type is left alone.
//! `execute` instead re-derives the witness with a fresh `decide_path_common` over fresh `lstat` facts. That
//! is also the plan's rule that a guard is "asked again right before `execute`": the verdict stored at
//! discovery is never trusted at delete time, and the identity found now must equal the planned one.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use disk_cleanup_core::guard::{decide_path_common, Facts, GuardDecision};
use disk_cleanup_core::ports::{Env, SystemProbe};
use disk_cleanup_core::roots::RootSet;
use disk_cleanup_core::selector::Selection;
use disk_cleanup_core::target::{Guarded, Target};
use disk_cleanup_core::{
    AbortFlag, Allowed, AllowedTarget, ApplyToken, CacheFacts, Candidate, CandidateId, DeleteError,
    DeleteFailure, DeletePolicy, Freed, Identity, Kind, KindFacts, Locator, Outcome, RootDev,
    SelectorId, SkipReason,
};

use crate::delete_tree;
use crate::dirops::{open_parent, open_verified_root, DirOps};
use crate::discover_build::symlink_between;
use crate::discover_cache::{discover_caches, CacheEnv};

/// How a cache row is emptied. Story 2.3.2 (`cache_cmd.rs`) owns `Command`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalVia {
    Path,
    Command,
}

/// Rows whose own tool empties them (`go clean -cache`, `pip cache purge`, `brew cleanup`,
/// `apt-get clean`); every other row is removed by path.
pub fn removal_via(facts: &CacheFacts) -> RemovalVia {
    const COMMAND_LABELS: [&str; 3] = ["Go build cache", "pip cache", "Homebrew cache"];
    if facts.apt || COMMAND_LABELS.contains(&facts.label.as_str()) {
        RemovalVia::Command
    } else {
        RemovalVia::Path
    }
}

fn failure(error: DeleteError) -> DeleteFailure {
    DeleteFailure {
        error,
        freed: Freed::default(),
    }
}

/// Discovery, guard and `delete_tree` removal of the path-driven cache rows.
pub struct CacheTarget<D: DirOps> {
    roots: RootSet,
    /// Absolute table rows (apt) live below this directory (`/` in production).
    fs_root: PathBuf,
    ops: D,
    /// Candidates already pointing at a command row; never removed here.
    command_rows: Mutex<HashSet<CandidateId>>,
}

impl<D: DirOps> CacheTarget<D> {
    pub fn new(roots: RootSet, fs_root: PathBuf, ops: D) -> CacheTarget<D> {
        CacheTarget {
            roots,
            fs_root,
            ops,
            command_rows: Mutex::new(HashSet::new()),
        }
    }

    pub fn ops(&self) -> &D {
        &self.ops
    }

    /// A fresh `Allowed` for the candidate's path, or why it is skipped now.
    pub fn rederive(&self, c: &Candidate, probe: &dyn SystemProbe) -> Result<Allowed, SkipReason> {
        let Locator::Path(path) = c.locator() else {
            return Err(SkipReason::OutsideRoot);
        };
        let md = std::fs::symlink_metadata(path).map_err(|_| SkipReason::Gone)?;
        let ident = Identity::of(&md);
        let home_dev = std::fs::metadata(self.roots.home()).map_or(ident.dev, |m| m.dev());
        let mounts = probe.mounts().map(|m| {
            m.into_iter()
                .map(|e| e.mount_point)
                .collect::<Vec<PathBuf>>()
        });
        let facts = Facts {
            kind: Kind::Cache,
            path: path.clone(),
            ident,
            is_symlink: md.file_type().is_symlink() || symlink_between(self.roots.home(), path),
            root_dev: RootDev(home_dev),
            is_mount_point: mounts.is_ok_and(|m| m.iter().any(|p| p == path)),
        };
        match decide_path_common(&facts, &self.roots) {
            GuardDecision::Skip(r) => Err(r),
            GuardDecision::Allow(a) if a.ident() != c.ident() => Err(SkipReason::IdentityChanged),
            GuardDecision::Allow(a) => Ok(a),
        }
    }

    /// Removes the cache directory the witness names, below a handle chain opened without following links.
    fn remove(
        &self,
        allowed: &Allowed,
        abort: &AbortFlag,
        proof: &ApplyToken,
    ) -> Result<Freed, DeleteFailure> {
        let path = allowed.path();
        let home = self.roots.home();
        let base: &Path = if path.starts_with(home) {
            home
        } else {
            &self.fs_root
        };
        let name = path
            .file_name()
            .ok_or_else(|| failure(DeleteError::ProtectedPath))?;
        let rel_parent = path
            .parent()
            .and_then(|p| p.strip_prefix(base).ok())
            .ok_or_else(|| failure(DeleteError::OutsideScope))?;
        let base_md = std::fs::metadata(base).map_err(|_| failure(DeleteError::OutsideScope))?;
        let base_id = Identity::of(&base_md);
        let root = open_verified_root(&self.ops, base, base_id).map_err(failure)?;
        let parent = open_parent(&self.ops, &root, rel_parent).map_err(failure)?;
        let opened = self
            .ops
            .fstat(root.dir())
            .map_err(|_| failure(DeleteError::OutsideScope))?;
        let policy = DeletePolicy {
            root_dev: RootDev(base_id.dev),
            root_mnt: opened.mnt_id,
            home: home.to_path_buf(),
            home_id: std::fs::metadata(home)
                .map(|m| Identity::of(&m))
                .map_err(|_| failure(DeleteError::OutsideScope))?,
            scan_roots: self.roots.scan_roots().to_vec(),
            allowed: self
                .roots
                .allowed(Kind::Cache)
                .into_iter()
                .find(|t| matches!(t, AllowedTarget::Exact(p) if p == path))
                .unwrap_or_else(|| AllowedTarget::Exact(PathBuf::from("/"))),
            target: path.to_path_buf(),
            require_sibling_any: Vec::new(),
            max_depth: 256,
            restore_owner_write: true,
        };
        delete_tree(
            &self.ops,
            &parent,
            OsStr::new(name),
            allowed.ident(),
            &policy,
            abort,
            proof,
        )
    }
}

impl<D: DirOps + 'static> Target for CacheTarget<D> {
    fn name(&self) -> &'static str {
        "caches"
    }

    fn selectors(&self) -> &'static [SelectorId] {
        &[SelectorId::Caches, SelectorId::CacheApt]
    }

    fn discover(&self, env: &Env, _selection: &Selection) -> Vec<Candidate> {
        let mut roots = self.roots.clone();
        let found = discover_caches(
            &CacheEnv {
                fs_root: &self.fs_root,
                ops: &self.ops,
                runner: env.runner,
                probe: env.probe,
            },
            &mut roots,
        );
        let mut command_rows = self
            .command_rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        found
            .items
            .into_iter()
            .map(|item| {
                if matches!(item.candidate.facts(), KindFacts::Cache(f) if removal_via(f) == RemovalVia::Command)
                {
                    command_rows.insert(item.candidate.id());
                }
                item.candidate
            })
            .collect()
    }

    fn guard(&self, c: &Candidate, env: &Env) -> Guarded {
        match self.rederive(c, env.probe) {
            Ok(_) => Guarded::Allow,
            Err(r) => Guarded::Skip(r),
        }
    }

    fn execute(
        &self,
        c: &Candidate,
        env: &Env,
        proof: &ApplyToken,
    ) -> Result<Freed, DeleteFailure> {
        let is_command_row = self
            .command_rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&c.id());
        if is_command_row {
            return Err(failure(DeleteError::Refused));
        }
        match self.rederive(c, env.probe) {
            Ok(a) => self.remove(&a, env.guard.abort_flag(), proof),
            // A vanished cache is success by idempotence (I7).
            Err(SkipReason::Gone) => Ok(Freed::default()),
            Err(r) => Err(failure(delete_error_of(r))),
        }
    }

    fn finish(&self, _outcomes: &[Outcome], _env: &Env) -> Vec<String> {
        Vec::new()
    }
}

fn delete_error_of(r: SkipReason) -> DeleteError {
    match r {
        SkipReason::Symlink => DeleteError::Symlink,
        SkipReason::CrossDevice => DeleteError::CrossDevice,
        SkipReason::IdentityChanged => DeleteError::IdentityMismatch,
        SkipReason::ProtectedPath | SkipReason::MountPoint => DeleteError::ProtectedPath,
        _ => DeleteError::OutsideScope,
    }
}
