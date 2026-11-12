//! Build-directory discovery for the read-only report (Story 1.3.1): one `dunnage_scan` pass finds
//! `node_modules` and `target` directories, then each goes through the cheap guards (`Allowed`, Story 1.1.3)
//! and only the survivors are probed for their real last-use time.
//!
//! The probe is lazy and sidecar-backed because of spike 0.2.6: a 2 minute dry run was 99% worktree
//! discovery, and the floor left only 4.6 to 6.6 s for everything after the scan. A candidate that a cheaper
//! guard skipped, or that is too young by its own mtime, is never walked.

use std::collections::{BTreeMap, HashSet};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use dunnage_core::guard::{decide_path_common, Facts, GuardDecision};
use dunnage_core::ports::{Clock, SystemProbe};
use dunnage_core::roots::RootSet;
use dunnage_core::{
    AllocatedBytes, Allowed, BuildDirFacts, BuildDirFlavor, Candidate, DeleteError, Identity, Kind,
    KindFacts, Locator, RootDev, SkipReason,
};
use dunnage_scan::scan::{BackendChoice, BuildKind, ScanOptions, ScanReport};
use dunnage_scan::walker::CacheConfig;

use crate::dirops::DirOps;
use crate::mtime_probe::{
    probe_newest_mtime, unix_secs, NewestStore, OwnMtime, ProbeTarget, Recency,
};

/// The walker cache is trusted for this long, matching metascope's default.
pub const WALKER_CACHE_MAX_AGE: Duration = Duration::from_hours(24);

#[derive(Debug, thiserror::Error)]
pub enum DiscoverError {
    #[error("scan failed: {0}")]
    Scan(String),
}

/// Which build-directory kinds `--kinds` selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildKinds {
    pub node_modules: bool,
    pub cargo_target: bool,
}

impl BuildKinds {
    pub const ALL: BuildKinds = BuildKinds {
        node_modules: true,
        cargo_target: true,
    };

    fn selects(self, f: BuildDirFlavor) -> bool {
        match f {
            BuildDirFlavor::NodeModules => self.node_modules,
            BuildDirFlavor::CargoTarget => self.cargo_target,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BuildOpts {
    /// The effective age floor, already `max(--min-age, 60 min)`.
    pub min_age: Duration,
    pub kinds: BuildKinds,
    /// `--max-seconds`.
    pub max_duration: Option<Duration>,
    /// `--rescan`: the walker cache and its sidecar are not read.
    pub rescan: bool,
    /// The walker cache file; the newest-mtime sidecar sits beside it. `None` disables both.
    pub walker_cache: Option<PathBuf>,
}

/// One build directory as the scan reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawBuildDir {
    pub path: PathBuf,
    pub flavor: BuildDirFlavor,
    pub group: PathBuf,
    pub lockfile_hash: Option<u64>,
    pub est_bytes: u64,
}

fn flavor_of(k: BuildKind) -> BuildDirFlavor {
    match k {
        BuildKind::NodeModules => BuildDirFlavor::NodeModules,
        BuildKind::CargoTarget => BuildDirFlavor::CargoTarget,
    }
}

impl RawBuildDir {
    pub fn from_report(report: &ScanReport) -> Vec<RawBuildDir> {
        report
            .build_roots
            .iter()
            .map(|b| RawBuildDir {
                path: b.path.clone(),
                flavor: flavor_of(b.kind),
                group: b.group.clone(),
                lockfile_hash: b.lockfile_hash,
                est_bytes: b.stats.allocated_bytes,
            })
            .collect()
    }
}

/// `--rescan` keeps the cache config with `rescan: true` (existing records are neither read nor trusted, fresh
/// ones are written); no `walker_cache` (`--no-cache`) gives `cache: None`, which reads and writes nothing;
/// `--max-seconds` becomes `max_duration`. The walker backend is chosen explicitly: it is the one the spike measured.
pub fn scan_options(roots: &RootSet, opts: &BuildOpts) -> ScanOptions {
    let mut so = ScanOptions::new(roots.scan_roots().to_vec());
    so.backend = BackendChoice::Walker;
    so.max_duration = opts.max_duration;
    so.cache = opts.walker_cache.as_ref().map(|path| CacheConfig {
        path: path.clone(),
        rescan: opts.rescan,
        max_age: WALKER_CACHE_MAX_AGE,
    });
    so
}

/// Build directories sharing a lockfile hash, same kind: heuristic "same tree" evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateCluster {
    pub flavor: BuildDirFlavor,
    pub lockfile_hash: u64,
    pub members: Vec<PathBuf>,
}

#[derive(Debug)]
pub enum BuildVerdict {
    /// Passed every guard and the probe; the only way to a delete is through this witness.
    Will(Allowed),
    Skip(SkipReason),
}

#[derive(Debug)]
pub struct BuildItem {
    pub candidate: Candidate,
    pub verdict: BuildVerdict,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProbeCounts {
    /// Directories whose tree was walked.
    pub walked: usize,
    /// Directories a sidecar entry ended without a walk.
    pub from_sidecar: usize,
}

#[derive(Debug, Default)]
pub struct BuildDiscovery {
    pub items: Vec<BuildItem>,
    pub clusters: Vec<DuplicateCluster>,
    /// `ScanReport.truncated`: sizes are lower bounds and clustered dirs are kept.
    pub partial_scan: bool,
    pub probes: ProbeCounts,
    pub scan_errors: u64,
    pub dirs_listed: u64,
    pub dirs_cached: u64,
    pub mounts_unreadable: bool,
}

/// What the decision stage needs from the outside world.
pub struct BuildEnv<'a, D> {
    pub ops: &'a D,
    pub roots: &'a RootSet,
    pub probe: &'a dyn SystemProbe,
    pub clock: &'a dyn Clock,
}

/// Runs the scan, then [`decide_build_dirs`].
pub fn discover_build_dirs<D: DirOps>(
    env: &BuildEnv<'_, D>,
    opts: &BuildOpts,
) -> Result<BuildDiscovery, DiscoverError> {
    let report = dunnage_scan::scan::scan(&scan_options(env.roots, opts))
        .map_err(|e| DiscoverError::Scan(e.to_string()))?;
    let mut store = match (&opts.walker_cache, opts.rescan) {
        (Some(p), false) => NewestStore::open(NewestStore::sidecar_path(p)),
        (Some(p), true) => NewestStore::open_ignoring_contents(NewestStore::sidecar_path(p)),
        (None, _) => NewestStore::in_memory(),
    };
    let mut d = decide_build_dirs(
        env,
        &RawBuildDir::from_report(&report),
        report.truncated,
        opts,
        &mut store,
    );
    // Best effort: a failed save costs the next run a full walk, never correctness.
    let _ = store.save();
    d.scan_errors = report.errors;
    d.dirs_listed = report.dirs_listed;
    d.dirs_cached = report.dirs_cached;
    Ok(d)
}

struct Lstat {
    ident: Identity,
    is_symlink: bool,
    own: OwnMtime,
}

fn lstat(path: &Path) -> io::Result<Lstat> {
    let m = std::fs::symlink_metadata(path)?;
    Ok(Lstat {
        ident: Identity::of(&m),
        is_symlink: m.file_type().is_symlink(),
        own: (m.mtime(), u32::try_from(m.mtime_nsec()).unwrap_or(0)),
    })
}

/// True when a directory strictly between `base` and `path` is a symlink: `lstat` of the final component
/// alone does not see `~/.cargo -> /elsewhere`.
pub(crate) fn symlink_between(base: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(base) else {
        return false;
    };
    let mut cur = base.to_path_buf();
    let mut comps = rel.components().peekable();
    while let Some(c) = comps.next() {
        if comps.peek().is_none() {
            break;
        }
        cur.push(c);
        if std::fs::symlink_metadata(&cur).is_ok_and(|m| m.file_type().is_symlink()) {
            return true;
        }
    }
    false
}

fn mount_points(probe: &dyn SystemProbe) -> (HashSet<PathBuf>, bool) {
    match probe.mounts() {
        Ok(m) => (m.into_iter().map(|e| e.mount_point).collect(), false),
        Err(_) => (HashSet::new(), true),
    }
}

fn kind_of(f: BuildDirFlavor) -> Kind {
    match f {
        BuildDirFlavor::NodeModules => Kind::NodeModules,
        BuildDirFlavor::CargoTarget => Kind::CargoTarget,
    }
}

fn clusters_of(raws: &[RawBuildDir]) -> Vec<DuplicateCluster> {
    let mut by: BTreeMap<(u8, u64), (BuildDirFlavor, Vec<PathBuf>)> = BTreeMap::new();
    for r in raws {
        if let Some(h) = r.lockfile_hash {
            let k = (u8::from(r.flavor == BuildDirFlavor::CargoTarget), h);
            by.entry(k)
                .or_insert_with(|| (r.flavor, Vec::new()))
                .1
                .push(r.path.clone());
        }
    }
    by.into_iter()
        .filter(|(_, (_, m))| m.len() > 1)
        .map(|((_, h), (flavor, mut members))| {
            members.sort();
            DuplicateCluster {
                flavor,
                lockfile_hash: h,
                members,
            }
        })
        .collect()
}

/// How one directory fared, before it is wrapped in a `Candidate`.
struct Outcome {
    verdict: BuildVerdict,
    newest: Option<i64>,
    note: Option<String>,
}

impl Outcome {
    fn skip(r: SkipReason) -> Outcome {
        Outcome {
            verdict: BuildVerdict::Skip(r),
            newest: None,
            note: None,
        }
    }

    fn skip_with(r: SkipReason, note: impl Into<String>) -> Outcome {
        Outcome {
            verdict: BuildVerdict::Skip(r),
            newest: None,
            note: Some(note.into()),
        }
    }
}

struct Run<'a, D> {
    env: &'a BuildEnv<'a, D>,
    opts: &'a BuildOpts,
    now: i64,
    mounts: HashSet<PathBuf>,
    clustered: HashSet<PathBuf>,
    partial: bool,
    counts: ProbeCounts,
}

impl<D: DirOps> Run<'_, D> {
    fn root_dev_of(&self, path: &Path, fallback: Identity) -> RootDev {
        let root = self
            .env
            .roots
            .scan_roots()
            .iter()
            .find(|r| path.starts_with(r));
        match root.and_then(|r| std::fs::metadata(r).ok()) {
            Some(m) => RootDev(m.dev()),
            None => RootDev(fallback.dev),
        }
    }

    fn guard(&self, raw: &RawBuildDir, st: &Lstat) -> Result<Allowed, SkipReason> {
        let in_root = self
            .env
            .roots
            .scan_roots()
            .iter()
            .find(|r| raw.path.starts_with(r));
        let facts = Facts {
            kind: kind_of(raw.flavor),
            path: raw.path.clone(),
            ident: st.ident,
            is_symlink: st.is_symlink || in_root.is_some_and(|r| symlink_between(r, &raw.path)),
            root_dev: self.root_dev_of(&raw.path, st.ident),
            is_mount_point: self.mounts.contains(&raw.path),
        };
        match decide_path_common(&facts, self.env.roots) {
            GuardDecision::Allow(a) => Ok(a),
            GuardDecision::Skip(r) => Err(r),
        }
    }

    fn evaluate(&mut self, raw: &RawBuildDir, st: &Lstat, store: &mut NewestStore) -> Outcome {
        if !self.opts.kinds.selects(raw.flavor) {
            return Outcome::skip(SkipReason::KindNotSelected);
        }
        let allowed = match self.guard(raw, st) {
            Ok(a) => a,
            Err(r) => return Outcome::skip(r),
        };
        let floor_secs = i64::try_from(self.opts.min_age.as_secs()).unwrap_or(i64::MAX);
        if self.now - st.own.0 < floor_secs {
            return Outcome::skip(SkipReason::TooRecent);
        }
        if self.partial && self.clustered.contains(&raw.path) {
            return Outcome::skip_with(
                SkipReason::KeptAsNewest,
                "cluster may be incomplete (partial scan)",
            );
        }
        self.probe(raw, st, allowed, floor_secs, store)
    }

    fn probe(
        &mut self,
        raw: &RawBuildDir,
        st: &Lstat,
        allowed: Allowed,
        floor_secs: i64,
        store: &mut NewestStore,
    ) -> Outcome {
        let floor = crate::mtime_probe::to_system_time(self.now - floor_secs, 0);
        let target = ProbeTarget {
            path: &raw.path,
            ident: st.ident,
            own: st.own,
            root_dev: self.root_dev_of(&raw.path, st.ident),
        };
        let probe = match probe_newest_mtime(self.env.ops, target, floor, store) {
            Ok(p) => p,
            Err(DeleteError::IdentityMismatch) => {
                return Outcome::skip(SkipReason::IdentityChanged)
            }
            Err(DeleteError::Io(libc::ENOENT)) => return Outcome::skip(SkipReason::Gone),
            Err(e) => {
                return Outcome::skip_with(SkipReason::TooRecent, format!("cannot probe: {e:?}"))
            }
        };
        if probe.stats.is_some() {
            self.counts.walked += 1;
        } else {
            self.counts.from_sidecar += 1;
        }
        match probe.recency {
            Recency::Young { newest, .. } => Outcome {
                verdict: BuildVerdict::Skip(SkipReason::TooRecent),
                newest: Some(unix_secs(newest)),
                note: None,
            },
            Recency::Old { newest } => Outcome {
                verdict: BuildVerdict::Will(allowed),
                newest: Some(newest.map_or(st.own.0, unix_secs)),
                note: None,
            },
            Recency::Unknown { why } => Outcome::skip_with(SkipReason::TooRecent, why),
        }
    }
}

/// The decision stage, separate from the scan so it is testable on constructed input. Order per directory:
/// `KindNotSelected`, the common path guard (`OutsideRoot`, `Symlink`, ...), too young by its own mtime, kept
/// by a partial scan's cluster, and only then the probe. Every returned directory is a candidate, skipped or
/// not; `Will` carries the `Allowed` witness.
pub fn decide_build_dirs<D: DirOps>(
    env: &BuildEnv<'_, D>,
    raws: &[RawBuildDir],
    partial_scan: bool,
    opts: &BuildOpts,
    store: &mut NewestStore,
) -> BuildDiscovery {
    let clusters = clusters_of(raws);
    let (mounts, mounts_unreadable) = mount_points(env.probe);
    let mut run = Run {
        env,
        opts,
        now: env.clock.now_epoch(),
        mounts,
        clustered: clusters
            .iter()
            .flat_map(|c| c.members.iter().cloned())
            .collect(),
        partial: partial_scan,
        counts: ProbeCounts::default(),
    };
    let mut items = Vec::with_capacity(raws.len());
    for raw in raws {
        let (ident, out, age) = match lstat(&raw.path) {
            Ok(st) => {
                let out = run.evaluate(raw, &st, store);
                let age = u64::try_from(run.now - st.own.0).unwrap_or(0);
                (st.ident, out, age)
            }
            Err(_) => (
                Identity { dev: 0, ino: 0 },
                Outcome::skip(SkipReason::Gone),
                0,
            ),
        };
        let facts = KindFacts::BuildDir(BuildDirFacts {
            flavor: raw.flavor,
            group: raw.group.clone(),
            age_secs: age,
            lockfile_hash: raw.lockfile_hash,
        });
        items.push(BuildItem {
            candidate: Candidate::new(
                Locator::Path(raw.path.clone()),
                facts,
                AllocatedBytes(raw.est_bytes),
                out.newest,
                ident,
            ),
            verdict: out.verdict,
            note: out.note,
        });
    }
    BuildDiscovery {
        items,
        clusters,
        partial_scan,
        probes: run.counts,
        mounts_unreadable,
        ..BuildDiscovery::default()
    }
}
