//! The apply-path composition root (Story 2.3.4, Task 2.3.4a). One place wires the gate stamp, the reserves,
//! the run lock, the audit log, the plan store, the apply token and the confinement, in the order of
//! `design/ux.md` 1.4:
//!
//! `parse_args, kind_availability, check_stamp, plan_id_gate, unclean_notice, marker, ensure_reserves,
//! run_lock, audit_open, starvation_preflight, header_rest, execute, run_end, summary, closing`
//!
//! [`compose_apply`] is that sequence over ports (so a test drives it with fakes and a [`Trace`]);
//! [`run_apply`] builds the real ports and is the only place that mints the capability: it holds the single
//! production call of `grant_apply`, the `check_stamp` step with the hash of `std::env::current_exe()` and the
//! `/dev/tty` `BYPASS` prompt, `confine_mode`, and the construction of `RealAuditSink`, `RealPlanStore`,
//! `RealRunGuard` and `RealSpaceMeter` into the `Env`.
//!
//! A run refused in steps 1 to 4 creates no state file: nothing here touches the state directory before the
//! plan-id gate has passed, and `ensure_reserves` runs before the lock and before the first state write.

use std::cell::Cell;
use std::ffi::OsString;
use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use dunnage_adapters::audit::{unclean_run_warning, RealAuditSink};
use dunnage_adapters::gate_stamp::{
    current_exe_sha256, open_controlling_tty, read_stamp, stamp_path,
};
use dunnage_adapters::lock::RealRunGuard;
use dunnage_adapters::signals::{SignalState, SwallowBrokenPipe};
use dunnage_adapters::space_meter::run_meter;
use dunnage_adapters::state::{
    append_outcome, LastPlanRead, OutcomeLine, OutcomeMode, Owner, RealPlanStore, StateDir,
    StatePaths, OUTCOME_SCHEMA,
};
use dunnage_adapters::treestat::TreestatSizer;
use dunnage_adapters::{RealDirOps, RealProbe, RealSpaceMeter};
use dunnage_core::executor::{ExitClass, Refused, RunHooks};
use dunnage_core::explore::confirm::ConfirmedPlan;
use dunnage_core::explore::mark_check::OwnState;
use dunnage_core::plan::PlanId;
use dunnage_core::plan_state::BuildInfo;
use dunnage_core::ports::{Clock, Env, SystemProbe};
use dunnage_core::roots::RootSet;
use dunnage_core::selector::KindSelector;
use dunnage_core::{
    build_flavor, ApplyToken, Candidate, Dirs, DiskVerdict, FatalError, Locator, Warning,
};

use crate::app::{self, ApplyHooks, ApplyMode, Io, Ports};
use crate::apply::render_marker;
use crate::args::{self, Cli, Parsed};
use crate::explore::apply::{
    compose_explorer_apply, ExplorerApplyPorts, ExplorerRun, NOT_IN_BUILD as EXPLORER_NOT_IN_BUILD,
};
use crate::progress::{progress_mode, Progress};
use crate::terminal::{self, Terminal};
use crate::wiring::audit::{build_info, stamp_gate, GateExit, StampInputs, StampOutcome};
use crate::wiring::confine::{self, Confine};
use crate::wiring::env::{open_state, read_root, tool_dirs, RealFs, SystemClock};
use crate::wiring::kinds::{KindCtx, KindRegistry};
use crate::wiring::run_guard::{install_apply, run_lock_path};
use crate::wiring::token::grant_apply;

const EXIT_USAGE: i32 = 64;
const EXIT_FATAL: i32 = 2;

/// The names of the steps a run went through, in order.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Trace(Vec<&'static str>);

impl Trace {
    pub fn steps(&self) -> &[&'static str] {
        &self.0
    }

    pub(crate) fn push(&mut self, name: &'static str) {
        self.0.push(name);
    }
}

/// A cloneable writer, so the hooks and the renderer write to the same stream in order.
#[derive(Clone)]
pub struct Sink(Arc<Mutex<Box<dyn Write + Send>>>);

impl Sink {
    pub fn new(w: impl Write + Send + 'static) -> Sink {
        Sink(Arc::new(Mutex::new(Box::new(w))))
    }

    fn line(&self, text: &str) {
        let mut w = self;
        let _ = w.write_all(format!("{text}\n").as_bytes());
        let _ = w.flush();
    }

    fn flush_inner(&self) -> io::Result<()> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .flush()
    }
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_inner()
    }
}

impl Write for &Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_inner()
    }
}

/// The state-directory work an apply does between the plan-id gate and the lock. Real: [`RealApplyState`].
pub trait ApplyState: Sync {
    /// `Ok(Some(text))` when the previous run did not finish; `Err(text)` when the audit log is unreadable
    /// (an unknown schema), which ends the run.
    fn unclean_notice(&self) -> Result<Option<String>, String>;

    /// Creates the state directory and the two reserves before any other state write. `Ok(Some(note))` when
    /// reserves were created.
    fn ensure_reserves(&self) -> Result<Option<String>, FatalError>;
}

/// The real state directory.
pub struct RealApplyState {
    pub dirs: Dirs,
    pub euid: u32,
}

impl ApplyState for RealApplyState {
    fn unclean_notice(&self) -> Result<Option<String>, String> {
        let audit = StatePaths {
            dir: self.dirs.state.clone(),
        }
        .audit();
        match unclean_run_warning(&audit) {
            Ok(w) => Ok(w.map(|w| w.text())),
            Err(e) => Err(e.text()),
        }
    }

    fn ensure_reserves(&self) -> Result<Option<String>, FatalError> {
        let state = open_state(&self.dirs, self.euid);
        match (state.dir, state.open_errno) {
            (Some(_), _) => Ok(state.reserves_created.then(|| {
                Warning::ReservesCreated {
                    state_dir: self.dirs.state.clone(),
                }
                .text()
            })),
            (None, errno) => Err(FatalError::AuditUnwritable {
                path: self.dirs.state.join("audit.jsonl"),
                errno: errno.unwrap_or(5),
            }),
        }
    }
}

/// The ports [`compose_apply`] runs over.
pub struct ApplyWiring<'a> {
    pub ports: &'a Ports<'a>,
    pub state: &'a dyn ApplyState,
    pub stderr_tty: bool,
}

/// Watches the executor's fixed points: records the trace, runs the unclean notice, writes L1 as the run
/// commits, creates the reserves, and prints the `[n/m] removing` lines.
struct Hooks<'a> {
    trace: Mutex<Vec<&'static str>>,
    state: &'a dyn ApplyState,
    out: Sink,
    err: Sink,
    term: &'a Terminal,
    json: bool,
    bypass: Mutex<Option<String>>,
    marker_done: AtomicBool,
    progress: Mutex<Progress<'a>>,
    started: Instant,
}

impl Hooks<'_> {
    fn refusal(plan_id: PlanId, text: String, fatal: Option<FatalError>) -> Box<Refused> {
        Box::new(Refused {
            exit: ExitClass::Fatal,
            notes: Vec::new(),
            text,
            refusal: None,
            plan_id: Some(plan_id),
            fatal,
        })
    }
}

impl RunHooks for Hooks<'_> {
    fn step(&self, name: &'static str) {
        self.trace
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(name);
    }

    fn committed(&self, plan_id: PlanId) -> Result<(), Box<Refused>> {
        self.step("unclean_notice");
        match self.state.unclean_notice() {
            Ok(Some(text)) => self.err.line(&text),
            Ok(None) => {}
            Err(text) => return Err(Hooks::refusal(plan_id, text, None)),
        }
        self.step("marker");
        if !self.json {
            let bypass = self
                .bypass
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let text = render_marker(plan_id, bypass.as_deref(), self.term);
            let mut out = &self.out;
            let _ = out.write_all(text.as_bytes());
            let _ = out.flush();
            self.marker_done.store(true, Ordering::SeqCst);
        }
        self.step("ensure_reserves");
        match self.state.ensure_reserves() {
            Ok(Some(note)) => self.err.line(&note),
            Ok(None) => {}
            Err(f) => return Err(Hooks::refusal(plan_id, f.text(), Some(f))),
        }
        Ok(())
    }

    fn item_started(&self, n: usize, m: usize, c: &Candidate) {
        let what = match c.locator() {
            Locator::Path(p) | Locator::Steam(_, p) => p.display().to_string(),
            Locator::Docker(d) => d.0.clone(),
        };
        let ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .removing(ms, n, m, &what);
    }
}

impl ApplyHooks for Hooks<'_> {
    fn marker_printed(&self) -> bool {
        self.marker_done.load(Ordering::SeqCst)
    }

    fn stamped(&self, warning: Option<&str>) {
        *self
            .bypass
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = warning.map(str::to_string);
    }
}

/// `--apply` over `w`'s ports, recording each step in `trace`. Returns the exit code.
pub fn compose_apply(
    argv: Vec<OsString>,
    w: &ApplyWiring<'_>,
    out: &Sink,
    err: &Sink,
    trace: &mut Trace,
) -> i32 {
    trace.push("parse_args");
    let cli = match args::parse(argv) {
        Parsed::Run(c) => c,
        Parsed::Exit {
            code,
            stdout,
            stderr,
        } => {
            let _ = (&mut &*out).write_all(stdout.as_bytes());
            let _ = (&mut &*err).write_all(stderr.as_bytes());
            return code;
        }
    };
    if !cli.apply || cli.command.is_some() {
        err.line("error: compose_apply needs a cleaner --apply run. Nothing was deleted.");
        return EXIT_USAGE;
    }
    let p = w.ports;
    let registered = {
        let mut v: Vec<_> = p
            .targets
            .iter()
            .flat_map(|t| t.selectors().iter().copied())
            .collect();
        v.sort();
        v.dedup();
        v
    };
    let sel = match cli.selection(&registered, &KindSelector::default_apply()) {
        Ok(s) => s,
        Err(e) => {
            err.line(&e.0);
            return EXIT_USAGE;
        }
    };
    let mut progress_sink = err.clone();
    let mode = progress_mode(
        p.stdout_tty,
        w.stderr_tty,
        cli.quiet,
        cli.json,
        cli.no_progress,
    );
    let hooks = Hooks {
        trace: Mutex::new(Vec::new()),
        state: w.state,
        out: out.clone(),
        err: err.clone(),
        term: &p.term,
        json: cli.json,
        bypass: Mutex::new(None),
        marker_done: AtomicBool::new(false),
        progress: Mutex::new(Progress::new(mode, &mut progress_sink, 0)),
        started: Instant::now(),
    };
    let (mut o, mut e) = (out.clone(), err.clone());
    let code = app::apply_flow(
        &cli,
        &sel,
        p,
        &mut Io {
            out: &mut o,
            err: &mut e,
        },
        &hooks,
    );
    for step in hooks
        .trace
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        trace.push(step);
    }
    code
}

fn unix_now() -> i64 {
    SystemClock.now_epoch()
}

/// Which kind of apply `run_apply` serves. Both variants go through the one function below: the same
/// confinement, state directory, audit sink, run lock, stamp step and capability mint site. The explorer's
/// variant takes only the [`ConfirmedPlan`] (the typed count bound to the plan the owner reviewed); the
/// per-run switches in [`ExplorerRun`] carry no authority.
pub enum ApplyRequest<'a> {
    /// `dunnage --apply ...`: `argv` is parsed again by [`compose_apply`].
    Cleaner { argv: Vec<OsString>, cli: &'a Cli },
    /// The explorer's `d` after the typed count (Story 6.6.3). Bypasses `parse_args`, `kind_availability` and
    /// the selection (review C4).
    Explorer {
        confirmed: ConfirmedPlan,
        run: ExplorerRun,
    },
}

/// The one place the capability is minted (`scripts/check-token-sites.sh --expect-sites 1`). The cleaner
/// calls it before it builds the registry that deletes through it; the explorer calls it only after its
/// plan-id and stamp gates, so a refused explorer run mints nothing.
fn mint_apply() -> ApplyToken {
    grant_apply()
}

/// Every real port of an apply, owned in one place (the cleaner and the explorer build the same ones).
struct RealApply {
    dirs: Dirs,
    euid: u32,
    paths: StatePaths,
    owner: Option<Owner>,
    probe: RealProbe,
    audit: RealAuditSink,
    plans: RealPlanStore,
    space: Box<dyn dunnage_core::ports::SpaceMeter>,
    runner: Box<dyn dunnage_core::ports::CommandRunner>,
    guard: RealRunGuard,
    confined: bool,
    build: BuildInfo,
}

impl RealApply {
    /// `None` when the confinement allows no runner (a refused run). `guard` is built by the caller because
    /// the cleaner installs the process's signal handlers with it and the explorer must not.
    fn build(
        dirs: &Dirs,
        confinement: &Confine,
        guard: impl FnOnce(&Dirs, Option<Owner>) -> Option<RealRunGuard>,
    ) -> Option<RealApply> {
        let runner = confine::apply_runner(confinement)?;
        let probe = RealProbe;
        let euid = probe.euid();
        let paths = StatePaths {
            dir: dirs.state.clone(),
        };
        let owner = Owner::for_euid(euid, &dirs.state);
        let guard = guard(dirs, owner)?;
        let audit = RealAuditSink::new(paths.audit(), paths.audit_reserve(), owner);
        let plans = RealPlanStore::new(
            StateDir {
                paths: paths.clone(),
                owner,
            },
            Arc::new(unix_now),
        );
        let space = run_meter(RealSpaceMeter::new(probe.platform()));
        Some(RealApply {
            dirs: dirs.clone(),
            euid,
            paths,
            owner,
            probe,
            audit,
            plans,
            space,
            runner,
            guard,
            confined: matches!(confinement, Confine::Root(_)),
            build: build_info(),
        })
    }

    fn env<'a>(&'a self, fs: &'a RealFs, clock: &'a SystemClock) -> Env<'a> {
        Env {
            fs,
            runner: self.runner.as_ref(),
            clock,
            probe: &self.probe,
            space: self.space.as_ref(),
            audit: &self.audit,
            guard: &self.guard,
            plans: &self.plans,
            dirs: self.dirs.clone(),
        }
    }

    /// The stamp step for `gate`; `ungated` is `DUNNAGE_UNGATED` (the explorer passes `None`: it owns the
    /// terminal, so there is no break-glass prompt).
    fn stamp(&self, gate: char, ungated: Option<&str>) -> Result<StampOutcome, GateExit> {
        let installed = read_stamp(&stamp_path(&self.dirs.state));
        let exe_sha256 = current_exe_sha256().unwrap_or_default();
        stamp_gate(
            &StampInputs {
                build: &self.build,
                stamp: installed.as_ref(),
                exe_sha256: &exe_sha256,
                required_gate: gate,
                confined: self.confined,
                ungated_var: ungated,
            },
            &open_controlling_tty,
        )
    }
}

fn run_id_now() -> String {
    format!("{:x}-{:x}", unix_now(), std::process::id())
}

/// The real apply: builds every real port, mints the capability and runs [`compose_apply`] (the cleaner) or
/// [`compose_explorer_apply`] (the explorer).
///
/// # Panics
/// If the capability is asked for twice in one run, which neither sequence does.
pub fn run_apply(request: ApplyRequest<'_>, base: &Dirs, roots: &RootSet) -> i32 {
    let dirs = tool_dirs(base);
    let vars = confine::current_vars();
    let confinement = confine::confine_mode(&vars, build_flavor(), cfg!(debug_assertions));
    match request {
        ApplyRequest::Cleaner { argv, cli } => {
            if let Confine::Refuse(msg) = confinement {
                eprintln!("{msg}");
                return EXIT_USAGE;
            }
            run_cleaner_apply(argv, cli, &dirs, roots, &confinement)
        }
        ApplyRequest::Explorer { confirmed, run } => {
            if let Confine::Refuse(msg) = confinement {
                run.refuse(msg);
                return EXIT_USAGE;
            }
            run_explorer_apply(confirmed, &run, base, &dirs, &confinement)
        }
    }
}

fn run_explorer_apply(
    confirmed: ConfirmedPlan,
    run: &ExplorerRun,
    base: &Dirs,
    dirs: &Dirs,
    confinement: &Confine,
) -> i32 {
    if !crate::wiring::kinds::explorer_selection_registered() {
        run.refuse(EXPLORER_NOT_IN_BUILD);
        return EXIT_USAGE;
    }
    let Some(real) = RealApply::build(dirs, confinement, |d, owner| {
        // No signal handlers here: the explorer's loop owns them and sets `run.abort`.
        Some(RealRunGuard::new(
            run_lock_path(&d.state),
            owner,
            Arc::new(SignalState::new()),
        ))
    }) else {
        run.refuse(EXPLORER_NOT_IN_BUILD);
        return EXIT_USAGE;
    };
    let (fs, clock) = (RealFs, SystemClock);
    let env = real.env(&fs, &clock);
    let read = read_root();
    let stamp = |gate: char| real.stamp(gate, None);
    let mint = mint_apply;
    let ops = |t: &ApplyToken| {
        confine::apply_dir_ops(confinement, t).unwrap_or_else(|| RealDirOps::confined_to(&read))
    };
    let resolve = || crate::wiring::explore::resolve_roots_now(base);
    let sizer = TreestatSizer::new(RealDirOps::confined_to(&read));
    let exe = std::env::current_exe().unwrap_or_default();
    let ports = ExplorerApplyPorts {
        env: &env,
        stamp: &stamp,
        mint: &mint,
        ops: &ops,
        resolve_roots: &resolve,
        own_state: OwnState::canonical(base, exe, &crate::wiring::explore::canonical_lenient),
        sizer: &sizer,
        extents: None,
        build: real.build.clone(),
        run_id: run_id_now(),
        tty: std::io::stdin().is_terminal(),
    };
    let mut trace = Trace::default();
    compose_explorer_apply(confirmed, &ports, run, &mut trace).exit
}

fn run_cleaner_apply(
    args: Vec<OsString>,
    cli: &Cli,
    dirs: &Dirs,
    roots: &RootSet,
    confinement: &Confine,
) -> i32 {
    let read = read_root();
    let fixture_mode = matches!(confinement, Confine::Root(_));

    // Phase one: which selectors may delete in this build? A build with none never mints the capability.
    let read_only_ops = {
        let read = read.clone();
        move || dunnage_adapters::RealDirOps::confined_to(&read)
    };
    let probe_ctx = KindCtx {
        roots: roots.clone(),
        fs_root: read.clone(),
        ops: &read_only_ops,
        fixture_mode,
    };
    let any_capable = !KindRegistry::build(&probe_ctx).apply_capable().is_empty();

    // Phase two: the capability (once), the registry that deletes through it, the real ports.
    let token = any_capable.then(mint_apply);
    let apply_ops = || match &token {
        Some(t) => confine::apply_dir_ops(confinement, t).unwrap_or_else(&read_only_ops),
        None => read_only_ops(),
    };
    let ctx = KindCtx {
        roots: roots.clone(),
        fs_root: read.clone(),
        ops: &apply_ops,
        fixture_mode,
    };
    let registry = KindRegistry::build(&ctx);
    let capable = registry.apply_capable();
    let token_cell = Cell::new(token);
    let mint = || {
        token_cell
            .take()
            .unwrap_or_else(|| panic!("the apply capability is minted once per run"))
    };

    let Some(real) = RealApply::build(dirs, confinement, |d, owner| {
        match install_apply(&d.state, owner) {
            Ok(g) => Some(g),
            Err(e) => {
                eprintln!(
                    "error: cannot install the signal handlers ({e}). Nothing was deleted. Run again."
                );
                None
            }
        }
    }) else {
        return EXIT_FATAL;
    };
    if real.plans.read() == LastPlanRead::Unreadable {
        // Told on stderr why the gate will say "no dry run" (`design/ux.md` 3.5); nothing is created.
        eprintln!("{}", real.plans.unreadable_warning().text());
    }
    let (fs, clock) = (RealFs, SystemClock);
    let env = real.env(&fs, &clock);
    let (paths, owner) = (real.paths.clone(), real.owner);
    let build = real.build.clone();
    let run_id = run_id_now();
    let via = crate::wiring::env::tool_env("VIA").and_then(|v| v.into_string().ok());
    let ungated = crate::wiring::env::tool_env("UNGATED").and_then(|v| v.into_string().ok());
    let stamp = |gate: char| real.stamp(gate, ungated.as_deref());
    let outcome_run_id = run_id.clone();
    let outcome_via = via.clone();
    let outcome = |m: OutcomeMode, verdict: DiskVerdict, free: u64, unalloc: Option<u64>| {
        let line = OutcomeLine {
            schema: OUTCOME_SCHEMA,
            ts: unix_now(),
            run_id: outcome_run_id.clone(),
            mode: m,
            verdict,
            free_bytes: free,
            unallocated_bytes: unalloc,
            via: outcome_via.clone(),
        };
        if let Err(e) = append_outcome(&paths, owner, &line) {
            eprintln!("warning: could not record the run in outcome.jsonl ({e}).");
        }
    };
    let targets = registry.targets();
    let ports = Ports {
        env: &env,
        targets,
        apply: if capable.is_empty() {
            ApplyMode::Disabled
        } else {
            ApplyMode::Granted(&mint)
        },
        apply_capable: Some(&capable),
        stamp: &stamp,
        build,
        roots: roots.run_roots(),
        scan_roots: roots.scan_roots().len(),
        exe: std::env::current_exe().unwrap_or_default(),
        term: terminal::detect(cli.color_choice()),
        stdout_tty: std::io::stdout().is_terminal(),
        run_id,
        via,
        outcome: &outcome,
        reserves_note: None,
        config_roots: None,
    };
    let state = RealApplyState {
        dirs: dirs.clone(),
        euid: real.euid,
    };
    let out = Sink::new(SwallowBrokenPipe::new(std::io::stdout()));
    let err = Sink::new(std::io::stderr());
    let mut trace = Trace::default();
    compose_apply(
        args,
        &ApplyWiring {
            ports: &ports,
            state: &state,
            stderr_tty: std::io::stderr().is_terminal(),
        },
        &out,
        &err,
        &mut trace,
    )
}

/// Whether the command line is a cleaner `--apply`, the only run [`run_apply`] serves.
pub fn is_cleaner_apply(cli: &Cli) -> bool {
    cli.apply && cli.command.is_none()
}
