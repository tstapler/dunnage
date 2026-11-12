//! The `explorer-selection` target (Task 6.6.3e, ADR-006): removes the directories the owner marked in
//! `dunnage explore`, one `delete_tree` each, only the ones the reviewed plan listed.
//!
//! It has no discovery of its own: the marks are the items. Everything a delete trusts is read again at apply
//! time and compared with what the owner reviewed:
//!
//! - the scan roots are the ones `RootSet::resolve_on` gave *now*; the longest root that holds the path is
//!   used, and its `(dev, ino)` must equal the one in the reviewed plan (the open root is compared with the
//!   *planned* identity, never with a fresh read of itself, which would be circular, review C8);
//! - rules M1 to M5 run again against fresh `lstat` and mount data (`judge_fresh`), and the `DeletePolicy` is
//!   built only from the resulting `Allowed` witness (`DeletePolicy::for_allowed_roots`);
//! - the parent chain is opened fd by fd from the root with `O_NOFOLLOW`, the final entry is `fstat`ed and
//!   must equal the planned [`ItemIdentity`] by value, and `delete_tree` gets the *planned* identity as
//!   `expect`, never the fresh one (review 2, N4);
//! - the root's mount id is read from the opened root and a missing one refuses the item (review C9).
//!
//! A struct literal of `DeletePolicy` does not appear here: `check-token-sites.sh` fails the build if it does.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use dunnage_core::delete_types::FileKind;
use dunnage_core::explore::confirm::ReviewedItem;
use dunnage_core::explore::mark_check::OwnState;
use dunnage_core::explorer_plan::{judge_fresh, mount_facts, FreshCheck};
use dunnage_core::floor_rule::ForAllowedError;
use dunnage_core::plan::ItemIdentity;
use dunnage_core::ports::Env;
use dunnage_core::roots::{strictly_below, RootIdentity, RootSet};
use dunnage_core::selector::Selection;
use dunnage_core::target::{Guarded, Target};
use dunnage_core::{
    AbortFlag, AllocatedBytes, Allowed, ApplyToken, Candidate, CandidateId, DeleteError,
    DeleteFailure, DeletePolicy, ExplorerFacts, Freed, Identity, KindFacts, Locator, RootDev,
    SelectorId, SkipReason,
};

use crate::delete_tree;
use crate::dirops::{open_parent, open_verified_root, DirOps};

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn failure(error: DeleteError) -> DeleteFailure {
    DeleteFailure {
        error,
        freed: Freed::default(),
    }
}

/// The candidate of one reviewed item, from the `lstat` taken now. The explorer's apply builds its items with
/// this, so the target and the apply agree on what an item is.
pub fn candidate_of(path: &Path, st: &dunnage_core::delete_types::StatInfo) -> Candidate {
    Candidate::new(
        Locator::Path(path.to_path_buf()),
        KindFacts::Explorer(ExplorerFacts {
            is_dir: st.kind == FileKind::Dir,
        }),
        AllocatedBytes(st.blocks.saturating_mul(512)),
        Some(st.mtime_secs),
        st.ident,
    )
}

/// The engine's refusal as the catalogue reason, never `Symlink` for something that is not a link.
fn skip_of(e: ForAllowedError) -> DeleteError {
    match e {
        ForAllowedError::ChangedSincePlan => DeleteError::IdentityMismatch,
        ForAllowedError::WrongKind | ForAllowedError::NotADirectory => DeleteError::Refused,
        ForAllowedError::ProtectedPath => DeleteError::ProtectedPath,
    }
}

pub struct ExplorerSelectionTarget<D: DirOps> {
    /// The roots resolved at apply time.
    roots: RootSet,
    own_state: OwnState,
    ops: D,
    reviewed: Vec<ReviewedItem>,
    reviewed_roots: Vec<RootIdentity>,
    /// The `Allowed` of an item whose pre-delete guard just passed, taken by the `execute` that follows.
    verified: Mutex<HashMap<CandidateId, Allowed>>,
}

impl<D: DirOps> ExplorerSelectionTarget<D> {
    /// `roots` are re-resolved by the caller at apply time; `reviewed` and `reviewed_roots` come from the
    /// owner's [`ReviewedPlan`](dunnage_core::explore::confirm::ReviewedPlan).
    pub fn new(
        roots: RootSet,
        own_state: OwnState,
        ops: D,
        reviewed: Vec<ReviewedItem>,
        reviewed_roots: Vec<RootIdentity>,
    ) -> ExplorerSelectionTarget<D> {
        ExplorerSelectionTarget {
            roots,
            own_state,
            ops,
            reviewed,
            reviewed_roots,
            verified: Mutex::new(HashMap::new()),
        }
    }

    fn planned(&self, path: &Path) -> Option<ItemIdentity> {
        self.reviewed
            .iter()
            .find(|i| i.path == path)
            .map(|i| i.identity)
    }

    /// The longest scan root that holds `path` and the identity the plan recorded for it. `IdentityChanged`
    /// when the root is not in the plan or the root now resolves to another directory.
    fn root_of(&self, path: &Path) -> Result<(PathBuf, Identity), SkipReason> {
        let Some(root) = self.roots.root_of(path) else {
            // A planned root that held the path no longer resolves to a root (replaced by a symlink or moved):
            // that is a change since the plan, not a path outside every root.
            let was_planned = self
                .reviewed_roots
                .iter()
                .any(|r| strictly_below(&r.path, path));
            return Err(if was_planned {
                SkipReason::IdentityChanged
            } else {
                SkipReason::OutsideRoot
            });
        };
        let planned = self
            .reviewed_roots
            .iter()
            .find(|r| r.path == root)
            .ok_or(SkipReason::IdentityChanged)?;
        let fresh = self
            .roots
            .root_identities()
            .iter()
            .find(|r| r.path == root)
            .ok_or(SkipReason::IdentityChanged)?;
        if fresh.ident != planned.ident {
            return Err(SkipReason::IdentityChanged);
        }
        Ok((root.to_path_buf(), planned.ident))
    }

    /// The full, fresh decision for `c`: planned, root unchanged, same thing behind the path, rules M1 to M5.
    fn recheck(&self, c: &Candidate, env: &Env) -> Result<Allowed, SkipReason> {
        let Locator::Path(path) = c.locator() else {
            return Err(SkipReason::OutsideRoot);
        };
        let planned = self.planned(path).ok_or(SkipReason::OutsideRoot)?;
        self.root_of(path)?;
        let st = env.fs.lstat(path).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ENOTDIR) {
                SkipReason::IdentityChanged
            } else {
                SkipReason::ProtectedPath
            }
        })?;
        if ItemIdentity::of(&st) != planned {
            return Err(SkipReason::IdentityChanged);
        }
        let (mounts, mount_points) = mount_facts(env.probe);
        let fc = FreshCheck {
            roots: &self.roots,
            own_state: &self.own_state,
            fs: env.fs,
        };
        judge_fresh(path, &st, &mounts, &mount_points, &fc).allowed
    }

    fn remove(
        &self,
        allowed: &Allowed,
        planned: ItemIdentity,
        abort: &AbortFlag,
        proof: &ApplyToken,
    ) -> Result<Freed, DeleteFailure> {
        let path = allowed.path();
        let (base, base_id) = self.root_of(path).map_err(|_| {
            // The caller already checked the root; a change since then is a change.
            failure(DeleteError::IdentityMismatch)
        })?;
        let name = path
            .file_name()
            .ok_or_else(|| failure(DeleteError::ProtectedPath))?;
        let rel_parent = path
            .parent()
            .and_then(|p| p.strip_prefix(&base).ok())
            .ok_or_else(|| failure(DeleteError::OutsideScope))?;
        let root = open_verified_root(&self.ops, &base, base_id).map_err(failure)?;
        let parent = open_parent(&self.ops, &root, rel_parent).map_err(failure)?;
        let opened_root = self
            .ops
            .fstat(root.dir())
            .map_err(|_| failure(DeleteError::OutsideScope))?;
        // Without the root's mount id a bind mount of another directory could not be told apart: fail closed.
        let root_mnt = opened_root
            .mnt_id
            .ok_or_else(|| failure(DeleteError::OutsideScope))?;

        // The final entry as it is now, by value against the planned identity. A symlink, a file or another
        // inode in its place is "changed since the plan", not a symlink refusal.
        let entry = self.ops.stat_at(parent.dir(), name).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                failure(DeleteError::IdentityMismatch)
            } else {
                failure(DeleteError::Io(e.raw_os_error().unwrap_or(libc::EIO)))
            }
        })?;
        if ItemIdentity::of(&entry) != planned {
            return Err(failure(DeleteError::IdentityMismatch));
        }
        // The opened directory (not the name) is what must not be `$HOME` or an ancestor of it, whatever
        // path it was reached by (review 2, N3).
        let dir = self
            .ops
            .open_dir(parent.dir(), name)
            .map_err(|e| failure(crate::dirops::map_open_error(&e)))?;
        let opened = self
            .ops
            .fstat(&dir)
            .map_err(|e| failure(crate::dirops::map_open_error(&e)))?;
        drop(dir);
        if ItemIdentity::of(&opened) != planned {
            return Err(failure(DeleteError::IdentityMismatch));
        }
        if self.roots.in_home_chain(opened.ident) {
            return Err(failure(DeleteError::ProtectedPath));
        }

        let policy = DeletePolicy::for_allowed_roots(
            allowed,
            RootDev(base_id.dev),
            root_mnt,
            &self.roots,
            planned,
        )
        .map_err(|e| failure(skip_of(e)))?;
        delete_tree(
            &self.ops,
            &parent,
            name,
            Identity {
                dev: planned.dev,
                ino: planned.ino,
            },
            &policy,
            abort,
            proof,
        )
    }
}

impl<D: DirOps + 'static> Target for ExplorerSelectionTarget<D> {
    fn name(&self) -> &'static str {
        "explorer-selection"
    }

    fn selectors(&self) -> &'static [SelectorId] {
        &[SelectorId::ExplorerSelection]
    }

    /// The marks of the selection that still exist and were reviewed: the items by definition.
    fn discover(&self, env: &Env, selection: &Selection) -> Vec<Candidate> {
        selection
            .only_sorted()
            .into_iter()
            .filter(|p| self.planned(p).is_some())
            .filter_map(|p| env.fs.lstat(p).ok().map(|st| candidate_of(p, &st)))
            .collect()
    }

    fn guard(&self, c: &Candidate, env: &Env) -> Guarded {
        match self.recheck(c, env) {
            Ok(a) => {
                lock(&self.verified).insert(c.id(), a);
                Guarded::Allow
            }
            Err(r) => Guarded::Skip(r),
        }
    }

    fn execute(
        &self,
        c: &Candidate,
        env: &Env,
        proof: &ApplyToken,
    ) -> Result<Freed, DeleteFailure> {
        let allowed = match lock(&self.verified).remove(&c.id()) {
            Some(a) => a,
            None => self
                .recheck(c, env)
                .map_err(|r| failure(crate::targets::build_dir::delete_error_of(r)))?,
        };
        let planned = self
            .planned(allowed.path())
            .ok_or_else(|| failure(DeleteError::OutsideScope))?;
        // The run guard's flag is the one the signal handler sets, so a signal stops a large tree at its next
        // entry (review 2, N8).
        self.remove(&allowed, planned, env.guard.abort_flag(), proof)
    }
}
