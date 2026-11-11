//! The `steam` kind (Story 4.2.2): orphan prefixes, orphan shader caches and uninstalled-game remnants.
//!
//! Whole-kind facts come first and are one gate: Steam running or downloading, a listed library that is not
//! mounted, a library file that does not parse, an installed set that is empty while the folders it would
//! protect are not (`KindGate::Unavailable`). Then each item is judged by the pure `decide_steam` over fresh
//! facts. The same gate and the same decision are re-asked right before the delete (`guard_before_delete`),
//! and `execute` removes only through `delete_tree` under `Within { <library>/steamapps/<sub>, pattern }`.
//!
//! DECISION (the missing `Allowed` witness, as in `build_dir.rs`): the pre-delete guard parks the `Allowed`
//! it obtained for the one `execute` that follows; `execute` without one runs the whole check itself.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use disk_cleanup_core::guard::Facts;
use disk_cleanup_core::ports::Env;
use disk_cleanup_core::roots::RootSet;
use disk_cleanup_core::selector::{Discard, Selection};
use disk_cleanup_core::target::{Guarded, KindGate, Target};
use disk_cleanup_core::targets::steam_guard::{
    decide_steam, SteamGuard, SteamGuardFacts, SAVES_BASE, SAVE_DIRS,
};
use disk_cleanup_core::{
    AbortFlag, AllocatedBytes, Allowed, AllowedTarget, AppId, ApplyToken, Candidate, CandidateId,
    DeleteError, DeleteFailure, DeletePolicy, FileKind, Freed, Identity, Kind, KindFacts, Locator,
    MountEntry, NamePattern, RootDev, SelectorId, SkipReason, SteamFacts,
};

use super::orphans::{is_orphan, list_orphans};
use super::remnants::{is_remnant, list_remnants};
use super::{default_steam_roots, plan_gate, unmounted_libraries, SteamGate, SteamInstall};
use crate::delete_tree;
use crate::dirops::{open_parent, open_verified_root, DirOps};
use crate::mtime_probe::{
    probe_newest_mtime, to_system_time, unix_secs, NewestStore, ProbeTarget, Recency,
};
use crate::targets::build_dir::{
    delete_error_of, failure, lock, root_meta_dev, DETAIL_MOUNTS_UNREADABLE,
};
use crate::treestat::{measure, MeasureOpts, SkipWhy};

type Verdict = (SkipReason, Option<String>);

fn skip(r: SkipReason) -> Verdict {
    (r, None)
}

/// What discovery decided for one candidate; the plan-time guard starts from it.
#[derive(Debug, Clone, Default)]
struct Entry {
    skip: Option<Verdict>,
    /// The plan-row note of an item that will be removed.
    note: Option<String>,
}

#[derive(Debug, Default)]
struct Discovered {
    min_age: Duration,
    saves: Option<Discard>,
    entries: HashMap<CandidateId, Entry>,
    notes: Vec<String>,
}

/// What a fresh look at one entry found, before any age, saves or orphan rule.
struct Gathered {
    ident: Identity,
    kind: FileKind,
    mtime: (i64, u32),
    blocks: u64,
    root_dev: RootDev,
    path_facts: Facts,
}

pub struct SteamTarget<D: DirOps> {
    roots: RootSet,
    ops: D,
    state: Mutex<Discovered>,
    verified: Mutex<HashMap<CandidateId, Allowed>>,
}

fn steam_root(env: &Env) -> Option<PathBuf> {
    default_steam_roots(&env.dirs.home, env.probe.platform())
        .into_iter()
        .next()
}

fn reason_of(g: &SteamGate) -> SkipReason {
    match g {
        SteamGate::Skip(r) | SteamGate::Unavailable(r) => *r,
    }
}

/// The whole-kind gate: `Ok(None)` when no Steam install exists (nothing to list, nothing wrong).
fn open_gate(env: &Env) -> Result<Option<SteamInstall>, SteamGate> {
    let Some(root) = steam_root(env) else {
        return Ok(None);
    };
    let install = plan_gate(env.probe, &root)?;
    install.check_installed_set()?;
    Ok(Some(install))
}

fn pattern_of(kind: Kind) -> Option<(NamePattern, &'static str)> {
    match kind {
        Kind::SteamPrefix => Some((NamePattern::SteamAppId, "compatdata")),
        Kind::SteamShaderCache => Some((NamePattern::SteamAppId, "shadercache")),
        Kind::SteamRemnant => Some((NamePattern::SteamInstallDir, "common")),
        _ => None,
    }
}

/// The first of `SAVE_DIRS` that holds a file, a link or anything unreadable. Nothing is followed.
pub fn find_saves(prefix: &Path) -> Option<String> {
    let base = prefix.join(SAVES_BASE);
    SAVE_DIRS
        .iter()
        .find(|d| holds_files(&base.join(d)))
        .map(|d| (*d).to_string())
}

fn holds_files(dir: &Path) -> bool {
    match fs::symlink_metadata(dir) {
        Err(e) => return e.kind() != io::ErrorKind::NotFound,
        Ok(m) if m.is_dir() => {}
        Ok(_) => return true,
    }
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else {
            return true;
        };
        for e in rd {
            let Ok(e) = e else { return true };
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                _ => return true,
            }
        }
    }
    false
}

fn app_id_of(path: &Path) -> Option<AppId> {
    let name = path.file_name()?.to_str()?;
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    name.parse().ok().map(AppId)
}

impl<D: DirOps> SteamTarget<D> {
    pub fn new(roots: RootSet, ops: D) -> SteamTarget<D> {
        SteamTarget {
            roots,
            ops,
            state: Mutex::new(Discovered::default()),
            verified: Mutex::new(HashMap::new()),
        }
    }

    /// Opens the base directory without following links, `fstatat`s the entry itself and builds the `Facts`
    /// of `decide_path_common`. A base that is itself a link is refused: the rules about "one component below
    /// `compatdata`" are lexical and only sound when the base is the real directory. When the mount table
    /// cannot be read no path can be shown to be clear of a mount point, so the answer is `MountPoint`.
    fn gather(
        &self,
        kind: Kind,
        path: &Path,
        mounts: Option<&[MountEntry]>,
    ) -> Result<Gathered, Verdict> {
        if !self.roots.admits(kind, path) {
            return Err(skip(SkipReason::OutsideRoot));
        }
        let (Some(name), Some(base)) = (path.file_name(), path.parent()) else {
            return Err(skip(SkipReason::OutsideRoot));
        };
        if fs::canonicalize(base).ok().as_deref() != Some(base) {
            return Err(skip(SkipReason::Symlink));
        }
        let base_md = fs::metadata(base).map_err(|_| skip(SkipReason::OutsideRoot))?;
        let vroot = open_verified_root(&self.ops, base, Identity::of(&base_md)).map_err(|e| {
            skip(match e {
                DeleteError::Symlink => SkipReason::Symlink,
                DeleteError::Io(libc::ENOENT) => SkipReason::Gone,
                DeleteError::IdentityMismatch => SkipReason::IdentityChanged,
                _ => SkipReason::OutsideRoot,
            })
        })?;
        let st = match self.ops.stat_at(vroot.dir(), name) {
            Ok(st) => st,
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Err(skip(SkipReason::Gone)),
            Err(_) => return Err(skip(SkipReason::IdentityChanged)),
        };
        let root_dev = self
            .ops
            .fstat(vroot.dir())
            .map_or(RootDev(root_meta_dev(&base_md)), |s| RootDev(s.ident.dev));
        let Some(mounts) = mounts else {
            return Err((
                SkipReason::MountPoint,
                Some(DETAIL_MOUNTS_UNREADABLE.into()),
            ));
        };
        let path_facts = Facts {
            kind,
            path: path.to_path_buf(),
            ident: st.ident,
            is_symlink: st.kind == FileKind::Symlink,
            root_dev,
            is_mount_point: mounts.iter().any(|m| m.mount_point == path),
        };
        Ok(Gathered {
            ident: st.ident,
            kind: st.kind,
            mtime: (st.mtime_secs, st.mtime_nsec),
            blocks: st.blocks,
            root_dev,
            path_facts,
        })
    }

    /// Size and newest mtime inside by one full walk (discovery). `None` newest when part of the tree could
    /// not be read: unknown age is never old enough.
    fn walk(&self, path: &Path, g: &Gathered) -> (u64, Option<i64>) {
        if g.kind != FileKind::Dir || g.path_facts.is_symlink {
            return (g.blocks * 512, Some(g.mtime.0));
        }
        let Ok(root) = open_verified_root(&self.ops, path, g.ident) else {
            return (g.blocks * 512, None);
        };
        let stats = measure(&self.ops, root.dir(), g.root_dev, MeasureOpts::default());
        let partial = stats
            .skipped
            .iter()
            .any(|s| matches!(s.why, SkipWhy::Unreadable(_) | SkipWhy::TooDeep));
        let newest = if partial {
            None
        } else {
            Some(stats.newest_mtime.map_or(g.mtime.0, unix_secs))
        };
        (stats.allocated.0, newest)
    }

    /// Newest mtime inside, stopping at the first entry younger than the floor (pre-delete re-check).
    fn probe(
        &self,
        path: &Path,
        g: &Gathered,
        floor_secs: i64,
        now: i64,
    ) -> Result<Option<i64>, Verdict> {
        if now - g.mtime.0 < floor_secs {
            return Ok(Some(g.mtime.0));
        }
        let target = ProbeTarget {
            path,
            ident: g.ident,
            own: g.mtime,
            root_dev: g.root_dev,
        };
        let floor = to_system_time(now - floor_secs, 0);
        match probe_newest_mtime(&self.ops, target, floor, &mut NewestStore::in_memory()) {
            Ok(p) => Ok(match p.recency {
                Recency::Young { newest, .. } => Some(unix_secs(newest)),
                Recency::Old { newest } => Some(newest.map_or(g.mtime.0, unix_secs)),
                Recency::Unknown { .. } => None,
            }),
            Err(DeleteError::IdentityMismatch) => Err(skip(SkipReason::IdentityChanged)),
            Err(DeleteError::Io(libc::ENOENT)) => Err(skip(SkipReason::Gone)),
            Err(_) => Ok(None),
        }
    }

    fn settings(&self) -> (Duration, Option<Discard>) {
        let st = lock(&self.state);
        (st.min_age, st.saves)
    }

    /// The gate, then the fresh decision for `c`; the age rule and the saves rule included.
    fn full_check(&self, c: &Candidate, env: &Env) -> Result<Allowed, Verdict> {
        let install = match open_gate(env) {
            Ok(Some(i)) => i,
            Ok(None) => return Err(skip(SkipReason::LibraryUnmounted)),
            Err(g) => return Err(skip(reason_of(&g))),
        };
        let (kind, path, still_orphan) = match (c.facts(), c.locator()) {
            (KindFacts::Steam(SteamFacts::Remnant { .. }), Locator::Path(p)) => {
                let name = p.file_name().ok_or(skip(SkipReason::OutsideRoot))?;
                (Kind::SteamRemnant, p, is_remnant(&install, name))
            }
            (KindFacts::Steam(SteamFacts::Prefix { .. }), Locator::Steam(_, p)) => {
                let id = app_id_of(p).ok_or(skip(SkipReason::OutsideRoot))?;
                (Kind::SteamPrefix, p, is_orphan(&install, id))
            }
            (KindFacts::Steam(SteamFacts::ShaderCache { .. }), Locator::Steam(_, p)) => {
                let id = app_id_of(p).ok_or(skip(SkipReason::OutsideRoot))?;
                (Kind::SteamShaderCache, p, is_orphan(&install, id))
            }
            _ => return Err(skip(SkipReason::OutsideRoot)),
        };
        let now = env.clock.now_epoch();
        let (min_age, saves) = self.settings();
        let mounts = env.probe.mounts().ok();
        let g = self.gather(kind, path, mounts.as_deref())?;
        let cheap_ok = g.kind == FileKind::Dir
            && !g.path_facts.is_symlink
            && g.ident == c.ident()
            && g.ident.on_dev(g.root_dev)
            && !g.path_facts.is_mount_point
            && still_orphan;
        let floor_secs = i64::try_from(min_age.as_secs()).unwrap_or(i64::MAX);
        let (newest, saves_dir) = if cheap_ok {
            let newest = self.probe(path, &g, floor_secs, now)?;
            let dir = (kind == Kind::SteamPrefix && saves.is_none())
                .then(|| find_saves(path))
                .flatten();
            (newest, dir)
        } else {
            (None, None)
        };
        let facts = SteamGuardFacts {
            path: g.path_facts,
            planned: c.ident(),
            still_orphan,
            newest_mtime: newest,
            saves_dir,
        };
        match decide_steam(&facts, &self.roots, now, min_age, saves) {
            SteamGuard::Allow(a) => Ok(a),
            SteamGuard::Skip { reason, detail } => Err((reason, detail)),
        }
    }

    fn remove(
        &self,
        allowed: &Allowed,
        abort: &AbortFlag,
        proof: &ApplyToken,
    ) -> Result<Freed, DeleteFailure> {
        let path = allowed.path();
        let (pattern, _) =
            pattern_of(allowed.kind()).ok_or_else(|| failure(DeleteError::Refused))?;
        let base = path
            .parent()
            .ok_or_else(|| failure(DeleteError::OutsideScope))?;
        let name = path
            .file_name()
            .ok_or_else(|| failure(DeleteError::ProtectedPath))?;
        let base_md = fs::metadata(base).map_err(|_| failure(DeleteError::OutsideScope))?;
        let base_id = Identity::of(&base_md);
        let root = open_verified_root(&self.ops, base, base_id).map_err(failure)?;
        let parent = open_parent(&self.ops, &root, Path::new("")).map_err(failure)?;
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
                name: pattern,
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

    /// One discovered entry as a candidate plus its plan-time verdict.
    #[allow(clippy::too_many_arguments)]
    fn inspect(
        &self,
        kind: Kind,
        path: &Path,
        locator: Locator,
        facts: SteamFacts,
        now: i64,
        mounts: Option<&[MountEntry]>,
        libraries: usize,
    ) -> Result<(Candidate, Entry), Verdict> {
        let g = self.gather(kind, path, mounts)?;
        let (min_age, saves) = self.settings();
        let (bytes, newest) = self.walk(path, &g);
        let saves_dir =
            (kind == Kind::SteamPrefix && g.kind == FileKind::Dir && !g.path_facts.is_symlink)
                .then(|| find_saves(path))
                .flatten();
        let guard_facts = SteamGuardFacts {
            path: g.path_facts.clone(),
            planned: g.ident,
            still_orphan: true,
            newest_mtime: newest,
            saves_dir,
        };
        let entry = match decide_steam(&guard_facts, &self.roots, now, min_age, saves) {
            SteamGuard::Allow(_) => Entry {
                skip: None,
                note: Some(note_for(kind, libraries)),
            },
            SteamGuard::Skip { reason, detail } => Entry {
                skip: Some((reason, detail)),
                note: None,
            },
        };
        let c = Candidate::new(
            locator,
            KindFacts::Steam(facts),
            AllocatedBytes(bytes),
            newest,
            g.ident,
        );
        Ok((c, entry))
    }
}

fn note_for(kind: Kind, libraries: usize) -> String {
    let s = if libraries == 1 {
        "library"
    } else {
        "libraries"
    };
    match kind {
        Kind::SteamRemnant => format!("no installdir names it in {libraries} {s}"),
        _ => format!("no appmanifest in {libraries} {s}"),
    }
}

impl<D: DirOps + 'static> Target for SteamTarget<D> {
    fn name(&self) -> &'static str {
        "steam"
    }

    fn selectors(&self) -> &'static [SelectorId] {
        &[SelectorId::Steam]
    }

    fn gate(&self, env: &Env) -> KindGate {
        match open_gate(env) {
            Ok(_) => KindGate::Available,
            Err(g) => KindGate::Unavailable(reason_of(&g)),
        }
    }

    fn unavailable_detail(&self, env: &Env) -> Option<String> {
        let root = steam_root(env)?;
        match open_gate(env).err()? {
            SteamGate::Unavailable(SkipReason::UnrecognisedOutput) => Some("steam".into()),
            g if reason_of(&g) == SkipReason::LibraryUnmounted => Some(
                unmounted_libraries(&root)
                    .first()
                    .unwrap_or(&root)
                    .to_string_lossy()
                    .into_owned(),
            ),
            _ => None,
        }
    }

    fn discover(&self, env: &Env, selection: &Selection) -> Vec<Candidate> {
        let min_age =
            Duration::from_secs(selection.effective_min_age(SelectorId::Steam).unwrap_or(0));
        *lock(&self.state) = Discovered {
            min_age,
            saves: selection.steam_saves,
            ..Discovered::default()
        };
        let Ok(Some(install)) = open_gate(env) else {
            return Vec::new();
        };
        let now = env.clock.now_epoch();
        let mounts = env.probe.mounts().ok();
        let libs = install.libraries.len();
        let mut out = Vec::new();
        let mut entries = HashMap::new();
        let mut notes: Vec<String> = Vec::new();
        let mut keep = |found: Result<(Candidate, Entry), Verdict>, path: &Path| match found {
            Ok((c, e)) => {
                entries.insert(c.id(), e);
                out.push(c);
            }
            // The folder that holds the entries is a link, so no rule about "one level below" holds.
            Err((SkipReason::Symlink, _)) => {
                let note = format!(
                    "steam: {} is a symlink; nothing listed there",
                    path.parent().unwrap_or(path).display()
                );
                if !notes.contains(&note) {
                    notes.push(note);
                }
            }
            // Vanished or unreadable meanwhile: it cannot be offered.
            Err(_) => {}
        };
        for o in list_orphans(&install) {
            let facts = if o.kind == Kind::SteamPrefix {
                SteamFacts::Prefix { app_id: o.app_id }
            } else {
                SteamFacts::ShaderCache { app_id: o.app_id }
            };
            keep(
                self.inspect(
                    o.kind,
                    &o.path,
                    Locator::Steam(o.app_id, o.path.clone()),
                    facts,
                    now,
                    mounts.as_deref(),
                    libs,
                ),
                &o.path,
            );
        }
        for r in list_remnants(&install) {
            keep(
                self.inspect(
                    Kind::SteamRemnant,
                    &r.path,
                    Locator::Path(r.path.clone()),
                    SteamFacts::Remnant {
                        install_dir: r.name.clone(),
                    },
                    now,
                    mounts.as_deref(),
                    libs,
                ),
                &r.path,
            );
        }
        let mut st = lock(&self.state);
        st.entries = entries;
        st.notes = notes;
        out
    }

    fn guard(&self, c: &Candidate, _env: &Env) -> Guarded {
        match lock(&self.state)
            .entries
            .get(&c.id())
            .and_then(|e| e.skip.clone())
        {
            Some((r, _)) => Guarded::Skip(r),
            None => Guarded::Allow,
        }
    }

    fn guard_before_delete(&self, c: &Candidate, env: &Env) -> Guarded {
        match self.full_check(c, env) {
            Ok(a) => {
                lock(&self.verified).insert(c.id(), a);
                Guarded::Allow
            }
            Err((r, _)) => Guarded::Skip(r),
        }
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
            None => match self.full_check(c, env) {
                Ok(a) => a,
                // A vanished item is success by idempotence (I7).
                Err((SkipReason::Gone, _)) => return Ok(Freed::default()),
                Err((r, _)) => return Err(failure(delete_error_of(r))),
            },
        };
        self.remove(&allowed, env.guard.abort_flag(), proof)
    }

    fn plan_notes(&self, _env: &Env, _selection: &Selection) -> Vec<String> {
        lock(&self.state).notes.clone()
    }

    fn detail(&self, c: &Candidate) -> Option<String> {
        let st = lock(&self.state);
        let e = st.entries.get(&c.id())?;
        match &e.skip {
            Some((_, d)) => d.clone(),
            None => e.note.clone(),
        }
    }
}
