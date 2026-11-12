//! The explorer's apply (Task 6.6.3b, ADR-006 section 4 and amendments A3, A5, A7): the sequence that turns a
//! [`ConfirmedPlan`] into deletions, written once and reached from `wiring::compose::run_apply` through
//! `ApplyRequest::Explorer`. This file never mints the capability: `compose` hands it a `mint` closure that
//! it calls after the plan-id and stamp gates, and `check-token-sites.sh --expect-sites 1` fails the build if
//! a second `grant_apply` appears anywhere else.
//!
//! The order (each name is pushed onto the [`Trace`]):
//!
//! 1. `check_plan_id`  : `plan::check_plan_id` over the marks' fresh `lstat` identities and the roots resolved now (a changed mark or root refuses the whole run)
//! 2. `check_stamp`    : the gate stamp for gate D
//! 3. `run_lock`       : the run lock, non-blocking
//! 4. `audit_open`     : `run_start`; then `explorer_confirm` (synced)
//! 5. `remeasure`      : total more than 10% or 1 GiB above the confirmed one ends the run, `run_end ... refused`
//! 6. `resolve_roots`  : the roots resolved again, now
//! 7. `mint`           : the capability, as late as possible: every refusal above leaves none in existence
//! 8. `execute`        : per item: M1 to M5 again, fd-relative open, `fstat` against the planned identity,
//!    `intent`, `delete_tree`, `result`
//! 9. `settle`, 10. `run_end`
//!
//! Steps 2, 4, 5 (`run_start`), 8 and 10 are the cleaner's `check_stamp`, `run_lock`, `audit_open`, `execute`
//! and `run_end` of `wiring/compose.rs:1-8` and run the same ports; step 1 is the explorer's form of the
//! cleaner's `plan_id_gate` (the same `destructive_gate`). The cleaner's `parse_args`, `kind_availability`,
//! `unclean_notice`, `marker`, `ensure_reserves`, `starvation_preflight`, `header_rest`, `summary` and `closing`
//! have no counterpart: the explorer bypasses the argument parser and the selection, the state directory was
//! opened when the explorer started, and the screen (Story 6.6.4) prints the result.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use disk_cleanup_adapters::dirops::DirOps;
use disk_cleanup_adapters::signals::term_signal;
use disk_cleanup_adapters::targets::explorer_selection::{candidate_of, ExplorerSelectionTarget};
use disk_cleanup_core::audit_types::{ConfirmedItem, ExplorerConfirm};
use disk_cleanup_core::explore::confirm::{ConfirmedPlan, ReviewedPlan};
use disk_cleanup_core::explore::mark_check::OwnState;
use disk_cleanup_core::explore::msg::{
    DeleteItemOutcome, DeleteItemResult, DeleteMsg, DeleteSummary,
};
use disk_cleanup_core::explorer_plan::{measure_attributed, TreeSizer};
use disk_cleanup_core::plan::{check_plan_id, ExplorerGateRefusal, ItemIdentity, PlanId};
use disk_cleanup_core::plan_state::{BuildInfo, SaveOutcome};
use disk_cleanup_core::ports::{Env, ExtentProbe};
use disk_cleanup_core::roots::{strictly_below, RootSet};
use disk_cleanup_core::selector::Selection;
use disk_cleanup_core::target::{Guarded, Target};
use disk_cleanup_core::{
    ApplyToken, Candidate, DeleteError, DeleteFailure, DiskVerdict, FatalError, Freed, Intent,
    ItemResult, Locator, Outcome, RunEnd, RunStart, SkipReason, Status, AUDIT_SCHEMA,
};

use crate::units;
use crate::wiring::audit::{GateExit, StampOutcome};
use crate::wiring::compose::Trace;

/// The gate the explorer's kind needs.
const REQUIRED_GATE: char = 'D';

/// What `d` says in a build that does not register `explorer-selection` (every build but Gate D).
pub const NOT_IN_BUILD: &str = disk_cleanup_core::explore::ports::DELETE_NOT_IN_BUILD;

pub const STAMP_REFUSED: &str =
    "Deleting is disabled in this build: run 'make gate' and 'make install-gated'. Nothing was deleted.";

/// A re-measured total more than this fraction above the confirmed one refuses the delete.
const GROWTH_FRACTION_DIVISOR: u64 = 10;
/// A re-measured total more than this many bytes above the confirmed one refuses the delete.
const GROWTH_BYTES: u64 = 1 << 30;

/// The error row of a tree that the abort flag stopped after it had removed something.
const STOPPED_PARTLY: &str = "stopped by a signal; the directory is partly removed";

const EXIT_REFUSED: i32 = 64;
const EXIT_FATAL: i32 = 2;
const EXIT_PARTIAL: i32 = 1;

/// Whether the tree grew enough since the owner confirmed that the typed count no longer covers it.
pub fn grew_too_much(then: u64, now: u64) -> bool {
    let grown = now.saturating_sub(then);
    grown > then / GROWTH_FRACTION_DIVISOR || grown > GROWTH_BYTES
}

/// `Changed since you confirmed: 40.1 GiB then, 55.0 GiB now. Nothing was deleted. Press d to review again.`
pub fn changed_text(then: u64, now: u64) -> String {
    format!(
        "Changed since you confirmed: {} then, {} now. Nothing was deleted. Press d to review again.",
        units::size(then),
        units::size(now)
    )
}

/// The reason text of a skipped item as the result list shows it.
pub fn skip_text(r: SkipReason) -> String {
    match r {
        SkipReason::IdentityChanged => "changed since the plan".to_string(),
        other => other.reason_text().to_string(),
    }
}

/// The exit code of a run ended by `sig`: the explorer folds SIGHUP and SIGQUIT into 143 as its views do.
pub fn exit_for_signal(sig: i32) -> i32 {
    match sig {
        2 => 130,
        1 | 3 | 15 => 143,
        other => 128 + other,
    }
}

/// What the caller of the explorer's apply controls besides the confirmed plan.
pub struct ExplorerRun {
    /// Set by the delete key's owner (the loop, on Esc or a signal); the apply stops at the next entry.
    pub abort: Arc<AtomicBool>,
    /// Where the per-item messages go (the TUI's `DeleteHandle`); `None` for a headless run.
    pub events: Option<SyncSender<DeleteMsg>>,
}

impl ExplorerRun {
    pub fn headless() -> ExplorerRun {
        ExplorerRun {
            abort: Arc::new(AtomicBool::new(false)),
            events: None,
        }
    }

    /// A whole-run refusal before anything ran: nothing was deleted.
    pub fn refuse(&self, text: &str) {
        self.send(DeleteMsg::Refused(text.to_string()));
    }

    fn send(&self, m: DeleteMsg) {
        if let Some(tx) = &self.events {
            let _ = tx.send(m);
        }
    }
}

/// Every port the sequence reads, so a test drives it over fakes and the real wiring supplies the real ones.
pub struct ExplorerApplyPorts<'a, D: DirOps> {
    pub env: &'a Env<'a>,
    /// The stamp step for the gate the kind needs (no break-glass prompt: the explorer owns the terminal).
    pub stamp: &'a dyn Fn(char) -> Result<StampOutcome, GateExit>,
    /// Mints the capability. Called once, after the plan-id and stamp gates.
    pub mint: &'a dyn Fn() -> ApplyToken,
    /// The directory operations that delete; built from the capability after it exists.
    pub ops: &'a dyn Fn(&ApplyToken) -> D,
    /// `RootSet::resolve_on` again, now; the plan's roots are never trusted.
    pub resolve_roots: &'a dyn Fn() -> Result<RootSet, String>,
    pub own_state: OwnState,
    pub sizer: &'a dyn TreeSizer,
    pub extents: Option<&'a dyn ExtentProbe>,
    pub build: BuildInfo,
    pub run_id: String,
    /// The count was read from a terminal (self-reported in `explorer_confirm`).
    pub tty: bool,
}

/// How the apply ended.
#[derive(Debug, Clone, PartialEq)]
pub struct ExplorerReport {
    pub exit: i32,
    /// The whole-run refusal text (nothing was deleted), `None` once the first item was attempted.
    pub refused: Option<String>,
    pub outcomes: Vec<Outcome>,
    pub summary: DeleteSummary,
}

impl ExplorerReport {
    fn refused(exit: i32, text: String) -> ExplorerReport {
        ExplorerReport {
            exit,
            refused: Some(text),
            outcomes: Vec::new(),
            summary: DeleteSummary::default(),
        }
    }
}

fn refuse(run: &ExplorerRun, exit: i32, text: String) -> ExplorerReport {
    run.send(DeleteMsg::Refused(text.clone()));
    ExplorerReport::refused(exit, text)
}

fn lock_text(e: &FatalError) -> String {
    match e {
        FatalError::LockHeld { pid } => {
            format!("another disk-cleanup apply is running (pid {pid}). Nothing was deleted.")
        }
        other => other.text(),
    }
}

/// Why the marked set itself cannot be deleted: the `--only` limits (M8) or one mark inside another (M6).
fn set_refusal(sel: &Selection) -> Option<String> {
    if let Err(e) = sel.validate_only() {
        return Some(format!("{}. Nothing was deleted.", e.0));
    }
    let sorted = sel.only_sorted();
    sorted.iter().find_map(|inner| {
        sorted
            .iter()
            .find(|o| strictly_below(o, inner))
            .map(|outer| {
                format!(
                    "error: {} is inside {}, which is also marked. Nothing was deleted.",
                    disk_cleanup_core::escape::escape_os(inner.as_os_str(), false),
                    disk_cleanup_core::escape::escape_os(outer.as_os_str(), false)
                )
            })
    })
}

/// The state of the run that the closing lines need.
struct Books<'a> {
    w_env: &'a Env<'a>,
    run_id: &'a str,
    outcomes: Vec<Outcome>,
    audit_errno: Option<i32>,
}

impl Books<'_> {
    fn record(&mut self, o: Outcome) {
        if let Err(e) = self.w_env.audit.result(&ItemResult(o.clone())) {
            self.audit_errno = Some(e.raw_os_error().unwrap_or(5));
        }
        self.outcomes.push(o);
    }

    fn skip(&mut self, c: &Candidate, reason: SkipReason) -> DeleteItemOutcome {
        self.record(outcome_of(
            self.w_env,
            self.run_id,
            c,
            Status::Skipped(reason),
            Freed::default(),
            Instant::now(),
        ));
        DeleteItemOutcome::Skipped {
            reason: skip_text(reason),
        }
    }
}

fn outcome_of(
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
        path: match c.locator() {
            Locator::Path(p) | Locator::Steam(_, p) => Some(p.clone()),
            Locator::Docker(_) => None,
        },
        measured_bytes: (status == Status::Removed || attributed > 0).then_some(attributed),
        status,
        est_bytes: c.est_bytes().0,
        freed,
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        trimmed_bytes: None,
    }
}

/// A tree the abort flag stopped before it removed anything is skipped; one it stopped half way is an error
/// row that says so (the entries already removed stay in `freed`).
fn status_of_failure(f: &DeleteFailure, aborted: bool) -> Status {
    if aborted && f.error == DeleteError::Refused {
        return if f.freed.entries_removed == 0 {
            Status::Skipped(SkipReason::Aborted)
        } else {
            Status::Error(STOPPED_PARTLY.to_string())
        };
    }
    match f.error.skip_reason() {
        Some(r) => Status::Skipped(r),
        None => Status::Error(f.error.text()),
    }
}

fn end_record(
    env: &Env,
    run_id: &str,
    attributed: u64,
    interrupted: bool,
    refused: Option<String>,
) -> RunEnd {
    RunEnd {
        run_id: run_id.to_string(),
        ts: env.clock.now_epoch(),
        verdict_after: DiskVerdict::Unknown,
        attributed_bytes: attributed,
        measured_drop: None,
        attribution_pct: None,
        causes: Vec::new(),
        explained: false,
        interrupted,
        refused,
    }
}

/// Sets the run guard's flag from the caller's switch and from a terminal signal, until `done`.
fn bridge_aborts(run: &ExplorerRun, env: &Env, done: &AtomicBool) {
    loop {
        if done.load(Ordering::SeqCst) {
            break;
        }
        if run.abort.load(Ordering::SeqCst) || term_signal().is_some() {
            env.guard.abort_flag().set();
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Stops the bridge when the sequence ends, by returning or by unwinding (a panic must not leave the scope
/// waiting for a thread that never ends).
struct StopOnDrop<'a>(&'a AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// The explorer's apply over `w`'s ports, recording each step in `trace`. Consumes the confirmation: one typed
/// count authorises one run of this plan and nothing else.
pub fn compose_explorer_apply<D: DirOps + 'static>(
    confirmed: ConfirmedPlan,
    w: &ExplorerApplyPorts<'_, D>,
    run: &ExplorerRun,
    trace: &mut Trace,
) -> ExplorerReport {
    let done = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| bridge_aborts(run, w.env, &done));
        let _stop = StopOnDrop(&done);
        sequence(confirmed, w, run, trace)
    })
}

fn sequence<D: DirOps + 'static>(
    confirmed: ConfirmedPlan,
    w: &ExplorerApplyPorts<'_, D>,
    run: &ExplorerRun,
    trace: &mut Trace,
) -> ExplorerReport {
    let env = w.env;
    let (reviewed, typed) = confirmed.into_parts();
    let (plan_id, count, confirmed_bytes) =
        (reviewed.plan_id(), reviewed.count(), reviewed.bytes());
    let paths: Vec<PathBuf> = reviewed.items().iter().map(|i| i.path.clone()).collect();
    let sel = Selection::explorer(paths);

    trace.push("check_plan_id");
    // The limits of the screen hold here too: a plan that did not come from `p` (more than 20 marks, a path over
    // 512 bytes, a mark inside another) is refused before it can authorise anything.
    if let Some(text) = set_refusal(&sel) {
        return refuse(run, EXIT_REFUSED, text);
    }
    // The id is recomputed from what is on disk now, never from the reviewed identities: a mark or a scan root
    // replaced since the review makes the whole run refuse here, before anything is deleted. The per-item
    // `fstat` compare in the target stays as the second line of defence for changes after this gate.
    let Some(recomputed) = fresh_plan_id(env, w, &reviewed) else {
        return refuse(run, EXIT_REFUSED, ExplorerGateRefusal::Changed.text());
    };
    if let Err(r) = check_plan_id(
        &sel,
        plan_id,
        SaveOutcome::Saved,
        env.plans.load_last().as_ref(),
        env.probe.euid(),
        recomputed,
        env.clock.now_epoch(),
    ) {
        return refuse(run, EXIT_REFUSED, r.text());
    }

    trace.push("check_stamp");
    let Ok(stamp) = (w.stamp)(REQUIRED_GATE) else {
        return refuse(run, EXIT_REFUSED, STAMP_REFUSED.to_string());
    };

    trace.push("run_lock");
    if let Err(e) = env.guard.acquire() {
        return refuse(run, EXIT_FATAL, lock_text(&e));
    }

    trace.push("audit_open");
    let (stamp_summary, break_glass) = match &stamp {
        StampOutcome::Confined => (None, None),
        StampOutcome::Verified(s) => (Some(s.clone()), None),
        StampOutcome::BrokeGlass { break_glass, .. } => (None, Some(break_glass.clone())),
    };
    let start = RunStart {
        schema: AUDIT_SCHEMA,
        run_id: w.run_id.clone(),
        ts: env.clock.now_epoch(),
        euid: env.probe.euid(),
        kinds: sel.kinds_resolved(),
        plan_id: Some(plan_id.to_string()),
        build: w.build.clone(),
        stamp: stamp_summary,
        break_glass,
        roots: reviewed_roots(&reviewed),
        verdict_start: None,
    };
    if let Err(e) = env.audit.run_start(&start) {
        return audit_unwritable(env, run, e.raw_os_error().unwrap_or(5));
    }
    trace.push("explorer_confirm");
    let line = ExplorerConfirm {
        run_id: w.run_id.clone(),
        ts: env.clock.now_epoch(),
        plan_id: plan_id.to_string(),
        count,
        bytes: confirmed_bytes,
        typed,
        tty: w.tty,
        items: reviewed
            .items()
            .iter()
            .map(|i| ConfirmedItem {
                path: i.path.clone(),
                path_hex: None,
                dev: i.identity.dev,
                ino: i.identity.ino,
            })
            .collect(),
    };
    if let Err(e) = env.audit.explorer_confirm(&line) {
        let report = audit_unwritable(env, run, e.raw_os_error().unwrap_or(5));
        let _ = env.audit.run_end(&end_record(
            env,
            &w.run_id,
            0,
            false,
            report.refused.clone(),
        ));
        return report;
    }

    trace.push("remeasure");
    // Only items strictly below a reviewed scan root are walked: a path the guards will refuse (a root, `$HOME`,
    // `/`) must not cost a walk of the tree above it first.
    let now_bytes: u64 = reviewed
        .items()
        .iter()
        .filter(|i| {
            reviewed
                .roots()
                .iter()
                .any(|r| strictly_below(&r.path, &i.path))
        })
        .map(|i| measure_attributed(&i.path, w.sizer, w.extents).unwrap_or(0))
        .sum();
    if grew_too_much(confirmed_bytes, now_bytes) {
        let text = changed_text(confirmed_bytes, now_bytes);
        return close_refused(env, w, run, EXIT_REFUSED, text);
    }

    trace.push("resolve_roots");
    let roots = match (w.resolve_roots)() {
        Ok(r) => r,
        Err(text) => {
            return close_refused(
                env,
                w,
                run,
                EXIT_REFUSED,
                format!("{text} Nothing was deleted."),
            );
        }
    };

    // As late as possible: every refusal above leaves no capability in existence.
    trace.push("mint");
    let token = (w.mint)();

    trace.push("execute");
    let target = ExplorerSelectionTarget::new(
        roots,
        w.own_state.clone(),
        (w.ops)(&token),
        reviewed.items().to_vec(),
        reviewed.roots().to_vec(),
    );
    let mut books = Books {
        w_env: env,
        run_id: &w.run_id,
        outcomes: Vec::new(),
        audit_errno: None,
    };
    let total = reviewed.count();
    for (n, item) in reviewed.items().iter().enumerate() {
        run.send(DeleteMsg::Progress {
            index: n + 1,
            total,
        });
        let c = candidate_for(env, item);
        let result = if env.guard.abort_flag().is_set() {
            books.skip(&c, SkipReason::Aborted)
        } else {
            step(&target, env, &token, &c, &mut books)
        };
        run.send(DeleteMsg::Item(DeleteItemResult {
            path: item.path.clone(),
            outcome: result,
        }));
        if books.audit_errno.is_some() {
            env.guard.abort_flag().set();
        }
    }

    trace.push("settle");
    let meter = env.dirs.home.as_path();
    let _ = env.space.settle(meter, Duration::from_secs(30));

    trace.push("run_end");
    // A signal or Esc that arrived after the last item finished interrupted nothing.
    let stopped_early = books
        .outcomes
        .iter()
        .any(|o| matches!(&o.status, Status::Skipped(SkipReason::Aborted)))
        || books
            .outcomes
            .iter()
            .any(|o| matches!(&o.status, Status::Error(m) if m == STOPPED_PARTLY));
    let interrupted =
        stopped_early && (term_signal().is_some() || run.abort.load(Ordering::SeqCst));
    let removed = books
        .outcomes
        .iter()
        .filter(|o| o.status == Status::Removed)
        .count();
    let bytes: u64 = books
        .outcomes
        .iter()
        .filter(|o| o.status == Status::Removed)
        .map(|o| o.measured_bytes.unwrap_or(0))
        .sum();
    let end_errno = env
        .audit
        .run_end(&end_record(env, &w.run_id, bytes, interrupted, None))
        .err()
        .map(|e| e.raw_os_error().unwrap_or(5));
    let any_error = books
        .outcomes
        .iter()
        .any(|o| matches!(o.status, Status::Error(_)));
    // The engine sets the flag itself on ENOSPC, EDQUOT or EIO: not a signal, so the run ended on a fatal error.
    let engine_aborted = env.guard.abort_flag().is_set() && !interrupted;
    let exit = if books.audit_errno.or(end_errno).is_some() || engine_aborted {
        EXIT_FATAL
    } else if interrupted {
        exit_for_signal(term_signal().unwrap_or(2))
    } else if any_error {
        EXIT_PARTIAL
    } else {
        0
    };
    let summary = DeleteSummary {
        removed,
        bytes,
        interrupted,
    };
    run.send(DeleteMsg::Done(summary));
    ExplorerReport {
        exit,
        refused: None,
        outcomes: books.outcomes,
        summary,
    }
}

/// The plan-id of the reviewed marks as they are now (`lstat` of each, the scan roots resolved again). `None`
/// when a mark can no longer be read: it is not what was reviewed. A root set that cannot be resolved keeps the
/// reviewed roots, so the existing `resolve_roots` refusal (with its `run_end`) reports it.
fn fresh_plan_id<D: DirOps>(
    env: &Env,
    w: &ExplorerApplyPorts<'_, D>,
    reviewed: &ReviewedPlan,
) -> Option<PlanId> {
    let mut fresh = Vec::with_capacity(reviewed.items().len());
    for item in reviewed.items() {
        let st = env.fs.lstat(&item.path).ok()?;
        fresh.push((item.path.as_path(), ItemIdentity::of(&st)));
    }
    let roots = match (w.resolve_roots)() {
        Ok(set) => set.root_identities().to_vec(),
        Err(_) => reviewed.roots().to_vec(),
    };
    Some(PlanId::for_explorer(fresh, &roots))
}

fn reviewed_roots(
    reviewed: &disk_cleanup_core::explore::confirm::ReviewedPlan,
) -> disk_cleanup_core::RunRoots {
    disk_cleanup_core::RunRoots {
        scan_roots: reviewed.roots().iter().map(|r| r.path.clone()).collect(),
        ..disk_cleanup_core::RunRoots::default()
    }
}

/// The candidate of a reviewed item from the `lstat` taken now; an entry that cannot be read gets the
/// planned identity, and the guard then refuses it as changed.
fn candidate_for(env: &Env, item: &disk_cleanup_core::explore::confirm::ReviewedItem) -> Candidate {
    match env.fs.lstat(&item.path) {
        Ok(st) => candidate_of(&item.path, &st),
        Err(_) => Candidate::new(
            Locator::Path(item.path.clone()),
            disk_cleanup_core::KindFacts::Explorer(disk_cleanup_core::ExplorerFacts {
                is_dir: true,
            }),
            disk_cleanup_core::AllocatedBytes(0),
            None,
            disk_cleanup_core::Identity {
                dev: item.identity.dev,
                ino: item.identity.ino,
            },
        ),
    }
}

/// One item: guard, `intent`, `delete_tree` through the target, `result`.
fn step<D: DirOps + 'static>(
    target: &ExplorerSelectionTarget<D>,
    env: &Env,
    token: &ApplyToken,
    c: &Candidate,
    books: &mut Books<'_>,
) -> DeleteItemOutcome {
    if let Guarded::Skip(r) = target.guard_before_delete(c, env) {
        return books.skip(c, r);
    }
    if let Err(e) = env
        .audit
        .intent(&Intent::of(c, books.run_id, env.clock.now_epoch()))
    {
        books.audit_errno = Some(e.raw_os_error().unwrap_or(5));
        return books.skip(c, SkipReason::Aborted);
    }
    let started = Instant::now();
    let (status, freed) = match target.execute(c, env, token) {
        Ok(freed) => (Status::Removed, freed),
        Err(f) => (
            status_of_failure(&f, env.guard.abort_flag().is_set()),
            f.freed,
        ),
    };
    let shown = match &status {
        Status::Removed => DeleteItemOutcome::Removed {
            bytes: freed.attributed_bytes.0,
        },
        Status::Skipped(r) => DeleteItemOutcome::Skipped {
            reason: skip_text(*r),
        },
        Status::Error(m) => DeleteItemOutcome::Failed { reason: m.clone() },
        Status::Running => DeleteItemOutcome::Failed {
            reason: "still running".to_string(),
        },
    };
    books.record(outcome_of(env, books.run_id, c, status, freed, started));
    shown
}

fn audit_unwritable(env: &Env, run: &ExplorerRun, errno: i32) -> ExplorerReport {
    let f = FatalError::AuditUnwritable {
        path: env.dirs.state.join("audit.jsonl"),
        errno,
    };
    refuse(run, EXIT_FATAL, f.text())
}

/// Ends a run that has `run_start` and `explorer_confirm` in the log but deletes nothing.
fn close_refused<D: DirOps>(
    env: &Env,
    w: &ExplorerApplyPorts<'_, D>,
    run: &ExplorerRun,
    exit: i32,
    text: String,
) -> ExplorerReport {
    let _ = env
        .audit
        .run_end(&end_record(env, &w.run_id, 0, false, Some(text.clone())));
    refuse(run, exit, text)
}
