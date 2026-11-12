#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]
//! A rig of port fakes and a scripted `Target` for the fake-driven acceptance criteria of Story 1.2.4: the
//! command line is parsed by the real `args::parse` and run by the real `app::run`, so only the outside world
//! is fake.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use dunnage_cli::app::{self, ApplyMode, Io, Ports};
use dunnage_cli::args::{self, Parsed};
use dunnage_cli::terminal::Terminal;
use dunnage_core::delete_types::{DeleteFailure, Freed};
use dunnage_core::ports::Env;
use dunnage_core::selector::Selection;
use dunnage_core::target::{Guarded, KindGate, Target};
use dunnage_core::testing::{
    FakeAuditSink, FakeClock, FakeCommandRunner, FakeFs, FakePlanStore, FakeProbe, FakeRunGuard,
    FakeSpaceMeter,
};
use dunnage_core::{
    AllocatedBytes, ApplyToken, BuildInfo, Candidate, CandidateId, Dirs, DiskVerdict, RunRoots,
    SelectorId,
};

pub const NOW: i64 = 1_800_000_000;

pub fn freed(bytes: u64) -> Freed {
    Freed {
        attributed_bytes: AllocatedBytes(bytes),
        entries_removed: 1,
        ..Freed::default()
    }
}

pub type ExecFn = Box<dyn Fn(&Candidate) -> Result<Freed, DeleteFailure> + Send + Sync>;

/// A target that serves `selectors`, lists `candidates` and records what it was asked to execute.
pub struct FakeTarget {
    pub selectors: &'static [SelectorId],
    pub candidates: Vec<Candidate>,
    pub exec: ExecFn,
    pub executed: Mutex<Vec<CandidateId>>,
    /// What `Target::partial_scan` answers (a walking kind that hit `--max-seconds`).
    pub partial: Option<u64>,
}

impl FakeTarget {
    pub fn new(selectors: &'static [SelectorId], candidates: Vec<Candidate>) -> FakeTarget {
        FakeTarget {
            selectors,
            candidates,
            exec: Box::new(|c| Ok(freed(c.est_bytes().0))),
            executed: Mutex::new(Vec::new()),
            partial: None,
        }
    }

    pub fn executed(&self) -> Vec<CandidateId> {
        self.executed.lock().unwrap().clone()
    }
}

impl Target for FakeTarget {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn selectors(&self) -> &'static [SelectorId] {
        self.selectors
    }

    fn gate(&self, _env: &Env) -> KindGate {
        KindGate::Available
    }

    fn discover(&self, _env: &Env, _sel: &Selection) -> Vec<Candidate> {
        self.candidates.clone()
    }

    fn guard(&self, _c: &Candidate, _env: &Env) -> Guarded {
        Guarded::Allow
    }

    fn partial_scan(&self) -> Option<u64> {
        self.partial
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

pub struct Rig {
    pub fs: FakeFs,
    pub runner: FakeCommandRunner,
    pub clock: Arc<FakeClock>,
    pub probe: FakeProbe,
    pub space: FakeSpaceMeter,
    pub audit: FakeAuditSink,
    pub guard: Arc<FakeRunGuard>,
    pub plans: FakePlanStore,
    pub outcomes: Mutex<Vec<DiskVerdict>>,
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
            outcomes: Mutex::new(Vec::new()),
        }
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
                state: PathBuf::from("/home/u/.local/state/dunnage"),
                cache: PathBuf::from("/home/u/.cache/dunnage"),
                config: PathBuf::from("/home/u/.config/dunnage"),
            },
        }
    }
}

pub fn build() -> BuildInfo {
    BuildInfo {
        bin_version: "0.1.0".into(),
        git_commit: "abc".into(),
        dirty: false,
        metascope_rev: "none".into(),
        patched: false,
        metascope_dirty: false,
    }
}

/// What a run printed.
#[derive(Debug)]
pub struct Ran {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Parses `argv` and runs it. `apply` decides whether the build can delete at all.
pub fn run(rig: &Rig, targets: &[&dyn Target], argv: &[&str], apply: bool) -> Ran {
    let cli = match args::parse(argv.iter().copied()) {
        Parsed::Run(c) => c,
        Parsed::Exit {
            code,
            stdout,
            stderr,
        } => {
            return Ran {
                code,
                stdout,
                stderr,
            }
        }
    };
    let env = rig.env();
    let mint = || ApplyToken::for_tests();
    let outcome = |_m, v, _f, _u| rig.outcomes.lock().unwrap().push(v);
    let ports = Ports {
        env: &env,
        targets: targets.to_vec(),
        apply: if apply {
            ApplyMode::Granted(&mint)
        } else {
            ApplyMode::Disabled
        },
        apply_capable: None,
        build: build(),
        roots: RunRoots::default(),
        scan_roots: 1,
        exe: PathBuf::from("/usr/bin/dunnage"),
        term: Terminal::pipe(),
        stdout_tty: false,
        run_id: "run-1".into(),
        via: None,
        outcome: &outcome,
        reserves_note: None,
        config_roots: None,
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = app::run(
        &cli,
        &ports,
        &mut Io {
            out: &mut out,
            err: &mut err,
        },
    );
    Ran {
        code,
        stdout: String::from_utf8(out).unwrap(),
        stderr: String::from_utf8(err).unwrap(),
    }
}

/// The id of the plan a dry run of `argv` printed.
pub fn plan_id_of(rig: &Rig, targets: &[&dyn Target], argv: &[&str]) -> String {
    let mut a: Vec<&str> = argv.to_vec();
    a.push("--json");
    let r = run(rig, targets, &a, true);
    assert_eq!(r.code, 0, "{r:?}");
    let doc: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    doc["plan_id"].as_str().unwrap().to_string()
}

pub fn executed_total(ts: &[&FakeTarget]) -> usize {
    ts.iter().map(|t| t.executed().len()).sum()
}
