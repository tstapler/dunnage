//! The explorer's application state (Task 6.5.2b): the tree, the keys' state, the running scan, the planner
//! and the stack of nested scan roots. [`App`] holds no terminal; the loop in [`crate::event_loop`] feeds it
//! messages, keys and the time, and asks it for a [`Screen`] to draw. Workers (the file lister, the planner)
//! run on detached threads and answer on one internal channel, so a blocked worker never holds up the UI.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread;

use disk_cleanup_core::escape::escape;
use disk_cleanup_core::explore::confirm::ReviewedPlan;
use disk_cleanup_core::explore::delete_flow::{
    rescan_target, DPress, DeleteRun, Prompt, PromptKey, Remeasured, ReviewedSet, Submitted, Tally,
    MSG_PLAN_CHANGED, MSG_REVIEW_FIRST,
};
use disk_cleanup_core::explore::headroom::headroom_line;
use disk_cleanup_core::explore::hint::{hint_of_node, kind_line, shape_of_node};
use disk_cleanup_core::explore::input::{
    handle_key, Action, ExitKind, ExploreState, KeyEnv, MarkQuery, Mode, Rescan,
};
use disk_cleanup_core::explore::keymap::Key;
use disk_cleanup_core::explore::layout::{layout_for_width, Layout, Tier};
use disk_cleanup_core::explore::mark_check::MarkOk;
use disk_cleanup_core::explore::marks::MarkSetId;
use disk_cleanup_core::explore::msg::{
    ClassifyFacts, FileRows, MarkRefusal, NodeEvent, PlanPreview, ScanMode, ScanMsg, ScanSummary,
    StopCause,
};
use disk_cleanup_core::explore::ports::{
    DeleteHandle, DirLister, ExplorerActions, Handoff, PreviewHandle, ScanHandle, ScanOpts,
    ScanSource, TermSignals, TypedCountSource, NEEDS_TERMINAL,
};
use disk_cleanup_core::explore::rows::ROW_CAP;
use disk_cleanup_core::explore::rows::{Badge, Sort};
use disk_cleanup_core::explore::tree::{ExploreTree, NodeId};
use disk_cleanup_core::explore::view::ViewState;
use disk_cleanup_core::ports::SharedEstimate;
use disk_cleanup_core::roots::RootSet;
use disk_cleanup_core::snapshot_types::FsSnapshot;
use ratatui::Frame;

use crate::overlays::UnreadableEntry;
use crate::theme::Theme;
use crate::view::{
    adjust_scroll, list_capacity, render, row_path, DeleteScreen, ScanFacts, Screen,
};

/// Nested scan roots the owner can stack with Enter on an `x` row (O-2).
pub const MAX_NESTED_ROOTS: usize = 4;
pub const MSG_NESTED_LIMIT: &str = "4 nested scans: go back with h";
/// How long the loop waits for an aborted delete to report before it restores the terminal and leaves anyway
/// (review 2, N8: the abort ends the running tree promptly, so the wait normally ends with `run_end` written).
pub const ABORT_WAIT_MS: u64 = 5_000;
pub const MSG_DELETE_CANCELLED: &str = "Cancelled. Nothing was deleted; the marks are kept.";
pub const CLOSED_LINE: &str = "EXPLORE CLOSED - nothing was deleted.";
pub const CLOSED_PARTIAL_LINE: &str =
    "Closed while scanning; the tree was partial. Nothing was deleted.";

type MarkCheck = dyn Fn(&MarkQuery<'_>) -> Result<MarkOk, MarkRefusal> + Send + Sync;
type OwnerOf = dyn Fn(&Path) -> Option<(u32, u32)> + Send + Sync;
type DeviceLabel = dyn Fn(&Path) -> Option<String> + Send + Sync;

/// What a run is configured with. Everything the explorer needs of the outside world is here or in
/// [`Ports`]; the `tui` crate reads neither the environment nor the filesystem.
pub struct ExplorerConfig {
    pub scan_roots: Vec<PathBuf>,
    /// Template for every scan the loop starts; `generation` is set per scan.
    pub scan_opts: ScanOpts,
    pub sort: Sort,
    /// The cleaner's roots: they decide the `+` and `?` badges.
    pub roots: RootSet,
    /// The cleaner's scan roots, for the refusal texts.
    pub cleaner_roots: Vec<PathBuf>,
    pub config_path: PathBuf,
    pub home: Option<PathBuf>,
    pub facts: ClassifyFacts,
    pub tty: bool,
    pub color: bool,
    pub mark_check: Arc<MarkCheck>,
    pub owner_of: Option<Arc<OwnerOf>>,
    pub device_label: Option<Arc<DeviceLabel>>,
    pub now_unix: i64,
    /// `12 min ago` when a cache is painted first.
    pub cache_age: Option<String>,
    pub initial_headroom: Result<FsSnapshot, String>,
    pub headroom_path: PathBuf,
    /// Shown in the footer until the first key (the wiring's `another apply is running` note).
    pub start_note: Option<String>,
    /// The audit log a delete appends to: named by the prompt and the closing line.
    pub audit_log: Option<PathBuf>,
    /// How long the loop waits for an aborted delete to report before it leaves anyway.
    pub abort_wait_ms: u64,
}

impl ExplorerConfig {
    pub fn new(scan_roots: Vec<PathBuf>, roots: RootSet) -> ExplorerConfig {
        ExplorerConfig {
            headroom_path: scan_roots.first().cloned().unwrap_or_default(),
            scan_roots,
            scan_opts: ScanOpts::default(),
            sort: Sort::default(),
            roots,
            cleaner_roots: Vec::new(),
            config_path: PathBuf::from("config.toml"),
            home: None,
            facts: ClassifyFacts::default(),
            tty: true,
            color: false,
            mark_check: Arc::new(|_| Ok(MarkOk::default())),
            owner_of: None,
            device_label: None,
            now_unix: 0,
            cache_age: None,
            initial_headroom: Err("not read yet".to_string()),
            start_note: None,
            audit_log: None,
            abort_wait_ms: ABORT_WAIT_MS,
        }
    }
}

/// The ports the loop calls. Shared, so a worker thread can hold one.
#[derive(Clone)]
pub struct Ports {
    pub scan: Arc<dyn ScanSource>,
    pub lister: Arc<dyn DirLister>,
    pub signals: Arc<dyn TermSignals>,
    pub actions: Arc<dyn ExplorerActions>,
    /// Whether the typed count can be collected (stdin is a terminal).
    pub typed: Arc<dyn TypedCountSource>,
}

/// An answer from a worker thread.
enum Aux {
    Files {
        generation: u32,
        dir: NodeId,
        rows: FileRows,
    },
    Planner(PreviewHandle),
    /// The in-session review of the marks (`ticket` names the planning it belongs to).
    Reviewed {
        ticket: u64,
        result: Result<ReviewedPlan, String>,
    },
    Finished(Handoff),
}

/// Why a plan is being made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Purpose {
    /// `p`, or `d` with no plan of this session: show the preview, keep the review.
    Preview { from_d: bool },
    /// `d` on a reviewed set: measure again and open the prompt.
    Remeasure,
}

enum PreviewState {
    NotWanted,
    Pending,
    Ready(PlanPreview),
    Failed(String),
}

struct Planning {
    cancel: Arc<AtomicBool>,
    handle: Option<PreviewHandle>,
    purpose: Purpose,
    /// The marks the plan was started for.
    set: MarkSetId,
    ticket: u64,
    preview: PreviewState,
    review: Option<Result<ReviewedPlan, String>>,
}

struct Running {
    run: DeleteRun,
    handle: DeleteHandle,
}

/// Which delete screen has the keys.
enum Stage {
    None,
    Prompt(Prompt),
    Deleting(Running),
    Result(DeleteRun),
}

/// The tree and view of a root left behind by Enter on an `x` row.
struct Saved {
    tree: ExploreTree,
    view: ViewState,
    scroll_top: usize,
    scan_roots: Vec<PathBuf>,
    root_note: Option<String>,
    summary: Option<ScanSummary>,
    stop: StopCause,
    scan_elapsed_ms: u64,
}

/// How the run ends. Decided by the app, executed by the loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ending {
    /// Return to the shell with this code and these lines on stdout (after restore).
    Leave { code: i32, lines: Vec<OsString> },
}

pub struct App {
    pub cfg: ExplorerConfig,
    ports: Ports,
    pub tree: ExploreTree,
    pub state: ExploreState,
    pub scroll_top: usize,
    scan: Option<ScanHandle>,
    scan_roots: Vec<PathBuf>,
    scan_started_ms: u64,
    scan_elapsed_ms: Option<u64>,
    summary: Option<ScanSummary>,
    stop: StopCause,
    pub headroom: Result<FsSnapshot, String>,
    facts: ClassifyFacts,
    shared: HashMap<PathBuf, SharedEstimate>,
    errnos: HashMap<OsString, Vec<i32>>,
    cache_age: Option<String>,
    aux_tx: Sender<Aux>,
    aux_rx: Receiver<Aux>,
    planning: Option<Planning>,
    planning_ticket: u64,
    finishing: Option<Arc<AtomicBool>>,
    pub plan: Option<PlanPreview>,
    /// The line above the preview's keys.
    plan_note: Option<String>,
    reviewed: ReviewedSet,
    stage: Stage,
    tally: Tally,
    /// Set by Ctrl-C or a signal during a delete: the exit code, and when the wait for the abort ends.
    abort_exit: Option<(i32, u64)>,
    dirty: bool,
    stack: Vec<Saved>,
    root_note: Option<String>,
    notice: Option<String>,
    diagnostics: Vec<String>,
    pub banner: Option<String>,
    pub ending: Option<Ending>,
    pub size: (u16, u16),
    pub events_applied: u64,
    pub force_clear: bool,
    first_headroom_seen: bool,
}

fn badge_fn<'a>(
    tree: &'a ExploreTree,
    roots: &'a RootSet,
    facts: &'a ClassifyFacts,
) -> impl Fn(NodeId) -> Badge + 'a {
    move |id| hint_of_node(tree, id, roots, facts).badge()
}

impl App {
    pub fn new(cfg: ExplorerConfig, ports: Ports, size: (u16, u16)) -> App {
        let (aux_tx, aux_rx) = channel();
        let state = ExploreState::new(ViewState::new(cfg.sort));
        let start_note = cfg.start_note.clone();
        App {
            headroom: cfg.initial_headroom.clone(),
            facts: cfg.facts.clone(),
            scan_roots: cfg.scan_roots.clone(),
            cfg,
            ports,
            tree: ExploreTree::new(),
            state,
            scroll_top: 0,
            scan: None,
            scan_started_ms: 0,
            scan_elapsed_ms: None,
            summary: None,
            stop: StopCause::None,
            shared: HashMap::new(),
            errnos: HashMap::new(),
            cache_age: None,
            aux_tx,
            aux_rx,
            planning: None,
            planning_ticket: 0,
            finishing: None,
            plan: None,
            plan_note: None,
            reviewed: ReviewedSet::new(),
            stage: Stage::None,
            tally: Tally::new(),
            abort_exit: None,
            dirty: false,
            stack: Vec::new(),
            root_note: None,
            notice: start_note,
            diagnostics: Vec::new(),
            banner: None,
            ending: None,
            size,
            events_applied: 0,
            force_clear: false,
            first_headroom_seen: false,
        }
    }

    // ----- scans -----

    pub fn start_initial_scan(&mut self, now_ms: u64) {
        let roots = self.scan_roots.clone();
        let mut opts = self.cfg.scan_opts.clone();
        opts.generation = self.tree.generation();
        self.start_scan(&roots, &opts, now_ms);
    }

    fn start_scan(&mut self, roots: &[PathBuf], opts: &ScanOpts, now_ms: u64) {
        if let Some(old) = self.scan.take() {
            old.detach();
        }
        self.scan = Some(self.ports.scan.start(roots, opts));
        self.scan_started_ms = now_ms;
        self.scan_elapsed_ms = None;
        self.summary = None;
        self.stop = StopCause::None;
        self.state.input.scan_running = true;
        self.state.input.stop_requested = false;
    }

    pub fn scan_running(&self) -> bool {
        self.scan_elapsed_ms.is_none() && self.scan.is_some()
    }

    /// Detaches a running scan so no cache write is left half-done.
    pub fn detach_scan(&mut self) {
        if let Some(s) = self.scan.take() {
            s.detach();
        }
    }

    /// Applies at most `budget` of messages from the scan channel and the workers; returns
    /// `(messages applied, whether the last receive returned data or the budget ran out)`.
    pub fn drain(&mut self, budget: std::time::Duration, now_ms: u64) -> (usize, bool) {
        let started = std::time::Instant::now();
        let mut applied = 0;
        let mut more = false;
        self.drain_aux(now_ms);
        while let Some(handle) = self.scan.as_ref() {
            match handle.rx.try_recv() {
                Ok(msg) => {
                    applied += 1;
                    self.apply(msg, now_ms);
                    if started.elapsed() >= budget {
                        more = true;
                        break;
                    }
                    more = true;
                }
                Err(TryRecvError::Empty) => {
                    more = false;
                    break;
                }
                Err(TryRecvError::Disconnected) => {
                    if self.scan_elapsed_ms.is_none() {
                        self.apply(
                            ScanMsg::SourceFailed(
                                "the scan source stopped without finishing".into(),
                            ),
                            now_ms,
                        );
                    }
                    self.scan = None;
                    more = false;
                    break;
                }
            }
        }
        self.poll_planner();
        self.poll_delete(now_ms);
        (applied, more)
    }

    fn apply(&mut self, msg: ScanMsg, now_ms: u64) {
        match msg {
            ScanMsg::Batch {
                generation,
                mode,
                events,
            } => {
                self.events_applied += events.len() as u64;
                for e in &events {
                    if let NodeEvent::Unreadable { name, errno, .. } = e {
                        self.errnos.entry(name.clone()).or_default().push(*errno);
                    }
                }
                match mode {
                    ScanMode::Paint => self.cache_age = self.cfg.cache_age.clone(),
                    ScanMode::Revalidate => self.cache_age = None,
                    ScanMode::Fresh => {}
                }
                if let Err(e) = self.tree.apply_batch(generation, mode, events) {
                    self.diagnostics.push(format!("warning: {e}"));
                }
                self.ensure_view(now_ms);
            }
            ScanMsg::Finished(summary) => {
                if summary.generation != self.tree.generation() {
                    return;
                }
                self.tree.finish(summary.truncated);
                self.scan_elapsed_ms = Some(now_ms.saturating_sub(self.scan_started_ms));
                self.stop = summary.stop;
                self.cache_age = None;
                self.state.input.scan_running = false;
                for n in &summary.notes {
                    self.diagnostics
                        .push(format!("warning: {}", escape(n.as_bytes(), self.cfg.tty)));
                }
                self.summary = Some(summary);
                self.ensure_view(now_ms);
            }
            ScanMsg::SourceFailed(why) => {
                let text = format!("scan failed: {}", escape(why.as_bytes(), self.cfg.tty));
                self.diagnostics.push(format!("error: {text}"));
                self.notice = Some(text);
                self.tree.finish(true);
                self.scan_elapsed_ms = Some(now_ms.saturating_sub(self.scan_started_ms));
                self.state.input.scan_running = false;
                self.cache_age = None;
            }
            ScanMsg::Headroom(snap) => {
                self.headroom = Ok(snap);
                self.first_disk_note();
            }
            ScanMsg::FileRows { .. } => {}
            ScanMsg::Shared { dir, estimate } => {
                self.shared.insert(dir, estimate);
            }
            ScanMsg::Facts(f) => self.facts = f,
            ScanMsg::Preview(p) => self.preview_arrived(Ok(p)),
            ScanMsg::PreviewFailed(text) => self.preview_arrived(Err(text)),
        }
    }

    fn first_disk_note(&mut self) {
        if self.first_headroom_seen {
            return;
        }
        self.first_headroom_seen = true;
        let line = headroom_line(
            &self.cfg.headroom_path,
            self.headroom.as_ref().map_err(String::as_str),
            200,
        );
        if line.ends_with("WARN") || line.ends_with("CRIT") {
            self.notice = Some(
                "disk nearly full: this view writes nothing but the scan cache; a delete needs the 1 MiB audit reserve"
                    .to_string(),
            );
        }
    }

    /// Opens the first root once it exists.
    fn ensure_view(&mut self, now_ms: u64) {
        if self.state.view.dir().is_some() {
            return;
        }
        let Some(root) = self.tree.roots().next() else {
            return;
        };
        let badge = badge_fn(&self.tree, &self.cfg.roots, &self.facts);
        let req = self.state.view.open_dir(&self.tree, root, now_ms, &badge);
        self.spawn_listing(req.dir, req.generation, req.cancel);
    }

    fn spawn_listing(&self, dir: NodeId, generation: u32, cancel: Arc<AtomicBool>) {
        let path = self.tree.path_of(dir);
        let lister = Arc::clone(&self.ports.lister);
        let tx = self.aux_tx.clone();
        let _ = thread::Builder::new()
            .name("explore-lister".into())
            .spawn(move || {
                let rows = lister.list_files(&path, ROW_CAP, &cancel);
                let _ = tx.send(Aux::Files {
                    generation,
                    dir,
                    rows,
                });
            });
    }

    fn drain_aux(&mut self, now_ms: u64) {
        while let Ok(aux) = self.aux_rx.try_recv() {
            match aux {
                Aux::Files {
                    generation,
                    dir,
                    rows,
                } => {
                    let exists = |name: &std::ffi::OsStr| {
                        self.tree.children(dir).any(|c| self.tree.name(c) == name)
                    };
                    self.state.view.accept_files(generation, dir, rows, &exists);
                }
                Aux::Planner(h) => {
                    if let Some(p) = self.planning.as_mut() {
                        p.handle = Some(h);
                    } else {
                        h.cancel();
                    }
                }
                Aux::Reviewed { ticket, result } => {
                    if let Some(p) = self.planning.as_mut().filter(|p| p.ticket == ticket) {
                        p.review = Some(result);
                        self.advance_planning();
                    }
                }
                Aux::Finished(h) => {
                    if self
                        .finishing
                        .take()
                        .is_some_and(|c| !c.load(std::sync::atomic::Ordering::SeqCst))
                    {
                        self.leave_with(h.exit_code, h.lines);
                    }
                }
            }
        }
        let _ = now_ms;
    }

    fn poll_planner(&mut self) {
        let Some(p) = self.planning.as_mut() else {
            return;
        };
        let Some(h) = p.handle.as_ref() else { return };
        match h.rx.try_recv() {
            Ok(ScanMsg::Preview(plan)) => self.preview_arrived(Ok(plan)),
            Ok(ScanMsg::PreviewFailed(text)) => self.preview_arrived(Err(text)),
            _ => {}
        }
    }

    pub fn busy_planning(&self) -> bool {
        self.planning.is_some() || self.finishing.is_some()
    }

    fn mark_paths(&self) -> Vec<PathBuf> {
        self.state.marks.iter().map(|m| m.path.clone()).collect()
    }

    /// Starts the planner for the preview (unless this is the measure before the prompt) and the review of
    /// the marks, each on its own worker; `advance_planning` finishes when both have answered.
    fn start_plan(&mut self, purpose: Purpose) {
        debug_assert!(
            !self.busy_planning(),
            "the callers check for a plan in flight"
        );
        self.planning_ticket += 1;
        let ticket = self.planning_ticket;
        let cancel = Arc::new(AtomicBool::new(false));
        let wants_preview = matches!(purpose, Purpose::Preview { .. });
        self.planning = Some(Planning {
            cancel: Arc::clone(&cancel),
            handle: None,
            purpose,
            set: self.state.marks.identity(),
            ticket,
            preview: if wants_preview {
                PreviewState::Pending
            } else {
                PreviewState::NotWanted
            },
            review: None,
        });
        let marks = self.mark_paths();
        if wants_preview {
            let actions = Arc::clone(&self.ports.actions);
            let tx = self.aux_tx.clone();
            let marks = marks.clone();
            let _ = thread::Builder::new()
                .name("explore-planner".into())
                .spawn(move || {
                    let h = actions.preview(&marks, cancel);
                    let _ = tx.send(Aux::Planner(h));
                });
        }
        let actions = Arc::clone(&self.ports.actions);
        let tx = self.aux_tx.clone();
        let spawned = thread::Builder::new()
            .name("explore-review".into())
            .spawn(move || {
                let result = actions.review(&marks);
                let _ = tx.send(Aux::Reviewed { ticket, result });
            });
        if spawned.is_err() {
            let _ = self.aux_tx.send(Aux::Reviewed {
                ticket,
                result: Err("could not start the review thread. Nothing was deleted.".to_string()),
            });
        }
    }

    fn preview_arrived(&mut self, result: Result<PlanPreview, String>) {
        let Some(p) = self.planning.as_mut() else {
            return;
        };
        if matches!(p.preview, PreviewState::Pending) {
            p.preview = match result {
                Ok(plan) => PreviewState::Ready(plan),
                Err(text) => PreviewState::Failed(text),
            };
            self.advance_planning();
        }
    }

    /// Finishes the plan once the preview (when wanted) and the review have both answered.
    fn advance_planning(&mut self) {
        let done = self
            .planning
            .as_ref()
            .is_some_and(|p| !matches!(p.preview, PreviewState::Pending) && p.review.is_some());
        if !done {
            return;
        }
        let Some(p) = self.planning.take() else {
            return;
        };
        self.dirty = true;
        let Some(review) = p.review else { return };
        let same_marks = self.state.marks.identity() == p.set;
        match p.purpose {
            Purpose::Remeasure => match review {
                Ok(fresh) => match self.reviewed.remeasured(&p.set, &self.state.marks, fresh) {
                    Remeasured::Prompt(prompt) => self.stage = Stage::Prompt(prompt),
                    Remeasured::Changed => {
                        self.state.input.message = Some(MSG_PLAN_CHANGED.to_string());
                    }
                },
                Err(text) => {
                    self.reviewed.clear();
                    self.state.input.message = Some(text);
                }
            },
            Purpose::Preview { from_d } => {
                let (review, note) = match review {
                    Ok(plan) => (Some(plan), from_d.then(|| MSG_REVIEW_FIRST.to_string())),
                    Err(text) if from_d => {
                        self.reviewed.clear();
                        self.state.input.message = Some(text);
                        return;
                    }
                    Err(text) => (None, Some(text)),
                };
                match p.preview {
                    PreviewState::Ready(preview) => {
                        // A plan counts as reviewed only once its preview was put on the screen.
                        match review {
                            Some(plan) if same_marks => self.reviewed.hold(&self.state.marks, plan),
                            _ => self.reviewed.clear(),
                        }
                        self.plan = Some(preview);
                        self.plan_note = note;
                    }
                    PreviewState::Failed(text) => {
                        self.reviewed.clear();
                        self.state.input.message = Some(text);
                    }
                    PreviewState::Pending | PreviewState::NotWanted => self.reviewed.clear(),
                }
            }
        }
    }

    fn start_finish(&mut self) {
        if self.finishing.is_some() {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.finishing = Some(Arc::clone(&cancel));
        let marks = self.mark_paths();
        let actions = Arc::clone(&self.ports.actions);
        let tx = self.aux_tx.clone();
        let _ = thread::Builder::new()
            .name("explore-finish".into())
            .spawn(move || {
                let h = actions.finish(&marks);
                let _ = tx.send(Aux::Finished(h));
            });
    }

    /// Esc while the planner runs.
    pub fn cancel_planning(&mut self) -> bool {
        let mut did = false;
        if let Some(p) = self.planning.take() {
            p.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(h) = p.handle {
                h.cancel();
            }
            did = true;
        }
        if let Some(c) = self.finishing.take() {
            c.store(true, std::sync::atomic::Ordering::SeqCst);
            did = true;
        }
        if did {
            self.state.input.message = Some("Planning cancelled. Nothing was deleted.".to_string());
        }
        did
    }

    // ----- keys -----

    fn tier(&self) -> Tier {
        match layout_for_width(usize::from(self.size.0), usize::from(self.size.1)) {
            Layout::Tiered(l) => l.tier,
            Layout::TooSmall { .. } => Tier::Narrow,
        }
    }

    pub fn page(&self) -> usize {
        list_capacity(
            usize::from(self.size.0),
            usize::from(self.size.1),
            self.state.input.show_info,
        )
        .max(1)
    }

    /// One key. Returns whether the screen must be redrawn now.
    pub fn on_key(&mut self, key: Key, now_ms: u64) -> bool {
        match self.stage {
            Stage::Prompt(_) => {
                self.prompt_key(key);
                return true;
            }
            Stage::Deleting(_) => {
                if key == Key::Ctrl('c') {
                    self.begin_abort(130, now_ms);
                }
                return true;
            }
            Stage::Result(_) => {
                self.result_key(key);
                return true;
            }
            Stage::None => {}
        }
        if self.plan.is_some() {
            self.plan_key(key);
            return true;
        }
        if self.busy_planning() {
            match key {
                Key::Esc => {
                    self.cancel_planning();
                    return true;
                }
                Key::Char('q') => {
                    self.leave_closed();
                    return true;
                }
                Key::Char('p') => return false,
                _ => {}
            }
        }
        let tier = self.tier();
        let page = self.page();
        let outcome = {
            let badge = badge_fn(&self.tree, &self.cfg.roots, &self.facts);
            let mark_check = Arc::clone(&self.cfg.mark_check);
            let env = KeyEnv {
                now_ms,
                tier: map_tier(tier),
                page,
                tty: self.cfg.tty,
                roots: &self.cfg.cleaner_roots,
                config_path: &self.cfg.config_path,
                badge: &badge,
                mark_check: &*mark_check,
            };
            handle_key(&mut self.state, &self.tree, key, &env)
        };
        self.notice = None;
        self.do_action(outcome.action, now_ms);
        true
    }

    fn plan_key(&mut self, key: Key) {
        match key {
            Key::Esc => {
                self.plan = None;
                self.plan_note = None;
            }
            Key::Enter => {
                self.plan = None;
                self.plan_note = None;
                self.start_finish();
            }
            Key::Char('d') => {
                self.plan = None;
                self.plan_note = None;
                self.press_d();
            }
            Key::Ctrl('c') => self.leave_with(130, vec![]),
            Key::Char('q') => self.leave_closed(),
            _ => {}
        }
    }

    fn leave_closed(&mut self) {
        let line = if self.scan_elapsed_ms.is_none() && self.scan.is_some() {
            CLOSED_PARTIAL_LINE
        } else {
            CLOSED_LINE
        };
        self.leave_with(0, vec![OsString::from(line)]);
    }

    /// Ends the run with `base` as the exit code and `lines` for stdout. After a delete the closing line
    /// replaces the `nothing was deleted` ones and the exit code reflects an `ERR` item (1) or a btrfs
    /// refusal (3) when `base` is 0 (UX-T38).
    fn leave_with(&mut self, base: i32, mut lines: Vec<OsString>) {
        self.detach_scan();
        if let Some(closing) = self.closing_line() {
            lines.retain(|l| {
                l.to_str() != Some(CLOSED_LINE) && l.to_str() != Some(CLOSED_PARTIAL_LINE)
            });
            lines.push(OsString::from(closing));
        }
        self.ending = Some(Ending::Leave {
            code: self.tally.exit_code(base),
            lines,
        });
    }

    fn closing_line(&self) -> Option<String> {
        let audit = self
            .cfg
            .audit_log
            .as_deref()
            .unwrap_or_else(|| Path::new("audit.jsonl"));
        self.tally.closing_line(audit, self.cfg.tty)
    }

    /// What to print after a signal ended the run: the closing line when a delete ran.
    pub fn signal_lines(&self) -> Vec<OsString> {
        self.closing_line()
            .map(OsString::from)
            .into_iter()
            .collect()
    }

    // ----- delete -----

    /// `d`: the reviewed-set rule (UX-T34).
    fn press_d(&mut self) {
        if self.planning.is_some() || self.finishing.is_some() {
            return;
        }
        let press = self.reviewed.press_d(&self.state.marks);
        if press == DPress::NothingMarked {
            self.state.input.message =
                Some(disk_cleanup_core::explore::input::MSG_NOTHING_MARKED.to_string());
            return;
        }
        if !self.ports.typed.is_terminal() {
            self.state.input.message = Some(NEEDS_TERMINAL.to_string());
            return;
        }
        match press {
            DPress::NothingMarked => {}
            DPress::ReviewFirst => self.start_plan(Purpose::Preview { from_d: true }),
            DPress::Remeasure => self.start_plan(Purpose::Remeasure),
        }
    }

    fn prompt_key(&mut self, key: Key) {
        let Stage::Prompt(prompt) = &mut self.stage else {
            return;
        };
        match prompt.key(key) {
            PromptKey::Edited | PromptKey::Ignored => {}
            PromptKey::Cancel => {
                if let Stage::Prompt(p) = std::mem::replace(&mut self.stage, Stage::None) {
                    let (set, plan) = p.cancel();
                    self.reviewed.restore(set, plan);
                }
                self.state.input.message = Some(MSG_DELETE_CANCELLED.to_string());
            }
            PromptKey::Interrupt => {
                self.stage = Stage::None;
                self.leave_with(130, vec![]);
            }
            PromptKey::Submit => {
                if let Stage::Prompt(p) = std::mem::replace(&mut self.stage, Stage::None) {
                    match p.submit() {
                        Submitted::Accepted(confirmed) => {
                            let planned = confirmed.count();
                            let handle = self.ports.actions.delete(confirmed);
                            self.reviewed.clear();
                            self.state.input.message = None;
                            self.stage = Stage::Deleting(Running {
                                run: DeleteRun::new(planned),
                                handle,
                            });
                        }
                        Submitted::Refused(p) => self.stage = Stage::Prompt(p),
                    }
                }
            }
        }
    }

    fn result_key(&mut self, key: Key) {
        match key {
            Key::Esc => self.stage = Stage::None,
            Key::Ctrl('c') => {
                self.stage = Stage::None;
                self.leave_with(130, vec![]);
            }
            _ => {}
        }
    }

    pub fn delete_running(&self) -> bool {
        matches!(self.stage, Stage::Deleting(_))
    }

    /// Asks the running delete to stop at the next item boundary and leaves with `code` once it reported,
    /// or after `abort_wait_ms` ([`ABORT_WAIT_MS`] by default) when it does not.
    pub fn begin_abort(&mut self, code: i32, now_ms: u64) {
        if self.abort_exit.is_some() {
            return;
        }
        if let Stage::Deleting(r) = &self.stage {
            r.handle.abort();
            self.abort_exit = Some((code, now_ms.saturating_add(self.cfg.abort_wait_ms)));
            self.dirty = true;
        }
    }

    fn poll_delete(&mut self, now_ms: u64) {
        let Stage::Deleting(r) = &mut self.stage else {
            return;
        };
        for _ in 0..256 {
            match r.handle.rx.try_recv() {
                Ok(m) => {
                    r.run.apply(m);
                    self.dirty = true;
                    if r.run.is_finished() {
                        break;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    r.run.lose();
                    self.dirty = true;
                    break;
                }
            }
        }
        let overdue = self
            .abort_exit
            .is_some_and(|(_, deadline)| now_ms >= deadline);
        if overdue {
            r.run.lose();
        }
        if r.run.is_finished() {
            self.finish_delete(now_ms);
        }
    }

    /// The delete reported: add it to the session, drop the marks of settled items, rescan where it removed
    /// something and show the result, or leave when an abort was waiting for it.
    fn finish_delete(&mut self, now_ms: u64) {
        let Stage::Deleting(r) = std::mem::replace(&mut self.stage, Stage::None) else {
            return;
        };
        let run = r.run;
        self.tally.add(&run);
        for path in run.settled() {
            self.state.marks.remove(path);
        }
        if let Some(target) = rescan_target(&run.removed_paths()) {
            self.rescan_dir(&target, now_ms);
        }
        self.dirty = true;
        if let Some((code, _)) = self.abort_exit {
            self.leave_with(code, vec![]);
        } else {
            self.stage = Stage::Result(run);
        }
    }

    /// The node of `path` in the tree, if the scan reached it.
    fn find_node(&self, path: &Path) -> Option<NodeId> {
        for root in self.tree.roots() {
            let Ok(rest) = path.strip_prefix(self.tree.path_of(root)) else {
                continue;
            };
            let mut cur = root;
            for part in rest.components() {
                cur = self
                    .tree
                    .children(cur)
                    .find(|c| self.tree.name(*c) == part.as_os_str())?;
            }
            return Some(cur);
        }
        None
    }

    /// Rescans `target`, whose rows read `.` until it is done. The view stays where it is when it already
    /// shows `target` or something below it; otherwise it moves to `target`, because a rescan drops the
    /// subtree the view would be standing in.
    fn rescan_dir(&mut self, target: &Path, now_ms: u64) {
        let Some(node) = self.find_node(target) else {
            return;
        };
        let inside = self.state.view.dir().is_some_and(|v| {
            let mut up = Some(node);
            while let Some(n) = up {
                if n == v {
                    return true;
                }
                up = self.tree.parent(n);
            }
            false
        });
        if !inside {
            let badge = badge_fn(&self.tree, &self.cfg.roots, &self.facts);
            let req = self.state.view.open_dir(&self.tree, node, now_ms, &badge);
            self.spawn_listing(req.dir, req.generation, req.cancel);
        }
        let mut opts = self.cfg.scan_opts.clone();
        opts.paint_from_cache = false;
        opts.generation = self.tree.begin_rescan(node);
        self.start_scan(&[target.to_path_buf()], &opts, now_ms);
        self.rebuild_view_now(now_ms);
    }

    /// Whether something the screen shows changed outside a key (a delete message, a finished plan).
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    fn do_action(&mut self, action: Action, now_ms: u64) {
        match action {
            Action::None | Action::Redraw => {
                if matches!(action, Action::Redraw) {
                    self.force_clear = true;
                }
            }
            Action::Delete => self.press_d(),
            Action::Preview => self.start_plan(Purpose::Preview { from_d: false }),
            Action::Rescan(r) => self.rescan(r, now_ms),
            Action::NewRoot(path) => self.new_root(&path, now_ms),
            Action::PopRoot => self.pop_root(),
            Action::StopScan => {
                if let Some(s) = self.scan.as_ref() {
                    s.cancel();
                }
                self.state.input.stop_requested = true;
            }
            Action::ListFiles(req) => self.spawn_listing(req.dir, req.generation, req.cancel),
            Action::Exit(ExitKind::Closed) => self.leave_closed(),
            Action::Exit(ExitKind::Interrupted) => self.leave_with(130, vec![]),
            Action::Exit(ExitKind::Plan) => self.start_finish(),
        }
    }

    fn rescan(&mut self, what: Rescan, now_ms: u64) {
        let mut opts = self.cfg.scan_opts.clone();
        opts.paint_from_cache = false;
        match what {
            Rescan::Directory(path) => {
                let Some(dir) = self.state.view.dir() else {
                    return;
                };
                opts.generation = self.tree.begin_rescan(dir);
                self.start_scan(&[path], &opts, now_ms);
            }
            Rescan::All => {
                let roots: Vec<NodeId> = self.tree.roots().collect();
                if let [only] = roots[..] {
                    opts.generation = self.tree.begin_rescan(only);
                } else {
                    self.tree = ExploreTree::new();
                    self.state.view = ViewState::new(self.state.view.sort());
                    opts.generation = 0;
                }
                let paths = self.scan_roots.clone();
                self.start_scan(&paths, &opts, now_ms);
            }
        }
        self.rebuild_view_now(now_ms);
    }

    /// The owner asked for a different set of rows (a rescan): rebuild at once instead of waiting for the
    /// throttled re-sort, which would keep rows of the dropped subtree on screen for two seconds.
    fn rebuild_view_now(&mut self, now_ms: u64) {
        let text = self.state.view.text_filter().to_string();
        let badge = badge_fn(&self.tree, &self.cfg.roots, &self.facts);
        self.state
            .view
            .set_text_filter(&text, &self.tree, now_ms, &badge);
    }

    fn new_root(&mut self, path: &Path, now_ms: u64) {
        if self.stack.len() >= MAX_NESTED_ROOTS {
            self.state.input.message = Some(MSG_NESTED_LIMIT.to_string());
            return;
        }
        if self.scan_elapsed_ms.is_none() {
            self.tree.finish(true);
        }
        self.detach_scan();
        let sort = self.state.view.sort();
        let saved = Saved {
            tree: std::mem::take(&mut self.tree),
            view: std::mem::replace(&mut self.state.view, ViewState::new(sort)),
            scroll_top: self.scroll_top,
            scan_roots: std::mem::replace(&mut self.scan_roots, vec![path.to_path_buf()]),
            root_note: self.root_note.take(),
            summary: self.summary.take(),
            stop: self.stop,
            scan_elapsed_ms: self.scan_elapsed_ms.unwrap_or(0),
        };
        self.stack.push(saved);
        self.state.input.root_depth = self.stack.len();
        self.scroll_top = 0;
        let device = self
            .cfg
            .device_label
            .as_ref()
            .and_then(|f| f(path))
            .map_or(String::new(), |d| format!(": {d}"));
        self.root_note = Some(format!("(another filesystem{device})"));
        let mut opts = self.cfg.scan_opts.clone();
        opts.generation = 0;
        opts.cross_fs = false;
        opts.paint_from_cache = false;
        self.start_scan(&[path.to_path_buf()], &opts, now_ms);
    }

    fn pop_root(&mut self) {
        let Some(saved) = self.stack.pop() else {
            return;
        };
        self.detach_scan();
        self.tree = saved.tree;
        self.state.view = saved.view;
        self.scroll_top = saved.scroll_top;
        self.scan_roots = saved.scan_roots;
        self.root_note = saved.root_note;
        self.summary = saved.summary;
        self.stop = saved.stop;
        self.scan_elapsed_ms = Some(saved.scan_elapsed_ms);
        self.state.input.root_depth = self.stack.len();
        self.state.input.scan_running = false;
        self.state.input.stop_requested = false;
    }

    // ----- time and size -----

    pub fn elapsed_tenths(&self, now_ms: u64) -> u64 {
        self.scan_elapsed_ms
            .unwrap_or_else(|| now_ms.saturating_sub(self.scan_started_ms))
            / 100
    }

    /// The throttled re-sort and the scroll position.
    pub fn tick_view(&mut self, now_ms: u64) {
        if self.state.view.dir().is_some() {
            let badge = badge_fn(&self.tree, &self.cfg.roots, &self.facts);
            self.state.view.order_snapshot(&self.tree, now_ms, &badge);
        }
        self.scroll_top = adjust_scroll(
            self.scroll_top,
            self.state.view.cursor_index(),
            self.page(),
            self.state.view.rows().len(),
        );
    }

    /// Diagnostics for stderr after the restore: queued warnings, already escaped.
    pub fn take_diagnostics(&mut self) -> Vec<String> {
        std::mem::take(&mut self.diagnostics)
    }

    pub fn scan_stopped_by_signal_note(&self) -> bool {
        self.scan_running()
    }

    // ----- drawing -----

    fn unreadable_entries(&self) -> Vec<UnreadableEntry> {
        self.tree
            .iter()
            .filter(|id| self.tree.is_unreadable(*id))
            .map(|id| {
                let errno = self.errnos.get(self.tree.name(id)).and_then(|v| {
                    let first = *v.first()?;
                    v.iter().all(|e| *e == first).then_some(first)
                });
                UnreadableEntry {
                    path: self.tree.path_of(id),
                    errno,
                }
            })
            .collect()
    }

    pub fn render(&self, frame: &mut Frame<'_>, now_ms: u64) {
        let unreadable = if self.state.input.mode == Mode::Errors {
            self.unreadable_entries()
        } else {
            Vec::new()
        };
        let cursor = self.state.view.cursor_row();
        let (kind, shared, owner) = if self.state.input.show_info {
            cursor.map_or((None, None, None), |row| {
                let path = row_path(&self.tree, row);
                let kind = match row.id {
                    disk_cleanup_core::explore::rows::RowId::Dir(id) => {
                        let hint = hint_of_node(&self.tree, id, &self.cfg.roots, &self.facts);
                        let shape = shape_of_node(&self.tree, id, &self.facts);
                        Some(kind_line(hint, shape))
                    }
                    _ => None,
                };
                let shared = self.shared.get(&path).and_then(SharedEstimate::pane_text);
                let owner = self.cfg.owner_of.as_ref().and_then(|f| f(&path));
                (kind, shared, owner)
            })
        } else {
            (None, None, None)
        };
        let screen = Screen {
            tree: &self.tree,
            state: &self.state,
            theme: Theme::new(self.cfg.color),
            tty: self.cfg.tty,
            home: self.cfg.home.as_deref(),
            headroom: self.headroom.as_ref().map_err(String::as_str),
            headroom_path: &self.cfg.headroom_path,
            scan: ScanFacts {
                elapsed_tenths: self.elapsed_tenths(now_ms),
                stop: self.stop,
                max_seconds: self.cfg.scan_opts.max_seconds,
                summary: self.summary.as_ref(),
                cache_age: self.cache_age.as_deref(),
            },
            planning: self.busy_planning(),
            banner: self.banner.as_deref(),
            plan: self.plan.as_ref(),
            plan_note: self.plan_note.as_deref(),
            delete: match &self.stage {
                Stage::None => None,
                Stage::Prompt(p) => Some(DeleteScreen::Prompt(p)),
                Stage::Deleting(r) => Some(DeleteScreen::Running {
                    run: &r.run,
                    stopping: self.abort_exit.is_some(),
                }),
                Stage::Result(run) => Some(DeleteScreen::Result(run)),
            },
            audit_log: self.cfg.audit_log.as_deref(),
            scroll_top: self.scroll_top,
            kind_line: kind,
            shared,
            owner,
            now_unix: self.cfg.now_unix,
            unreadable: &unreadable,
            root_note: self.root_note.as_deref(),
            scan_root: self
                .scan_roots
                .first()
                .map_or(Path::new(""), PathBuf::as_path),
            notice: self.notice.as_deref(),
        };
        render(frame, &screen);
    }
}

fn map_tier(t: Tier) -> disk_cleanup_core::explore::layout::Tier {
    t
}
