#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]
//! Shared fixtures of the Story 1.1.4 tests: a scripted `Target`, candidate makers and a rig of port fakes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use disk_cleanup_core::audit_types::RunRoots;
use disk_cleanup_core::delete_types::{DeleteFailure, Freed};
use disk_cleanup_core::executor::{Executor, Mode, Refused, RunMeta, RunOptions, RunReport};
use disk_cleanup_core::plan::PlanId;
use disk_cleanup_core::ports::Env;
use disk_cleanup_core::selector::{KindSelector, Selection};
use disk_cleanup_core::target::{Guarded, KindGate, Target};
use disk_cleanup_core::testing::{
    EventLog, FakeAuditSink, FakeClock, FakeCommandRunner, FakeFs, FakePlanStore, FakeProbe,
    FakeRunGuard, FakeSpaceMeter,
};
use disk_cleanup_core::{
    AllocatedBytes, ApplyToken, BuildDirFacts, BuildDirFlavor, BuildInfo, CacheFacts, Candidate,
    CandidateId, Dirs, DockerFacts, DockerId, Identity, KindFacts, Locator, SelectorId,
};

pub const NOW: i64 = 1_700_000_000;
pub const DAY: i64 = 86_400;

pub fn cache(i: u64, bytes: u64) -> Candidate {
    Candidate::new(
        Locator::Path(PathBuf::from(format!("/home/u/.cache/c{i}"))),
        KindFacts::Cache(CacheFacts {
            label: format!("cache {i}"),
            apt: false,
        }),
        AllocatedBytes(bytes),
        Some(NOW - 10 * DAY),
        Identity { dev: 1, ino: i },
    )
}

/// A `node_modules` of the given age in days.
pub fn build_dir(i: u64, bytes: u64, age_days: i64) -> Candidate {
    Candidate::new(
        Locator::Path(PathBuf::from(format!("/home/u/code/p{i}/node_modules"))),
        KindFacts::BuildDir(BuildDirFacts {
            flavor: BuildDirFlavor::NodeModules,
            group: PathBuf::from(format!("/home/u/code/p{i}")),
            age_secs: (age_days * DAY) as u64,
            lockfile_hash: None,
        }),
        AllocatedBytes(bytes),
        Some(NOW - age_days * DAY),
        Identity {
            dev: 1,
            ino: 1000 + i,
        },
    )
}

/// A build dir at an exact path, old enough for any rule.
pub fn build_dir_at(path: &str) -> Candidate {
    let p = PathBuf::from(path);
    Candidate::new(
        Locator::Path(p.clone()),
        KindFacts::BuildDir(BuildDirFacts {
            flavor: BuildDirFlavor::NodeModules,
            group: p.parent().map(PathBuf::from).unwrap_or_default(),
            age_secs: (70 * DAY) as u64,
            lockfile_hash: None,
        }),
        AllocatedBytes(4096),
        Some(NOW - 70 * DAY),
        Identity {
            dev: 1,
            ino: 5000 + path.len() as u64,
        },
    )
}

pub fn docker_cache(i: u64, bytes: u64) -> Candidate {
    Candidate::new(
        Locator::Docker(DockerId(format!("bc{i}"))),
        KindFacts::Docker(DockerFacts::BuildCache),
        AllocatedBytes(bytes),
        None,
        Identity { dev: 0, ino: i },
    )
}

pub fn freed(bytes: u64) -> Freed {
    Freed {
        attributed_bytes: AllocatedBytes(bytes),
        entries_removed: 1,
        ..Freed::default()
    }
}

pub type ExecFn = Box<dyn Fn(&Candidate) -> Result<Freed, DeleteFailure> + Send + Sync>;
/// Gets the candidate and how many times `guard` was asked about it before (0 = plan time).
pub type GuardFn = Box<dyn Fn(&Candidate, usize) -> Guarded + Send + Sync>;

pub struct FakeTarget {
    pub name: &'static str,
    pub selectors: &'static [SelectorId],
    pub gate: KindGate,
    pub detail: Option<String>,
    pub candidates: Vec<Candidate>,
    pub guard: GuardFn,
    pub exec: ExecFn,
    pub on_discover: Box<dyn Fn() + Send + Sync>,
    pub executed: Mutex<Vec<CandidateId>>,
    guard_calls: Mutex<HashMap<CandidateId, usize>>,
}

impl FakeTarget {
    pub fn new(selectors: &'static [SelectorId], candidates: Vec<Candidate>) -> FakeTarget {
        FakeTarget {
            name: "fake",
            selectors,
            gate: KindGate::Available,
            detail: None,
            candidates,
            guard: Box::new(|_, _| Guarded::Allow),
            exec: Box::new(|c| Ok(freed(c.est_bytes().0))),
            on_discover: Box::new(|| {}),
            executed: Mutex::new(Vec::new()),
            guard_calls: Mutex::new(HashMap::new()),
        }
    }

    pub fn caches(candidates: Vec<Candidate>) -> FakeTarget {
        FakeTarget::new(&[SelectorId::Caches], candidates)
    }

    pub fn executed(&self) -> Vec<CandidateId> {
        self.executed.lock().unwrap().clone()
    }
}

impl Target for FakeTarget {
    fn name(&self) -> &'static str {
        self.name
    }

    fn selectors(&self) -> &'static [SelectorId] {
        self.selectors
    }

    fn gate(&self, _env: &Env) -> KindGate {
        self.gate
    }

    fn unavailable_detail(&self, _env: &Env) -> Option<String> {
        self.detail.clone()
    }

    fn discover(&self, _env: &Env, _sel: &Selection) -> Vec<Candidate> {
        (self.on_discover)();
        self.candidates.clone()
    }

    fn guard(&self, c: &Candidate, _env: &Env) -> Guarded {
        let n = {
            let mut m = self.guard_calls.lock().unwrap();
            let n = m.entry(c.id()).or_insert(0);
            *n += 1;
            *n - 1
        };
        (self.guard)(c, n)
    }

    fn execute(
        &self,
        c: &Candidate,
        _env: &Env,
        _proof: &ApplyToken,
    ) -> Result<Freed, DeleteFailure> {
        self.executed.lock().unwrap().push(c.id());
        (self.exec)(c)
    }
}

/// One of each port fake. Fields are public so a test swaps in a scripted one before `env()`.
pub struct Rig {
    pub fs: FakeFs,
    pub runner: FakeCommandRunner,
    pub clock: Arc<FakeClock>,
    pub probe: FakeProbe,
    pub space: FakeSpaceMeter,
    pub audit: FakeAuditSink,
    pub guard: Arc<FakeRunGuard>,
    pub plans: FakePlanStore,
}

impl Rig {
    pub fn new() -> Rig {
        Rig {
            fs: FakeFs::new(),
            runner: FakeCommandRunner::new(),
            clock: Arc::new(FakeClock::at(NOW)),
            probe: FakeProbe::new(),
            space: FakeSpaceMeter::new(
                vec![FakeSpaceMeter::plain_snapshot("/home/u", 500, 1000)],
                FakeSpaceMeter::stable(),
            ),
            audit: FakeAuditSink::new(),
            guard: Arc::new(FakeRunGuard::new()),
            plans: FakePlanStore::new(),
        }
    }

    pub fn with_log(mut self, log: &EventLog) -> Rig {
        self.space = FakeSpaceMeter::new(
            vec![FakeSpaceMeter::plain_snapshot("/home/u", 500, 1000)],
            FakeSpaceMeter::stable(),
        )
        .with_log(log);
        self.audit = FakeAuditSink::new().with_log(log);
        self.guard = Arc::new(FakeRunGuard::new().with_log(log));
        self
    }

    pub fn env(&self) -> Env<'_> {
        Env {
            fs: &self.fs,
            runner: &self.runner,
            clock: &*self.clock,
            probe: &self.probe,
            space: &self.space,
            audit: &self.audit,
            guard: &*self.guard,
            plans: &self.plans,
            dirs: Dirs {
                home: PathBuf::from("/home/u"),
                state: PathBuf::from("/home/u/.local/state/disk-cleanup"),
                cache: PathBuf::from("/home/u/.cache/disk-cleanup"),
                config: PathBuf::from("/home/u/.config/disk-cleanup"),
            },
        }
    }
}

pub fn options(jobs: usize) -> RunOptions {
    RunOptions {
        jobs,
        settle_timeout: Duration::from_secs(5),
        meter_path: None,
        meta: RunMeta {
            run_id: "run-1".into(),
            build: BuildInfo {
                bin_version: "0.1.0".into(),
                git_commit: "abc".into(),
                dirty: false,
                metascope_rev: "none".into(),
                patched: false,
                metascope_dirty: false,
            },
            stamp: None,
            break_glass: None,
            roots: RunRoots::default(),
            verdict_start: None,
        },
    }
}

pub fn caches_selection() -> Selection {
    Selection::new(
        KindSelector::of(&[SelectorId::Caches]),
        KindSelector::default(),
    )
}

pub fn selection(ids: &[SelectorId]) -> Selection {
    Selection::new(KindSelector::of(ids), KindSelector::default())
}

pub fn run(
    rig: &Rig,
    targets: &[&dyn Target],
    sel: &Selection,
    apply: bool,
    jobs: usize,
    supplied: Option<PlanId>,
) -> Result<RunReport, Box<Refused>> {
    let env = rig.env();
    let ex = Executor::new(&env, targets.to_vec(), options(jobs));
    let mode = if apply {
        Mode::Apply(ApplyToken::for_tests())
    } else {
        Mode::Plan
    };
    ex.run(sel, &mode, supplied)
}

pub fn spin_until(f: impl Fn() -> bool) {
    let n = AtomicUsize::new(0);
    while !f() {
        std::thread::yield_now();
        assert!(
            n.fetch_add(1, Ordering::Relaxed) < 50_000_000,
            "spin timed out"
        );
    }
}
