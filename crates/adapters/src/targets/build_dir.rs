//! Delete-time re-check of one build dir (Story 3.2.1): gathers fresh facts through `DirOps` (the entry's
//! own `fstatat`, so a nested subvolume shows its own `st_dev`), then asks the pure `decide_build_dir`.
//! [`BuildDirTarget`] (Story 3.2.4) is the `build-dirs` kind built on it: discovery, keep-newest planning, the
//! running-build check and removal through `delete_tree`.
//!
//! DECISION (the missing `Allowed` witness, as in `cache.rs`): `Target::guard` returns no `Allowed`. The
//! pre-delete guard (`guard_before_delete`) runs the full re-check and parks the `Allowed` it obtained for the
//! one `execute` that follows; `execute` without that parked witness runs the re-check itself, so no removal
//! rests on a verdict stored at discovery.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use disk_cleanup_core::guard::Facts;
use disk_cleanup_core::ports::{Env, SystemProbe};
use disk_cleanup_core::roots::RootSet;
use disk_cleanup_core::selector::Selection;
use disk_cleanup_core::target::{Guarded, Target};
use disk_cleanup_core::targets::build_dir::{
    decide_build_dir, effective_min_age, kept_note, plan_keep_newest, run_state_skip, BuildFacts,
    BuildGuard, ClusterInput, KeepNewest, Placement, Reproducibility, DETAIL_LINKED_OUTSIDE,
    DETAIL_NO_LOCKFILE, FLOOR_RAISED_NOTE,
};
use disk_cleanup_core::{
    AbortFlag, Allowed, AllowedTarget, ApplyToken, BuildDirFlavor, Candidate, CandidateId,
    DeleteError, DeleteFailure, DeletePolicy, FileKind, Freed, Identity, Kind, KindFacts, Locator,
    NamePattern, RootDev, SelectorId, SkipReason, StatInfo,
};
use disk_cleanup_scan::classify::classify_path;
use disk_cleanup_scan::scan::BuildKind;

use crate::delete_tree;
use crate::dirops::{open_parent, open_verified_root, DirOps};
use crate::discover_build::{
    discover_build_dirs, symlink_between, BuildEnv, BuildKinds, BuildOpts, BuildVerdict,
};
use crate::mtime_probe::{
    probe_newest_mtime, to_system_time, unix_secs, NewestStore, ProbeTarget, Recency,
};

const LOCKFILES: [&str; 4] = [
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lockb",
];

/// The decision plus the newest mtime the probe found (for the plan row), when it looked.
#[derive(Debug)]
pub struct BuildRecheck {
    pub guard: BuildGuard,
    pub newest_mtime: Option<i64>,
}

fn skip(r: SkipReason) -> BuildRecheck {
    BuildRecheck {
        guard: BuildGuard::Skip {
            reason: r,
            detail: None,
        },
        newest_mtime: None,
    }
}

fn skip_with(r: SkipReason, detail: &str) -> BuildRecheck {
    BuildRecheck {
        guard: BuildGuard::Skip {
            reason: r,
            detail: Some(detail.to_string()),
        },
        newest_mtime: None,
    }
}

fn kind_of(f: BuildDirFlavor) -> Kind {
    match f {
        BuildDirFlavor::NodeModules => Kind::NodeModules,
        BuildDirFlavor::CargoTarget => Kind::CargoTarget,
    }
}

fn marker_present(path: &Path, flavor: BuildDirFlavor) -> bool {
    matches!(
        (classify_path(path), flavor),
        (Some(BuildKind::NodeModules), BuildDirFlavor::NodeModules)
            | (Some(BuildKind::CargoTarget), BuildDirFlavor::CargoTarget)
    )
}

/// Lexical `..` and `.` collapse; never touches the filesystem.
fn collapse(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// True when the symlink `link` points outside `project`. `lstat` and `readlink` only: the target is never
/// followed, so the verdict does not depend on what the target is.
fn link_escapes(link: &Path, project: &Path) -> bool {
    let Ok(target) = std::fs::read_link(link) else {
        return true;
    };
    let abs = if target.is_absolute() {
        target
    } else {
        link.parent().unwrap_or(project).join(target)
    };
    !collapse(&abs).starts_with(project)
}

fn dir_entries(dir: &Path) -> Vec<(std::ffi::OsString, bool)> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| {
                    let is_link = e.file_type().is_ok_and(|t| t.is_symlink());
                    (e.file_name(), is_link)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Whether `npm install` could recreate `node_modules`: no top-level (or `@scope/` or `.bin`) symlink
/// leaves the project, and a lockfile sits next to it.
fn node_modules_reproducible(nm: &Path) -> Reproducibility {
    let Some(project) = nm.parent() else {
        return Reproducibility::NotChecked;
    };
    let mut to_check: Vec<PathBuf> = vec![nm.to_path_buf(), nm.join(".bin")];
    for (name, is_link) in dir_entries(nm) {
        let p = nm.join(&name);
        if is_link {
            if link_escapes(&p, project) {
                return Reproducibility::LinkedOutside;
            }
        } else if name.to_str().is_some_and(|n| n.starts_with('@')) {
            to_check.push(p);
        }
    }
    for dir in to_check {
        for (name, is_link) in dir_entries(&dir) {
            if is_link && link_escapes(&dir.join(name), project) {
                return Reproducibility::LinkedOutside;
            }
        }
    }
    let has_lock = LOCKFILES
        .iter()
        .any(|l| std::fs::symlink_metadata(project.join(l)).is_ok_and(|m| m.file_type().is_file()));
    if has_lock {
        Reproducibility::Reproducible
    } else {
        Reproducibility::NoLockfile
    }
}

/// What the re-check needs from outside.
pub struct RecheckEnv<'a, D> {
    pub ops: &'a D,
    pub roots: &'a RootSet,
    pub probe: &'a dyn SystemProbe,
}

/// What a fresh look at one build dir found, before any age or reproducibility rule.
struct Gathered {
    st: StatInfo,
    root_dev: RootDev,
    path_facts: Facts,
    marker: bool,
}

/// Opens the scan root and the parent without following links, `fstatat`s the entry itself and builds the
/// `Facts` of `decide_path_common`. The mount table is read here: when it cannot be read, no path can be
/// shown to be clear of a mount point, so the answer is `Skip(MountPoint)` and never "no mount points".
fn gather<D: DirOps>(
    env: &RecheckEnv<'_, D>,
    flavor: BuildDirFlavor,
    path: &Path,
) -> Result<Gathered, BuildRecheck> {
    let Some(root) = env
        .roots
        .scan_roots()
        .iter()
        .find(|r| path.starts_with(r) && path.as_os_str() != r.as_os_str())
    else {
        return Err(skip(SkipReason::OutsideRoot));
    };
    let (Some(name), Some(parent_abs)) = (path.file_name(), path.parent()) else {
        return Err(skip(SkipReason::OutsideRoot));
    };
    let Ok(rel_parent) = parent_abs.strip_prefix(root) else {
        return Err(skip(SkipReason::OutsideRoot));
    };
    let Ok(root_meta) = std::fs::metadata(root) else {
        return Err(skip(SkipReason::OutsideRoot));
    };
    let opened = open_verified_root(env.ops, root, Identity::of(&root_meta))
        .and_then(|vr| open_parent(env.ops, &vr, rel_parent).map(|p| (vr, p)));
    let (vroot, parent) = match opened {
        Ok(x) => x,
        Err(DeleteError::Symlink) => return Err(skip(SkipReason::Symlink)),
        Err(DeleteError::Io(libc::ENOENT)) => return Err(skip(SkipReason::Gone)),
        Err(DeleteError::IdentityMismatch) => return Err(skip(SkipReason::IdentityChanged)),
        Err(_) => return Err(skip(SkipReason::OutsideRoot)),
    };
    let st = match env.ops.stat_at(parent.dir(), name) {
        Ok(st) => st,
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Err(skip(SkipReason::Gone)),
        Err(_) => return Err(skip(SkipReason::IdentityChanged)),
    };
    let root_dev = env
        .ops
        .fstat(vroot.dir())
        .map_or(RootDev(root_meta_dev(&root_meta)), |s| RootDev(s.ident.dev));
    let Ok(mounts) = env.probe.mounts() else {
        return Err(skip_with(SkipReason::MountPoint, DETAIL_MOUNTS_UNREADABLE));
    };
    let path_facts = Facts {
        kind: kind_of(flavor),
        path: path.to_path_buf(),
        ident: st.ident,
        is_symlink: st.kind == FileKind::Symlink || symlink_between(root, path),
        root_dev,
        is_mount_point: mounts.iter().any(|m| m.mount_point == path),
    };
    Ok(Gathered {
        st,
        root_dev,
        path_facts,
        marker: marker_present(path, flavor),
    })
}

/// The detail of a `Skip(MountPoint)` that stands for "the mount table could not be read".
pub const DETAIL_MOUNTS_UNREADABLE: &str = "the mount table could not be read";

/// Re-validates `path` (a build dir planned with identity `planned`) against fresh state. `now` is Unix
/// seconds and `min_age` the `--min-age` flag; the 60-minute floor is applied by the pure decision.
pub fn recheck_build_dir<D: DirOps>(
    env: &RecheckEnv<'_, D>,
    flavor: BuildDirFlavor,
    path: &Path,
    planned: Identity,
    now: i64,
    min_age: Duration,
    store: &mut NewestStore,
) -> BuildRecheck {
    let Gathered {
        st,
        root_dev,
        path_facts,
        marker,
    } = match gather(env, flavor, path) {
        Ok(g) => g,
        Err(r) => return r,
    };
    let own = (st.mtime_secs, st.mtime_nsec);
    let (eff, _) = effective_min_age(min_age);
    let floor_secs = i64::try_from(eff.as_secs()).unwrap_or(i64::MAX);

    // The expensive probes run only when every cheaper guard already passed.
    let cheap_ok = st.kind == FileKind::Dir
        && !path_facts.is_symlink
        && st.ident == planned
        && marker
        && st.ident.on_dev(root_dev)
        && !path_facts.is_mount_point;
    let newest = if !cheap_ok {
        None
    } else if now - own.0 < floor_secs {
        Some(own.0)
    } else {
        let target = ProbeTarget {
            path,
            ident: st.ident,
            own,
            root_dev,
        };
        let floor = to_system_time(now - floor_secs, 0);
        match probe_newest_mtime(env.ops, target, floor, store) {
            Ok(p) => match p.recency {
                Recency::Young { newest, .. } => Some(unix_secs(newest)),
                Recency::Old { newest } => Some(newest.map_or(own.0, unix_secs)),
                Recency::Unknown { .. } => None,
            },
            Err(DeleteError::IdentityMismatch) => return skip(SkipReason::IdentityChanged),
            Err(DeleteError::Io(libc::ENOENT)) => return skip(SkipReason::Gone),
            Err(_) => None,
        }
    };
    let reproducibility = if flavor == BuildDirFlavor::NodeModules && cheap_ok && newest.is_some() {
        node_modules_reproducible(path)
    } else {
        Reproducibility::NotChecked
    };
    let facts = BuildFacts {
        path: path_facts,
        planned,
        marker_present: marker,
        newest_mtime: newest,
        reproducibility,
    };
    BuildRecheck {
        guard: decide_build_dir(&facts, env.roots, now, min_age),
        newest_mtime: newest,
    }
}

pub(crate) fn root_meta_dev(m: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    m.dev()
}

// ------------------------------------------------------------------ the build-dirs target

/// What discovery decided for one candidate; the plan-time guard starts from it.
#[derive(Debug, Clone, Default)]
struct Entry {
    skip: Option<(SkipReason, Option<String>)>,
    /// Size of the duplicate set the dir belongs to.
    duplicates: Option<usize>,
}

#[derive(Debug, Default)]
struct Discovered {
    /// The age flag with the 60 minute floor applied (`Selection::effective_min_age`).
    min_age: Duration,
    /// `build-dirs` is in `--kinds`; an unselected kind is listed as `RISK` rows and is not probed further.
    selected: bool,
    entries: HashMap<CandidateId, Entry>,
    notes: Vec<String>,
}

fn flavor_of(c: &Candidate) -> Option<BuildDirFlavor> {
    match c.facts() {
        KindFacts::BuildDir(f) => Some(f.flavor),
        _ => None,
    }
}

fn marker_name(f: BuildDirFlavor) -> &'static str {
    match f {
        BuildDirFlavor::NodeModules => "package.json",
        BuildDirFlavor::CargoTarget => "Cargo.toml",
    }
}

pub(crate) fn failure(error: DeleteError) -> DeleteFailure {
    DeleteFailure {
        error,
        freed: Freed::default(),
    }
}

/// How a guard verdict that is not a scope error reaches the outcome: as an `ERR` row, never as success.
pub(crate) fn delete_error_of(r: SkipReason) -> DeleteError {
    match r {
        SkipReason::Symlink => DeleteError::Symlink,
        SkipReason::CrossDevice => DeleteError::CrossDevice,
        SkipReason::IdentityChanged => DeleteError::IdentityMismatch,
        SkipReason::ProtectedPath | SkipReason::MountPoint => DeleteError::ProtectedPath,
        SkipReason::OutsideRoot => DeleteError::OutsideScope,
        _ => DeleteError::Refused,
    }
}

/// The `build-dirs` kind: `node_modules` and cargo `target` below the scan roots.
pub struct BuildDirTarget<D: DirOps> {
    roots: RootSet,
    ops: D,
    state: Mutex<Discovered>,
    /// The `Allowed` of an item whose pre-delete guard just passed, taken by the `execute` that follows.
    verified: Mutex<HashMap<CandidateId, Allowed>>,
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl<D: DirOps> BuildDirTarget<D> {
    pub fn new(roots: RootSet, ops: D) -> BuildDirTarget<D> {
        BuildDirTarget {
            roots,
            ops,
            state: Mutex::new(Discovered::default()),
            verified: Mutex::new(HashMap::new()),
        }
    }

    fn ranked(items: &[crate::discover_build::BuildItem]) -> Vec<ClusterInput> {
        items
            .iter()
            .filter_map(|i| {
                let KindFacts::BuildDir(f) = i.candidate.facts() else {
                    return None;
                };
                let Locator::Path(p) = i.candidate.locator() else {
                    return None;
                };
                Some(ClusterInput {
                    path: p.clone(),
                    kind: match f.flavor {
                        BuildDirFlavor::NodeModules => Kind::NodeModules,
                        BuildDirFlavor::CargoTarget => Kind::CargoTarget,
                    },
                    lockfile_hash: f.lockfile_hash,
                    newest_mtime: i.candidate.newest_mtime(),
                })
            })
            .collect()
    }

    /// The full, fresh decision for `c`; the age rule and reproducibility included.
    fn recheck(
        &self,
        c: &Candidate,
        env: &Env,
        min_age: Duration,
    ) -> Result<Allowed, (SkipReason, Option<String>)> {
        let (Some(flavor), Locator::Path(path)) = (flavor_of(c), c.locator()) else {
            return Err((SkipReason::OutsideRoot, None));
        };
        let renv = RecheckEnv {
            ops: &self.ops,
            roots: &self.roots,
            probe: env.probe,
        };
        let r = recheck_build_dir(
            &renv,
            flavor,
            path,
            c.ident(),
            env.clock.now_epoch(),
            min_age,
            &mut NewestStore::in_memory(),
        );
        match r.guard {
            BuildGuard::Allow(a) => Ok(a),
            BuildGuard::Skip { reason, detail } => Err((reason, detail)),
        }
    }

    fn min_age(&self) -> Duration {
        lock(&self.state).min_age
    }

    fn remove(
        &self,
        allowed: &Allowed,
        flavor: BuildDirFlavor,
        abort: &AbortFlag,
        proof: &ApplyToken,
    ) -> Result<Freed, DeleteFailure> {
        let path = allowed.path();
        let base = self
            .roots
            .scan_roots()
            .iter()
            .find(|r| path.starts_with(r))
            .ok_or_else(|| failure(DeleteError::OutsideScope))?;
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
        let home = self.roots.home().to_path_buf();
        let policy = DeletePolicy {
            root_dev: RootDev(base_id.dev),
            root_mnt: opened.mnt_id,
            home_id: std::fs::metadata(&home)
                .map(|m| Identity::of(&m))
                .map_err(|_| failure(DeleteError::OutsideScope))?,
            home,
            scan_roots: self.roots.scan_roots().to_vec(),
            allowed: AllowedTarget::Within {
                root: base.clone(),
                name: match flavor {
                    BuildDirFlavor::NodeModules => NamePattern::NodeModules,
                    BuildDirFlavor::CargoTarget => NamePattern::CargoTarget,
                },
            },
            target: path.to_path_buf(),
            require_sibling_any: vec![OsString::from(marker_name(flavor))],
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

impl<D: DirOps + 'static> Target for BuildDirTarget<D> {
    fn name(&self) -> &'static str {
        "build-dirs"
    }

    fn selectors(&self) -> &'static [SelectorId] {
        &[SelectorId::BuildDirs]
    }

    fn discover(&self, env: &Env, selection: &Selection) -> Vec<Candidate> {
        let min_age = Duration::from_secs(
            selection
                .effective_min_age(SelectorId::BuildDirs)
                .unwrap_or(0),
        );
        let opts = BuildOpts {
            min_age,
            kinds: BuildKinds::ALL,
            max_duration: None,
            rescan: false,
            walker_cache: None,
        };
        let benv = BuildEnv {
            ops: &self.ops,
            roots: &self.roots,
            probe: env.probe,
            clock: env.clock,
        };
        let mut st = Discovered {
            min_age,
            selected: selection.selects(SelectorId::BuildDirs),
            ..Discovered::default()
        };
        let found = match discover_build_dirs(&benv, &opts) {
            Ok(d) => d,
            Err(e) => {
                st.notes.push(format!("build-dirs: {e}; nothing listed"));
                *lock(&self.state) = st;
                return Vec::new();
            }
        };
        if found.mounts_unreadable {
            st.notes.push(format!(
                "build-dirs: {}; every build dir is skipped as a mount point",
                super::build_dir::DETAIL_MOUNTS_UNREADABLE
            ));
        }
        let keep = KeepNewest(selection.keep_newest.map_or(1, |n| n as usize));
        let inputs = Self::ranked(&found.items);
        let placed = plan_keep_newest(&inputs, keep);
        let mut set_size: HashMap<(Kind, u64), usize> = HashMap::new();
        for i in &inputs {
            if let Some(h) = i.lockfile_hash {
                *set_size.entry((i.kind, h)).or_default() += 1;
            }
        }
        let mut out = Vec::with_capacity(found.items.len());
        for ((item, placement), input) in found.items.into_iter().zip(placed).zip(&inputs) {
            let c = item.candidate;
            let size = input
                .lockfile_hash
                .and_then(|h| set_size.get(&(input.kind, h)).copied())
                .filter(|n| *n > 1);
            let mut entry = Entry {
                skip: None,
                duplicates: size,
            };
            match (item.verdict, placement) {
                (BuildVerdict::Skip(r), _) => entry.skip = Some((r, item.note)),
                (BuildVerdict::Will(_), Placement::Kept { of }) => {
                    entry.skip = Some((SkipReason::KeptAsNewest, Some(kept_note(of))));
                }
                (BuildVerdict::Will(_), _) => {}
            }
            st.entries.insert(c.id(), entry);
            out.push(c);
        }
        *lock(&self.state) = st;
        out
    }

    fn guard(&self, c: &Candidate, env: &Env) -> Guarded {
        let (skip, selected) = {
            let st = lock(&self.state);
            (
                st.entries.get(&c.id()).and_then(|e| e.skip.clone()),
                st.selected,
            )
        };
        if let Some((r, _)) = skip {
            return Guarded::Skip(r);
        }
        let (Some(flavor), Locator::Path(path)) = (flavor_of(c), c.locator()) else {
            return Guarded::Skip(SkipReason::OutsideRoot);
        };
        // Cheap probes only, and only for items the plan would remove: the tree walk and the other fresh
        // checks belong to `guard_before_delete`.
        if !selected {
            return Guarded::Allow;
        }
        if flavor == BuildDirFlavor::NodeModules {
            let why = match node_modules_reproducible(path) {
                Reproducibility::LinkedOutside => Some(DETAIL_LINKED_OUTSIDE),
                Reproducibility::NoLockfile => Some(DETAIL_NO_LOCKFILE),
                Reproducibility::NotChecked => Some("not checked"),
                Reproducibility::Reproducible => None,
            };
            if let Some(why) = why {
                if let Some(e) = lock(&self.state).entries.get_mut(&c.id()) {
                    e.skip = Some((SkipReason::NotReproducible, Some(why.to_string())));
                }
                return Guarded::Skip(SkipReason::NotReproducible);
            }
        }
        match run_state_skip(env.probe.run_state(path)) {
            Some(r) => Guarded::Skip(r),
            None => Guarded::Allow,
        }
    }

    fn guard_before_delete(&self, c: &Candidate, env: &Env) -> Guarded {
        if let Some((r, _)) = lock(&self.state)
            .entries
            .get(&c.id())
            .and_then(|e| e.skip.clone())
        {
            return Guarded::Skip(r);
        }
        let allowed = match self.recheck(c, env, self.min_age()) {
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
        let Some(flavor) = flavor_of(c) else {
            return Err(failure(DeleteError::Refused));
        };
        let parked = lock(&self.verified).remove(&c.id());
        let allowed = match parked {
            Some(a) => a,
            None => match self.recheck(c, env, self.min_age()) {
                Ok(a) => {
                    if let Some(r) = run_state_skip(env.probe.run_state(a.path())) {
                        return Err(failure(delete_error_of(r)));
                    }
                    a
                }
                // A vanished build dir is success by idempotence (I7).
                Err((SkipReason::Gone, _)) => return Ok(Freed::default()),
                Err((r, _)) => return Err(failure(delete_error_of(r))),
            },
        };
        self.remove(&allowed, flavor, env.guard.abort_flag(), proof)
    }

    fn detail(&self, c: &Candidate) -> Option<String> {
        lock(&self.state)
            .entries
            .get(&c.id())
            .and_then(|e| e.skip.as_ref())
            .and_then(|(r, d)| {
                (*r == SkipReason::NotReproducible)
                    .then(|| d.clone())
                    .flatten()
            })
    }

    fn duplicate_set_size(&self, c: &Candidate) -> Option<usize> {
        lock(&self.state)
            .entries
            .get(&c.id())
            .and_then(|e| e.duplicates)
    }

    fn plan_notes(&self, _env: &Env, selection: &Selection) -> Vec<String> {
        let mut notes = lock(&self.state).notes.clone();
        if selection
            .min_age_secs
            .is_some_and(|s| s < effective_floor_secs())
        {
            notes.push(FLOOR_RAISED_NOTE.to_string());
        }
        if let Some(w) = selection
            .keep_newest
            .and_then(|n| KeepNewest(n as usize).warning())
        {
            notes.push(w.to_string());
        }
        notes
    }
}

fn effective_floor_secs() -> u64 {
    effective_min_age(Duration::ZERO).0.as_secs()
}
