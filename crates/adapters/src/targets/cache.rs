//! Path caches removed through `delete_tree` (Story 2.3.1).
//!
//! DECISION (the missing `Allowed` witness): `Target::guard` returns `Guarded::Allow` without the `Allowed`
//! that discovery obtained, and the trait is shared with every other kind, so its return type is left alone.
//! `execute` instead re-derives the witness with a fresh `decide_path_common` over fresh `lstat` facts. That
//! is also the plan's rule that a guard is "asked again right before `execute`": the verdict stored at
//! discovery is never trusted at delete time, and the identity found now must equal the planned one.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use disk_cleanup_core::cache_targets::{cache_command, cache_table, root_decision, CacheSource};
use disk_cleanup_core::guard::{decide_path_common, Facts, GuardDecision};
use disk_cleanup_core::ports::{Env, SystemProbe};
use disk_cleanup_core::roots::RootSet;
use disk_cleanup_core::selector::Selection;
use disk_cleanup_core::target::{Guarded, KindGate, Target};
use disk_cleanup_core::{
    AbortFlag, Allowed, AllowedTarget, ApplyToken, CacheFacts, Candidate, CandidateId, DeleteError,
    DeleteFailure, DeletePolicy, Freed, Identity, Kind, KindFacts, Locator, Outcome, RootDev,
    SelectorId, SkipReason,
};

use crate::delete_tree;
use crate::dirops::{open_parent, open_verified_root, DirOps};
use crate::discover_build::symlink_between;
use crate::discover_cache::{discover_caches, CacheEnv, CacheVerdict};
use crate::targets::cache_cmd::{plan_command, run_cache_command, CmdContext, CmdPlan};

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
    /// Grows during discovery: a path a tool reported (`go env GOCACHE`) is admitted as an `Exact` target.
    roots: Mutex<RootSet>,
    /// Absolute table rows (apt) live below this directory (`/` in production).
    fs_root: PathBuf,
    ops: D,
    /// What discovery decided per candidate: `None` removable, `Some(reason)` skipped (root rules, a tool
    /// answer outside the roots, a missing tool). The guard starts from it, so a verdict is never lost.
    verdicts: Mutex<HashMap<CandidateId, Option<SkipReason>>>,
    /// Rows a tool empties (`go clean -cache`, `apt-get clean`, ...), with the row's label.
    command_rows: Mutex<HashMap<CandidateId, String>>,
    /// Skipped rows of a tool that is not installed: the program the note names (`pip3`).
    missing_tools: Mutex<HashMap<CandidateId, String>>,
}

impl<D: DirOps> CacheTarget<D> {
    pub fn new(roots: RootSet, fs_root: PathBuf, ops: D) -> CacheTarget<D> {
        CacheTarget {
            roots: Mutex::new(roots),
            fs_root,
            ops,
            verdicts: Mutex::new(HashMap::new()),
            command_rows: Mutex::new(HashMap::new()),
            missing_tools: Mutex::new(HashMap::new()),
        }
    }

    fn roots(&self) -> std::sync::MutexGuard<'_, RootSet> {
        self.roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
        let home_dev = std::fs::metadata(self.roots().home()).map_or(ident.dev, |m| m.dev());
        let mounts = probe.mounts().map(|m| {
            m.into_iter()
                .map(|e| e.mount_point)
                .collect::<Vec<PathBuf>>()
        });
        let facts = Facts {
            kind: Kind::Cache,
            path: path.clone(),
            ident,
            is_symlink: md.file_type().is_symlink() || symlink_between(self.roots().home(), path),
            root_dev: RootDev(home_dev),
            is_mount_point: mounts.is_ok_and(|m| m.iter().any(|p| p == path)),
        };
        match decide_path_common(&facts, &self.roots()) {
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
        // Copied out so parallel removals do not queue behind the roots lock.
        let (home, scan_roots, allowed_target) = {
            let roots = self.roots();
            (
                roots.home().to_path_buf(),
                roots.scan_roots().to_vec(),
                roots
                    .allowed(Kind::Cache)
                    .into_iter()
                    .find(|t| matches!(t, AllowedTarget::Exact(p) if p == path))
                    .unwrap_or_else(|| AllowedTarget::Exact(PathBuf::from("/"))),
            )
        };
        let home = home.as_path();
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
            scan_roots,
            allowed: allowed_target,
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

/// The default location a tool-owned cache has when the tool is not installed, so the row can still be shown
/// (`Skipped(ToolMissing)`); nothing is ever removed from it.
fn default_dir(home: &Path, tool: &str, macos: bool) -> PathBuf {
    let base = if macos { "Library/Caches" } else { ".cache" };
    home.join(base)
        .join(if tool == "go" { "go-build" } else { "pip" })
}

impl<D: DirOps> CacheTarget<D> {
    fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn row_needs_root(label: &str, probe: &dyn SystemProbe) -> bool {
        cache_table(probe.platform())
            .iter()
            .any(|r| r.label == label && r.needs_root)
    }

    /// The command of a command-driven row, planned against the tools installed now.
    fn plan_for(label: &str, env: &Env) -> Option<CmdPlan> {
        let spec = cache_command(label)?;
        let ctx = CmdContext {
            runner: env.runner,
            probe: env.probe,
            explicit_root: false,
        };
        Some(plan_command(
            spec,
            Self::row_needs_root(label, env.probe),
            &ctx,
        ))
    }

    fn command_label(&self, c: &Candidate) -> Option<String> {
        Self::lock(&self.command_rows).get(&c.id()).cloned()
    }

    fn missing_tool_candidate(&self, tool: &str, env: &Env) -> Option<Candidate> {
        let platform = env.probe.platform();
        let row = cache_table(platform).into_iter().find(|r| {
            matches!(r.source, CacheSource::Tool(t) if
                (t == disk_cleanup_core::roots::CacheTool::Go) == (tool == "go"))
        })?;
        let program = cache_command(row.label)?.programs.last().copied()?;
        let dir = default_dir(
            self.roots().home(),
            tool,
            platform == disk_cleanup_core::Platform::Macos,
        );
        let c = Candidate::new(
            Locator::Path(dir),
            KindFacts::Cache(CacheFacts {
                label: row.label.to_string(),
                apt: false,
            }),
            disk_cleanup_core::AllocatedBytes(0),
            None,
            Identity { dev: 0, ino: 0 },
        );
        Self::lock(&self.verdicts).insert(c.id(), Some(SkipReason::ToolMissing));
        Self::lock(&self.missing_tools).insert(c.id(), program.to_string());
        Some(c)
    }
}

impl<D: DirOps + 'static> Target for CacheTarget<D> {
    fn name(&self) -> &'static str {
        "caches"
    }

    fn selectors(&self) -> &'static [SelectorId] {
        &[SelectorId::Caches, SelectorId::CacheApt]
    }

    fn selector_gate(&self, selector: SelectorId, env: &Env) -> KindGate {
        if selector == SelectorId::CacheApt && env.probe.euid() != 0 {
            return KindGate::Unavailable(SkipReason::NeedsRoot);
        }
        KindGate::Available
    }

    fn discover(&self, env: &Env, _selection: &Selection) -> Vec<Candidate> {
        let found = discover_caches(
            &CacheEnv {
                fs_root: &self.fs_root,
                ops: &self.ops,
                runner: env.runner,
                probe: env.probe,
            },
            &mut self.roots(),
        );
        let mut out: Vec<Candidate> = Vec::new();
        for item in found.items {
            let id = item.candidate.id();
            if let KindFacts::Cache(f) = item.candidate.facts() {
                if removal_via(f) == RemovalVia::Command {
                    Self::lock(&self.command_rows).insert(id, f.label.clone());
                }
            }
            let verdict = match item.verdict {
                CacheVerdict::Will(_) => None,
                CacheVerdict::Skip(r) => Some(r),
            };
            Self::lock(&self.verdicts).insert(id, verdict);
            out.push(item.candidate);
        }
        for tool in found.missing_tools {
            out.extend(self.missing_tool_candidate(tool, env));
        }
        out
    }

    fn guard(&self, c: &Candidate, env: &Env) -> Guarded {
        if let Some(Some(r)) = Self::lock(&self.verdicts).get(&c.id()).copied() {
            return Guarded::Skip(r);
        }
        if let Err(r) = self.rederive(c, env.probe) {
            return Guarded::Skip(r);
        }
        match self.command_label(c).and_then(|l| Self::plan_for(&l, env)) {
            Some(CmdPlan::Skip { reason, .. }) => Guarded::Skip(reason),
            Some(CmdPlan::Ready(_)) | None => {
                // Path rows still obey the root rule (a root run never removes user caches).
                let apt = matches!(c.facts(), KindFacts::Cache(f) if f.apt);
                match root_decision(apt, env.probe.euid(), false) {
                    Some(r) => Guarded::Skip(r),
                    None => Guarded::Allow,
                }
            }
        }
    }

    fn detail(&self, c: &Candidate) -> Option<String> {
        Self::lock(&self.missing_tools).get(&c.id()).cloned()
    }

    fn execute(
        &self,
        c: &Candidate,
        env: &Env,
        proof: &ApplyToken,
    ) -> Result<Freed, DeleteFailure> {
        let allowed = match self.rederive(c, env.probe) {
            Ok(a) => a,
            // A vanished cache is success by idempotence (I7).
            Err(SkipReason::Gone) => return Ok(Freed::default()),
            Err(r) => return Err(failure(delete_error_of(r))),
        };
        let Some(label) = self.command_label(c) else {
            return self.remove(&allowed, env.guard.abort_flag(), proof);
        };
        match Self::plan_for(&label, env) {
            Some(CmdPlan::Ready(ready)) => run_cache_command(
                &self.ops,
                allowed.path(),
                allowed.ident(),
                RootDev(allowed.ident().dev),
                &ready.cmd,
                env.runner,
                proof,
            ),
            _ => Err(failure(DeleteError::Refused)),
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
