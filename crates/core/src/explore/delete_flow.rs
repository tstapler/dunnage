//! The pure parts of the delete flow (Task 6.6.4a; UX-T34, UX-T35, UX-T38). `core` holds no terminal and
//! deletes nothing: this file decides what `d` means for the marks and the plan held in the session, how the
//! typed count is edited, what a stream of [`DeleteMsg`] adds up to and what the closing line says. The loop
//! in the `tui` crate owns the handles and the screen and calls these.
//!
//! The reviewed-set rule: a delete needs the in-process [`ReviewedPlan`] of exactly the marked set. A plan
//! saved in `last_plan` by an earlier session never counts (review C10), so a restart with the same marks
//! plans and reviews again.

use std::path::{Path, PathBuf};

use super::confirm::{Confirm, ConfirmedPlan, ReviewedPlan};
use super::fmt::fmt_size_cell;
use super::keymap::Key;
use super::marks::{MarkSetId, Marks};
use super::msg::{DeleteItemOutcome, DeleteItemResult, DeleteMsg, DeleteSummary};
use crate::shell_quote::shell_quote;

/// `d` on a set with no plan of this session: the plan runs and the owner reads it first.
pub const MSG_REVIEW_FIRST: &str = "Review the plan, then press d again.";
/// The fresh measure at `d` found other items than the plan the owner reviewed.
pub const MSG_PLAN_CHANGED: &str =
    "The marked items changed since the plan. Review the plan, then press d again.";
/// The most digits the prompt takes; the count is at most 20 (O-5), so a longer entry is never right.
pub const ENTRY_MAX: usize = 6;

// ----- the plan the session holds -----

/// What `d` does with the marks it finds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DPress {
    /// `Nothing is marked. Space marks an item.`
    NothingMarked,
    /// No plan of this session covers exactly these marks: plan now, show the preview, delete nothing.
    ReviewFirst,
    /// The plan covers them: measure again, then open the prompt with the fresh figures.
    Remeasure,
}

/// What the fresh measure at `d` led to.
#[derive(Debug)]
pub enum Remeasured {
    Prompt(Prompt),
    /// The marks or the items changed since the plan; the held plan is gone and `d` plans again.
    Changed,
}

/// The in-session reviewed plan, keyed by the marked set it was made for. At most one is held.
#[derive(Debug, Default)]
pub struct ReviewedSet {
    held: Option<(MarkSetId, ReviewedPlan)>,
}

impl ReviewedSet {
    pub fn new() -> ReviewedSet {
        ReviewedSet::default()
    }

    /// Keeps `plan` as the review of `marks`, replacing any earlier one.
    pub fn hold(&mut self, marks: &Marks, plan: ReviewedPlan) {
        self.held = Some((marks.identity(), plan));
    }

    pub fn clear(&mut self) {
        self.held = None;
    }

    pub fn is_empty(&self) -> bool {
        self.held.is_none()
    }

    /// Whether the held plan was made for exactly these marks.
    pub fn covers(&self, marks: &Marks) -> bool {
        self.held
            .as_ref()
            .is_some_and(|(id, _)| *id == marks.identity())
    }

    pub fn press_d(&self, marks: &Marks) -> DPress {
        if marks.is_empty() {
            DPress::NothingMarked
        } else if self.covers(marks) {
            DPress::Remeasure
        } else {
            DPress::ReviewFirst
        }
    }

    /// The fresh review `fresh` that `d` asked for with the marks `asked`. It opens the prompt only when
    /// the marks are still `asked`, the held plan covers them and the fresh plan-id equals the held one (the
    /// same items, identities and roots); the prompt then shows the fresh figures. Anything else drops the
    /// held plan.
    pub fn remeasured(
        &mut self,
        asked: &MarkSetId,
        marks: &Marks,
        fresh: ReviewedPlan,
    ) -> Remeasured {
        let same = self.covers(marks)
            && marks.identity() == *asked
            && self
                .held
                .as_ref()
                .is_some_and(|(_, held)| held.plan_id() == fresh.plan_id());
        self.held = None;
        if same {
            Remeasured::Prompt(Prompt::new(Confirm::new(fresh), asked.clone()))
        } else {
            Remeasured::Changed
        }
    }

    /// Esc at the prompt: the review is kept for the set it was made for.
    pub fn restore(&mut self, set: MarkSetId, plan: ReviewedPlan) {
        self.held = Some((set, plan));
    }
}

// ----- the prompt -----

/// What a key did at the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKey {
    /// The entry or the message changed: redraw.
    Edited,
    /// Not a key of the prompt: nothing happened.
    Ignored,
    /// Enter: [`Prompt::submit`] decides.
    Submit,
    /// Esc: cancel, keep the marks.
    Cancel,
    /// Ctrl-C: leave the program.
    Interrupt,
}

/// What Enter led to.
#[derive(Debug)]
pub enum Submitted {
    /// The exact count: the one value the delete takes.
    Accepted(ConfirmedPlan),
    /// A wrong entry: the same prompt, emptied, with `That is not N. Nothing was deleted.`
    Refused(Prompt),
}

/// The open prompt: the [`Confirm`] of one reviewed plan and what has been typed so far.
#[derive(Debug)]
pub struct Prompt {
    confirm: Confirm,
    set: MarkSetId,
    entry: String,
    message: Option<String>,
}

impl Prompt {
    pub fn new(confirm: Confirm, set: MarkSetId) -> Prompt {
        Prompt {
            confirm,
            set,
            entry: String::new(),
            message: None,
        }
    }

    /// `Type 2 and press Enter to delete, Esc to cancel:`
    pub fn prompt_line(&self) -> String {
        self.confirm.prompt_line()
    }

    pub fn entry(&self) -> &str {
        &self.entry
    }

    /// `That is not 2. Nothing was deleted.` after a wrong entry.
    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    pub fn count(&self) -> usize {
        self.confirm.count()
    }

    pub fn bytes(&self) -> u64 {
        self.confirm.bytes()
    }

    pub fn plan(&self) -> &ReviewedPlan {
        self.confirm.reviewed()
    }

    /// One key. Only ASCII digits, Backspace, Enter, Esc and Ctrl-C do anything: the footer hint `d` and
    /// every item key are ignored here, and so is a digit past [`ENTRY_MAX`].
    pub fn key(&mut self, key: Key) -> PromptKey {
        match key {
            Key::Ctrl('c') => PromptKey::Interrupt,
            Key::Esc => PromptKey::Cancel,
            Key::Enter => PromptKey::Submit,
            Key::Backspace => {
                if self.entry.pop().is_some() {
                    self.message = None;
                    PromptKey::Edited
                } else {
                    PromptKey::Ignored
                }
            }
            Key::Char(c) if c.is_ascii_digit() => {
                if self.entry.len() >= ENTRY_MAX {
                    return PromptKey::Ignored;
                }
                self.entry.push(c);
                self.message = None;
                PromptKey::Edited
            }
            _ => PromptKey::Ignored,
        }
    }

    /// Enter: checks the entry against the count.
    pub fn submit(self) -> Submitted {
        match self.confirm.accepts(&self.entry) {
            Ok(confirmed) => Submitted::Accepted(confirmed),
            Err(refusal) => {
                let message = refusal.text().to_string();
                Submitted::Refused(Prompt {
                    confirm: refusal.into_confirm(),
                    set: self.set,
                    entry: String::new(),
                    message: Some(message),
                })
            }
        }
    }

    /// Esc: the plan and the marked set it was made for, to hold again.
    pub fn cancel(self) -> (MarkSetId, ReviewedPlan) {
        (self.set, self.confirm.into_reviewed())
    }
}

// ----- a delete in progress and its result -----

/// How the stream of one delete ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunEnd {
    Running,
    Done(DeleteSummary),
    /// Nothing was deleted; the text is the whole message.
    Refused(String),
    /// The worker's channel closed without `Done` or `Refused`.
    Lost,
}

pub const MSG_WORKER_LOST: &str =
    "The delete stopped without reporting. Check the audit log to see what was removed.";

/// What the loop has seen of one delete.
#[derive(Debug, Clone)]
pub struct DeleteRun {
    planned: usize,
    index: usize,
    items: Vec<DeleteItemResult>,
    end: RunEnd,
}

impl DeleteRun {
    pub fn new(planned: usize) -> DeleteRun {
        DeleteRun {
            planned,
            index: 0,
            items: Vec::new(),
            end: RunEnd::Running,
        }
    }

    /// Takes one message; whatever follows `Done` or `Refused` is ignored.
    pub fn apply(&mut self, msg: DeleteMsg) {
        if self.end != RunEnd::Running {
            return;
        }
        match msg {
            DeleteMsg::Progress { index, total } => {
                self.index = index;
                self.planned = total;
            }
            DeleteMsg::Item(r) => self.items.push(r),
            DeleteMsg::Done(s) => self.end = RunEnd::Done(s),
            DeleteMsg::Refused(t) => self.end = RunEnd::Refused(t),
        }
    }

    /// The channel closed: a run that never said it ended is lost.
    pub fn lose(&mut self) {
        if self.end == RunEnd::Running {
            self.end = RunEnd::Lost;
        }
    }

    pub fn is_finished(&self) -> bool {
        self.end != RunEnd::Running
    }

    pub fn end(&self) -> &RunEnd {
        &self.end
    }

    pub fn items(&self) -> &[DeleteItemResult] {
        &self.items
    }

    /// `(index, total)` for the title `DELETING 1 of 2`; the first item counts from the start.
    pub fn progress(&self) -> (usize, usize) {
        (self.index.max(1).min(self.planned.max(1)), self.planned)
    }

    pub fn refused(&self) -> Option<&str> {
        match &self.end {
            RunEnd::Refused(t) => Some(t),
            RunEnd::Lost => Some(MSG_WORKER_LOST),
            _ => None,
        }
    }

    pub fn interrupted(&self) -> bool {
        matches!(&self.end, RunEnd::Done(s) if s.interrupted)
    }

    pub fn removed(&self) -> usize {
        self.items
            .iter()
            .filter(|i| matches!(i.outcome, DeleteItemOutcome::Removed { .. }))
            .count()
    }

    pub fn failed(&self) -> usize {
        self.items
            .iter()
            .filter(|i| matches!(i.outcome, DeleteItemOutcome::Failed { .. }))
            .count()
    }

    /// The measured bytes of the removed items.
    pub fn freed(&self) -> u64 {
        self.items
            .iter()
            .map(|i| match i.outcome {
                DeleteItemOutcome::Removed { bytes } => bytes,
                _ => 0,
            })
            .fold(0, u64::saturating_add)
    }

    /// Paths whose mark is dropped: removed or skipped. A failed item keeps its mark, and an item the run
    /// never reached (an abort) stays marked and unreported.
    pub fn settled(&self) -> Vec<&Path> {
        self.items
            .iter()
            .filter(|i| !matches!(i.outcome, DeleteItemOutcome::Failed { .. }))
            .map(|i| i.path.as_path())
            .collect()
    }

    /// Paths that left the disk.
    pub fn removed_paths(&self) -> Vec<PathBuf> {
        self.items
            .iter()
            .filter(|i| matches!(i.outcome, DeleteItemOutcome::Removed { .. }))
            .map(|i| i.path.clone())
            .collect()
    }
}

/// The directory to rescan after a delete: the deepest directory that holds the parents of every removed
/// path (their lowest common ancestor). `None` when nothing was removed.
pub fn rescan_target(removed: &[PathBuf]) -> Option<PathBuf> {
    let mut parents = removed.iter().filter_map(|p| p.parent());
    let mut common: PathBuf = parents.next()?.to_path_buf();
    for p in parents {
        while !p.starts_with(&common) {
            if !common.pop() {
                return None;
            }
        }
    }
    Some(common)
}

// ----- the session's tally and the closing line -----

/// `Refused` text of the btrfs starvation preflight (UX 1.2 row 3).
pub const STARVED_MARK: &str = "btrfs unallocated";

/// What the deletes of the session add up to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tally {
    attempted: bool,
    removed: usize,
    bytes: u64,
    had_error: bool,
    starved: bool,
}

impl Tally {
    pub fn new() -> Tally {
        Tally::default()
    }

    /// Adds a finished run. A refused run attempted nothing; a run that began counts even when it removed
    /// nothing, because the audit log then has its lines.
    pub fn add(&mut self, run: &DeleteRun) {
        match run.end() {
            RunEnd::Running => {}
            RunEnd::Refused(t) => {
                if t.contains(STARVED_MARK) {
                    self.starved = true;
                }
            }
            RunEnd::Lost | RunEnd::Done(_) => {
                self.attempted = true;
                self.removed += run.removed();
                self.bytes = self.bytes.saturating_add(run.freed());
                self.had_error |= run.failed() > 0 || matches!(run.end(), RunEnd::Lost);
            }
        }
    }

    pub fn attempted(&self) -> bool {
        self.attempted
    }

    pub fn removed(&self) -> usize {
        self.removed
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The exit code of a normal leave: `base` unless the session had an `ERR` item (1) or a delete refused
    /// by the btrfs preflight (3). `base` is kept when it is already a signal code.
    pub fn exit_code(&self, base: i32) -> i32 {
        if base != 0 {
            base
        } else if self.had_error {
            1
        } else if self.starved {
            3
        } else {
            0
        }
    }

    /// `EXPLORE CLOSED - deleted 3 items, 40.1 GiB measured. Audit log: <path>`; `None` when no delete ran.
    pub fn closing_line(&self, audit_log: &Path, tty: bool) -> Option<String> {
        if !self.attempted {
            return None;
        }
        let noun = if self.removed == 1 { "item" } else { "items" };
        Some(format!(
            "EXPLORE CLOSED - deleted {} {noun}, {} measured. Audit log: {}",
            self.removed,
            fmt_size_cell(self.bytes),
            shell_quote(audit_log.as_os_str(), !tty)
        ))
    }
}
