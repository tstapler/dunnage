//! The human renderer for the plan report (`design/ux.md` S1) and the pieces the apply summary shares:
//! the Filesystem block, the row table and the output buffer that word-wraps every non-command line.

use std::path::Path;

use disk_cleanup_core::plan::BIN;
use disk_cleanup_core::snapshot_types::FsSnapshot;
use disk_cleanup_core::summary::DiskVerdict;

use crate::layout::{wrap_line, RowCells, Table, APPLY_COLS, PLAN_COLS};
use crate::terminal::{Terminal, Tint};
use crate::units;
use crate::view::{display_path, FsBlock, ItemRow, PlanInput, PlanView, RowStatus};

pub const DRY_RUN_FIRST: &str = "DRY RUN - nothing will be deleted.";
pub const DRY_RUN_LAST: &str = "DRY RUN - nothing was deleted.";
const SUBTOTAL_LABEL_W: usize = 46;
const SUBTOTAL_VERB_W: usize = 10;
const ESTIMATES: &str =
    "Estimates are file-size sums; reflinks or snapshots can make the disk free less.";

/// Output lines, each word-wrapped to the terminal width unless pushed raw.
pub struct Out<'a> {
    pub lines: Vec<String>,
    pub term: &'a Terminal,
}

impl<'a> Out<'a> {
    pub fn new(term: &'a Terminal) -> Out<'a> {
        Out {
            lines: Vec::new(),
            term,
        }
    }

    /// A line that is never wrapped (a pasteable command).
    pub fn raw(&mut self, line: impl Into<String>) {
        self.lines.push(line.into());
    }

    pub fn blank(&mut self) {
        self.lines.push(String::new());
    }

    /// One paragraph wrapped to the width, continuation lines indented by `rest`.
    pub fn wrapped(&mut self, first: &str, rest: &str, text: &str) {
        self.lines
            .extend(wrap_line(first, rest, text, self.term.width));
    }

    pub fn text(&mut self, text: &str) {
        self.wrapped("", "", text);
    }

    /// Indented explanation lines.
    pub fn indented(&mut self, indent: &str, text: &str) {
        self.wrapped(indent, indent, text);
    }

    pub fn finish(self) -> String {
        let mut s = self.lines.join("\n");
        s.push('\n');
        s
    }
}

pub fn errno_name(errno: i32) -> String {
    match errno {
        28 => "ENOSPC".into(),
        122 => "EDQUOT".into(),
        13 => "EACCES".into(),
        1 => "EPERM".into(),
        30 => "EROFS".into(),
        2 => "ENOENT".into(),
        n => format!("errno {n}"),
    }
}

fn verdict_word(v: DiskVerdict) -> &'static str {
    match v {
        DiskVerdict::Ok => "OK",
        DiskVerdict::Warn => "WARN",
        DiskVerdict::Crit => "CRIT",
        DiskVerdict::Unknown => "unknown",
    }
}

pub fn verdict_text(v: DiskVerdict) -> String {
    format!("verdict: {}", verdict_word(v))
}

fn label(name: &str) -> String {
    format!("  {name:<13}  ")
}

/// The Filesystem block of S1.
pub fn fs_block(out: &mut Out, fs: &FsBlock, home: Option<&Path>) {
    fs_block_with(out, fs, home, false);
}

/// The Filesystem block; S4 (`status`) puts the verdict on the title line, S1 on the last row.
pub fn fs_block_with(out: &mut Out, fs: &FsBlock, home: Option<&Path>, verdict_on_title: bool) {
    let s: &FsSnapshot = &fs.snapshot;
    let path = display_path(s.path.as_os_str(), home, out.term.tty);
    if verdict_on_title {
        out.text(&format!(
            "Filesystem {path}  ({})    {}",
            s.fs_type,
            verdict_text(fs.verdict)
        ));
    } else {
        out.text(&format!("Filesystem {path}  ({})", s.fs_type));
    }
    out.text(&format!(
        "{}used {} of {}   avail {}",
        label("df"),
        units::fs_size(s.used_bytes),
        units::fs_size(s.total_bytes),
        units::fs_size(s.avail_bytes)
    ));
    let verdict = if verdict_on_title {
        String::new()
    } else {
        verdict_text(fs.verdict)
    };
    let Some(b) = s.btrfs else {
        out.text(&format!(
            "{}n/a (filesystem is {})",
            label("btrfs"),
            s.fs_type
        ));
        if !verdict_on_title {
            out.text(&format!("{}{verdict}", label("")));
        }
        return;
    };
    out.text(&format!(
        "{}used {} of {} allocated",
        label("data"),
        units::fs_size(b.data.bytes_used),
        units::fs_size(b.data.total_bytes)
    ));
    out.text(&format!(
        "{}used {} of {}",
        label("metadata"),
        units::fs_size(b.metadata.bytes_used),
        units::fs_size(b.metadata.total_bytes)
    ));
    let un = format!("{}{}", label("unallocated"), units::fs_size(b.unallocated));
    if verdict.is_empty() {
        out.text(&un);
        return;
    }
    let pad = 54_usize.saturating_sub(un.chars().count()).max(2);
    out.text(&format!("{un}{}{verdict}", " ".repeat(pad)));
}

/// The fixed text of a mode marker line, tinted bold on a colour terminal.
pub fn marker(out: &mut Out, text: &str) {
    let lines = wrap_line("", "", text, out.term.width);
    for (i, l) in lines.into_iter().enumerate() {
        let l = if i == 0 {
            let word_end = l.find(" - ").unwrap_or(l.len());
            let word = l[..word_end].to_string();
            out.term.paint_lead(&l, &word, Tint::Bold)
        } else {
            l
        };
        out.raw(l);
    }
}

fn status_tint(s: RowStatus) -> Tint {
    match s {
        RowStatus::Free | RowStatus::Freed => Tint::Green,
        RowStatus::Risk | RowStatus::Skip => Tint::Yellow,
        RowStatus::Err => Tint::Red,
    }
}

/// Whether the cell of a column prints in this table, for the kind-width scan.
fn kind_text(r: &ItemRow, verbose: bool) -> String {
    if verbose {
        r.kind.id().to_string()
    } else {
        crate::view::family(r.kind).to_string()
    }
}

fn plan_reason(r: &ItemRow, verbose: bool) -> String {
    let base = match r.status {
        RowStatus::Risk => format!("({})", r.risk_notes.join("; ")),
        _ => r.reason_text.clone().unwrap_or_default(),
    };
    let action = r
        .next_action
        .as_deref()
        .filter(|a| verbose && !crate::reasontext::is_none_action(a));
    match (base.is_empty(), action) {
        (_, None) => base,
        (true, Some(a)) => format!("next: {a}"),
        (false, Some(a)) => format!("{base} (next: {a})"),
    }
}

/// The plan table, capped at `top` rows (0 prints all), and the `... K more rows` line.
pub fn plan_table(out: &mut Out, rows: &[ItemRow], top: usize, verbose: bool, home: Option<&Path>) {
    if rows.is_empty() {
        return;
    }
    let shown = if top == 0 {
        rows.len()
    } else {
        top.min(rows.len())
    };
    let kw = rows[..shown]
        .iter()
        .map(|r| kind_text(r, verbose).len())
        .max()
        .unwrap_or(4);
    let table = Table::new(out.term.width, kw, &PLAN_COLS, 1);
    out.lines.extend(table.header());
    for r in &rows[..shown] {
        let cells = RowCells {
            status: r.status.word().to_string(),
            kind: kind_text(r, verbose),
            cells: vec![
                tier_word(r),
                units::size(r.est_bytes),
                units::age(r.age_secs),
            ],
            path: display_path(&r.label, home, out.term.tty),
            reason: plan_reason(r, verbose),
        };
        push_row(out, &table, &cells, r.status);
    }
    if shown < rows.len() {
        out.text(&format!(
            "... {} more rows (--top 0 for all)",
            rows.len() - shown
        ));
    }
}

pub fn tier_word(r: &ItemRow) -> String {
    match r.tier() {
        disk_cleanup_core::kind::Risk::Safe => "safe",
        disk_cleanup_core::kind::Risk::Guarded => "guarded",
        disk_cleanup_core::kind::Risk::Destructive => "destructive",
    }
    .to_string()
}

pub fn push_row(out: &mut Out, table: &Table, cells: &RowCells, status: RowStatus) {
    let mut lines = table.row(cells);
    if let Some(first) = lines.first_mut() {
        *first = out
            .term
            .paint_lead(first, &cells.status, status_tint(status));
    }
    out.lines.extend(lines);
}

/// Table and row builder of the apply rows live here so both screens share the layout.
pub fn apply_table(width: usize, kind_width: usize) -> Table {
    Table::new(width, kind_width, &APPLY_COLS, 1)
}

fn subtotal(out: &mut Out, label: &str, verb: Option<&str>, size: &str, tail: &str) {
    let verb = verb.unwrap_or("");
    let line = format!("{label:<SUBTOTAL_LABEL_W$}{verb:<SUBTOTAL_VERB_W$} {size:>9}   {tail}");
    if line.chars().count() <= out.term.width {
        out.raw(line);
        return;
    }
    let tight = if verb.is_empty() {
        format!("{label}  {size}   {tail}")
    } else {
        format!("{label}  {verb} {size}  {tail}")
    };
    if tight.chars().count() <= out.term.width {
        out.raw(tight);
        return;
    }
    out.text(label);
    let rest = [verb, size, tail]
        .iter()
        .filter(|p| !p.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" ");
    out.indented("  ", &rest);
}

fn decision_block(out: &mut Out, view: &PlanView, input: &PlanInput) {
    let partial = input.partial_scan.is_some();
    let sel = input.selection;
    subtotal(
        out,
        &format!("Selected now  (--kinds {})", sel.kinds_resolved()),
        Some("would free"),
        &units::size_lower_bound(view.selected.bytes, partial),
        &units::count(view.selected.items, "item", "items"),
    );
    if let Some(g) = &view.add_on {
        subtotal(
            out,
            &format!("Add {}  (their RISK rows)", g.kinds_text()),
            Some("would free"),
            &format!("+{}", units::size_lower_bound(g.bytes, partial)),
            &units::count(g.items, "item", "items"),
        );
    }
    if let Some(g) = &view.needs {
        subtotal(
            out,
            "Needs naming and root/other conditions",
            None,
            &units::size_lower_bound(g.bytes, partial),
            &units::count(g.kinds.len(), "kind", "kinds"),
        );
    }
    out.text(ESTIMATES);
}

fn not_usable_block(out: &mut Out, n: &crate::view::NotUsable) {
    out.wrapped(
        "  ",
        "    ",
        &format!(
            "plan-id NOT usable: could not save it ({} on {}).",
            errno_name(n.errno),
            n.state_path
        ),
    );
    let lines: Vec<String> = if n.safe_apply_printed {
        vec![
            "The RISK rows cannot be applied from this dry run. Run the safe apply printed above first".into(),
            format!("({BIN} --apply: it needs no plan-id and frees space), then dry run again."),
            "If that apply stops with an audit-log error, free about 1 MiB by hand and retry.".into(),
        ]
    } else {
        vec![format!(
            "There is no safe apply to run first. Free about 1 MiB in {} by hand (empty the trash, remove one disposable file), run '{BIN} status' for the verdict, then dry run again.",
            n.state_dir
        )]
    };
    for l in lines {
        out.indented("    ", &l);
    }
}

fn next_block(out: &mut Out, view: &PlanView) {
    if view.next.is_empty() && view.not_usable.is_none() {
        return;
    }
    out.text("Next:");
    for c in &view.next {
        out.raw(format!("  {}", c.command));
        out.indented("      ", &c.explain);
    }
    if let Some(n) = &view.not_usable {
        not_usable_block(out, n);
    }
}

fn notes_block(out: &mut Out, input: &PlanInput) {
    let mut notes: Vec<String> = input.notes.clone();
    if !input.not_built.is_empty() {
        notes.push(format!(
            "not available in this build: {}",
            input.not_built.join(", ")
        ));
    }
    if notes.is_empty() {
        return;
    }
    out.blank();
    out.text("Notes:");
    for n in notes {
        out.wrapped("  ", "    ", &n);
    }
}

fn scan_line(input: &PlanInput) -> String {
    let s = &input.scan;
    format!(
        "Scanned {} in {:.1}s ({}, {} backend, {} dirs, {} unreadable)",
        units::count(s.roots, "root", "roots"),
        s.secs,
        if s.cache_warm {
            "cache warm"
        } else {
            "cache cold"
        },
        s.backend,
        group_thousands(s.dirs),
        s.unreadable
    )
}

fn group_thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn header_lines(out: &mut Out, view: &PlanView, input: &PlanInput) {
    let saved = if view.plan_saved { "" } else { " (NOT saved)" };
    marker(
        out,
        &format!("{DRY_RUN_FIRST}   plan-id {}{saved}", view.header_id),
    );
    if let Some(note) = &input.wrapper_note {
        // Line 2 of the report, whole: a stdout-only consumer must read the NOTE as one line (UX-50).
        out.raw(note.clone());
    }
    if let Some(n) = input.partial_scan {
        out.text(&format!(
            "PARTIAL SCAN (hit --max-seconds {n}): sizes are lower bounds"
        ));
    }
}

/// The whole plan report. `view` must come from `PlanView::build(input)`.
pub fn render_plan(input: &PlanInput, view: &PlanView, term: &Terminal) -> String {
    let mut out = Out::new(term);
    header_lines(&mut out, view, input);
    if input.opts.quiet {
        out.text(&view.selected_text(input.partial_scan.is_some()));
        next_block(&mut out, view);
        out.text(DRY_RUN_LAST);
        return out.finish();
    }
    out.text(&scan_line(input));
    if view.rows.is_empty() {
        out.text("Nothing to free under the current selectors.");
    } else {
        out.blank();
        decision_block(&mut out, view, input);
    }
    if let Some(fs) = &input.fs {
        out.blank();
        fs_block(&mut out, fs, input.home.as_deref());
    }
    if !view.rows.is_empty() {
        out.blank();
        next_block(&mut out, view);
        out.blank();
        plan_table(
            &mut out,
            &view.rows,
            input.opts.top,
            input.opts.verbose,
            input.home.as_deref(),
        );
    }
    notes_block(&mut out, input);
    out.text(DRY_RUN_LAST);
    out.finish()
}

/// The wrapper `NOTE:` of `design/ux.md` S10 for a plan run, or `None` when no wrapper started it.
/// `dry_run_flag` is whether `-n` was typed; the bare habit gets the other text (UX-58).
pub fn wrapper_note(via: Option<&str>, dry_run_flag: bool) -> Option<String> {
    via.filter(|v| !v.is_empty())?;
    Some(if dry_run_flag {
        "NOTE: dry-run is now the default; -n is a no-op. Copy a Next: line to delete (old behaviour deleted unless -n; destructive kinds need the --plan-id on that line).".to_string()
    } else {
        "NOTE: disk-cleanup no longer deletes by default; this was a dry run. Copy a Next: line from the report to delete.".to_string()
    })
}

/// `disk-cleanup status` (S4).
pub fn render_status(fs: &FsBlock, home: Option<&Path>, term: &Terminal) -> String {
    let mut out = Out::new(term);
    fs_block_with(&mut out, fs, home, true);
    out.text(&format!(
        "Hint: '{BIN}' (dry run) lists what could be freed."
    ));
    out.finish()
}

/// `disk-cleanup kinds` (S5): one aligned row per selector.
pub fn render_kinds(rows: &[crate::render_json::KindRow], term: &Terminal) -> String {
    let na = |o: &Option<String>| o.clone().unwrap_or_else(|| "n/a".into());
    let table: Vec<[String; 7]> = rows
        .iter()
        .map(|r| {
            [
                r.id.into(),
                r.family.into(),
                r.tier.into(),
                (if r.default { "yes" } else { "no" }).into(),
                r.available.clone(),
                na(&r.min_age),
                r.touches.into(),
            ]
        })
        .collect();
    let head = [
        "ID",
        "FAMILY",
        "TIER",
        "DEFAULT",
        "AVAILABLE",
        "MIN-AGE",
        "TOUCHES",
    ]
    .map(String::from);
    let mut w = [0_usize; 7];
    for row in std::iter::once(&head).chain(table.iter()) {
        for (i, c) in row.iter().enumerate() {
            w[i] = w[i].max(c.chars().count());
        }
    }
    let mut out = Out::new(term);
    for row in std::iter::once(&head).chain(table.iter()) {
        let line: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{c:<w$}", w = w[i]))
            .collect();
        out.raw(line.join("  ").trim_end().to_string());
    }
    out.finish()
}
