//! The `cargo-home` kind: unpacked crates (`registry/src/<index>/<crate>-<version>`) and git checkouts
//! (`git/checkouts/<repo>/<rev>`) below `CARGO_HOME` that nothing has touched for the age floor. Both are
//! working copies cargo re-creates from what stays: `registry/cache/*.crate` and `git/db`. Those, the
//! registry index, `bin/` and the credentials file are never listed.
//!
//! Age is the newest mtime inside the entry, never `atime` (a `relatime` or `noatime` mount makes it say
//! nothing about use). No delete reaches the entry through a symlink: `CARGO_HOME`, `registry/`, `git/`, the
//! index or repo directory and the entry itself are each checked with `lstat`, and the engine opens every
//! level with `O_NOFOLLOW`.
//!
//! DECISION (the missing `Allowed` witness, as in `build_dir.rs`): `guard_before_delete` runs the full
//! re-check and parks the `Allowed`; `execute` without a parked witness runs the re-check itself.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use dunnage_core::guard::Facts;
use dunnage_core::ports::Env;
use dunnage_core::roots::RootSet;
use dunnage_core::selector::{Selection, CARGO_HOME_FLOOR_SECS};
use dunnage_core::target::{Guarded, Target};
use dunnage_core::targets::build_dir::{run_state_skip, BuildGuard};
use dunnage_core::targets::cargo::{cargo_home_min_age, decide_cargo_home, CargoHomeFacts};
use dunnage_core::{
    AbortFlag, AllocatedBytes, Allowed, AllowedTarget, ApplyToken, Candidate, CandidateId,
    CargoHomeArea, CargoHomeFacts as CandidateFacts, DeleteError, DeleteFailure, DeletePolicy,
    FileKind, Freed, Identity, Kind, KindFacts, Locator, NamePattern, RootDev, SelectorId,
    SkipReason,
};

use crate::delete_tree;
use crate::dirops::{open_parent, open_verified_root, DirOps};
use crate::discover_build::symlink_between;
use crate::mtime_probe::{to_system_time, unix_secs};
use crate::targets::build_dir::{
    delete_error_of, failure, lock, root_meta_dev, DETAIL_MOUNTS_UNREADABLE,
};
use crate::treestat::{measure, MeasureOpts, SkipWhy};

const AREAS: [(CargoHomeArea, &str); 2] = [
    (CargoHomeArea::RegistrySrc, "registry/src"),
    (CargoHomeArea::GitCheckout, "git/checkouts"),
];

type Verdict = (SkipReason, Option<String>);

#[derive(Debug, Default)]
struct Discovered {
    min_age: Duration,
    notes: Vec<String>,
}

pub struct CargoHomeTarget<D: DirOps> {
    roots: RootSet,
    ops: D,
    state: Mutex<Discovered>,
    /// The `Allowed` of an item whose pre-delete guard just passed, taken by the `execute` that follows.
    verified: Mutex<HashMap<CandidateId, Allowed>>,
}

fn plain_dir(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_dir())
}

/// The names of the plain (not linked) subdirectories of `dir`, sorted; hidden ones included only when
/// `hidden`.
fn subdirs(dir: &Path, hidden: bool) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| hidden || !n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

impl<D: DirOps> CargoHomeTarget<D> {
    pub fn new(roots: RootSet, ops: D) -> CargoHomeTarget<D> {
        CargoHomeTarget {
            roots,
            ops,
            state: Mutex::new(Discovered::default()),
            verified: Mutex::new(HashMap::new()),
        }
    }

    fn min_age(&self) -> Duration {
        lock(&self.state).min_age
    }

    /// Full measure of `path` (`lstat` first, then `openat` with `O_NOFOLLOW`).
    fn measure_entry(
        &self,
        path: &Path,
        dev: RootDev,
    ) -> Option<(Identity, AllocatedBytes, Option<i64>, bool)> {
        let m = fs::symlink_metadata(path)
            .ok()
            .filter(|m| m.file_type().is_dir())?;
        let ident = Identity::of(&m);
        let root = open_verified_root(&self.ops, path, ident).ok()?;
        let stats = measure(&self.ops, root.dir(), dev, MeasureOpts::default());
        let readable = !stats
            .skipped
            .iter()
            .any(|s| matches!(s.why, SkipWhy::Unreadable(_) | SkipWhy::TooDeep));
        Some((
            ident,
            stats.allocated,
            stats.newest_mtime.map(unix_secs).or(Some(m.mtime())),
            readable,
        ))
    }

    fn facts_for(area: CargoHomeArea, source: &str, age: i64) -> KindFacts {
        KindFacts::CargoHome(CandidateFacts {
            area,
            source: source.to_string(),
            age_secs: u64::try_from(age).unwrap_or(0),
        })
    }

    /// Re-validates `path` against fresh state; the verdict, or why not.
    fn recheck(&self, c: &Candidate, env: &Env) -> Result<Allowed, Verdict> {
        let Locator::Path(path) = c.locator() else {
            return Err((SkipReason::OutsideRoot, None));
        };
        let skip = |r| Err((r, None));
        let (Some(name), Some(index_dir)) = (path.file_name(), path.parent()) else {
            return skip(SkipReason::OutsideRoot);
        };
        let (Some(source), Some(base)) = (index_dir.file_name(), index_dir.parent()) else {
            return skip(SkipReason::OutsideRoot);
        };
        if !self.roots.admits(Kind::CargoHome, path) {
            return skip(SkipReason::OutsideRoot);
        }
        let home = self.roots.home();
        let is_symlink = symlink_between(home, path)
            || !plain_dir(base)
            || fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
        let Ok(base_md) = fs::metadata(base) else {
            return skip(SkipReason::Gone);
        };
        let base_id = Identity::of(&base_md);
        let opened = open_verified_root(&self.ops, base, base_id)
            .and_then(|vr| open_parent(&self.ops, &vr, Path::new(source)).map(|p| (vr, p)));
        let (vroot, parent) = match opened {
            Ok(x) => x,
            Err(DeleteError::Symlink) => return skip(SkipReason::Symlink),
            Err(DeleteError::Io(libc::ENOENT)) => return skip(SkipReason::Gone),
            Err(DeleteError::IdentityMismatch) => return skip(SkipReason::IdentityChanged),
            Err(_) => return skip(SkipReason::OutsideRoot),
        };
        let st = match self.ops.stat_at(parent.dir(), name) {
            Ok(st) => st,
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return skip(SkipReason::Gone),
            Err(_) => return skip(SkipReason::IdentityChanged),
        };
        let root_dev = self
            .ops
            .fstat(vroot.dir())
            .map_or(RootDev(base_id.dev), |s| RootDev(s.ident.dev));
        let Ok(mounts) = env.probe.mounts() else {
            return Err((
                SkipReason::MountPoint,
                Some(DETAIL_MOUNTS_UNREADABLE.to_string()),
            ));
        };
        let path_facts = Facts {
            kind: Kind::CargoHome,
            path: path.clone(),
            ident: st.ident,
            is_symlink: is_symlink || st.kind == FileKind::Symlink,
            root_dev,
            is_mount_point: mounts.iter().any(|m| m.mount_point == *path),
        };
        let now = env.clock.now_epoch();
        let min_age = self.min_age();
        let floor_secs = i64::try_from(cargo_home_min_age(min_age).as_secs()).unwrap_or(i64::MAX);
        let cheap_ok = st.kind == FileKind::Dir
            && !path_facts.is_symlink
            && st.ident == c.ident()
            && st.ident.on_dev(root_dev)
            && !path_facts.is_mount_point;
        let newest = if cheap_ok {
            let floor = to_system_time(now - floor_secs, 0);
            match open_verified_root(&self.ops, path, st.ident) {
                Ok(root) => {
                    let stats = measure(
                        &self.ops,
                        root.dir(),
                        root_dev,
                        MeasureOpts {
                            stop_if_newer_than: Some(floor),
                        },
                    );
                    let unreadable = stats
                        .skipped
                        .iter()
                        .any(|s| matches!(s.why, SkipWhy::Unreadable(_) | SkipWhy::TooDeep));
                    if unreadable && !stats.stopped_early {
                        None
                    } else {
                        Some(stats.newest_mtime.map_or(st.mtime_secs, unix_secs))
                    }
                }
                Err(DeleteError::IdentityMismatch) => return skip(SkipReason::IdentityChanged),
                Err(DeleteError::Io(libc::ENOENT)) => return skip(SkipReason::Gone),
                Err(_) => None,
            }
        } else {
            None
        };
        let facts = CargoHomeFacts {
            path: path_facts,
            planned: c.ident(),
            newest_mtime: newest,
        };
        match decide_cargo_home(&facts, &self.roots, now, min_age) {
            BuildGuard::Allow(a) => Ok(a),
            BuildGuard::Skip { reason, detail } => Err((reason, detail)),
        }
    }

    fn remove(
        &self,
        allowed: &Allowed,
        abort: &AbortFlag,
        proof: &ApplyToken,
    ) -> Result<Freed, DeleteFailure> {
        let path = allowed.path();
        let name = path
            .file_name()
            .ok_or_else(|| failure(DeleteError::ProtectedPath))?;
        let index_dir = path
            .parent()
            .ok_or_else(|| failure(DeleteError::OutsideScope))?;
        let base = index_dir
            .parent()
            .ok_or_else(|| failure(DeleteError::OutsideScope))?;
        let source = index_dir
            .file_name()
            .ok_or_else(|| failure(DeleteError::OutsideScope))?;
        let base_md = fs::metadata(base).map_err(|_| failure(DeleteError::OutsideScope))?;
        let base_id = Identity::of(&base_md);
        let root = open_verified_root(&self.ops, base, base_id).map_err(failure)?;
        let parent = open_parent(&self.ops, &root, Path::new(source)).map_err(failure)?;
        let opened = self
            .ops
            .fstat(root.dir())
            .map_err(|_| failure(DeleteError::OutsideScope))?;
        let home = self.roots.home().to_path_buf();
        let policy = DeletePolicy {
            root_dev: RootDev(base_id.dev),
            root_mnt: opened.mnt_id,
            home_id: fs::metadata(&home)
                .map(|m| Identity::of(&m))
                .map_err(|_| failure(DeleteError::OutsideScope))?,
            home,
            scan_roots: self.roots.scan_roots().to_vec(),
            allowed: AllowedTarget::Within {
                root: base.to_path_buf(),
                name: NamePattern::CargoHomeEntry,
            },
            target: path.to_path_buf(),
            require_sibling_any: Vec::new(),
            max_depth: 256,
            restore_owner_write: true,
        };
        delete_tree(
            &self.ops,
            &parent,
            name,
            allowed.ident(),
            &policy,
            abort,
            proof,
        )
    }
}

impl<D: DirOps + 'static> Target for CargoHomeTarget<D> {
    fn name(&self) -> &'static str {
        "cargo-home"
    }

    fn selectors(&self) -> &'static [SelectorId] {
        &[SelectorId::CargoHome]
    }

    fn discover(&self, env: &Env, selection: &Selection) -> Vec<Candidate> {
        let min_age = Duration::from_secs(
            selection
                .effective_min_age(SelectorId::CargoHome)
                .unwrap_or(CARGO_HOME_FLOOR_SECS),
        );
        let floor_secs = i64::try_from(cargo_home_min_age(min_age).as_secs()).unwrap_or(i64::MAX);
        let mut st = Discovered {
            min_age,
            notes: Vec::new(),
        };
        let now = env.clock.now_epoch();
        let mut out = Vec::new();
        match self.roots.cargo_home() {
            None => st.notes.push(
                "cargo-home: CARGO_HOME is not a directory below $HOME; nothing listed".to_string(),
            ),
            Some(ch) => {
                let home = self.roots.home().to_path_buf();
                for (area, rel) in AREAS {
                    let base = ch.join(rel);
                    if !plain_dir(&base) {
                        continue;
                    }
                    if symlink_between(&home, &base) {
                        st.notes.push(format!(
                            "cargo-home: {} is reached through a symlink; skipped",
                            base.display()
                        ));
                        continue;
                    }
                    let Ok(base_md) = fs::metadata(&base) else {
                        continue;
                    };
                    let dev = RootDev(root_meta_dev(&base_md));
                    for source in subdirs(&base, true) {
                        let index_dir = base.join(&source);
                        for name in subdirs(&index_dir, false) {
                            let path = index_dir.join(&name);
                            let Ok(own) = fs::symlink_metadata(&path) else {
                                continue;
                            };
                            if now.saturating_sub(own.mtime()) < floor_secs {
                                continue;
                            }
                            let Some((ident, bytes, newest, readable)) =
                                self.measure_entry(&path, dev)
                            else {
                                continue;
                            };
                            let Some(newest) = newest.filter(|n| readable && now - n >= floor_secs)
                            else {
                                continue;
                            };
                            out.push(Candidate::new(
                                Locator::Path(path.clone()),
                                Self::facts_for(area, &source, now - newest),
                                bytes,
                                Some(newest),
                                ident,
                            ));
                        }
                    }
                }
            }
        }
        *lock(&self.state) = st;
        out
    }

    fn guard(&self, c: &Candidate, env: &Env) -> Guarded {
        let Locator::Path(path) = c.locator() else {
            return Guarded::Skip(SkipReason::OutsideRoot);
        };
        match run_state_skip(env.probe.run_state(path)) {
            Some(r) => Guarded::Skip(r),
            None => Guarded::Allow,
        }
    }

    fn guard_before_delete(&self, c: &Candidate, env: &Env) -> Guarded {
        let allowed = match self.recheck(c, env) {
            Ok(a) => a,
            Err((r, _)) => return Guarded::Skip(r),
        };
        if let Some(r) = run_state_skip(env.probe.run_state(allowed.path())) {
            return Guarded::Skip(r);
        }
        lock(&self.verified).insert(c.id(), allowed);
        Guarded::Allow
    }

    fn execute(
        &self,
        c: &Candidate,
        env: &Env,
        proof: &ApplyToken,
    ) -> Result<Freed, DeleteFailure> {
        let parked = lock(&self.verified).remove(&c.id());
        let allowed = match parked {
            Some(a) => a,
            None => match self.recheck(c, env) {
                Ok(a) => {
                    if let Some(r) = run_state_skip(env.probe.run_state(a.path())) {
                        return Err(failure(delete_error_of(r)));
                    }
                    a
                }
                // A vanished entry is success by idempotence (I7).
                Err((SkipReason::Gone, _)) => return Ok(Freed::default()),
                Err((r, _)) => return Err(failure(delete_error_of(r))),
            },
        };
        self.remove(&allowed, env.guard.abort_flag(), proof)
    }

    fn plan_notes(&self, _env: &Env, _selection: &Selection) -> Vec<String> {
        lock(&self.state).notes.clone()
    }
}
