//! The test-only `fixture-destructive` target (Story 2.3.4): removes directories named `stale-*` below the
//! scan roots through `delete_tree`, so the Gate B end-to-end tests exercise the destructive-flow gate, the
//! `Next:` echo, the spent-id journey and the `last_plan` merge without any kind that Gates C and D own.
//!
//! It exists only in a `test-support` build; [`fixture_destructive_target`] returns `None` otherwise, which is
//! what keeps the string out of a release binary.

use dunnage_core::roots::RootSet;
use dunnage_core::target::Target;

use crate::dirops::DirOps;

#[cfg(feature = "test-support")]
mod imp {
    use std::collections::HashSet;
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use dunnage_core::ports::Env;
    use dunnage_core::roots::RootSet;
    use dunnage_core::selector::Selection;
    use dunnage_core::target::{Guarded, Target};
    use dunnage_core::{
        AllocatedBytes, AllowedTarget, ApplyToken, Candidate, CandidateId, DeleteError,
        DeleteFailure, DeletePolicy, FixtureFacts, Freed, Identity, KindFacts, Locator, RootDev,
        SelectorId, SkipReason,
    };

    use crate::delete_tree;
    use crate::dirops::{open_parent, open_verified_root, DirOps};
    use crate::mtime_probe::unix_secs;
    use crate::treestat::{measure, MeasureOpts};

    const PREFIX: &str = "stale-";
    const MAX_DEPTH: usize = 6;

    fn failure(error: DeleteError) -> DeleteFailure {
        DeleteFailure {
            error,
            freed: Freed::default(),
        }
    }

    pub struct FixtureTarget<D: DirOps> {
        roots: RootSet,
        ops: D,
        /// Ids kept by `--keep-newest`, decided at discovery.
        kept: Mutex<HashSet<CandidateId>>,
    }

    impl<D: DirOps> FixtureTarget<D> {
        pub fn new(roots: RootSet, ops: D) -> FixtureTarget<D> {
            FixtureTarget {
                roots,
                ops,
                kept: Mutex::new(HashSet::new()),
            }
        }

        fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
            let Ok(rd) = std::fs::read_dir(dir) else {
                return;
            };
            let mut names: Vec<_> = rd.flatten().collect();
            names.sort_by_key(std::fs::DirEntry::file_name);
            for e in names {
                let Ok(t) = e.file_type() else { continue };
                if !t.is_dir() {
                    continue;
                }
                if e.file_name().to_string_lossy().starts_with(PREFIX) {
                    out.push(e.path());
                } else if depth < MAX_DEPTH {
                    Self::walk(&e.path(), depth + 1, out);
                }
            }
        }

        fn scan_roots(&self, selection: &Selection) -> Vec<PathBuf> {
            if selection.roots.is_empty() {
                self.roots.scan_roots().to_vec()
            } else {
                selection.roots.clone()
            }
        }

        fn containing_root(&self, path: &Path) -> Option<PathBuf> {
            self.roots
                .scan_roots()
                .iter()
                .find(|r| path.starts_with(r))
                .cloned()
                .or_else(|| {
                    path.starts_with(self.roots.home())
                        .then(|| self.roots.home().to_path_buf())
                })
        }

        fn candidate_of(&self, dir: &Path) -> Option<Candidate> {
            let md = std::fs::symlink_metadata(dir).ok()?;
            let ident = Identity::of(&md);
            let root = open_verified_root(&self.ops, dir, ident).ok()?;
            let stats = measure(
                &self.ops,
                root.dir(),
                RootDev(ident.dev),
                MeasureOpts::default(),
            );
            Some(Candidate::new(
                Locator::Path(dir.to_path_buf()),
                KindFacts::Fixture(FixtureFacts {
                    name: dir.file_name()?.to_string_lossy().into_owned(),
                }),
                AllocatedBytes(stats.allocated.0),
                stats.newest_mtime.map(unix_secs),
                ident,
            ))
        }

        fn fresh_skip(&self, c: &Candidate) -> Option<SkipReason> {
            let Locator::Path(path) = c.locator() else {
                return Some(SkipReason::OutsideRoot);
            };
            let Ok(md) = std::fs::symlink_metadata(path) else {
                return Some(SkipReason::Gone);
            };
            if md.file_type().is_symlink() {
                return Some(SkipReason::Symlink);
            }
            if Identity::of(&md) != c.ident() {
                return Some(SkipReason::IdentityChanged);
            }
            if self.roots.is_scan_root(path) || self.containing_root(path).is_none() {
                return Some(SkipReason::OutsideRoot);
            }
            None
        }

        fn remove(
            &self,
            c: &Candidate,
            env: &Env,
            proof: &ApplyToken,
        ) -> Result<Freed, DeleteFailure> {
            let Locator::Path(path) = c.locator() else {
                return Err(failure(DeleteError::OutsideScope));
            };
            let base = self
                .containing_root(path)
                .ok_or_else(|| failure(DeleteError::OutsideScope))?;
            let name = path
                .file_name()
                .ok_or_else(|| failure(DeleteError::ProtectedPath))?;
            let rel_parent = path
                .parent()
                .and_then(|p| p.strip_prefix(&base).ok())
                .ok_or_else(|| failure(DeleteError::OutsideScope))?;
            let base_id = Identity::of(
                &std::fs::metadata(&base).map_err(|_| failure(DeleteError::OutsideScope))?,
            );
            let root = open_verified_root(&self.ops, &base, base_id).map_err(failure)?;
            let parent = open_parent(&self.ops, &root, rel_parent).map_err(failure)?;
            let opened = self
                .ops
                .fstat(root.dir())
                .map_err(|_| failure(DeleteError::OutsideScope))?;
            let home = self.roots.home().to_path_buf();
            let policy = DeletePolicy {
                root_dev: RootDev(base_id.dev),
                root_mnt: opened.mnt_id,
                home_id: std::fs::metadata(&home)
                    .map(|m| Identity::of(&m))
                    .map_err(|_| failure(DeleteError::OutsideScope))?,
                home,
                scan_roots: self.roots.scan_roots().to_vec(),
                allowed: AllowedTarget::Exact(path.clone()),
                target: path.clone(),
                require_sibling_any: Vec::new(),
                max_depth: 256,
                restore_owner_write: true,
            };
            delete_tree(
                &self.ops,
                &parent,
                OsStr::new(name),
                c.ident(),
                &policy,
                env.guard.abort_flag(),
                proof,
            )
        }
    }

    impl<D: DirOps + 'static> Target for FixtureTarget<D> {
        fn name(&self) -> &'static str {
            "fixture-destructive"
        }

        fn selectors(&self) -> &'static [SelectorId] {
            &[SelectorId::FixtureDestructive]
        }

        fn discover(&self, _env: &Env, selection: &Selection) -> Vec<Candidate> {
            let mut dirs = Vec::new();
            for root in self.scan_roots(selection) {
                Self::walk(&root, 0, &mut dirs);
            }
            dirs.dedup();
            let found: Vec<Candidate> = dirs.iter().filter_map(|d| self.candidate_of(d)).collect();
            let mut kept = self
                .kept
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            kept.clear();
            if let Some(n) = selection.keep_newest {
                let mut by_age: Vec<&Candidate> = found.iter().collect();
                // Newest first; an unknown mtime counts as newest, so it is kept.
                by_age.sort_by_key(|c| std::cmp::Reverse(c.newest_mtime().unwrap_or(i64::MAX)));
                kept.extend(by_age.iter().take(n as usize).map(|c| c.id()));
            }
            found
        }

        fn guard(&self, c: &Candidate, _env: &Env) -> Guarded {
            if let Some(r) = self.fresh_skip(c) {
                return Guarded::Skip(r);
            }
            let kept = self
                .kept
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if kept.contains(&c.id()) {
                return Guarded::Skip(SkipReason::KeptAsNewest);
            }
            Guarded::Allow
        }

        fn execute(
            &self,
            c: &Candidate,
            env: &Env,
            proof: &ApplyToken,
        ) -> Result<Freed, DeleteFailure> {
            match self.fresh_skip(c) {
                // A vanished directory is success by idempotence (I7).
                Some(SkipReason::Gone) => Ok(Freed::default()),
                Some(SkipReason::Symlink) => Err(failure(DeleteError::Symlink)),
                Some(SkipReason::IdentityChanged) => Err(failure(DeleteError::IdentityMismatch)),
                Some(_) => Err(failure(DeleteError::OutsideScope)),
                None => self.remove(c, env, proof),
            }
        }
    }
}

#[cfg(feature = "test-support")]
pub use imp::FixtureTarget;

/// The `fixture-destructive` target, or `None` in a build without `test-support`.
#[allow(unused_variables, clippy::needless_pass_by_value)] // both only feed the test-support branch
pub fn fixture_destructive_target<D: DirOps + 'static>(
    roots: RootSet,
    ops: D,
) -> Option<Box<dyn Target>> {
    #[cfg(feature = "test-support")]
    {
        Some(Box::new(FixtureTarget::new(roots, ops)))
    }
    #[cfg(not(feature = "test-support"))]
    {
        None
    }
}
