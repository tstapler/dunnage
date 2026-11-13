//! The exit hand-off report: the S1 plan report for the marks the cleaner knows as items, with one
//! `--apply ... --only` line per risk tier (Story 6.4.3, `tui/ux-explorer.md` UX-T5 and UX-T6).
//!
//! The pieces here are pure functions of a plan, so the report is tested without a planner. Nothing in this
//! file runs or even builds a delete: the lines are text the owner pastes.

use std::path::{Path, PathBuf};

use dunnage_core::candidate::Locator;
use dunnage_core::kind::{Risk, SelectorId};
use dunnage_core::plan::{Plan, PlanId, PlanItem, Planned, BIN};
use dunnage_core::selector::Selection;

use crate::view::{args_of, display_path, selection_over, NextCmd};

/// The last stdout line of a session that marked nothing.
pub const CLOSED_LINE: &str = "EXPLORE CLOSED - nothing was deleted.";

/// Paths named in the not-an-item note before `+N more`.
const NAMED: usize = 5;

/// The selection the report is about: the plan's own, with the kinds cut to the ones that have a row, so the
/// `Selected now (--kinds ...)` line names what the owner marked and not every kind the planner asked for.
pub fn report_selection(plan: &Plan, planned: &Selection) -> Selection {
    let mut ids: Vec<SelectorId> = plan.items.iter().map(|i| i.candidate.selector()).collect();
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        return planned.clone();
    }
    selection_over(planned, &ids)
}

fn tier_word(tier: Risk) -> &'static str {
    match tier {
        Risk::Safe => "safe",
        Risk::Guarded => "guarded",
        Risk::Destructive => "destructive",
    }
}

fn item_path(i: &PlanItem) -> Option<PathBuf> {
    match i.candidate.locator() {
        Locator::Path(p) | Locator::Steam(_, p) => Some(p.clone()),
        Locator::Docker(_) => None,
    }
}

/// One `Next:` command per risk tier that has a row the plan would remove (`FREE`). The command carries
/// `--only` for exactly that tier's marks, so a line that drops one is refused as `changed` where a plan-id
/// binds it; a Safe or Guarded line needs no plan-id and says that `--only` is a filter there.
pub fn tier_commands(plan: &Plan, report_sel: &Selection) -> Vec<NextCmd> {
    let mut out = Vec::new();
    for tier in [Risk::Safe, Risk::Guarded, Risk::Destructive] {
        let items: Vec<&PlanItem> = plan
            .items
            .iter()
            .filter(|i| i.planned == Planned::Will && i.candidate.selector().tier() == tier)
            .collect();
        if items.is_empty() {
            continue;
        }
        let mut ids: Vec<SelectorId> = items.iter().map(|i| i.candidate.selector()).collect();
        ids.sort();
        ids.dedup();
        let mut sel = selection_over(report_sel, &ids);
        sel.only = items.iter().filter_map(|i| item_path(i)).collect();
        // An empty `--only` selects the whole kind, so a tier whose rows have no path gets no line.
        if sel.only.is_empty() {
            continue;
        }
        let plan_id = sel
            .needs_plan_id()
            .then(|| PlanId::for_selection(plan, &sel));
        let id_text = plan_id.map(|i| format!("--plan-id {i}"));
        let command = [
            BIN,
            "--apply",
            &args_of(&sel),
            id_text.as_deref().unwrap_or(""),
        ]
        .iter()
        .filter(|p| !p.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" ");
        let n = items.len();
        out.push(NextCmd {
            command,
            explain: format!(
                "the {n} FREE {} row{} you marked ({})",
                tier_word(tier),
                if n == 1 { "" } else { "s" },
                sel.kinds_resolved()
            ),
            plan_id,
            safe_apply: plan_id.is_none(),
            only_filter_note: plan_id.is_none(),
            selection: sel,
        });
    }
    out
}

/// The `Notes:` line for marks the cleaner does not know as items: nothing printed here can delete them.
pub fn not_items_note(stale: &[PathBuf], home: Option<&Path>) -> Option<String> {
    if stale.is_empty() {
        return None;
    }
    let mut named: Vec<String> = stale
        .iter()
        .take(NAMED)
        .map(|p| display_path(p.as_os_str(), home, false))
        .collect();
    if stale.len() > NAMED {
        named.push(format!("+{} more", stale.len() - NAMED));
    }
    let n = stale.len();
    let (noun, verb, pronoun, can) = if n == 1 {
        ("path", "is", "it", "it can be")
    } else {
        ("paths", "are", "them", "they can be")
    };
    Some(format!(
        "{n} marked {noun} {verb} not an item of any cleanable kind, so no command printed here can delete {pronoun}; {can} deleted from inside explore only (press d there): {}",
        named.join(", ")
    ))
}

/// The text printed after the terminal is restored, split into the lines the loop writes.
pub fn lines_of(report: &str) -> Vec<std::ffi::OsString> {
    report.lines().map(std::ffi::OsString::from).collect()
}
