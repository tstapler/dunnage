//! The explorer's planner calls (`preview`, `finish`): the only file outside the planner that may name the
//! tolerant `--only` mode (Story 6.4.3). Both plan the marked set once through the cleaner's own `Executor`
//! in plan mode, so the rows, the guards and the plan-id are the cleaner's; neither can delete.
//!
//! The ports are owned (`Arc`/`Box`), because the planner runs on a worker thread that outlives the call.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::Arc;
use std::time::{Duration, Instant};

use disk_cleanup_core::executor::{Executor, Mode, RunOptions, RunReport};
use disk_cleanup_core::explore::msg::{MarkRefusal, PlanPreview, PreviewRow, ScanMsg};
use disk_cleanup_core::explore::ports::{ExplorerActions, Handoff, PreviewHandle};
use disk_cleanup_core::explore::refusal::RefusalCtx;
use disk_cleanup_core::outcome::SkipReason;
use disk_cleanup_core::plan::{OnlyMode, Plan, PlanId, Planned};
use disk_cleanup_core::plan_state::{BuildInfo, SaveOutcome};
use disk_cleanup_core::ports::{
    AuditSink, Clock, CommandRunner, Env, Fs, PlanStore, RunGuard, SpaceMeter, SystemProbe,
};
use disk_cleanup_core::selector::{KindSelector, Selection};
use disk_cleanup_core::target::Target;
use disk_cleanup_core::{Dirs, RunRoots, SelectorId};

use super::handoff::{self, CLOSED_LINE};
use crate::app::{floor_notes_for, fs_block, reason_facts_for};
use crate::reasontext::{self, ReasonFacts, TextCtx};
use crate::render::render_plan;
use crate::terminal::Terminal;
use crate::view::{saved_entries_for, Options, PlanInput, PlanView, RowStatus, ScanFacts};

/// The kinds a marked path can be an item of: the ones whose items are paths. Docker and btrfs items have
/// none, and the apt cache needs root.
const PATH_KINDS: [SelectorId; 4] = [
    SelectorId::Caches,
    SelectorId::Worktrees,
    SelectorId::BuildDirs,
    SelectorId::Steam,
];

/// Everything the planner needs, owned so a worker thread can hold it.
pub struct ExplorerPorts {
    pub fs: Arc<dyn Fs>,
    pub runner: Arc<dyn CommandRunner>,
    pub clock: Arc<dyn Clock>,
    pub probe: Arc<dyn SystemProbe>,
    pub space: Arc<dyn SpaceMeter>,
    pub audit: Arc<dyn AuditSink>,
    pub guard: Arc<dyn RunGuard>,
    pub plans: Arc<dyn PlanStore>,
    pub dirs: Dirs,
    pub targets: Vec<Box<dyn Target>>,
    pub build: BuildInfo,
    pub roots: RunRoots,
    pub run_id: String,
    pub jobs: usize,
}

impl ExplorerPorts {
    fn env(&self) -> Env<'_> {
        Env {
            fs: &*self.fs,
            runner: &*self.runner,
            clock: &*self.clock,
            probe: &*self.probe,
            space: &*self.space,
            audit: &*self.audit,
            guard: &*self.guard,
            plans: &*self.plans,
            dirs: self.dirs.clone(),
        }
    }

    fn target_refs(&self) -> Vec<&dyn Target> {
        self.targets.iter().map(|t| &**t).collect()
    }

    fn path_kinds(&self) -> Vec<SelectorId> {
        PATH_KINDS
            .into_iter()
            .filter(|k| self.targets.iter().any(|t| t.selectors().contains(k)))
            .collect()
    }
}

/// Whether a path (or something below it) is still being scanned; the loop owns the answer.
pub type ScanningProbe = dyn Fn(&Path) -> bool + Send + Sync;

/// A plan for the marks, saved. Both entry points build one.
struct MarkPlan {
    report: RunReport,
    /// The selection the plan-id belongs to, cut to the kinds that have a row.
    selection: Selection,
    next: Vec<crate::view::NextCmd>,
    saved: SaveOutcome,
    secs: f64,
}

pub struct RealExplorerActions {
    ports: Arc<ExplorerPorts>,
    still_scanning: Arc<ScanningProbe>,
}

/// The one-line footer message for a failed preview.
fn failed(text: impl Into<String>) -> ScanMsg {
    ScanMsg::PreviewFailed(text.into())
}

fn handle_with(msg: ScanMsg, cancel: Arc<AtomicBool>) -> PreviewHandle {
    let (tx, rx) = channel();
    let _ = tx.send(msg);
    PreviewHandle::new(rx, cancel)
}

impl RealExplorerActions {
    pub fn new(ports: Arc<ExplorerPorts>) -> RealExplorerActions {
        RealExplorerActions {
            ports,
            still_scanning: Arc::new(|_| false),
        }
    }

    /// `probe(path)` is true while `path` or something below it is still being scanned.
    #[must_use]
    pub fn with_scanning_probe(mut self, probe: Arc<ScanningProbe>) -> RealExplorerActions {
        self.still_scanning = probe;
        self
    }

    /// Checks the marks and starts the planner on a worker. `Err(StillScanning)` plans nothing.
    pub fn try_preview(
        &self,
        marks: &[PathBuf],
        cancel: Arc<AtomicBool>,
    ) -> Result<PreviewHandle, MarkRefusal> {
        if marks.iter().any(|m| (self.still_scanning)(m)) {
            return Err(MarkRefusal::StillScanning);
        }
        let (tx, rx) = channel();
        let ports = Arc::clone(&self.ports);
        let marks = marks.to_vec();
        let flag = Arc::clone(&cancel);
        let spawned = std::thread::Builder::new()
            .name("explore-preview".into())
            .spawn(move || {
                let msg = match plan_marks(&ports, &marks, &flag) {
                    Ok(mp) => ScanMsg::Preview(preview_of(&ports, &mp)),
                    Err(text) => failed(text),
                };
                if !flag.load(Ordering::SeqCst) {
                    let _ = tx.send(msg);
                }
            });
        if spawned.is_err() {
            return Ok(handle_with(
                failed("could not start the planner thread. Nothing was deleted."),
                cancel,
            ));
        }
        Ok(PreviewHandle::new(rx, cancel))
    }
}

impl ExplorerActions for RealExplorerActions {
    fn preview(&self, marks: &[PathBuf], cancel: Arc<AtomicBool>) -> PreviewHandle {
        match self.try_preview(marks, Arc::clone(&cancel)) {
            Ok(h) => h,
            Err(r) => {
                let cx = RefusalCtx {
                    path: Path::new(""),
                    roots: &self.ports.roots.scan_roots,
                    config_path: &self.ports.dirs.config,
                    real_path: None,
                    tty: false,
                };
                handle_with(failed(r.text(&cx)), cancel)
            }
        }
    }

    fn finish(&self, marks: &[PathBuf]) -> Handoff {
        if marks.is_empty() {
            return Handoff {
                lines: vec![CLOSED_LINE.into()],
                exit_code: 0,
            };
        }
        let never = AtomicBool::new(false);
        match plan_marks(&self.ports, marks, &never) {
            Ok(mp) => Handoff {
                lines: handoff::lines_of(&report_text(&self.ports, &mp)),
                exit_code: 0,
            },
            Err(text) => Handoff {
                lines: vec![text.into(), CLOSED_LINE.into()],
                exit_code: 0,
            },
        }
    }
}

/// Plans `marks` once, tolerantly, and saves the plan-id (reserve and merge as any dry run does).
fn plan_marks(
    ports: &ExplorerPorts,
    marks: &[PathBuf],
    cancel: &AtomicBool,
) -> Result<MarkPlan, String> {
    if marks.is_empty() {
        return Err("Nothing is marked. Space marks an item.".to_string());
    }
    let kinds = ports.path_kinds();
    if kinds.is_empty() {
        return Err("This build lists no kind whose items are paths. Nothing was deleted.".into());
    }
    let mut sel = Selection::new(KindSelector::of(&kinds), KindSelector::default());
    sel.only = marks.to_vec();
    if let Err(e) = sel.validate_only() {
        return Err(format!("{e}. Nothing was deleted."));
    }
    let env = ports.env();
    let opts = RunOptions {
        jobs: ports.jobs.max(1),
        settle_timeout: Duration::from_secs(30),
        meter_path: None,
        meta: disk_cleanup_core::executor::RunMeta {
            run_id: ports.run_id.clone(),
            build: ports.build.clone(),
            stamp: None,
            break_glass: None,
            roots: ports.roots.clone(),
            verdict_start: None,
        },
    };
    let started = Instant::now();
    let report = Executor::new(&env, ports.target_refs(), opts)
        .with_only_mode(OnlyMode::Tolerant)
        .run(&sel, &Mode::Plan, None)
        .map_err(|r| format!("{} Nothing was deleted.", r.text))?;
    let secs = started.elapsed().as_secs_f64();
    if cancel.load(Ordering::SeqCst) {
        return Err("Planning cancelled. Nothing was deleted.".to_string());
    }
    let selection = handoff::report_selection(&report.plan, &report.selection);
    let next = handoff::tier_commands(&report.plan, &selection);
    let euid = env.probe.euid();
    let input = plan_input(ports, &report, &selection, secs, SaveOutcome::Saved);
    let entries = saved_entries_for(&input, next.clone());
    let saved = env.plans.save_merged(entries, euid, report.plan.now);
    Ok(MarkPlan {
        report,
        selection,
        next,
        saved,
        secs,
    })
}

fn plan_input<'a>(
    ports: &ExplorerPorts,
    report: &'a RunReport,
    selection: &'a Selection,
    secs: f64,
    saved: SaveOutcome,
) -> PlanInput<'a> {
    let env = ports.env();
    let targets = ports.target_refs();
    let mut notes = report.notes.clone();
    notes.extend(floor_notes_for(&targets, selection));
    notes.extend(handoff::not_items_note(
        &report.stale_only,
        Some(&ports.dirs.home),
    ));
    PlanInput {
        plan: &report.plan,
        selection,
        saved,
        scan: ScanFacts {
            roots: ports.roots.scan_roots.len(),
            secs,
            cache_warm: false,
            backend: "walker".into(),
            dirs: 0,
            unreadable: 0,
        },
        fs: Some(fs_block(report)),
        partial_scan: None,
        not_built: Vec::new(),
        notes,
        wrapper_note: None,
        exe: PathBuf::from(disk_cleanup_core::plan::BIN),
        euid: env.probe.euid(),
        state_dir: ports.dirs.state.clone(),
        home: Some(ports.dirs.home.clone()),
        facts: reason_facts_for(&targets, &report.plan),
        opts: Options {
            top: 0,
            ..Options::default()
        },
        errors: Vec::new(),
        tty: false,
    }
}

/// The S1 report after restore, as one string.
fn report_text(ports: &ExplorerPorts, mp: &MarkPlan) -> String {
    let input = plan_input(ports, &mp.report, &mp.selection, mp.secs, mp.saved);
    let view = PlanView::build_with(&input, mp.next.clone());
    render_plan(&input, &view, &Terminal::pipe())
}

fn status_word(s: RowStatus) -> &'static str {
    match s {
        RowStatus::Free => "FREE",
        RowStatus::Risk => "RISK",
        _ => "SKIP",
    }
}

fn skip_reason_text(
    r: SkipReason,
    facts: &ReasonFacts,
    selection: &Selection,
    selector: SelectorId,
) -> String {
    let ctx = TextCtx {
        selection,
        selector,
        exe: Path::new(disk_cleanup_core::plan::BIN),
        ascii_only: true,
    };
    reasontext::reason(r, facts, &ctx)
}

/// The preview overlay's data: the plan's rows (one per mark that is still an item, a `SKIP` row for each
/// mark that is not), the saved plan-id and the guards line. It carries no `Next:` text.
fn preview_of(ports: &ExplorerPorts, mp: &MarkPlan) -> PlanPreview {
    let plan: &Plan = &mp.report.plan;
    let facts = reason_facts_for(&ports.target_refs(), plan);
    let none = ReasonFacts::default();
    let mut rows = Vec::new();
    let mut skips = Vec::new();
    let mut est = 0u64;
    for i in &plan.items {
        let c = &i.candidate;
        let path = match c.locator() {
            disk_cleanup_core::candidate::Locator::Path(p)
            | disk_cleanup_core::candidate::Locator::Steam(_, p) => p.clone(),
            disk_cleanup_core::candidate::Locator::Docker(_) => continue,
        };
        let status = match i.planned {
            Planned::Will => "FREE",
            Planned::WillNot(SkipReason::KindNotSelected) => "RISK",
            Planned::WillNot(r) => {
                let f = facts.get(&c.id()).unwrap_or(&none);
                skips.push(format!(
                    "{}: {}",
                    disk_cleanup_core::escape::escape_os(path.as_os_str(), false),
                    skip_reason_text(r, f, &mp.selection, c.selector())
                ));
                "SKIP"
            }
        };
        if i.planned == Planned::Will {
            est = est.saturating_add(c.est_bytes().0);
        }
        rows.push(PreviewRow {
            path,
            kind: c.selector().id().to_string(),
            est_bytes: c.est_bytes().0,
            status: status.to_string(),
        });
    }
    for p in &mp.report.stale_only {
        let cx = RefusalCtx {
            path: p,
            roots: &ports.roots.scan_roots,
            config_path: &ports.dirs.config,
            real_path: None,
            tty: false,
        };
        skips.push(MarkRefusal::NotAnItem.text(&cx));
        rows.push(PreviewRow {
            path: p.clone(),
            kind: "path".to_string(),
            est_bytes: 0,
            status: status_word(RowStatus::Skip).to_string(),
        });
    }
    let tier = mp
        .next
        .iter()
        .map(|c| c.selection.kinds_resolved())
        .collect::<Vec<_>>()
        .join(", ");
    let n = rows.len();
    PlanPreview {
        plan_id: PlanId::for_selection(plan, &mp.selection).to_string(),
        saved: !matches!(mp.saved, SaveOutcome::NotSaved(_)),
        rows,
        est_bytes: est,
        scope_line: format!(
            "{n} marked item{}, cleaner kinds {}",
            if n == 1 { "" } else { "s" },
            if tier.is_empty() { "none" } else { &tier }
        ),
        guards_line: if skips.is_empty() {
            "Guards run now: the cleaner's guards for each item's kind; none refused.".to_string()
        } else {
            format!(
                "Guards run now: {} SKIP - {}",
                skips.len(),
                skips.join(" | ")
            )
        },
    }
}
