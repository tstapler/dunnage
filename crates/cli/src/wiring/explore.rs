//! The composition root of `disk-cleanup explore` (Story 6.5.4, Task 6.5.4c): the real scan source, signals,
//! lister, mark check and planner, owned so the planner's worker thread can outlive any one call.
//!
//! Read-only. The one state it creates is the first-run reserve files (N-8), and only for the interactive
//! mode, whose `p` key saves a `last_plan`; the text and JSON views open no state directory at all. Nothing
//! here takes the run lock, appends to `audit.jsonl` or writes `outcome.jsonl`.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::TrySendError;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use disk_cleanup_adapters::explore::cache_paint::{
    cache_config, explorer_cache_path, paints, save_guard, CacheMode, DEFAULT_MAX_AGE,
};
use disk_cleanup_adapters::explore::facts::ClassifyFactsLoader;
use disk_cleanup_adapters::explore::scan_source::{Companion, WalkerScanSource};
use disk_cleanup_adapters::lock::RunLock;
use disk_cleanup_adapters::probe::basic::mount_sources;
use disk_cleanup_adapters::steam::default_steam_roots;
use disk_cleanup_adapters::{
    RealCommandRunner, RealDirLister, RealDirOps, RealProbe, RealSpaceMeter,
};
use disk_cleanup_core::explore::input::MarkQuery;
use disk_cleanup_core::explore::mark_check::{
    mark_check, MarkCtx, MarkEnv, MarkFacts, MarkMount, MarkOk, MountTable, OwnState, SymlinkFact,
};
use disk_cleanup_core::explore::msg::{ClassifyFacts, MarkRefusal, ScanMsg};
use disk_cleanup_core::explore::ports::{
    DirLister, ExplorerActions, ScanOpts, ScanSource, TermSignals,
};
use disk_cleanup_core::explore::rows::Sort;
use disk_cleanup_core::ports::{CommandRunner, DryRunRunner, SpaceMeter, SystemProbe};
use disk_cleanup_core::roots::RootSet;
use disk_cleanup_core::{Dirs, FsSnapshot};
use disk_cleanup_scan::walker::WalkerBackend;

use super::audit::build_info;
use super::env::{read_root, tool_dirs, Wired};
use super::kinds::{KindCtx, KindRegistry};
use super::run_guard::run_lock_path;
use crate::explore::actions::{ExplorerPorts, RealExplorerActions};
use crate::explore::text::ExploreEnv;

/// Walker threads when `--jobs` is not given: the UI thread shares the machine (SA s3).
pub const MAX_DEFAULT_JOBS: usize = 16;

/// `--jobs` default: `min(CPUs, 16)`.
pub fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map_or(4, std::num::NonZero::get)
        .min(MAX_DEFAULT_JOBS)
}

/// The headroom line is re-read this often (UX-T15).
const HEADROOM_EVERY: Duration = Duration::from_secs(2);

/// What the flags and the terminal decided.
pub struct Settings {
    /// What is displayed and scanned. Never an allowed root for marking (N-14).
    pub display_roots: Vec<PathBuf>,
    pub jobs: usize,
    pub sort: Sort,
    pub cache: CacheMode,
    pub max_seconds: Option<u64>,
    pub tty: bool,
}

pub type MarkCheckFn = dyn Fn(&MarkQuery<'_>) -> Result<MarkOk, MarkRefusal> + Send + Sync;
type OwnerOf = dyn Fn(&Path) -> Option<(u32, u32)> + Send + Sync;
type DeviceLabel = dyn Fn(&Path) -> Option<String> + Send + Sync;

/// Everything the interactive loop is wired with.
pub struct ExplorerEnv {
    pub scan: Arc<dyn ScanSource>,
    pub lister: Arc<dyn DirLister>,
    pub actions: Arc<dyn ExplorerActions>,
    pub scan_opts: ScanOpts,
    pub mark_check: Arc<MarkCheckFn>,
    pub owner_of: Arc<OwnerOf>,
    pub device_label: Arc<DeviceLabel>,
    pub cache_age: Option<String>,
    pub initial_headroom: Result<FsSnapshot, String>,
    pub headroom_path: PathBuf,
    /// `created 2 reserve files ...`, to print before the alternate screen is entered.
    pub reserves_note: Option<String>,
    pub config_path: PathBuf,
    pub cleaner_roots: Vec<PathBuf>,
    pub now_unix: i64,
    /// `another disk-cleanup apply is running (pid N); sizes are in flux`, while the run lock is held.
    pub start_note: Option<String>,
}

/// The footer note while another process holds the run lock. The probe takes no lock and creates no file.
pub fn in_flux_note(dirs: &Dirs) -> Option<String> {
    let state_dir = tool_dirs(dirs).state;
    RunLock::probe(&run_lock_path(&state_dir))
        .map(|pid| format!("another disk-cleanup apply is running (pid {pid}); sizes are in flux"))
}

fn walker_source(base: &Dirs, settings: &Settings, space: Arc<dyn SpaceMeter>) -> WalkerScanSource {
    let backend = WalkerBackend {
        cache: cache_config(base, settings.cache, DEFAULT_MAX_AGE),
    };
    WalkerScanSource::new(backend).with_save_guard(save_guard(space))
}

fn scan_opts(settings: &Settings, interactive: bool) -> ScanOpts {
    ScanOpts {
        generation: 0,
        cross_fs: false,
        threads: settings.jobs.max(1),
        // The text and JSON views wait for the whole scan, so a painted tree would only be thrown away.
        paint_from_cache: interactive && paints(settings.cache),
        max_seconds: settings.max_seconds,
    }
}

fn steam_roots(base: &Dirs) -> Vec<PathBuf> {
    default_steam_roots(&base.home, RealProbe.platform())
}

fn loader(roots: &RootSet, base: &Dirs) -> ClassifyFactsLoader {
    let runner: Arc<dyn CommandRunner> = Arc::new(DryRunRunner(RealCommandRunner::new()));
    ClassifyFactsLoader::new(roots.clone(), steam_roots(base), runner)
}

/// The inputs of the text and JSON views. Facts are loaded here, before the scan, because the views print
/// once at the end and nothing can be shown earlier anyway.
pub fn build_text_env(
    base: &Dirs,
    roots: &RootSet,
    settings: &Settings,
    signals: Arc<dyn TermSignals>,
) -> ExploreEnv {
    let space = Arc::new(RealSpaceMeter::new(RealProbe.platform()));
    let first = settings.display_roots.first().cloned().unwrap_or_default();
    let headroom = space.try_snapshot(&first).map_err(|e| e.to_string());
    let scan: Arc<dyn ScanSource> = Arc::new(walker_source(base, settings, space));
    let mut env = ExploreEnv::new(scan, signals, settings.display_roots.clone(), roots.clone());
    env.scan_opts = scan_opts(settings, false);
    env.facts = loader(roots, base).load();
    env.headroom = headroom;
    env.sort = settings.sort;
    env.tty = settings.tty;
    env.home = Some(base.home.clone());
    env.max_seconds = settings.max_seconds;
    env
}

/// `12 min ago`, for the title of a tree painted from the cache.
pub fn age_words(secs: u64) -> String {
    match secs {
        0..=59 => "under a minute ago".to_string(),
        60..=3599 => format!("{} min ago", secs / 60),
        3600..=86_399 => format!("{} h ago", secs / 3600),
        _ => format!("{} d ago", secs / 86_400),
    }
}

fn cache_age(base: &Dirs, settings: &Settings) -> Option<String> {
    if !paints(settings.cache) {
        return None;
    }
    let modified = std::fs::metadata(explorer_cache_path(base))
        .and_then(|m| m.modified())
        .ok()?;
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default();
    Some(age_words(age.as_secs()))
}

/// The headroom sampler and the facts loader run beside each scan, on the scan's own channel.
fn companion(
    roots: &RootSet,
    base: &Dirs,
    space: Arc<dyn SpaceMeter>,
    headroom_path: PathBuf,
) -> Companion {
    let facts_pending = Arc::new(AtomicBool::new(true));
    let (roots, base) = (roots.clone(), base.clone());
    Arc::new(move |_scan_roots, tx| {
        let (space, path, sampler_tx) = (Arc::clone(&space), headroom_path.clone(), tx.clone());
        let _ = std::thread::Builder::new()
            .name("explore-headroom".into())
            .spawn(move || loop {
                if let Ok(snap) = space.try_snapshot(&path) {
                    match sampler_tx.try_send(ScanMsg::Headroom(snap)) {
                        Err(TrySendError::Disconnected(_)) => return,
                        Ok(()) | Err(TrySendError::Full(_)) => {}
                    }
                }
                std::thread::sleep(HEADROOM_EVERY);
            });
        if facts_pending.swap(false, Ordering::SeqCst) {
            let facts_loader = loader(&roots, &base);
            let _ = std::thread::Builder::new()
                .name("explore-facts".into())
                .spawn(move || {
                    // A panic leaves the defaults: no Steam facts, so Steam rows stay `Shape`.
                    let facts = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        facts_loader.load()
                    }))
                    .unwrap_or_else(|_| ClassifyFacts::default());
                    let _ = tx.send(ScanMsg::Facts(facts));
                });
        }
    })
}

/// The facts of the mark rules, read with `lstat` on the spot. The row the query is about supplies the two
/// facts the scan knows (a boundary row, a directory still being scanned).
struct RealMarkFacts<'q> {
    row_path: &'q Path,
    scanning: bool,
    boundary: bool,
}

impl MarkFacts for RealMarkFacts<'_> {
    fn symlink_in(&self, path: &Path) -> Option<SymlinkFact> {
        let mut prefix = PathBuf::new();
        for part in path.components() {
            prefix.push(part);
            match std::fs::symlink_metadata(&prefix) {
                Ok(m) if m.file_type().is_symlink() => {
                    return Some(SymlinkFact {
                        real: std::fs::canonicalize(&prefix).ok(),
                    });
                }
                Ok(_) => {}
                Err(_) => return None,
            }
        }
        None
    }

    fn dev_of(&self, path: &Path) -> Option<u64> {
        std::fs::symlink_metadata(path).ok().map(|m| m.dev())
    }

    fn is_dir(&self, path: &Path) -> Option<bool> {
        std::fs::symlink_metadata(path).ok().map(|m| m.is_dir())
    }

    fn is_boundary(&self, path: &Path) -> bool {
        self.boundary && path == self.row_path
    }

    fn scanning(&self, path: &Path) -> bool {
        self.scanning && path == self.row_path
    }
}

/// `canonicalize` of the deepest ancestor that exists, with the missing tail appended unchanged.
pub fn canonical_lenient(path: &Path) -> PathBuf {
    let mut missing: Vec<&std::ffi::OsStr> = Vec::new();
    let mut at = path;
    loop {
        if let Ok(real) = std::fs::canonicalize(at) {
            return missing.iter().rev().fold(real, |p, c| p.join(c));
        }
        match (at.parent(), at.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name);
                at = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

fn mount_table() -> MountTable {
    MountTable::new(
        mount_sources()
            .unwrap_or_default()
            .into_iter()
            .map(|(point, source)| MarkMount { point, source })
            .collect(),
    )
}

/// Rules M1 to M8 over the real facts. The cleaner's roots (never the explore PATH) decide M1.
pub fn mark_check_fn(base: &Dirs, roots: &RootSet, exe: PathBuf, tty: bool) -> Arc<MarkCheckFn> {
    let scan_roots = roots.scan_roots().to_vec();
    let home = std::fs::canonicalize(&base.home).unwrap_or_else(|_| base.home.clone());
    let mounts = mount_table();
    let own_state = OwnState::canonical(base, exe, &canonical_lenient);
    Arc::new(move |q: &MarkQuery<'_>| {
        let facts = RealMarkFacts {
            row_path: q.path,
            scanning: q.row.is_directory() && !q.row.complete,
            boundary: q.row.boundary,
        };
        let env = MarkEnv {
            roots: &scan_roots,
            home: &home,
            mounts: &mounts,
            own_state: &own_state,
            facts: &facts,
            tty,
        };
        mark_check(
            q.path,
            &MarkCtx {
                env: &env,
                marks: q.marks,
            },
        )
    })
}

/// Wires the interactive explorer. Opens the state directory (the first run creates the reserve files).
pub fn build_explorer_env(base: &Dirs, roots: &RootSet, settings: &Settings) -> ExplorerEnv {
    let wired = Wired::build(base, true);
    let reserves_note = wired
        .state
        .dir
        .as_ref()
        .filter(|_| wired.state.reserves_created)
        .map(|d| {
            disk_cleanup_core::Warning::ReservesCreated {
                state_dir: d.paths.dir.clone(),
            }
            .text()
        });
    let Wired {
        dirs,
        fs,
        runner,
        clock,
        probe,
        space,
        audit,
        guard,
        plans,
        state: _,
    } = wired;
    let space: Arc<RealSpaceMeter> = Arc::new(space);
    let headroom_path = settings.display_roots.first().cloned().unwrap_or_default();
    let activity = Arc::new(AtomicUsize::new(0));
    let scan: Arc<dyn ScanSource> = Arc::new(
        walker_source(base, settings, space.clone())
            .with_activity(Arc::clone(&activity))
            .with_companion(companion(roots, base, space.clone(), headroom_path.clone())),
    );

    let read = read_root();
    let ops = {
        let read = read.clone();
        move || RealDirOps::confined_to(&read)
    };
    let registry = KindRegistry::build(&KindCtx {
        roots: roots.clone(),
        fs_root: read.clone(),
        ops: &ops,
        fixture_mode: std::env::var_os("DC_TEST_ROOT").is_some(),
    });
    let run_id = format!(
        "{:x}-{:x}",
        disk_cleanup_core::ports::Clock::now_epoch(&clock),
        std::process::id()
    );
    let ports = Arc::new(ExplorerPorts {
        fs: Arc::new(fs),
        runner: Arc::new(runner),
        clock: Arc::new(clock),
        probe: Arc::new(probe),
        space: space.clone(),
        audit: Arc::new(audit),
        guard: Arc::new(guard),
        plans: Arc::from(plans),
        dirs,
        targets: registry.into_targets(),
        build: build_info(),
        roots: roots.run_roots(),
        run_id,
        jobs: settings.jobs,
    });
    let still_scanning = {
        let activity = Arc::clone(&activity);
        move |_: &Path| activity.load(Ordering::SeqCst) > 0
    };
    let actions: Arc<dyn ExplorerActions> =
        Arc::new(RealExplorerActions::new(ports).with_scanning_probe(Arc::new(still_scanning)));

    let exe = std::env::current_exe().unwrap_or_default();
    let mounts = mount_table();
    let device_label: Arc<DeviceLabel> =
        Arc::new(move |p: &Path| mounts.at(p).map(|m| m.source.clone()));
    let owner_of: Arc<OwnerOf> = Arc::new(|p: &Path| {
        std::fs::symlink_metadata(p)
            .ok()
            .map(|m| (m.uid(), m.gid()))
    });
    let now_unix = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    ExplorerEnv {
        scan,
        lister: Arc::new(RealDirLister),
        actions,
        scan_opts: scan_opts(settings, true),
        mark_check: mark_check_fn(base, roots, exe, settings.tty),
        owner_of,
        device_label,
        cache_age: cache_age(base, settings),
        initial_headroom: space
            .try_snapshot(&headroom_path)
            .map_err(|e| e.to_string()),
        headroom_path,
        reserves_note,
        config_path: disk_cleanup_adapters::config::config_path(base),
        cleaner_roots: roots.scan_roots().to_vec(),
        now_unix,
        start_note: in_flux_note(base),
    }
}

#[cfg(test)]
mod tests {
    use super::age_words;

    #[test]
    fn age_words_should_pick_the_largest_unit_that_fits() {
        assert_eq!(age_words(5), "under a minute ago");
        assert_eq!(age_words(12 * 60 + 30), "12 min ago");
        assert_eq!(age_words(3 * 3600), "3 h ago");
        assert_eq!(age_words(2 * 86_400 + 5), "2 d ago");
    }
}
