//! The human renderer for an apply run: the header block of `design/ux.md` 3.1, the live rows of S2 and the
//! summary of S3.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use disk_cleanup_core::ids::CandidateId;
use disk_cleanup_core::kind::SelectorId;
use disk_cleanup_core::outcome::{Outcome, SkipReason, Status};
use disk_cleanup_core::plan::{PlanId, BIN};
use disk_cleanup_core::selector::Selection;
use disk_cleanup_core::snapshot_types::FsSnapshot;
use disk_cleanup_core::summary::{DiskVerdict, RunSummary};

use crate::escape::shell_quote;
use crate::layout::RowCells;
use crate::reasontext::{self, ReasonFacts, TextCtx};
use crate::render::{apply_table, marker, push_row, Out};
use crate::terminal::Terminal;
use crate::units;
use crate::view::{display_path, family, selection_over, ItemRow, RowStatus};

pub const APPLYING: &str = "APPLYING - deleting.";

pub fn finished_line(exit: i32) -> String {
    format!("APPLY FINISHED - exit {exit}.")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LastDryRun {
    Matches,
    /// The plan-id of the newest saved entry.
    Differs(String),
}

/// Everything an apply screen is built from.
pub struct ApplyInput<'a> {
    pub plan_id: PlanId,
    pub selection: &'a Selection,
    /// `WARNING: gate check bypassed (<reason>)`, known at step 3.
    pub gate_bypass: Option<String>,
    pub audit_path: Option<PathBuf>,
    pub selected_items: usize,
    pub selected_est: u64,
    pub last_dry_run: Option<LastDryRun>,
    pub summary: &'a RunSummary,
    pub exit_code: i32,
    /// Items an interrupt left unattempted.
    pub not_attempted: Option<usize>,
    pub exe: PathBuf,
    pub home: Option<PathBuf>,
    /// What an outcome without a path is called (docker objects, btrfs steps).
    pub labels: HashMap<CandidateId, OsString>,
    pub facts: HashMap<CandidateId, ReasonFacts>,
    pub verbose: bool,
    pub quiet: bool,
    pub tty: bool,
}

/// The rows of an apply: one per outcome, in completion order.
pub fn apply_rows(input: &ApplyInput) -> Vec<ItemRow> {
    let none = ReasonFacts::default();
    input
        .summary
        .outcomes
        .iter()
        .map(|o| {
            let ctx = TextCtx {
                selection: input.selection,
                selector: o.selector,
                exe: &input.exe,
                ascii_only: !input.tty,
            };
            let facts = input.facts.get(&o.id).unwrap_or(&none);
            let (status, reason, error_text) = match &o.status {
                Status::Removed => (RowStatus::Freed, None, None),
                Status::Skipped(SkipReason::KindNotSelected) => {
                    (RowStatus::Risk, Some(SkipReason::KindNotSelected), None)
                }
                Status::Skipped(r) => (RowStatus::Skip, Some(*r), None),
                Status::Error(t) => (RowStatus::Err, None, Some(t.clone())),
            };
            let freed = (status == RowStatus::Freed).then(|| match o.freed.attributed_bytes.0 {
                0 => o.measured_bytes.unwrap_or(0),
                n => n,
            });
            let label = o.path.as_ref().map_or_else(
                || {
                    input.labels.get(&o.id).cloned().unwrap_or_else(|| {
                        OsString::from(format!("{} {}", o.selector.id(), &o.id.as_hex()[..8]))
                    })
                },
                |p| p.as_os_str().to_owned(),
            );
            ItemRow {
                id: o.id,
                kind: o.kind,
                selector: o.selector,
                status,
                est_bytes: o.est_bytes,
                freed_bytes: freed,
                measured_bytes: o.measured_bytes,
                elapsed_ms: o.elapsed_ms,
                age_secs: None,
                path: o.path.clone(),
                label,
                reason,
                reason_text: reason.map(|r| reasontext::reason(r, facts, &ctx)),
                next_action: reason.map(|r| reasontext::next_action(r, facts, &ctx)),
                error_text,
                risk_notes: Vec::new(),
            }
        })
        .collect()
}

fn removed(o: &Outcome) -> bool {
    o.status == Status::Removed
}

/// Bytes the lead line counts: what each removed item attributed, else what was measured.
pub fn freed_total(s: &RunSummary) -> u64 {
    s.outcomes
        .iter()
        .filter(|o| removed(o))
        .map(|o| match o.freed.attributed_bytes.0 {
            0 => o.measured_bytes.unwrap_or(0),
            n => n,
        })
        .sum()
}

fn exit_meaning(exit: i32) -> &'static str {
    match exit {
        0 => "success",
        1 => "partial failure",
        2 => "stopped by an error",
        3 => "btrfs starved",
        130 => "interrupted",
        143 => "terminated",
        _ => "see above",
    }
}

fn skipped_by_guard(s: &RunSummary) -> usize {
    s.outcomes
        .iter()
        .filter(|o| matches!(&o.status, Status::Skipped(r) if *r != SkipReason::KindNotSelected))
        .count()
}

fn errors(s: &RunSummary) -> usize {
    s.outcomes
        .iter()
        .filter(|o| matches!(o.status, Status::Error(_)))
        .count()
}

fn lead_lines(out: &mut Out, input: &ApplyInput) {
    let s = input.summary;
    let n = s.outcomes.iter().filter(|o| removed(o)).count();
    let mut lead = format!(
        "Freed {} in {}.",
        units::size(freed_total(s)),
        units::count(n, "item", "items")
    );
    if let Some(after) = &s.fs_after {
        let _ = write!(
            lead,
            "  df avail is now {} (was {}).",
            units::size(after.avail_bytes),
            units::size(s.fs_before.avail_bytes)
        );
    }
    out.text(&lead);
    let guard = skipped_by_guard(s);
    let errs = errors(s);
    out.text(&format!(
        "{} skipped by guard, {}.  Exit code {} ({}).",
        units::count(guard, "item", "items"),
        units::count(errs, "error", "errors"),
        input.exit_code,
        exit_meaning(input.exit_code)
    ));
}

fn remainder_line(s: &RunSummary) -> String {
    let (Some(text), Some(drop)) = (&s.unattributed_text, s.measured_drop) else {
        return "Unattributed remainder n/a".into();
    };
    let un = s.unattributed.unwrap_or(0);
    let pct = if drop > 0 {
        format!(
            "   ({:.1}%)",
            100.0 * un.unsigned_abs() as f64 / drop as f64
        )
    } else {
        String::new()
    };
    let why = match un {
        u if u > 0 => "   likely shared extents / other writers",
        u if u < 0 => "   attributed more than the filesystem dropped",
        _ => "",
    };
    format!("Unattributed remainder {text}{pct}{why}")
}

fn attribution_block(out: &mut Out, s: &RunSummary) {
    out.text("Attribution");
    out.indented(
        "  ",
        &format!("Attributed to items {}", units::size(s.attributed_bytes)),
    );
    out.indented("  ", &s.measured_line);
    out.indented("  ", &remainder_line(s));
    out.indented("  ", &s.attribution_line);
    for l in [
        &s.followup_line,
        &s.docker_line,
        &s.shared_line,
        &s.snapshot_line,
    ]
    .into_iter()
    .flatten()
    {
        out.indented("  ", l);
    }
}

fn change_row(out: &mut Out, name: &str, before: &str, after: &str, change: &str) {
    if out.term.width >= 76 {
        out.raw(format!("  {name:<30}{before:<14}{after:<14}{change}"));
    } else {
        out.wrapped(
            "  ",
            "    ",
            &format!("{name}: {before} -> {after} ({change})"),
        );
    }
}

fn signed(after: u64, before: u64) -> String {
    units::signed(
        i64::try_from(after).unwrap_or(i64::MAX) - i64::try_from(before).unwrap_or(i64::MAX),
    )
}

fn before_after_table(out: &mut Out, b: &FsSnapshot, a: &FsSnapshot, home: Option<&Path>) {
    let path = display_path(b.path.as_os_str(), home, out.term.tty);
    if out.term.width >= 76 {
        out.raw(format!(
            "{:<32}{:<14}{:<14}CHANGE",
            format!("Filesystem {path}  ({})", b.fs_type),
            "BEFORE",
            "AFTER"
        ));
    } else {
        out.text(&format!("Filesystem {path}  ({})", b.fs_type));
    }
    change_row(
        out,
        "df avail",
        &units::fs_size(b.avail_bytes),
        &units::fs_size(a.avail_bytes),
        &signed(a.avail_bytes, b.avail_bytes),
    );
    let Some((bb, ab)) = b.btrfs.zip(a.btrfs) else {
        change_row(
            out,
            "df used",
            &units::fs_size(b.used_bytes),
            &units::fs_size(a.used_bytes),
            &signed(a.used_bytes, b.used_bytes),
        );
        return;
    };
    let rows = [
        ("data used", bb.data.bytes_used, ab.data.bytes_used),
        ("data allocated", bb.data.total_bytes, ab.data.total_bytes),
        ("unallocated", bb.unallocated, ab.unallocated),
    ];
    for (name, x, y) in rows {
        change_row(
            out,
            name,
            &units::fs_size(x),
            &units::fs_size(y),
            &signed(y, x),
        );
    }
}

fn unallocated_note(out: &mut Out, input: &ApplyInput) {
    let s = input.summary;
    let Some((b, a)) = s
        .fs_before
        .btrfs
        .zip(s.fs_after.as_ref().and_then(|f| f.btrfs))
    else {
        return;
    };
    let dropped = s.measured_drop.is_some_and(|d| d > 0);
    if !dropped || !s.offer_wider_kinds || a.unallocated > b.unallocated.saturating_add(1 << 30) {
        return;
    }
    out.blank();
    out.wrapped(
        "Note: ",
        "      ",
        "freed data is still inside allocated chunks, so 'unallocated' did not grow. To return chunks to unallocated, run:",
    );
    out.raw(format!(
        "        sudo {} --apply --kinds btrfs",
        shell_quote(input.exe.as_os_str(), !input.tty)
    ));
}

fn still_warn_block(out: &mut Out, input: &ApplyInput) {
    let s = input.summary;
    let word = match s.verdict_after {
        DiskVerdict::Warn => "WARN",
        DiskVerdict::Crit => "CRIT",
        _ => return,
    };
    let reason = match s.fs_after.as_ref() {
        Some(f) => f.btrfs.map_or_else(
            || format!("df avail {}", units::size(f.avail_bytes)),
            |b| format!("unallocated {}", units::size(b.unallocated)),
        ),
        None => "filesystem state unavailable".into(),
    };
    out.blank();
    if s.top_unselected.is_empty() {
        out.text(&format!(
            "Disk still {word} ({reason}). No unselected RISK rows remain. Run '{BIN} status' for the filesystem state."
        ));
        return;
    }
    // One line, as in the S3 sample: a test and a reader search for it whole.
    out.raw(format!(
        "Disk still {word} ({reason}). The largest RISK rows this run did not select:"
    ));
    let rows = apply_rows(input);
    let mut ids: Vec<SelectorId> = Vec::new();
    for o in &s.top_unselected {
        let label = rows
            .iter()
            .find(|r| r.id == o.id)
            .map(|r| display_path(&r.label, input.home.as_deref(), input.tty))
            .unwrap_or_default();
        out.wrapped(
            "  ",
            "                          ",
            &format!(
                "{:>9}  {:<15} {label}",
                units::size(o.est_bytes),
                o.selector.id()
            ),
        );
        ids.push(o.selector);
    }
    ids.sort();
    ids.dedup();
    let mut sel = selection_over(input.selection, &ids);
    if ids.contains(&SelectorId::DockerVolumes) && sel.docker_volumes.is_none() {
        sel.docker_volumes = Some(disk_cleanup_core::selector::DockerVolumes::Anonymous);
    }
    out.indented(
        "    ",
        "Dry run them, then use the plan-id on its Next: line:",
    );
    out.raw(format!("      {BIN} {}", crate::view::args_of(&sel)));
}

fn errors_section(out: &mut Out, input: &ApplyInput, rows: &[ItemRow]) {
    let errs: Vec<&ItemRow> = rows.iter().filter(|r| r.status == RowStatus::Err).collect();
    if errs.is_empty() {
        return;
    }
    out.text("Errors:");
    for r in errs {
        let text = r.error_text.clone().unwrap_or_default();
        let shown = display_path(&r.label, input.home.as_deref(), input.tty);
        out.wrapped("  ", "    ", &format!("{shown}   {text}."));
        if text.starts_with("permission denied") {
            out.indented(
                "    ",
                "Owned by another user (often root, from a container)? Look at it with:",
            );
            out.raw(format!(
                "      ls -ld -- {}",
                shell_quote(&r.label, !input.tty)
            ));
            out.indented(
                "    ",
                "No ownership command is printed for a path found on disk. Take ownership yourself or remove it as its owner, then dry run again and use the new plan-id. The tool never retries with sudo.",
            );
        }
    }
}

fn spent_line(out: &mut Out, input: &ApplyInput) {
    let n = input.summary.outcomes.iter().filter(|o| removed(o)).count();
    if !input.selection.needs_plan_id() || n == 0 || input.exit_code == 0 {
        return;
    }
    // Holds a command, so it is never broken.
    out.raw(format!(
        "plan-id {} is spent ({} removed). Dry run again, then use the new plan-id: {BIN} {}",
        input.plan_id,
        units::count(n, "item", "items"),
        input.selection.dry_run_args()
    ));
}

fn row_cells(r: &ItemRow, input: &ApplyInput) -> RowCells {
    let reason = match r.status {
        RowStatus::Skip => format!("guard: {}", r.reason_text.clone().unwrap_or_default()),
        RowStatus::Err => r.error_text.clone().unwrap_or_default(),
        _ => String::new(),
    };
    let action = r
        .next_action
        .as_deref()
        .filter(|a| input.verbose && r.status == RowStatus::Skip && !reasontext::is_none_action(a));
    let reason = action.map_or(reason.clone(), |a| format!("{reason} (next: {a})"));
    let done = r.status == RowStatus::Freed;
    RowCells {
        status: r.status.word().to_string(),
        kind: if input.verbose {
            r.kind.id()
        } else {
            family(r.kind)
        }
        .to_string(),
        cells: vec![
            if done {
                units::elapsed(r.elapsed_ms)
            } else {
                "n/a".into()
            },
            r.freed_bytes.map_or_else(|| "n/a".into(), units::size),
        ],
        path: display_path(&r.label, input.home.as_deref(), input.tty),
        reason,
    }
}

fn row_table(out: &mut Out, input: &ApplyInput, rows: &[ItemRow]) {
    let shown: Vec<&ItemRow> = rows
        .iter()
        .filter(|r| r.status != RowStatus::Risk)
        .filter(|r| !input.quiet || matches!(r.status, RowStatus::Err | RowStatus::Skip))
        .collect();
    if shown.is_empty() {
        return;
    }
    let kw = shown
        .iter()
        .map(|r| {
            if input.verbose {
                r.kind.id()
            } else {
                family(r.kind)
            }
            .len()
        })
        .max()
        .unwrap_or(4);
    let table = apply_table(out.term.width, kw);
    out.lines.extend(table.header());
    for r in shown {
        push_row(out, &table, &row_cells(r, input), r.status);
    }
}

/// L1, the optional WARNING and nothing else: the shape of a run refused by the lock, the audit log or the
/// starvation check (`design/ux.md` 3.1).
pub fn render_refused(
    plan_id: PlanId,
    gate_bypass: Option<&str>,
    exit_code: i32,
    term: &Terminal,
) -> String {
    let mut out = Out::new(term);
    marker(&mut out, &format!("{APPLYING} plan-id {plan_id}"));
    if let Some(w) = gate_bypass {
        out.text(w);
    }
    out.text(&finished_line(exit_code));
    out.finish()
}

fn header(out: &mut Out, input: &ApplyInput) {
    marker(out, &format!("{APPLYING} plan-id {}", input.plan_id));
    if let Some(w) = &input.gate_bypass {
        out.text(w);
    }
    if let Some(p) = &input.audit_path {
        out.text(&format!(
            "audit log: {}",
            display_path(p.as_os_str(), input.home.as_deref(), input.tty)
        ));
    }
    let kinds: Vec<&str> = input.selection.kinds().ids().map(SelectorId::id).collect();
    out.text(&format!(
        "Selected: {}   ({}, est. {})",
        kinds.join(", "),
        units::count(input.selected_items, "item", "items"),
        units::size(input.selected_est)
    ));
    match &input.last_dry_run {
        Some(LastDryRun::Matches) => out.text("Last dry run: matches."),
        Some(LastDryRun::Differs(id)) => {
            out.text(&format!("Last dry run: differs (it was plan-id {id})."));
        }
        None => {}
    }
}

fn quiet_summary(out: &mut Out, input: &ApplyInput, rows: &[ItemRow]) {
    let s = input.summary;
    lead_lines(out, input);
    row_table(out, input, rows);
    out.text(&s.measured_line);
    out.text(&s.attribution_line);
    out.text(&remainder_line(s));
    errors_section(out, input, rows);
    spent_line(out, input);
}

fn full_summary(out: &mut Out, input: &ApplyInput, rows: &[ItemRow]) {
    let s = input.summary;
    out.blank();
    lead_lines(out, input);
    out.blank();
    attribution_block(out, s);
    if let Some(a) = &s.fs_after {
        out.blank();
        before_after_table(out, &s.fs_before, a, input.home.as_deref());
    }
    unallocated_note(out, input);
    for n in &s.notes {
        out.wrapped("Note: ", "      ", n);
    }
    still_warn_block(out, input);
    if let Some(n) = input.not_attempted {
        out.text(&format!(
            "INTERRUPTED: {} not attempted",
            units::count(n, "item", "items")
        ));
    }
    if input.exit_code == 2 {
        let avail = s.fs_after.as_ref().unwrap_or(&s.fs_before).avail_bytes;
        out.text(&format!(
            "df shows {} free but btrfs metadata/unallocated is the limit. Remove one large file you know is disposable, then run '{BIN} status'.",
            units::size(avail)
        ));
    }
    errors_section(out, input, rows);
    spent_line(out, input);
}

/// The whole apply screen of a run that got past step 4 of `design/ux.md` 1.4.
pub fn render_apply(input: &ApplyInput, term: &Terminal) -> String {
    let mut out = Out::new(term);
    let rows = apply_rows(input);
    header(&mut out, input);
    if input.quiet {
        quiet_summary(&mut out, input, &rows);
    } else {
        out.blank();
        row_table(&mut out, input, &rows);
        full_summary(&mut out, input, &rows);
    }
    out.text(&finished_line(input.exit_code));
    out.finish()
}
