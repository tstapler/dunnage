//! `WorktreeTarget` (Story 3.1.3): discovery per main repo, the guard, and removal by `git worktree remove`
//! alone. Never `--force`, never `prune`: git's own refusal of dirty and locked trees is the second line of
//! defence, and the one command goes through `CommandRunner::remove_worktree`, whose argument can be built
//! only from an `Allowed` the guard returned.
//!
//! The guard is asked three times for an applied item: at plan time (the probe `discover` already made),
//! by the executor right before `execute`, and by `execute` itself. The latter two probe afresh and read the
//! admin entry's mtime before any `git` command (`probe_worktree` does, and `git worktree list` and
//! `status` run with `GIT_OPTIONAL_LOCKS=0` so neither moves it).

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use disk_cleanup_core::par::{default_workers, par_map};
use disk_cleanup_core::ports::{Cmd, Env};
use disk_cleanup_core::roots::RootSet;
use disk_cleanup_core::selector::Selection;
use disk_cleanup_core::target::{Guarded, ItemFacts, Target};
use disk_cleanup_core::targets::worktree_guard::{WorktreeGuard, WorktreeOpts};
use disk_cleanup_core::{
    AllocatedBytes, ApparentBytes, ApplyToken, Candidate, CandidateId, DeleteError, DeleteFailure,
    Freed, KindFacts, Locator, RootDev, SelectorId, SkipReason, WorktreeFacts, WorktreeRemoval,
};

use crate::dirops::{open_verified_root, DirOps};
use crate::mtime_probe::NewestStore;
use crate::targets::worktree_discover::{
    collect_repos, discover_worktrees, git_dir_parents, parse_porcelain_z, WorktreeSite,
};
use crate::targets::worktree_probe::{guard_worktree, last_commit, ProbeEnv, WorktreeProbed};
use crate::treestat::{measure, MeasureOpts};

/// Depth of the `.git` search below a scan root when the scan gave no repositories.
const REPO_SEARCH_DEPTH: usize = 6;

/// `--min-age` default when `discover` has not run (it always has in a real run).
const DEFAULT_MIN_AGE: Duration = Duration::from_hours(24 * 14);

struct Item {
    /// The plan-time answer; `None` is `Allow`.
    verdict: Option<SkipReason>,
    detail: Option<String>,
    facts: ItemFacts,
}

#[derive(Default)]
struct State {
    run: Option<WorktreeOpts>,
    items: HashMap<CandidateId, Item>,
    /// Ids whose plan-time answer has not been read yet: the first `guard` returns it, later ones re-probe.
    unread: HashSet<CandidateId>,
    warnings: Vec<String>,
    store: NewestStore,
}

pub struct WorktreeTarget<D: DirOps> {
    roots: RootSet,
    ops: D,
    state: Mutex<State>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn failure(error: DeleteError) -> DeleteFailure {
    DeleteFailure {
        error,
        freed: Freed::default(),
    }
}

/// How a guard skip that surfaces inside `execute` is reported. The executor's own guard call right before
/// `execute` reports every reason as a `SKIP` row; this path is the race window after that call, where only
/// the scope reasons have a `DeleteError` of their own and the rest read as a changed directory.
fn delete_error_of(r: SkipReason) -> DeleteError {
    match r {
        SkipReason::Symlink => DeleteError::Symlink,
        SkipReason::CrossDevice => DeleteError::CrossDevice,
        SkipReason::IdentityChanged => DeleteError::IdentityMismatch,
        SkipReason::ProtectedPath | SkipReason::MountPoint => DeleteError::ProtectedPath,
        SkipReason::OutsideRoot => DeleteError::OutsideScope,
        _ => DeleteError::Busy,
    }
}

/// The site of `c` as git lists it now: `locked`, `HEAD` and detachment may have changed since discovery.
/// `None` when git no longer lists the path (or cannot be asked).
fn relist(env: &Env, c: &Candidate) -> Option<WorktreeSite> {
    let (Locator::Path(path), KindFacts::Worktree(f)) = (c.locator(), c.facts()) else {
        return None;
    };
    let cmd = Cmd::new(
        "git",
        [
            "-C".as_ref(),
            f.main_repo.as_os_str(),
            "worktree".as_ref(),
            "list".as_ref(),
            "--porcelain".as_ref(),
            "-z".as_ref(),
        ],
        false,
    );
    let out = env.runner.run(&cmd).ok()?;
    let entry = parse_porcelain_z(&out.stdout)
        .into_iter()
        .find(|e| &e.path == path)?;
    Some(WorktreeSite {
        main_repo: f.main_repo.clone(),
        entry,
        ident: Some(c.ident()),
    })
}

/// The timestamp the plan's `--min-age` rule reads (`Selection::admits`): the last commit, which the
/// probe did not read for a worktree an earlier guard already stopped. A worktree without one falls
/// back to its admin entry, so a gone or locked row reports its own reason instead of `TooRecent`.
fn age_epoch(env: &Env, site: &WorktreeSite, probed: &WorktreeProbed) -> Option<i64> {
    let path = &site.entry.path;
    let commit = probed.candidate.last_commit_epoch.or_else(|| {
        probed
            .facts
            .as_ref()
            .and_then(|_| last_commit(env.runner, path))
    });
    commit.or_else(|| {
        let name = path.file_name()?;
        let admin = site.main_repo.join(".git/worktrees").join(name);
        std::fs::metadata(admin).ok().map(|m| m.mtime())
    })
}

impl<D: DirOps> WorktreeTarget<D> {
    pub fn new(roots: RootSet, ops: D) -> WorktreeTarget<D> {
        WorktreeTarget {
            roots,
            ops,
            state: Mutex::new(State::default()),
        }
    }

    fn opts(&self) -> WorktreeOpts {
        lock(&self.state).run.unwrap_or(WorktreeOpts {
            min_age: DEFAULT_MIN_AGE,
            discard_ignored: false,
        })
    }

    fn probe_env<'a>(&'a self, env: &'a Env<'a>) -> ProbeEnv<'a, D> {
        ProbeEnv {
            runner: env.runner,
            probe: env.probe,
            ops: &self.ops,
            roots: &self.roots,
        }
    }

    /// Probes and decides `site` now.
    fn probe(
        &self,
        env: &Env,
        site: &WorktreeSite,
        opts: WorktreeOpts,
    ) -> (WorktreeProbed, WorktreeGuard) {
        let mut store = std::mem::take(&mut lock(&self.state).store);
        let out = self.probe_with(env, site, opts, &mut store);
        lock(&self.state).store = store;
        out
    }

    fn probe_with(
        &self,
        env: &Env,
        site: &WorktreeSite,
        opts: WorktreeOpts,
        store: &mut NewestStore,
    ) -> (WorktreeProbed, WorktreeGuard) {
        guard_worktree(
            &self.probe_env(env),
            site,
            env.clock.now_epoch(),
            opts,
            store,
        )
    }

    /// A fresh guard of `c`: `Err` is the skip that stopped it, `Ok` the witness and the probe.
    fn fresh_guard(
        &self,
        env: &Env,
        c: &Candidate,
    ) -> Result<(disk_cleanup_core::Allowed, WorktreeProbed), SkipReason> {
        let site = relist(env, c).ok_or(SkipReason::Gone)?;
        let (probed, guard) = self.probe(env, &site, self.opts());
        match guard {
            WorktreeGuard::Allow(a) => Ok((a, probed)),
            WorktreeGuard::Skip { reason, .. } => Err(reason),
        }
    }

    fn measure_tree(&self, path: &std::path::Path, ident: disk_cleanup_core::Identity) -> Freed {
        let Ok(root) = open_verified_root(&self.ops, path, ident) else {
            return Freed::default();
        };
        let s = measure(
            &self.ops,
            root.dir(),
            RootDev(ident.dev),
            MeasureOpts::default(),
        );
        Freed {
            attributed_bytes: s.allocated,
            apparent_bytes: s.apparent,
            entries_removed: s.entries,
            inodes_freed: s.inodes,
        }
    }
}

fn item_of(probed: &WorktreeProbed, guard: &WorktreeGuard, discard: bool) -> Item {
    let c = &probed.candidate;
    let ignored = c.ignored.map(|i| (i.entries, i.bytes));
    let discards = match (guard, ignored) {
        (WorktreeGuard::Allow(_), Some((n, b))) if discard && n > 0 => Some((n, b)),
        _ => None,
    };
    Item {
        verdict: guard.skip_reason(),
        detail: guard.detail().map(str::to_string),
        facts: ItemFacts {
            unpushed: c.unpushed(),
            process: c.holder.clone(),
            ignored,
            discards,
        },
    }
}

impl<D: DirOps + 'static> Target for WorktreeTarget<D> {
    fn name(&self) -> &'static str {
        "worktrees"
    }

    fn selectors(&self) -> &'static [SelectorId] {
        &[SelectorId::Worktrees]
    }

    fn discover(&self, env: &Env, selection: &Selection) -> Vec<Candidate> {
        let opts = WorktreeOpts {
            min_age: Duration::from_secs(
                selection
                    .effective_min_age(SelectorId::Worktrees)
                    .unwrap_or(DEFAULT_MIN_AGE.as_secs()),
            ),
            discard_ignored: selection.worktree_ignored_discard(),
        };
        let roots: Vec<PathBuf> = if selection.roots.is_empty() {
            self.roots.scan_roots().to_vec()
        } else {
            selection.roots.clone()
        };
        let seeds = git_dir_parents(&roots, REPO_SEARCH_DEPTH);
        let repos = collect_repos(env.runner, &seeds);
        let found = discover_worktrees(env.runner, &repos.repos);
        let mut warnings = repos.warnings;
        warnings.extend(found.warnings);
        {
            let mut st = lock(&self.state);
            st.run = Some(opts);
            st.items.clear();
            st.unread.clear();
            st.warnings = warnings;
        }
        // Probes are independent per site (a few `git` runs and a tree measure each): run them side by side,
        // keep the order git listed them in.
        let probed = par_map(&found.sites, default_workers(), |site| {
            let mut store = NewestStore::in_memory();
            let (probed, guard) = self.probe_with(env, site, opts, &mut store);
            let ident = probed.facts.as_ref().map_or(probed.planned, |f| f.ident);
            let est = if matches!(guard, WorktreeGuard::Allow(_)) {
                self.measure_tree(&site.entry.path, ident).attributed_bytes
            } else {
                AllocatedBytes(0)
            };
            let c = Candidate::new(
                Locator::Path(site.entry.path.clone()),
                KindFacts::Worktree(WorktreeFacts {
                    main_repo: site.main_repo.clone(),
                    branch: site.entry.branch.clone(),
                }),
                est,
                age_epoch(env, site, &probed),
                probed.planned,
            );
            let item = item_of(&probed, &guard, opts.discard_ignored);
            (c, item, store)
        });
        let mut out = Vec::with_capacity(probed.len());
        let mut st = lock(&self.state);
        for (c, item, store) in probed {
            st.items.insert(c.id(), item);
            st.unread.insert(c.id());
            st.store.absorb(store);
            out.push(c);
        }
        drop(st);
        out
    }

    fn guard(&self, c: &Candidate, env: &Env) -> Guarded {
        {
            let mut st = lock(&self.state);
            if st.unread.remove(&c.id()) {
                if let Some(item) = st.items.get(&c.id()) {
                    return item.verdict.map_or(Guarded::Allow, Guarded::Skip);
                }
            }
        }
        let discard = self.opts().discard_ignored;
        let site = relist(env, c);
        let Some(site) = site else {
            return Guarded::Skip(SkipReason::Gone);
        };
        let (probed, guard) = self.probe(env, &site, self.opts());
        let item = item_of(&probed, &guard, discard);
        let verdict = item.verdict;
        lock(&self.state).items.insert(c.id(), item);
        verdict.map_or(Guarded::Allow, Guarded::Skip)
    }

    fn execute(
        &self,
        c: &Candidate,
        env: &Env,
        proof: &ApplyToken,
    ) -> Result<Freed, DeleteFailure> {
        let KindFacts::Worktree(f) = c.facts() else {
            return Err(failure(DeleteError::Refused));
        };
        let (allowed, _) = match self.fresh_guard(env, c) {
            Ok(x) => x,
            // A vanished worktree is success by idempotence; it is never pruned.
            Err(SkipReason::Gone) => return Ok(Freed::default()),
            Err(r) => return Err(failure(delete_error_of(r))),
        };
        // Measured just before the command, from the directory the guard verified.
        let freed = self.measure_tree(allowed.path(), allowed.ident());
        let removal = WorktreeRemoval::new(allowed, f.main_repo.clone())
            .map_err(|_| failure(DeleteError::Refused))?;
        match env.runner.remove_worktree(&removal, proof) {
            Ok(_) => Ok(Freed {
                apparent_bytes: ApparentBytes(freed.apparent_bytes.0),
                ..freed
            }),
            Err(error) => Err(DeleteFailure {
                error,
                freed: Freed::default(),
            }),
        }
    }

    fn detail(&self, c: &Candidate) -> Option<String> {
        lock(&self.state)
            .items
            .get(&c.id())
            .and_then(|i| i.detail.clone())
    }

    fn item_facts(&self, c: &Candidate) -> ItemFacts {
        lock(&self.state)
            .items
            .get(&c.id())
            .map(|i| i.facts.clone())
            .unwrap_or_default()
    }

    fn finish(&self, _outcomes: &[disk_cleanup_core::Outcome], _env: &Env) -> Vec<String> {
        lock(&self.state).warnings.clone()
    }
}
