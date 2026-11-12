//! The explorer's overlays as lines of text (Task 6.5.3b): the info pane, the `e` list of unreadable paths,
//! the plan preview, the too-small message and the empty-directory row. Every function maps data to plain
//! strings no wider than the width it is given, so the same lines serve the TUI and its snapshot tests.
//! Text handed in that holds a path is escaped by the caller or here with `escape_os`.

use std::path::{Path, PathBuf};

use disk_cleanup_core::escape::{escape_os, needs_escape};
use disk_cleanup_core::explore::delete_flow::{DeleteRun, Prompt, RunEnd};
use disk_cleanup_core::explore::fmt::fmt_size_cell;
use disk_cleanup_core::explore::layout::{cells, cut_name, cut_name_left, split_atoms, title_left};
use disk_cleanup_core::explore::msg::{DeleteItemOutcome, PlanPreview};
use disk_cleanup_core::shell_quote::shell_quote;
use disk_cleanup_core::snapshot_types::FsSnapshot;

/// Replaces every control or format character with `\u{HEX}`, so no raw ESC, C1 or bidi character reaches
/// a buffer cell whatever text a caller built.
pub fn clean(s: &str) -> String {
    use std::fmt::Write as _;
    if !s.chars().any(needs_escape) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if needs_escape(c) {
            let _ = write!(out, "\\u{{{:x}}}", u32::from(c));
        } else {
            out.push(c);
        }
    }
    out
}

/// Words broken at spaces into lines of at most `width` cells; a word longer than a line is cut between
/// atoms (an escape sequence is never split).
pub fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    let flush = |line: &mut String, lines: &mut Vec<String>| {
        if !line.is_empty() {
            lines.push(std::mem::take(line));
        }
    };
    for word in text.split(' ').filter(|w| !w.is_empty()) {
        let mut pieces: Vec<String> = Vec::new();
        if cells(word) > width {
            let mut cur = String::new();
            for atom in split_atoms(word) {
                if !cur.is_empty() && cells(&cur) + cells(atom) > width {
                    pieces.push(std::mem::take(&mut cur));
                }
                cur.push_str(atom);
            }
            pieces.push(cur);
        } else {
            pieces.push(word.to_string());
        }
        for p in pieces {
            if !line.is_empty() && cells(&line) + 1 + cells(&p) > width {
                flush(&mut line, &mut lines);
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(&p);
        }
    }
    flush(&mut line, &mut lines);
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

const LABEL_WIDTH: usize = 8;

/// `Label     body`, the body wrapped under the label.
pub fn wrap_labeled(label: &str, body: &str, width: usize) -> Vec<String> {
    let room = width.saturating_sub(LABEL_WIDTH).max(1);
    wrap_plain(body, room)
        .into_iter()
        .enumerate()
        .map(|(i, l)| {
            let head = if i == 0 { label } else { "" };
            format!("{head:<LABEL_WIDTH$}{l}")
        })
        .collect()
}

/// Everything the info pane shows for one row, as text the caller already worked out.
#[derive(Debug, Clone)]
pub struct InfoFacts {
    pub path: PathBuf,
    pub kind: String,
    pub usage: u64,
    pub apparent: u64,
    pub meta: Option<u64>,
    /// `None` for a file.
    pub items: Option<ItemCounts>,
    pub share_percent: String,
    pub newest_dir_mtime: Option<i64>,
    pub now_unix: i64,
    pub owner: Option<(u32, u32)>,
    pub shared: Option<String>,
    pub scan_state: String,
}

#[derive(Debug, Clone, Copy)]
pub struct ItemCounts {
    pub total: u64,
    pub files: u64,
    pub dirs: u64,
}

fn age_words(newest: Option<i64>, now: i64) -> String {
    let Some(t) = newest.filter(|t| *t > 0) else {
        return "unknown (no directory mtime recorded)".to_string();
    };
    let secs = now.saturating_sub(t).max(0);
    let span = if secs >= 86_400 {
        format!("{}d", secs / 86_400)
    } else if secs >= 3_600 {
        format!("{}h", secs / 3_600)
    } else {
        format!("{}m", secs / 60)
    };
    format!("{span} ago (newest directory mtime below)")
}

/// The info pane (`i`): a rule with the path, then one wrapped block per fact. Nothing is cut.
pub fn info_lines(f: &InfoFacts, width: usize, tty: bool) -> Vec<String> {
    let path = escape_os(f.path.as_os_str(), tty);
    let mut out = wrap_plain(&format!("-- Info: {path} "), width);
    if let Some(last) = out.last_mut() {
        let pad = width.saturating_sub(cells(last));
        if pad > 1 {
            last.push(' ');
            last.push_str(&"-".repeat(pad - 1));
        }
    }
    out.extend(wrap_labeled("Kind", &f.kind, width));
    let meta = f.meta.map_or("n/a (not btrfs)".to_string(), |m| {
        format!("{} (est.)", fmt_size_cell(m))
    });
    out.extend(wrap_labeled(
        "Usage",
        &format!(
            "{} on disk   Apparent {}   Metadata {meta}",
            fmt_size_cell(f.usage),
            fmt_size_cell(f.apparent)
        ),
        width,
    ));
    out.extend(wrap_labeled(
        "Shared",
        f.shared
            .as_deref()
            .unwrap_or("rootless estimate: not computed for this item"),
        width,
    ));
    let items = match f.items {
        Some(i) => format!(
            "{} (files {}, dirs {})    Share of parent {}",
            i.total, i.files, i.dirs, f.share_percent
        ),
        None => format!("n/a    Share of parent {}", f.share_percent),
    };
    out.extend(wrap_labeled("Items", &items, width));
    let owner = f.owner.map_or("uid n/a gid n/a".to_string(), |(u, g)| {
        format!("uid {u} gid {g}")
    });
    out.extend(wrap_labeled(
        "Age",
        &format!("{}   {owner}", age_words(f.newest_dir_mtime, f.now_unix)),
        width,
    ));
    out.extend(wrap_labeled("Scan", &f.scan_state, width));
    out.extend(wrap_labeled(
        "Guards",
        "not checked here; p runs them",
        width,
    ));
    out
}

/// The name of an errno the walker reports for an unreadable directory.
pub fn errno_words(errno: i32) -> String {
    let (name, text) = match errno {
        1 => ("EPERM", "Operation not permitted"),
        2 => ("ENOENT", "No such file or directory"),
        5 => ("EIO", "Input/output error"),
        13 => ("EACCES", "Permission denied"),
        20 => ("ENOTDIR", "Not a directory"),
        36 => ("ENAMETOOLONG", "File name too long"),
        40 => ("ELOOP", "Too many levels of symbolic links"),
        _ => return format!("errno {errno}"),
    };
    format!("{name} ({text})")
}

/// One unreadable directory for the `e` list.
#[derive(Debug, Clone)]
pub struct UnreadableEntry {
    pub path: PathBuf,
    /// `None` when the scan did not keep the reason.
    pub errno: Option<i32>,
}

pub const ERRORS_TITLE: &str = "UNREADABLE PATHS - nothing is deleted.";
pub const ERRORS_CLOSE: &str = "Esc or e closes";
pub const NO_SUDO: &str = "Owned by another user (often root, from a container)? Look at it with the command above; no ownership command is printed for a path found on disk. The tool never retries with sudo.";

/// The `e` overlay. Entries that do not fit the height are counted in a last line.
pub fn errors_lines(
    entries: &[UnreadableEntry],
    width: usize,
    height: usize,
    tty: bool,
) -> Vec<String> {
    let (t, c) = (cells(ERRORS_TITLE), cells(ERRORS_CLOSE));
    let mut out = vec![if t + c + 2 <= width {
        format!("{ERRORS_TITLE}{}{ERRORS_CLOSE}", " ".repeat(width - t - c))
    } else {
        cut_name(ERRORS_TITLE, width)
    }];
    out.push(String::new());
    if entries.is_empty() {
        out.push("No unreadable directories.".to_string());
        return out;
    }
    out.push(format!("{} unreadable", entries.len()));
    let tail = wrap_plain(NO_SUDO, width);
    let reserved = out.len() + tail.len() + 2;
    let mut shown = 0;
    for e in entries {
        let reason = e
            .errno
            .map_or("reason not recorded".to_string(), errno_words);
        let path = escape_os(e.path.as_os_str(), tty);
        let block = [
            wrap_plain(&format!("{reason}: {path}"), width.saturating_sub(2)),
            vec![format!(
                "ls -ld -- {}",
                shell_quote(e.path.as_os_str(), !tty)
            )],
        ];
        let lines: Vec<String> = block
            .into_iter()
            .flatten()
            .map(|l| format!("  {l}"))
            .collect();
        if out.len() + lines.len() + reserved > height && shown > 0 {
            break;
        }
        // The command is never wrapped: a wrapped command would not paste.
        out.extend(lines.into_iter().map(|l| cut_name_left(&l, width)));
        shown += 1;
    }
    if shown < entries.len() {
        out.push(format!("... and {} more", entries.len() - shown));
    }
    out.push(String::new());
    out.extend(tail);
    out
}

pub const PLAN_TITLE: &str = "PLAN PREVIEW - nothing is deleted.";
const NOT_USABLE: &str = "plan-id NOT usable: the plan could not be saved, so d and --apply --plan-id cannot use it. Nothing was deleted; free some disk space and press p again.";

/// The plan preview overlay (E6). It carries no `Next:` line: a command wrapped by a screen would not paste.
pub fn plan_lines(p: &PlanPreview, width: usize, height: usize, tty: bool) -> Vec<String> {
    plan_lines_noted(p, None, width, height, tty)
}

/// [`plan_lines`] with a line above the keys: `Review the plan, then press d again.` after a `d` that had no
/// plan to go on, or the reason this build or this plan cannot be deleted from.
pub fn plan_lines_noted(
    p: &PlanPreview,
    note: Option<&str>,
    width: usize,
    height: usize,
    tty: bool,
) -> Vec<String> {
    let id = if p.saved {
        format!("plan-id {} (saved)", clean(&p.plan_id))
    } else {
        format!("plan-id {} (NOT saved)", clean(&p.plan_id))
    };
    let mut out = vec![cut_name(&format!("{PLAN_TITLE}   {id}"), width)];
    out.extend(wrap_plain(
        &format!("Scope: {}", clean(&p.scope_line)),
        width,
    ));
    out.push(String::new());
    let n = p.rows.len();
    out.extend(wrap_plain(
        &format!(
            "Would free {} in {n} item{}.  Estimates are file-size sums; reflinks or snapshots can make the disk free less.",
            fmt_size_cell(p.est_bytes),
            if n == 1 { "" } else { "s" }
        ),
        width,
    ));
    if !p.saved {
        out.push(String::new());
        out.extend(wrap_plain(NOT_USABLE, width));
    }
    out.push(String::new());
    let kind_w = p
        .rows
        .iter()
        .map(|r| cells(&clean(&r.kind)))
        .max()
        .unwrap_or(4)
        .clamp(4, 14);
    out.push(cut_name(
        &format!("{:<6} {:<kind_w$} {:>9}  ITEM", "STATUS", "KIND", "SIZE"),
        width,
    ));
    let mut tail = vec![String::new(), clean(&p.guards_line)];
    if let Some(n) = note {
        tail.push(String::new());
        tail.push(clean(n));
    }
    tail.extend([
        String::new(),
        "d      delete these items (asks you to type the count)".to_string(),
        "Enter  leave the explorer; cleaner items are printed as the cleaner's dry run".to_string(),
        "Esc    back to the list (marks are kept)".to_string(),
    ]);
    let tail: Vec<String> = tail
        .iter()
        .flat_map(|l| {
            if l.is_empty() || cells(l) <= width {
                vec![l.clone()]
            } else {
                wrap_plain(l, width)
            }
        })
        .collect();
    let room = height.saturating_sub(out.len() + tail.len() + 1);
    let shown = n.min(room.max(1));
    for r in p.rows.iter().take(shown) {
        let head = format!(
            "{:<6} {:<kind_w$} {:>9}  ",
            clean(&r.status),
            cut_name(&clean(&r.kind), kind_w),
            fmt_size_cell(r.est_bytes)
        );
        let item = escape_os(r.path.as_os_str(), tty);
        let room = width.saturating_sub(cells(&head)).max(4);
        out.push(format!("{head}{}", cut_name_left(&item, room)));
    }
    if shown < n {
        out.push(format!("... and {} more items", n - shown));
    }
    out.extend(tail);
    out
}

/// `path` shown with `home` as `~` when it lies below it.
pub fn tilde(path: &Path, home: Option<&Path>) -> String {
    if let Some(h) = home {
        if let Ok(rest) = path.strip_prefix(h) {
            if rest.as_os_str().is_empty() {
                return "~".to_string();
            }
            return format!("~/{}", escape_os(rest.as_os_str(), true));
        }
    }
    escape_os(path.as_os_str(), true)
}

// ----- the delete screens (Task 6.6.4b; UX-T8, UX-T35, UX-T38) -----

pub const PROMPT_TITLE: &str = "DELETE - this cannot be undone; nothing goes to a trash.";

/// `5d2e77a1` from a plan-id.
fn id_text(id: impl std::fmt::Display) -> String {
    id.to_string()
}

/// What the prompt states besides the plan: the filesystem the space is freed on and where the audit goes.
#[derive(Debug, Clone, Copy)]
pub struct PromptFacts<'a> {
    pub headroom: Result<&'a FsSnapshot, &'a str>,
    pub headroom_path: &'a Path,
    pub audit_log: Option<&'a Path>,
    pub home: Option<&'a Path>,
}

fn fs_words(f: &PromptFacts<'_>, tty: bool) -> String {
    match f.headroom {
        Ok(s) => format!(
            "{} ({})",
            escape_os(s.path.as_os_str(), tty),
            clean(&s.fs_type)
        ),
        Err(_) => escape_os(f.headroom_path.as_os_str(), tty),
    }
}

/// The confirmation (E6): the count, the items, the total, the filesystem, the audit log and the typed entry.
/// The entry and its cursor are on the last line; a wrong entry's message is above it.
pub fn prompt_lines(
    p: &Prompt,
    f: &PromptFacts<'_>,
    width: usize,
    height: usize,
    tty: bool,
) -> Vec<String> {
    let n = p.count();
    let mut out = vec![cut_name(PROMPT_TITLE, width)];
    out.extend(wrap_plain(
        &format!(
            "Plan {} covers exactly {} marked item{}:",
            id_text(p.plan().plan_id()),
            n,
            if n == 1 { "" } else { "s" }
        ),
        width,
    ));
    let mut tail: Vec<String> = wrap_plain(
        &format!(
            "Total {} (attributed) on {}.",
            fmt_size_cell(p.bytes()),
            fs_words(f, tty)
        ),
        width,
    );
    let audit = f
        .audit_log
        .map_or_else(|| "not known".to_string(), |a| tilde(a, f.home));
    tail.extend(wrap_plain(&format!("Audit log: {audit}"), width));
    if let Some(m) = p.message() {
        tail.extend(wrap_plain(m, width));
    }
    tail.push(cut_name(
        &format!("{} {}_", p.prompt_line(), p.entry()),
        width,
    ));
    let room = height.saturating_sub(out.len() + tail.len()).max(1);
    let items = p.plan().items();
    // The "... and N more" line takes a row of its own, and the question is never the line that is cut.
    let shown = if items.len() <= room {
        items.len()
    } else {
        room.saturating_sub(1)
    };
    for i in items.iter().take(shown) {
        let path = tilde(&i.path, f.home);
        out.push(format!(
            "  {}",
            cut_name_left(&path, width.saturating_sub(2).max(4))
        ));
    }
    if shown < items.len() {
        out.push(format!("  ... and {} more items", items.len() - shown));
    }
    out.extend(tail);
    out
}

fn item_lines(run: &DeleteRun, home: Option<&Path>, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for i in run.items() {
        let path = tilde(&i.path, home);
        match &i.outcome {
            DeleteItemOutcome::Removed { bytes } => {
                let head = format!("removed  {:>9}  ", fmt_size_cell(*bytes));
                let room = width.saturating_sub(cells(&head)).max(4);
                out.push(format!("{head}{}", cut_name_left(&path, room)));
            }
            DeleteItemOutcome::Skipped { reason } => {
                out.extend(wrap_plain(&format!("skipped: {}", clean(reason)), width));
                out.push(format!(
                    "    {}",
                    cut_name_left(&path, width.saturating_sub(4).max(4))
                ));
            }
            DeleteItemOutcome::Failed { reason } => {
                out.extend(wrap_plain(&format!("error: {}", clean(reason)), width));
                out.push(format!(
                    "    {}",
                    cut_name_left(&path, width.saturating_sub(4).max(4))
                ));
            }
        }
    }
    out
}

/// A line with `left` flush left and `right` flush right when both fit.
fn spread(left: &str, right: &str, width: usize) -> String {
    let (l, r) = (cells(left), cells(right));
    if l + r + 2 <= width {
        format!("{left}{}{right}", " ".repeat(width - l - r))
    } else {
        cut_name(left, width)
    }
}

/// Keeps the last `height` lines of `lines`, saying how many earlier ones were left out.
fn last_lines(mut lines: Vec<String>, height: usize) -> Vec<String> {
    if lines.len() <= height {
        return lines;
    }
    let cut = lines.len() - height + 1;
    let mut out = vec![format!("... {cut} earlier lines")];
    out.extend(lines.drain(cut..));
    out
}

/// While the delete runs: `DELETING 1 of 2` and what is done so far. Never `APPLYING` or `DRY RUN`.
pub fn deleting_lines(
    run: &DeleteRun,
    stopping: bool,
    home: Option<&Path>,
    width: usize,
    height: usize,
) -> Vec<String> {
    let title = title_left(width, Some(run.progress()));
    let hint = if stopping {
        "stopping at the next item boundary"
    } else {
        "Ctrl-C stops at the next item"
    };
    let mut out = vec![spread(&title, hint, width), String::new()];
    let body = item_lines(run, home, width);
    out.extend(last_lines(body, height.saturating_sub(out.len())));
    out
}

/// After the delete: the result list, the freed figure and how to go on. A refused run shows its text.
pub fn result_lines(
    run: &DeleteRun,
    home: Option<&Path>,
    width: usize,
    height: usize,
) -> Vec<String> {
    const CLOSE: &str = "Esc returns to the list";
    if let Some(text) = run.refused() {
        let mut out = vec![
            spread("DELETE REFUSED - nothing was deleted.", CLOSE, width),
            String::new(),
        ];
        out.extend(wrap_plain(&clean(text), width));
        return out;
    }
    let (r, e) = (run.removed(), run.failed());
    let skipped = run.items().len() - r - e;
    let head = format!(
        "DELETE RESULT - {r} removed, {skipped} skipped, {e} error{}",
        if e == 1 { "" } else { "s" }
    );
    let mut out = vec![spread(&head, CLOSE, width), String::new()];
    let mut tail = vec![String::new()];
    tail.extend(wrap_plain(
        &format!(
            "Freed {} (measured per item). The headroom line moves as the filesystem settles.",
            fmt_size_cell(run.freed())
        ),
        width,
    ));
    if run.interrupted() {
        tail.extend(wrap_plain(
            "Stopped at an item boundary: items not reached stay marked.",
            width,
        ));
    }
    if matches!(run.end(), RunEnd::Done(_)) && e > 0 {
        tail.extend(wrap_plain(
            "Marks of items with an error stay; the others are dropped.",
            width,
        ));
    }
    let body = item_lines(run, home, width);
    out.extend(last_lines(
        body,
        height.saturating_sub(out.len() + tail.len()),
    ));
    out.extend(tail);
    out
}
