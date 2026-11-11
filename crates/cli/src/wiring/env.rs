//! `build_env` composition of Story 1.2.4 (Task 1.2.4e): the real adapters behind one `Env`, and the read-only
//! pieces this story needs that no earlier story owns (`RealFs`, `SystemClock`, the inert plan-mode guard).
//!
//! This is the plan-mode and status composition. The apply path, with the run lock, the signal handling, the
//! confinement and the apply token, is `wiring::compose` (Story 2.3.4).

use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use disk_cleanup_adapters::audit::RealAuditSink;
use disk_cleanup_adapters::state::{
    OutcomeLine, OutcomeMode, RealPlanStore, StateDir, OUTCOME_SCHEMA,
};
use disk_cleanup_adapters::{RealCommandRunner, RealProbe, RealSpaceMeter};
use disk_cleanup_core::delete_types::{AbortFlag, DirEntryInfo, FileKind, StatInfo};
use disk_cleanup_core::ports::{Clock, DryRunRunner, Env, Fs, PlanStore, RunGuard, SystemProbe};
use disk_cleanup_core::{
    Dirs, DiskVerdict, FatalError, Identity, SaveOutcome, SavedPlan, SavedSelection,
};

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_epoch(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
    }
}

fn kind_of(t: std::fs::FileType) -> FileKind {
    use std::os::unix::fs::FileTypeExt;
    if t.is_dir() {
        FileKind::Dir
    } else if t.is_file() {
        FileKind::File
    } else if t.is_symlink() {
        FileKind::Symlink
    } else if t.is_fifo() {
        FileKind::Fifo
    } else if t.is_socket() {
        FileKind::Socket
    } else if t.is_char_device() {
        FileKind::CharDev
    } else if t.is_block_device() {
        FileKind::BlockDev
    } else {
        FileKind::Other
    }
}

/// Read-only `Fs` over `std::fs` (never follows a final symlink).
pub struct RealFs;

impl Fs for RealFs {
    fn lstat(&self, path: &Path) -> io::Result<StatInfo> {
        let m = std::fs::symlink_metadata(path)?;
        Ok(StatInfo {
            ident: Identity::of(&m),
            kind: kind_of(m.file_type()),
            mode: m.mode() & 0o7777,
            nlink: m.nlink(),
            blocks: m.blocks(),
            size: m.size(),
            mtime_secs: m.mtime(),
            mtime_nsec: u32::try_from(m.mtime_nsec()).unwrap_or(0),
            mnt_id: disk_cleanup_adapters::mount_id_of(path),
        })
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntryInfo>> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(path)? {
            let e = e?;
            out.push(DirEntryInfo {
                name: e.file_name(),
                kind: e.file_type().map_or(FileKind::Other, kind_of),
                ino: std::os::unix::fs::DirEntryExt::ino(&e),
            });
        }
        Ok(out)
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn fs_type(&self, path: &Path) -> Option<u64> {
        disk_cleanup_adapters::fs_type_of(path)
    }
}

/// The plan-mode guard: it never locks and never sees a signal. The real one is Story 2.2.2.
#[derive(Default)]
pub struct InertRunGuard {
    flag: AbortFlag,
}

impl RunGuard for InertRunGuard {
    fn acquire(&self) -> Result<(), FatalError> {
        Ok(())
    }

    fn abort_flag(&self) -> &AbortFlag {
        &self.flag
    }

    fn interrupted(&self) -> Option<i32> {
        None
    }
}

/// A plan store for a state directory that could not be opened: nothing is saved, the plan is reported as
/// `NOT saved` (`design/ux.md` 1.4) and the run goes on.
pub struct UnsavedPlans(pub i32);

impl PlanStore for UnsavedPlans {
    fn load_last(&self) -> Option<SavedPlan> {
        None
    }

    fn save_merged(&self, _new: Vec<SavedSelection>, _euid: u32, _now: i64) -> SaveOutcome {
        SaveOutcome::NotSaved(self.0)
    }
}

/// The state directory of a run: `<XDG_STATE_HOME>/disk-cleanup`, opened (reserves created) or the errno.
pub struct State {
    pub dir: Option<StateDir>,
    pub open_errno: Option<i32>,
    pub reserves_created: bool,
}

/// Opens the state directory. Called by the modes that write state (plan, apply, status), after their
/// refusals, so a refused run leaves no file behind.
pub fn open_state(dirs: &Dirs, euid: u32) -> State {
    let avail = |p: &Path| disk_cleanup_adapters::statvfs::df(p).map(|d| d.avail_bytes);
    match StateDir::open(&dirs.state, euid, &avail) {
        Ok((sd, init)) => State {
            dir: Some(sd),
            open_errno: None,
            reserves_created: init.reserves_created,
        },
        Err(e) => State {
            dir: None,
            open_errno: Some(e.raw_os_error().unwrap_or(5)),
            reserves_created: false,
        },
    }
}

/// Everything `Env` borrows, owned in one place.
pub struct Wired {
    pub dirs: Dirs,
    pub fs: RealFs,
    pub runner: DryRunRunner<RealCommandRunner>,
    pub clock: SystemClock,
    pub probe: RealProbe,
    pub space: RealSpaceMeter,
    pub audit: RealAuditSink,
    pub guard: InertRunGuard,
    pub plans: Box<dyn PlanStore>,
    pub state: State,
}

/// `$XDG_STATE_HOME` and friends resolved by `wiring::dirs`; the tool's own state directory is the
/// `disk-cleanup` directory below it (`Dirs.state` of the `Env`).
pub fn tool_dirs(base: &Dirs) -> Dirs {
    Dirs {
        state: base.state.join("disk-cleanup"),
        ..base.clone()
    }
}

impl Wired {
    /// Builds the plan-mode composition. `open_state` is whether this mode writes state.
    pub fn build(base: &Dirs, open: bool) -> Wired {
        let dirs = tool_dirs(base);
        let probe = RealProbe;
        let euid = probe.euid();
        let state = if open {
            open_state(&dirs, euid)
        } else {
            State {
                dir: None,
                open_errno: None,
                reserves_created: false,
            }
        };
        let paths = disk_cleanup_adapters::state::StatePaths {
            dir: dirs.state.clone(),
        };
        let owner = state.dir.as_ref().and_then(|d| d.owner);
        let audit = RealAuditSink::new(paths.audit(), paths.audit_reserve(), owner);
        let plans: Box<dyn PlanStore> = match (&state.dir, state.open_errno) {
            (Some(sd), _) => {
                let now: Arc<dyn Fn() -> i64 + Send + Sync> = Arc::new(|| SystemClock.now_epoch());
                Box::new(RealPlanStore::new(sd.clone(), now))
            }
            (None, errno) => Box::new(UnsavedPlans(errno.unwrap_or(5))),
        };
        Wired {
            fs: RealFs,
            runner: DryRunRunner(RealCommandRunner::new()),
            clock: SystemClock,
            space: RealSpaceMeter::new(probe.platform()),
            probe,
            audit,
            guard: InertRunGuard::default(),
            plans,
            state,
            dirs,
        }
    }

    pub fn env(&self) -> Env<'_> {
        Env {
            fs: &self.fs,
            runner: &self.runner,
            clock: &self.clock,
            probe: &self.probe,
            space: &self.space,
            audit: &self.audit,
            guard: &self.guard,
            plans: self.plans.as_ref(),
            dirs: self.dirs.clone(),
        }
    }

    /// The one `outcome.jsonl` line of a run (Task 2.2.1e). A failure is the caller's warning.
    pub fn record_outcome(
        &self,
        mode: OutcomeMode,
        run_id: &str,
        verdict: DiskVerdict,
        free_bytes: u64,
        unallocated_bytes: Option<u64>,
        via: Option<&str>,
    ) -> io::Result<()> {
        let Some(sd) = &self.state.dir else {
            return Ok(());
        };
        disk_cleanup_adapters::state::append_outcome(
            &sd.paths,
            sd.owner,
            &OutcomeLine {
                schema: OUTCOME_SCHEMA,
                ts: self.clock.now_epoch(),
                run_id: run_id.to_string(),
                mode,
                verdict,
                free_bytes,
                unallocated_bytes,
                via: via.map(ToString::to_string),
            },
        )
    }
}

/// The directory reads are rooted at: `DC_TEST_ROOT` under the fixture harness, `/` otherwise. A path
/// outside it is refused by `RealDirOps`, so the fixture binary cannot read the owner's HOME.
pub fn read_root() -> PathBuf {
    std::env::var_os("DC_TEST_ROOT")
        .map(PathBuf::from)
        .and_then(|p| std::fs::canonicalize(p).ok())
        .unwrap_or_else(|| PathBuf::from("/"))
}
