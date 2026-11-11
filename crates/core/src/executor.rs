//! `Executor` (Story 1.1.4, Task 1.1.4b): discover, plan, gate, lock, audit, execute, settle. Everything
//! the executor touches comes through [`Env`]; it owns the bounded worker pool, the abort rule and the books.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::apply_token::ApplyToken;
use crate::audit_types::{
    BreakGlass, Intent, ItemResult, RunEnd, RunRoots, RunStart, AUDIT_SCHEMA,
};
use crate::candidate::{Candidate, Locator};
use crate::delete_types::{DeleteError, DeleteFailure, Freed};
use crate::ids::CandidateId;
use crate::kind::{Kind, SelectorId};
use crate::outcome::{Outcome, SkipReason, Status};
use crate::plan::{destructive_gate, GateRefusal, GateRenderCtx, Plan, PlanId, PlanItem, Planned};
use crate::plan_state::{BuildInfo, StampSummary};
use crate::ports::{Env, ExtentAttribution, ExtentProbe, Unavailable};
use crate::preflight;
use crate::reasons::FatalError;
use crate::selector::Selection;
use crate::snapshot_types::FsSnapshot;
use crate::summary::{
    DiskVerdict, FinishInput, PreDeleteSample, RunSummary, StageTimings, WalkerCounters,
};
use crate::target::{kind_order, Guarded, KindGate, Target};

/// The `--jobs` default: the worker bound when the flag is absent (Story 3.2.4, benchmark in `spikes.md`).
pub const DEFAULT_JOBS: usize = 4;

/// Fixed points of an apply that the composition root observes (Story 2.3.4). `step` names the point so a
/// test can assert the order of `design/ux.md` 1.4; `committed` runs once the plan-id gate has passed and may
/// still end the run; `item_started` feeds the live `[n/m] removing` progress lines.
pub trait RunHooks: Sync {
    fn step(&self, _name: &'static str) {}

    fn committed(&self, _plan_id: PlanId) -> Result<(), Box<Refused>> {
        Ok(())
    }

    fn item_started(&self, _n: usize, _m: usize, _c: &Candidate) {}
}

/// The hooks of a run that observes nothing.
pub struct NoHooks;

impl RunHooks for NoHooks {}

/// `Plan` cannot execute: there is no token in it. `Apply` carries the capability.
pub enum Mode {
    Plan,
    Apply(ApplyToken),
}

/// The exit code of a run, by class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitClass {
    Ok,
    /// At least one item ended `Error`, or every selected kind was unavailable.
    PartialFailure,
    /// Lock held, audit unwritable or failing, `ENOSPC`.
    Fatal,
    Starved,
    StateUnreadable,
    /// Usage error or a refused gate.
    Usage,
    /// Ended by SIGINT (2) or SIGTERM (15): `128 + signal`.
    Interrupted(i32),
}

impl ExitClass {
    pub fn code(self) -> i32 {
        match self {
            ExitClass::Ok => 0,
            ExitClass::PartialFailure => 1,
            ExitClass::Fatal => 2,
            ExitClass::Starved => 3,
            ExitClass::Usage => 64,
            ExitClass::StateUnreadable => 74,
            ExitClass::Interrupted(sig) => 128 + sig,
        }
    }

    fn of_fatal(e: &FatalError) -> ExitClass {
        match e.exit_code() {
            3 => ExitClass::Starved,
            74 => ExitClass::StateUnreadable,
            _ => ExitClass::Fatal,
        }
    }
}

/// Why the run stopped dispatching items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbortCause {
    NoSpace,
    Interrupted(i32),
    AuditFailed,
}

impl AbortCause {
    /// The note shown with every `Skipped(Aborted)` item.
    pub fn note(self) -> &'static str {
        match self {
            AbortCause::NoSpace => "aborted: ENOSPC",
            AbortCause::Interrupted(_) => "aborted: interrupted",
            AbortCause::AuditFailed => "aborted: audit log write failed",
        }
    }
}

/// The note a guard skip carries when the guard flipped after the scan (`design/ux.md` UX-16).
pub fn changed_since_scan_note(reason: SkipReason) -> String {
    format!("{} (changed since scan)", reason.reason_text())
}

/// What the audit record `run_start` carries besides what the executor knows itself.
#[derive(Debug, Clone)]
pub struct RunMeta {
    pub run_id: String,
    pub build: BuildInfo,
    pub stamp: Option<StampSummary>,
    pub break_glass: Option<BreakGlass>,
    pub roots: RunRoots,
    pub verdict_start: Option<DiskVerdict>,
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    /// `--jobs`: the bound of the worker pool.
    pub jobs: usize,
    pub settle_timeout: Duration,
    /// Where the filesystem is measured; defaults to HOME.
    pub meter_path: Option<PathBuf>,
    pub meta: RunMeta,
}

/// The two `data.bytes_used` samples taken before the first delete, with nothing deleted in between. They
/// feed the concurrent-write cause of the attribution (Task 1.2.2c).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreDeleteSamples {
    pub first: Option<u64>,
    pub second: Option<u64>,
    pub elapsed_secs: i64,
    /// False when under 2 s apart or either sample is missing: the attribution then reads an unmeasured
    /// concurrent-write cause.
    pub measurable: bool,
}

/// The most files the extent probe may open in one run.
const EXTENT_PROBE_BUDGET: usize = 4096;

/// Samples closer than this say nothing about concurrent writers.
const MIN_SAMPLE_GAP_SECS: i64 = 2;

impl PreDeleteSamples {
    fn new(first: Option<u64>, second: Option<u64>, elapsed_secs: i64) -> PreDeleteSamples {
        PreDeleteSamples {
            first,
            second,
            elapsed_secs,
            measurable: first.is_some() && second.is_some() && elapsed_secs >= MIN_SAMPLE_GAP_SECS,
        }
    }

    fn as_samples(&self) -> Vec<PreDeleteSample> {
        match (self.first, self.second) {
            (Some(a), Some(b)) => vec![
                PreDeleteSample {
                    at_ms: 0,
                    data_bytes_used: a,
                },
                PreDeleteSample {
                    at_ms: u64::try_from(self.elapsed_secs.max(0)).unwrap_or(0) * 1000,
                    data_bytes_used: b,
                },
            ],
            _ => Vec::new(),
        }
    }
}

/// A run that ended before any item was touched: a refused gate, an unavailable kind, a lock or audit failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub exit: ExitClass,
    /// Availability lines, printed first.
    pub notes: Vec<String>,
    pub text: String,
    pub refusal: Option<GateRefusal>,
    /// The plan-id of the run, known once the plan was built (every refusal after step 4).
    pub plan_id: Option<PlanId>,
    /// The run-level error when that is what refused the run.
    pub fatal: Option<FatalError>,
}

#[derive(Debug)]
pub struct RunReport {
    pub summary: RunSummary,
    pub plan: Plan,
    pub plan_id: PlanId,
    pub exit: ExitClass,
    /// Availability lines and post-kind notes.
    pub notes: Vec<String>,
    pub abort: Option<AbortCause>,
    pub fatal: Option<FatalError>,
    pub pre_delete: PreDeleteSamples,
    /// Items whose guard failed at delete time: counted as `skipped by guard`, not as errors.
    pub skipped_by_guard: Vec<CandidateId>,
}

pub struct Executor<'a> {
    env: &'a Env<'a>,
    targets: Vec<&'a dyn Target>,
    opts: RunOptions,
    extents: Option<&'a dyn ExtentProbe>,
}

struct Shared<'t> {
    token: &'t ApplyToken,
    run_id: &'t str,
    abort: Mutex<Option<AbortCause>>,
    audit_errno: Mutex<Option<i32>>,
    guard_skips: Mutex<Vec<CandidateId>>,
}

/// What `availability` found: one gate per active target, the once-per-kind lines, the unreachable selectors.
struct Availability {
    gates: Vec<(&'static str, KindGate)>,
    notes: Vec<String>,
    unavailable: Vec<SelectorId>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn path_of(c: &Candidate) -> Option<PathBuf> {
    match c.locator() {
        Locator::Path(p) | Locator::Steam(_, p) => Some(p.clone()),
        Locator::Docker(_) => None,
    }
}

fn outcome(
    env: &Env,
    run_id: &str,
    c: &Candidate,
    status: Status,
    freed: Freed,
    started: Instant,
) -> Outcome {
    let attributed = freed.attributed_bytes.0;
    Outcome {
        ts: env.clock.now_epoch(),
        run_id: run_id.to_string(),
        kind: c.kind(),
        selector: c.selector(),
        id: c.id(),
        path: path_of(c),
        measured_bytes: (status == Status::Removed || attributed > 0).then_some(attributed),
        status,
        est_bytes: c.est_bytes().0,
        freed,
        elapsed_ms: millis(started),
        trimmed_bytes: None,
    }
}

fn skip_outcome(env: &Env, run_id: &str, item: &PlanItem, reason: SkipReason) -> Outcome {
    outcome(
        env,
        run_id,
        &item.candidate,
        Status::Skipped(reason),
        Freed::default(),
        Instant::now(),
    )
}

fn status_of_failure(e: &DeleteError) -> Status {
    match e.skip_reason() {
        Some(r) => Status::Skipped(r),
        None => Status::Error(e.text()),
    }
}

fn is_no_space(e: &DeleteError) -> bool {
    matches!(e, DeleteError::NoSpace | DeleteError::Io(28))
}

/// The daemon-side bytes the removed Docker items report (their `measured_bytes` is the `docker system df`
/// before/after delta for the build cache and the listed size for an image); `None` when no Docker item was
/// removed, so the summary keeps the cause `unmeasured` instead of printing 0.
fn docker_delta(outcomes: &[Outcome]) -> Option<u64> {
    let removed = outcomes.iter().filter(|o| {
        o.status == Status::Removed
            && matches!(
                o.kind,
                Kind::DockerImage | Kind::DockerBuildCache | Kind::DockerVolume
            )
    });
    removed
        .map(|o| o.measured_bytes.unwrap_or(o.freed.attributed_bytes.0))
        .reduce(u64::saturating_add)
}

fn data_bytes_used(s: &FsSnapshot) -> Option<u64> {
    s.btrfs.map(|b| b.data.bytes_used)
}

fn millis(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn tilde(p: &Path, home: &Path) -> String {
    match p.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => p.display().to_string(),
    }
}

fn refused_fatal(e: &FatalError, notes: &[String], plan_id: PlanId) -> Box<Refused> {
    Box::new(Refused {
        exit: ExitClass::of_fatal(e),
        notes: notes.to_vec(),
        text: e.text(),
        refusal: None,
        plan_id: Some(plan_id),
        fatal: Some(e.clone()),
    })
}

fn audit_unwritable(env: &Env, errno: i32) -> FatalError {
    FatalError::AuditUnwritable {
        path: env.dirs.state.join("audit.jsonl"),
        errno,
    }
}

impl<'a> Executor<'a> {
    pub fn new(env: &'a Env<'a>, targets: Vec<&'a dyn Target>, opts: RunOptions) -> Executor<'a> {
        Executor {
            env,
            targets,
            opts,
            extents: None,
        }
    }

    /// Shared-extent attribution for the removed set (ADR-004). Without a probe the summary says the
    /// extents were not measured.
    #[must_use]
    pub fn with_extent_probe(mut self, probe: &'a dyn ExtentProbe) -> Executor<'a> {
        self.extents = Some(probe);
        self
    }

    fn extent_attribution(&self, outcomes: &[Outcome]) -> ExtentAttribution {
        let Some(probe) = self.extents else {
            return ExtentAttribution::Unavailable(Unavailable::NotBtrfs);
        };
        let removed: Vec<&Path> = outcomes
            .iter()
            .filter(|o| o.status == Status::Removed)
            .filter_map(|o| o.path.as_deref())
            .collect();
        probe.attribute(&removed, EXTENT_PROBE_BUDGET)
    }

    fn target_for(&self, c: &Candidate) -> Option<&'a dyn Target> {
        self.targets
            .iter()
            .copied()
            .find(|t| t.selectors().contains(&c.selector()))
    }

    fn meter_path(&self) -> &Path {
        self.opts
            .meter_path
            .as_deref()
            .unwrap_or(self.env.dirs.home.as_path())
    }

    /// Runs a plan (`Mode::Plan`) or an apply. In apply, the order is: availability, the plan-id gate, the
    /// run lock, the audit `run_start`, the deletes, settle, `run_end`. Nothing is deleted, locked or logged
    /// before the gate passes.
    pub fn run(
        &self,
        sel: &Selection,
        mode: &Mode,
        supplied: Option<PlanId>,
    ) -> Result<RunReport, Box<Refused>> {
        self.run_hooked(sel, mode, supplied, &NoHooks)
    }

    /// [`run`](Self::run) with the composition root watching the fixed points of an apply.
    pub fn run_hooked(
        &self,
        sel: &Selection,
        mode: &Mode,
        supplied: Option<PlanId>,
        hooks: &dyn RunHooks,
    ) -> Result<RunReport, Box<Refused>> {
        let env = self.env;
        let applying = matches!(mode, Mode::Apply(_));
        let discover_start = Instant::now();
        let fs_before = env.space.snapshot(self.meter_path());
        let t0 = env.clock.now_epoch();

        let mut avail = self.availability(sel, applying);
        if applying && every_selector_down(sel, &avail) {
            return Err(all_down_refusal(avail));
        }

        let mut candidates: Vec<Candidate> = Vec::new();
        for t in self.targets.iter().copied() {
            let selected = t.selectors().iter().any(|s| sel.selects(*s));
            let down = if selected {
                avail.is_unavailable(t)
            } else {
                // An unselected kind is still listed (its items are `RISK` rows), unless it is down.
                t.gate(env).is_unavailable()
            };
            if !down {
                candidates.extend(t.discover(env, sel));
            }
        }
        if let Some(refused) = only_refusal(sel, &candidates, &avail) {
            return Err(refused);
        }
        for t in self.targets.iter().copied() {
            let selected = t.selectors().iter().any(|s| sel.selects(*s));
            if selected && !avail.is_unavailable(t) {
                avail.notes.extend(t.plan_notes(env, sel));
            }
        }
        let discover_ms = millis(discover_start);
        let plan_start = Instant::now();
        let now = env.clock.now_epoch();
        let plan = Plan::build(sel, now, candidates, &|c| self.plan_guard(c, &avail));
        let plan_ms = millis(plan_start);
        let plan_id = PlanId::for_selection(&plan, sel);

        let Mode::Apply(token) = mode else {
            let stage = (discover_ms, plan_ms);
            return Ok(self.plan_report(plan, plan_id, fs_before, avail.notes, stage));
        };

        hooks.step("plan_id_gate");
        self.gate(sel, supplied, &plan, plan_id, &avail)?;
        hooks.committed(plan_id)?;
        let notes = avail.notes;
        hooks.step("run_lock");
        if let Err(e) = env.guard.acquire() {
            return Err(refused_fatal(&e, &notes, plan_id));
        }
        let run_id = self.opts.meta.run_id.clone();
        let start = RunStart {
            schema: AUDIT_SCHEMA,
            run_id: run_id.clone(),
            ts: env.clock.now_epoch(),
            euid: env.probe.euid(),
            kinds: sel.kinds_resolved(),
            plan_id: Some(plan_id.to_string()),
            build: self.opts.meta.build.clone(),
            stamp: self.opts.meta.stamp.clone(),
            break_glass: self.opts.meta.break_glass.clone(),
            roots: self.opts.meta.roots.clone(),
            verdict_start: self.opts.meta.verdict_start,
        };
        hooks.step("audit_open");
        if let Err(e) = env.audit.run_start(&start) {
            let fatal = audit_unwritable(env, e.raw_os_error().unwrap_or(5));
            return Err(refused_fatal(&fatal, &notes, plan_id));
        }
        hooks.step("starvation_preflight");
        let selected_kinds: Vec<Kind> = sel
            .kinds()
            .ids()
            .flat_map(|s| s.kinds().iter().copied())
            .collect();
        if let Some(fatal) = preflight::check(&fs_before, &selected_kinds).fatal() {
            // The log already holds this run's `run_start`; close it, or the next run would warn that this one
            // was killed.
            let _ = env.audit.run_end(&RunEnd {
                run_id: run_id.clone(),
                ts: env.clock.now_epoch(),
                verdict_after: DiskVerdict::Unknown,
                attributed_bytes: 0,
                measured_drop: None,
                attribution_pct: None,
                causes: Vec::new(),
                explained: false,
                interrupted: false,
            });
            return Err(refused_fatal(&fatal, &notes, plan_id));
        }
        hooks.step("header_rest");

        let second = env.space.snapshot(self.meter_path());
        let pre_delete = PreDeleteSamples::new(
            data_bytes_used(&fs_before),
            data_bytes_used(&second),
            env.clock.now_epoch() - t0,
        );

        let exec_start = Instant::now();
        let shared = Shared {
            token,
            run_id: &run_id,
            abort: Mutex::new(None),
            audit_errno: Mutex::new(None),
            guard_skips: Mutex::new(Vec::new()),
        };
        hooks.step("execute");
        let outcomes = self.execute_all(&plan, &shared, hooks);
        let execute_ms = millis(exec_start);

        let settle_start = Instant::now();
        let settled = env
            .space
            .settle(self.meter_path(), self.opts.settle_timeout);
        let fs_after = env.space.snapshot(self.meter_path());
        let settle_ms = settled.waited_ms.max(millis(settle_start));

        let abort = *lock(&shared.abort);
        let mut notes = notes;
        for t in &self.targets {
            let mine: Vec<Outcome> = outcomes
                .iter()
                .filter(|o| t.selectors().contains(&o.selector))
                .cloned()
                .collect();
            notes.extend(t.finish(&mine, env));
        }
        let extents = self.extent_attribution(&outcomes);
        let docker_delta_bytes = docker_delta(&outcomes);
        let summary = RunSummary::finish(FinishInput {
            fs_before,
            fs_after: Some(fs_after),
            settled: Some(settled),
            outcomes,
            extents,
            pre_delete: pre_delete.as_samples(),
            delete_window_ms: execute_ms + settle_ms,
            held: Vec::new(),
            snapshots: None,
            docker_delta_bytes,
            verdict_after: DiskVerdict::Unknown,
            timings: StageTimings {
                discover_ms,
                plan_ms,
                execute_ms,
                settle_ms,
            },
            walker: WalkerCounters::default(),
            notes: Vec::new(),
        });
        let end = RunEnd {
            run_id: run_id.clone(),
            ts: env.clock.now_epoch(),
            verdict_after: summary.verdict_after,
            attributed_bytes: summary.attributed_bytes,
            measured_drop: summary.measured_drop,
            attribution_pct: summary.attribution_pct,
            causes: summary.causes.clone(),
            explained: summary.explained,
            interrupted: matches!(abort, Some(AbortCause::Interrupted(_))),
        };
        hooks.step("run_end");
        let end_errno = env
            .audit
            .run_end(&end)
            .err()
            .map(|e| e.raw_os_error().unwrap_or(5));
        let audit_errno = lock(&shared.audit_errno).take().or(end_errno);
        let fatal = audit_errno.map(|errno| audit_unwritable(env, errno));

        let exit = match (abort, &fatal) {
            (Some(AbortCause::Interrupted(sig)), _) => ExitClass::Interrupted(sig),
            (Some(_), _) | (_, Some(_)) => ExitClass::Fatal,
            _ if summary
                .outcomes
                .iter()
                .any(|o| matches!(o.status, Status::Error(_))) =>
            {
                ExitClass::PartialFailure
            }
            _ => ExitClass::Ok,
        };
        let skipped_by_guard = std::mem::take(&mut *lock(&shared.guard_skips));
        Ok(RunReport {
            summary,
            plan,
            plan_id,
            exit,
            notes,
            abort,
            fatal,
            pre_delete,
            skipped_by_guard,
        })
    }

    fn plan_guard(&self, c: &Candidate, avail: &Availability) -> Guarded {
        let Some(t) = self.target_for(c) else {
            return Guarded::Skip(SkipReason::ToolMissing);
        };
        match avail.gate_of(t) {
            Some(KindGate::ListOnly(r)) => Guarded::Skip(r),
            _ => t.guard(c, self.env),
        }
    }

    fn availability(&self, sel: &Selection, applying: bool) -> Availability {
        availability_of(self.env, &self.targets, sel, applying)
    }

    fn gate(
        &self,
        sel: &Selection,
        supplied: Option<PlanId>,
        plan: &Plan,
        plan_id: PlanId,
        avail: &Availability,
    ) -> Result<(), Box<Refused>> {
        let env = self.env;
        let saved = env.plans.load_last();
        let keys = plan.keys(sel);
        let now = env.clock.now_epoch();
        let Err(refusal) = destructive_gate(sel, supplied, saved.as_ref(), plan_id, &keys, now)
        else {
            return Ok(());
        };
        let free = env.space.snapshot(&env.dirs.state).avail_bytes;
        let ctx = GateRenderCtx {
            selection: sel,
            saved: saved.as_ref(),
            euid: env.probe.euid(),
            state_dir_low: (free < 1 << 20).then(|| (tilde(&env.dirs.state, &env.dirs.home), free)),
            unavailable: avail.unavailable.clone(),
            home: Some(env.dirs.home.as_path()),
        };
        Err(Box::new(Refused {
            exit: ExitClass::Usage,
            notes: avail.notes.clone(),
            text: refusal.render(&ctx),
            refusal: Some(refusal),
            plan_id: Some(plan_id),
            fatal: None,
        }))
    }

    fn plan_report(
        &self,
        plan: Plan,
        plan_id: PlanId,
        fs_before: FsSnapshot,
        notes: Vec<String>,
        (discover_ms, plan_ms): (u64, u64),
    ) -> RunReport {
        let run_id = &self.opts.meta.run_id;
        let outcomes: Vec<Outcome> = plan
            .items
            .iter()
            .filter_map(|i| match i.planned {
                Planned::WillNot(r) => Some(skip_outcome(self.env, run_id, i, r)),
                Planned::Will => None,
            })
            .collect();
        let summary = RunSummary::finish(FinishInput {
            fs_before,
            fs_after: None,
            settled: None,
            outcomes,
            extents: ExtentAttribution::Unavailable(Unavailable::NotBtrfs),
            pre_delete: Vec::new(),
            delete_window_ms: 0,
            held: Vec::new(),
            snapshots: None,
            docker_delta_bytes: None,
            verdict_after: DiskVerdict::Unknown,
            timings: StageTimings {
                discover_ms,
                plan_ms,
                execute_ms: 0,
                settle_ms: 0,
            },
            walker: WalkerCounters::default(),
            notes: Vec::new(),
        });
        RunReport {
            summary,
            plan,
            plan_id,
            exit: ExitClass::Ok,
            notes,
            abort: None,
            fatal: None,
            pre_delete: PreDeleteSamples::new(None, None, 0),
            skipped_by_guard: Vec::new(),
        }
    }

    /// Items run kind by kind, in parallel inside a kind (bounded by `--jobs`); outcomes come back in plan
    /// order. Items the plan already skipped become `Skipped` outcomes without an audit line.
    fn execute_all(&self, plan: &Plan, shared: &Shared, hooks: &dyn RunHooks) -> Vec<Outcome> {
        let slots: Mutex<Vec<Option<Outcome>>> = Mutex::new(vec![None; plan.items.len()]);
        let mut groups: Vec<Vec<usize>> = Vec::new();
        for (idx, item) in plan.items.iter().enumerate() {
            match item.planned {
                Planned::WillNot(r) => {
                    lock(&slots)[idx] = Some(skip_outcome(self.env, shared.run_id, item, r));
                }
                Planned::Will => {
                    let order = kind_order(item.candidate.kind());
                    match groups.last_mut() {
                        Some(g) if kind_order(plan.items[g[0]].candidate.kind()) == order => {
                            g.push(idx);
                        }
                        _ => groups.push(vec![idx]),
                    }
                }
            }
        }
        let total: usize = groups.iter().map(Vec::len).sum();
        let started = AtomicUsize::new(0);
        for group in &groups {
            let next = AtomicUsize::new(0);
            let workers = self.opts.jobs.clamp(1, group.len());
            std::thread::scope(|s| {
                for _ in 0..workers {
                    s.spawn(|| {
                        while let Some(&idx) = group.get(next.fetch_add(1, Ordering::SeqCst)) {
                            let n = started.fetch_add(1, Ordering::SeqCst) + 1;
                            hooks.item_started(n, total, &plan.items[idx].candidate);
                            let o = self.run_one(&plan.items[idx], shared);
                            lock(&slots)[idx] = Some(o);
                        }
                    });
                }
            });
        }
        slots
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .into_iter()
            .flatten()
            .collect()
    }

    fn abort_cause(&self, shared: &Shared) -> Option<AbortCause> {
        if let Some(c) = *lock(&shared.abort) {
            return Some(c);
        }
        self.env
            .guard
            .abort_flag()
            .is_set()
            .then(|| AbortCause::Interrupted(self.env.guard.interrupted().unwrap_or(2)))
    }

    fn set_abort(&self, shared: &Shared, cause: AbortCause) {
        lock(&shared.abort).get_or_insert(cause);
        self.env.guard.abort_flag().set();
    }

    /// One item: abort check, guard again, audit intent, execute, audit result.
    fn run_one(&self, item: &PlanItem, shared: &Shared) -> Outcome {
        let env = self.env;
        let c = &item.candidate;
        let run_id = shared.run_id;
        if let Some(cause) = self.abort_cause(shared) {
            lock(&shared.abort).get_or_insert(cause);
            return skip_outcome(env, run_id, item, SkipReason::Aborted);
        }
        let Some(target) = self.target_for(c) else {
            return skip_outcome(env, run_id, item, SkipReason::ToolMissing);
        };
        if let Guarded::Skip(r) = target.guard_before_delete(c, env) {
            lock(&shared.guard_skips).push(c.id());
            return skip_outcome(env, run_id, item, r);
        }
        let intent = Intent::of(c, run_id, env.clock.now_epoch());
        if let Err(e) = env.audit.intent(&intent) {
            *lock(&shared.audit_errno) = Some(e.raw_os_error().unwrap_or(5));
            self.set_abort(shared, AbortCause::AuditFailed);
            return skip_outcome(env, run_id, item, SkipReason::Aborted);
        }
        let started = Instant::now();
        let out = match target.execute(c, env, shared.token) {
            Ok(freed) if target.left_running(c) => {
                if let Some(cause) = self.abort_cause(shared) {
                    lock(&shared.abort).get_or_insert(cause);
                }
                outcome(env, run_id, c, Status::Running, freed, started)
            }
            Ok(freed) => {
                let mut o = outcome(env, run_id, c, Status::Removed, freed, started);
                o.trimmed_bytes = target.trimmed_bytes(c);
                o
            }
            Err(DeleteFailure { error, freed }) => {
                if is_no_space(&error) {
                    self.set_abort(shared, AbortCause::NoSpace);
                }
                if error.skip_reason().is_some() {
                    lock(&shared.guard_skips).push(c.id());
                }
                outcome(env, run_id, c, status_of_failure(&error), freed, started)
            }
        };
        if let Err(e) = env.audit.result(&ItemResult(out.clone())) {
            *lock(&shared.audit_errno) = Some(e.raw_os_error().unwrap_or(5));
            self.set_abort(shared, AbortCause::AuditFailed);
        }
        out
    }
}

/// One gate per target that serves a selected selector, the once-per-kind lines, and the selectors that are
/// unreachable now.
fn availability_of(
    env: &Env,
    targets: &[&dyn Target],
    sel: &Selection,
    applying: bool,
) -> Availability {
    let mut a = Availability {
        gates: Vec::new(),
        notes: Vec::new(),
        unavailable: Vec::new(),
    };
    for t in targets
        .iter()
        .copied()
        .filter(|t| t.selectors().iter().any(|s| sel.selects(*s)))
    {
        let g = t.gate(env);
        let detail = t.unavailable_detail(env);
        if let Some(line) = g.skipped_line(t.name(), detail.as_deref(), applying) {
            a.notes.push(line);
            a.unavailable
                .extend(t.selectors().iter().copied().filter(|s| sel.selects(*s)));
        }
        a.gates.push((t.name(), g));
    }
    a
}

/// `--only PATH` must name, byte for byte, an item of the selected kinds (`design/ux.md` 1.1); anything else
/// ends the run with exit 64 before a plan-id is computed.
fn only_refusal(
    sel: &Selection,
    candidates: &[Candidate],
    avail: &Availability,
) -> Option<Box<Refused>> {
    let missing = sel.only.iter().find(|o| {
        !candidates
            .iter()
            .any(|c| sel.selects(c.selector()) && path_of(c).as_deref() == Some(o.as_path()))
    })?;
    let kinds = sel.kinds_resolved();
    Some(Box::new(Refused {
        exit: ExitClass::Usage,
        notes: avail.notes.clone(),
        text: format!(
            "error: --only {} is not an item of the selected kinds ({kinds}). It may be new, under the age floor, kept as the newest duplicate, or not a cleanable item at all, or spelled differently from the path the cleaner prints (no symlinks, no '.' or '..', no trailing slash); dry run without --only to list the items. Nothing was deleted.",
            crate::shell_quote::shell_quote(missing.as_os_str(), false)
        ),
        refusal: None,
        plan_id: None,
        fatal: None,
    }))
}

fn every_selector_down(sel: &Selection, a: &Availability) -> bool {
    !sel.kinds().is_empty() && sel.kinds().ids().all(|s| a.unavailable.contains(&s))
}

fn all_down_refusal(a: Availability) -> Box<Refused> {
    Box::new(Refused {
        exit: ExitClass::PartialFailure,
        text: a.notes.join("\n"),
        notes: a.notes,
        refusal: None,
        plan_id: None,
        fatal: None,
    })
}

/// Step 2 of `design/ux.md` 1.4: `Some(refusal)` when every selected selector is unavailable now (exit 1, no
/// plan-id error, no marker).
pub fn availability_refusal(
    env: &Env,
    targets: &[&dyn Target],
    sel: &Selection,
) -> Option<Box<Refused>> {
    let a = availability_of(env, targets, sel, true);
    every_selector_down(sel, &a).then(|| all_down_refusal(a))
}

impl Availability {
    fn gate_of(&self, t: &dyn Target) -> Option<KindGate> {
        self.gates
            .iter()
            .find(|(n, _)| *n == t.name())
            .map(|(_, g)| *g)
    }

    fn is_unavailable(&self, t: &dyn Target) -> bool {
        self.gate_of(t).is_some_and(|g| g.is_unavailable())
    }
}

#[cfg(test)]
mod docker_delta_tests {
    use super::*;

    fn out(kind: Kind, status: Status, measured: Option<u64>) -> Outcome {
        let locator = Locator::Docker(crate::DockerId(format!("{kind:?}")));
        Outcome {
            ts: 0,
            run_id: "r".into(),
            kind,
            selector: SelectorId::Docker,
            id: crate::CandidateId::new(kind, &locator),
            path: None,
            status,
            est_bytes: 0,
            measured_bytes: measured,
            freed: Freed::default(),
            elapsed_ms: 0,
            trimmed_bytes: None,
        }
    }

    #[test]
    fn docker_delta_should_sum_removed_docker_items_and_ignore_everything_else() {
        let outcomes = [
            out(Kind::DockerBuildCache, Status::Removed, Some(18_100)),
            out(Kind::DockerImage, Status::Removed, Some(900)),
            out(Kind::DockerImage, Status::Error("x".into()), Some(5_000)),
            out(Kind::Cache, Status::Removed, Some(7_000)),
        ];
        assert_eq!(docker_delta(&outcomes), Some(19_000));
    }

    #[test]
    fn docker_delta_should_be_none_When_no_docker_item_was_removed() {
        let outcomes = [out(Kind::Cache, Status::Removed, Some(7_000))];
        assert_eq!(docker_delta(&outcomes), None);
        assert_eq!(docker_delta(&[]), None);
    }
}
