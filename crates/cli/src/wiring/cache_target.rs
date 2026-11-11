//! The cache kinds as a read-only `Target` (the `caches` and `cache-apt` selectors): discovery through
//! `adapters::discover_cache`, guard from the verdict discovery reached, and no way to remove anything.
//!
//! This build ships no apply path (the token, the lock and the confinement are Stories 2.1.6 and 2.2.2), so
//! `execute` refuses. Story 2.3.4 replaces this with the registry's own cache target.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use disk_cleanup_adapters::dirops::RealDirOps;
use disk_cleanup_adapters::discover_cache::{discover_caches, CacheEnv, CacheVerdict};
use disk_cleanup_core::ports::Env;
use disk_cleanup_core::roots::RootSet;
use disk_cleanup_core::selector::Selection;
use disk_cleanup_core::target::{Guarded, Target};
use disk_cleanup_core::{
    ApplyToken, Candidate, CandidateId, DeleteError, DeleteFailure, Freed, Outcome, SelectorId,
    SkipReason,
};

pub struct CacheTarget {
    roots: RootSet,
    /// Reads are confined to this directory (`DC_TEST_ROOT` or `/`).
    read_root: PathBuf,
    /// Absolute table rows (apt) are looked up below this directory.
    fs_root: PathBuf,
    verdicts: Mutex<HashMap<CandidateId, Guarded>>,
}

impl CacheTarget {
    pub fn new(roots: RootSet, read_root: PathBuf, fs_root: PathBuf) -> CacheTarget {
        CacheTarget {
            roots,
            read_root,
            fs_root,
            verdicts: Mutex::new(HashMap::new()),
        }
    }
}

impl Target for CacheTarget {
    fn name(&self) -> &'static str {
        "caches"
    }

    fn selectors(&self) -> &'static [SelectorId] {
        &[SelectorId::Caches, SelectorId::CacheApt]
    }

    fn discover(&self, env: &Env, _selection: &Selection) -> Vec<Candidate> {
        let ops = RealDirOps::confined_to(&self.read_root);
        let mut roots = self.roots.clone();
        let found = discover_caches(
            &CacheEnv {
                fs_root: &self.fs_root,
                ops: &ops,
                runner: env.runner,
                probe: env.probe,
            },
            &mut roots,
        );
        let mut verdicts = self
            .verdicts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        found
            .items
            .into_iter()
            .map(|item| {
                let guard = match item.verdict {
                    CacheVerdict::Will(_) => Guarded::Allow,
                    CacheVerdict::Skip(r) => Guarded::Skip(r),
                };
                verdicts.insert(item.candidate.id(), guard);
                item.candidate
            })
            .collect()
    }

    fn guard(&self, c: &Candidate, _env: &Env) -> Guarded {
        self.verdicts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&c.id())
            .copied()
            .unwrap_or(Guarded::Skip(SkipReason::Gone))
    }

    fn execute(
        &self,
        _c: &Candidate,
        _env: &Env,
        _proof: &ApplyToken,
    ) -> Result<Freed, DeleteFailure> {
        Err(DeleteFailure {
            error: DeleteError::Refused,
            freed: Freed::default(),
        })
    }

    fn finish(&self, _outcomes: &[Outcome], _env: &Env) -> Vec<String> {
        Vec::new()
    }
}
