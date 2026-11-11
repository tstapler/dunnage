//! The input state machine (Task 6.2.4a; UX-T1, UX-T12, UX-T20). [`handle_key`] turns one key into a change of
//! the view, the marks or the modal state and returns an [`Action`] for the loop. `core` never deletes:
//! [`Action::Delete`] is a request, and the flow behind it (a saved plan for exactly this set, then the typed
//! count) belongs to Story 6.6.4. No variant of [`Action`] writes to disk or runs a command by itself.

use std::path::{Path, PathBuf};

use super::keymap::{lookup, Cmd, Key};
use super::layout::Tier;
use super::mark_check::MarkOk;
use super::marks::{Mark, Marks};
use super::msg::{MarkRefusal, StopCause};
use super::refusal::{count_items_below, count_text, RefusalCtx};
use super::rows::{Badge, Row, RowId, RowKind};
use super::tree::{ExploreTree, NodeId, ScanStatus};
use super::view::{ListingRequest, ViewState};
use crate::escape::escape_os;

/// `D`, Delete and `y` (UX-T1: a reflex cannot delete).
pub const MSG_ONLY_D_DELETES: &str =
    "Press d to review and delete the marked items. Space marks an item, p previews the plan.";
pub const MSG_NO_SHELL: &str = "No shell from here";
pub const MSG_NOTHING_MARKED: &str = "Nothing is marked. Space marks an item.";
pub const MSG_NOT_A_DIRECTORY: &str = "not a directory";
pub const MSG_LAST_ENTRY: &str = "last entry";
pub const MSG_TOP_OF_SCAN: &str = "top of the scan: re-run with a wider PATH to go higher";
pub const MSG_SUMMARY_ROW: &str =
    "That row stands for several entries. Open the directory and mark one directory or file.";
pub const MSG_STOPPING: &str = "Stopping the scan; what was found is kept.";

/// Why the explorer is leaving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitKind {
    /// No marks, or the owner chose to forget them: `EXPLORE CLOSED - nothing was deleted.`
    Closed,
    /// Leave and print the plan report for the marks the cleaner knows.
    Plan,
    /// Ctrl-C: exit 130, no plan.
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rescan {
    /// `r`: the directory the view shows.
    Directory(PathBuf),
    /// `R`: everything, like `--rescan`.
    All,
}

/// What the loop must do after a key. The variants change the view, the marks (already done by the time
/// this is returned), the exit request, a rescan or a new scan root; `Delete` and `Preview` are requests that
/// the loop answers with a plan or a prompt.
#[derive(Debug, Clone)]
pub enum Action {
    /// The key changed the state or nothing; the footer message may say why.
    None,
    Redraw,
    /// `d` with marks. The only variant that can lead to a deletion, and only behind a plan and a typed count.
    Delete,
    /// `p` with marks: run the planner, delete nothing.
    Preview,
    Rescan(Rescan),
    /// Enter on an `x` row: scan from there (O-2).
    NewRoot(PathBuf),
    /// `h` at a pushed root: back to the previous root's tree (UX-T20).
    PopRoot,
    /// Esc with a scan running.
    StopScan,
    /// The view moved to another directory: list its files.
    ListFiles(ListingRequest),
    Exit(ExitKind),
}

/// Which part of the screen has the keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    /// Typing after `/`.
    FilterEdit,
    Help,
    Errors,
    QuitPrompt,
}

/// The modal and toggle state besides the view and the marks.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct InputState {
    pub mode: Mode,
    pub show_info: bool,
    /// `u` in the wide tier.
    pub shared_column: bool,
    /// `t`: percent only, no bar.
    pub percent_only: bool,
    /// Set by the loop while a scan runs.
    pub scan_running: bool,
    /// Esc stopped the scan; the title reads `PARTIAL SCAN (stopped by you)`.
    pub stop_requested: bool,
    /// Roots pushed by Enter on an `x` row and not yet popped; the loop owns it.
    pub root_depth: usize,
    /// The footer line the last key produced.
    pub message: Option<String>,
}

impl Default for InputState {
    fn default() -> InputState {
        InputState {
            mode: Mode::Normal,
            show_info: false,
            shared_column: false,
            percent_only: false,
            scan_running: false,
            stop_requested: false,
            root_depth: 0,
            message: None,
        }
    }
}

/// Everything the keys act on.
#[derive(Debug)]
pub struct ExploreState {
    pub view: ViewState,
    pub marks: Marks,
    pub input: InputState,
}

impl ExploreState {
    pub fn new(view: ViewState) -> ExploreState {
        ExploreState {
            view,
            marks: Marks::new(),
            input: InputState::default(),
        }
    }

    /// The footer's message line: the quit prompt while it is open, else what the last key said.
    pub fn footer(&self) -> Option<String> {
        if self.input.mode == Mode::QuitPrompt {
            return self.marks.quit_prompt();
        }
        self.input.message.clone()
    }
}

/// What Space asks `mark_check` (Story 6.6.1) about.
#[derive(Debug, Clone, Copy)]
pub struct MarkQuery<'a> {
    pub path: &'a Path,
    pub row: &'a Row,
    /// The marks so far: rule M6 and the limits of M8 are decided against them.
    pub marks: &'a Marks,
}

/// The facts a key needs besides the state. Time is milliseconds on the loop's monotonic clock.
pub struct KeyEnv<'a> {
    pub now_ms: u64,
    pub tier: Tier,
    /// List rows on screen; a page is one less.
    pub page: usize,
    pub tty: bool,
    /// The cleaner's resolved roots and the config file, for the refusal texts.
    pub roots: &'a [PathBuf],
    pub config_path: &'a Path,
    pub badge: &'a dyn Fn(NodeId) -> Badge,
    /// Rules M1 to M8 of Story 6.6.1 (`mark_check`); the `Ok` carries a note for the footer.
    pub mark_check: &'a dyn Fn(&MarkQuery<'_>) -> Result<MarkOk, MarkRefusal>,
}

/// The action and the footer message of one key.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub action: Action,
    pub message: Option<String>,
}

/// One key in, one change of state and one request for the loop out.
pub fn handle_key(
    st: &mut ExploreState,
    tree: &ExploreTree,
    key: Key,
    env: &KeyEnv<'_>,
) -> Outcome {
    st.input.message = None;
    let action = dispatch(st, tree, key, env);
    Outcome {
        action,
        message: st.input.message.clone(),
    }
}

fn dispatch(st: &mut ExploreState, tree: &ExploreTree, key: Key, env: &KeyEnv<'_>) -> Action {
    if key == Key::Ctrl('c') {
        return Action::Exit(ExitKind::Interrupted);
    }
    match st.input.mode {
        Mode::Normal => normal(st, tree, key, env),
        Mode::FilterEdit => filter_edit(st, tree, key, env),
        Mode::Help => {
            if matches!(key, Key::Esc | Key::Char('?')) {
                st.input.mode = Mode::Normal;
            }
            Action::None
        }
        Mode::Errors => {
            if matches!(key, Key::Esc | Key::Char('e')) {
                st.input.mode = Mode::Normal;
            }
            Action::None
        }
        Mode::QuitPrompt => match key {
            Key::Enter => Action::Exit(ExitKind::Plan),
            Key::Char('x') => Action::Exit(ExitKind::Closed),
            Key::Esc => {
                st.input.mode = Mode::Normal;
                Action::None
            }
            _ => Action::None,
        },
    }
}

fn normal(st: &mut ExploreState, tree: &ExploreTree, key: Key, env: &KeyEnv<'_>) -> Action {
    st.view.note_key(env.now_ms);
    if matches!(key, Key::Char('D' | 'y') | Key::Delete) {
        st.input.message = Some(MSG_ONLY_D_DELETES.to_string());
        return Action::None;
    }
    if key == Key::Char('b') {
        st.input.message = Some(MSG_NO_SHELL.to_string());
        return Action::None;
    }
    let Some(def) = lookup(key) else {
        return Action::None;
    };
    command(st, tree, def.cmd, env)
}

fn command(st: &mut ExploreState, tree: &ExploreTree, cmd: Cmd, env: &KeyEnv<'_>) -> Action {
    let now = env.now_ms;
    let page = isize::try_from(env.page.saturating_sub(1).max(1)).unwrap_or(1);
    match cmd {
        Cmd::CursorDown => {
            let last = st.view.rows().len().saturating_sub(1);
            if st.view.cursor_index() >= last {
                st.input.message = Some(MSG_LAST_ENTRY.to_string());
            } else {
                st.view.move_cursor(1, now);
            }
            Action::None
        }
        Cmd::CursorUp => {
            st.view.move_cursor(-1, now);
            Action::None
        }
        Cmd::PageDown => {
            st.view.move_cursor(page, now);
            Action::None
        }
        Cmd::PageUp => {
            st.view.move_cursor(-page, now);
            Action::None
        }
        Cmd::First => {
            st.view.move_to(0, now);
            Action::None
        }
        Cmd::Last => {
            st.view.move_to(st.view.rows().len().saturating_sub(1), now);
            Action::None
        }
        Cmd::Open => open(st, tree, env),
        Cmd::Parent => parent(st, tree, env),
        Cmd::Sort(key) => {
            st.view.press_sort(key, tree, now, env.badge);
            Action::None
        }
        Cmd::SharedColumn => {
            if env.tier == Tier::Wide {
                st.input.shared_column = !st.input.shared_column;
            } else {
                st.input.show_info = true;
            }
            Action::None
        }
        Cmd::ShareBar => {
            st.input.percent_only = !st.input.percent_only;
            Action::None
        }
        Cmd::FilterText => {
            st.input.mode = Mode::FilterEdit;
            Action::None
        }
        Cmd::FilterCycle => {
            st.view.cycle_filter(tree, now, env.badge);
            Action::None
        }
        Cmd::Info => {
            st.input.show_info = !st.input.show_info;
            Action::None
        }
        Cmd::Mark => {
            toggle_mark(st, tree, env);
            Action::None
        }
        Cmd::ClearMarks => {
            let n = st.marks.clear();
            st.input.message = Some(if n == 0 {
                "Nothing is marked.".to_string()
            } else {
                format!("Forgot {n} marks.")
            });
            Action::None
        }
        Cmd::Preview => marked_or_message(st, Action::Preview),
        Cmd::Delete => marked_or_message(st, Action::Delete),
        Cmd::Rescan => match st.view.dir() {
            Some(d) => Action::Rescan(Rescan::Directory(tree.path_of(d))),
            None => Action::None,
        },
        Cmd::RescanAll => Action::Rescan(Rescan::All),
        Cmd::Escape => on_escape(st, tree, env),
        Cmd::Errors => {
            st.input.mode = Mode::Errors;
            Action::None
        }
        Cmd::Help => {
            st.input.mode = Mode::Help;
            Action::None
        }
        Cmd::Redraw => Action::Redraw,
        Cmd::Quit => {
            if st.marks.is_empty() {
                Action::Exit(ExitKind::Closed)
            } else {
                st.input.mode = Mode::QuitPrompt;
                Action::None
            }
        }
        Cmd::Interrupt => Action::Exit(ExitKind::Interrupted),
    }
}

fn marked_or_message(st: &mut ExploreState, action: Action) -> Action {
    if st.marks.is_empty() {
        st.input.message = Some(MSG_NOTHING_MARKED.to_string());
        Action::None
    } else {
        action
    }
}

/// Esc in the main view acts on the first applicable of: a filter, a running scan. (An overlay or a prompt
/// has already taken Esc in [`dispatch`], and it comes first.)
fn on_escape(st: &mut ExploreState, tree: &ExploreTree, env: &KeyEnv<'_>) -> Action {
    if st.view.filter_active() {
        st.view.clear_filters(tree, env.now_ms, env.badge);
        return Action::None;
    }
    if st.input.scan_running && !st.input.stop_requested {
        st.input.stop_requested = true;
        st.input.message = Some(MSG_STOPPING.to_string());
        return Action::StopScan;
    }
    Action::None
}

fn filter_edit(st: &mut ExploreState, tree: &ExploreTree, key: Key, env: &KeyEnv<'_>) -> Action {
    match key {
        Key::Enter => st.input.mode = Mode::Normal,
        Key::Esc => {
            st.input.mode = Mode::Normal;
            st.view.set_text_filter("", tree, env.now_ms, env.badge);
        }
        Key::Backspace => {
            let mut t = st.view.text_filter().to_string();
            t.pop();
            st.view.set_text_filter(&t, tree, env.now_ms, env.badge);
        }
        Key::Char(c) => {
            let mut t = st.view.text_filter().to_string();
            t.push(c);
            st.view.set_text_filter(&t, tree, env.now_ms, env.badge);
        }
        _ => {}
    }
    Action::None
}

fn open(st: &mut ExploreState, tree: &ExploreTree, env: &KeyEnv<'_>) -> Action {
    let Some(row) = st.view.cursor_row().cloned() else {
        return Action::None;
    };
    match (row.kind, &row.id) {
        (RowKind::Dir, RowId::Dir(id)) if row.boundary => Action::NewRoot(tree.path_of(*id)),
        (RowKind::Dir, RowId::Dir(id)) => {
            Action::ListFiles(st.view.open_dir(tree, *id, env.now_ms, env.badge))
        }
        (RowKind::Empty, _) => Action::None,
        _ => {
            st.input.message = Some(MSG_NOT_A_DIRECTORY.to_string());
            Action::None
        }
    }
}

fn parent(st: &mut ExploreState, tree: &ExploreTree, env: &KeyEnv<'_>) -> Action {
    if let Some(req) = st.view.leave(tree, env.now_ms, env.badge) {
        return Action::ListFiles(req);
    }
    if st.input.root_depth > 0 {
        return Action::PopRoot;
    }
    st.input.message = Some(MSG_TOP_OF_SCAN.to_string());
    Action::None
}

/// The path a markable row stands for, or `None` for a summary row.
fn row_path(tree: &ExploreTree, row: &Row) -> Option<PathBuf> {
    match &row.id {
        RowId::Dir(id) => Some(tree.path_of(*id)),
        RowId::File(dir, name) => Some(tree.path_of(*dir).join(name)),
        RowId::Files(_) | RowId::Smaller(_) | RowId::Empty(_) => None,
    }
}

/// Space: unmark a marked row; otherwise ask `mark_check`, then the set's own limits (path length, 20 marks).
fn toggle_mark(st: &mut ExploreState, tree: &ExploreTree, env: &KeyEnv<'_>) {
    let Some(row) = st.view.cursor_row().cloned() else {
        return;
    };
    let Some(path) = row_path(tree, &row) else {
        st.input.message = Some(MSG_SUMMARY_ROW.to_string());
        return;
    };
    let shown = escape_os(path.as_os_str(), env.tty);
    if st.marks.remove(&path) {
        st.input.message = Some(format!("unmarked {shown}"));
        return;
    }
    let refusal = (env.mark_check)(&MarkQuery {
        path: &path,
        row: &row,
        marks: &st.marks,
    })
    .and_then(|ok| {
        st.marks.add(Mark {
            path: path.clone(),
            est_bytes: row.usage,
            cleaner_item: row.badge == Badge::Candidate,
        })?;
        Ok(ok)
    });
    match refusal {
        Err(r) => {
            let cx = RefusalCtx {
                path: &path,
                roots: env.roots,
                config_path: env.config_path,
                real_path: None,
                tty: env.tty,
            };
            st.input.message = Some(r.text(&cx));
        }
        Ok(ok) => {
            let mut line = format!("marked {shown}");
            if let Some(note) = &ok.note {
                line = format!("{line}; {note}");
            }
            if let (RowKind::Dir, RowId::Dir(id)) = (row.kind, &row.id) {
                let below = count_items_below(tree, *id, &|n| (env.badge)(n) != Badge::None);
                if below > 0 {
                    line = format!(
                        "{line}; contains {} candidate items (press f to show only candidates)",
                        count_text(below)
                    );
                }
            }
            st.input.message = Some(line);
        }
    }
}

/// The title's scan state (`tui/ux-explorer.md` 3.1): what the scan is doing or why it stopped.
pub fn scan_title(
    st: &InputState,
    status: ScanStatus,
    stop: StopCause,
    max_seconds: Option<u64>,
    elapsed_tenths: u64,
) -> String {
    let secs = format!("{}.{}s", elapsed_tenths / 10, elapsed_tenths % 10);
    if st.stop_requested {
        return "PARTIAL SCAN (stopped by you)".to_string();
    }
    match status {
        ScanStatus::Scanning => format!("SCANNING {secs}"),
        ScanStatus::Complete => format!("SCAN DONE {secs}"),
        ScanStatus::Truncated => match (stop, max_seconds) {
            (StopCause::Deadline, Some(n)) => format!("PARTIAL SCAN (hit --max-seconds {n})"),
            (StopCause::Cancelled, _) => "PARTIAL SCAN (stopped by you)".to_string(),
            _ => "PARTIAL SCAN".to_string(),
        },
        ScanStatus::Partial(_) => "PARTIAL SCAN".to_string(),
    }
}
