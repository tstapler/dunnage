//! The main view (Task 6.5.3a): title, headroom, path line, header, rows and the three footer lines of
//! `tui/ux-explorer.md` section 4, composed as styled lines no wider than the terminal. [`compose`] is pure:
//! a [`Screen`] in, lines out, so `TestBackend` snapshots and the real terminal draw the same text.

#![allow(clippy::many_single_char_names)]

use std::path::{Path, PathBuf};

use disk_cleanup_core::escape::escape_os;
use disk_cleanup_core::explore::fmt::{fmt_size_cell, share_cell};
use disk_cleanup_core::explore::headroom::headroom_line;
use disk_cleanup_core::explore::help::help_lines;
use disk_cleanup_core::explore::input::{scan_title, ExploreState, Mode};
use disk_cleanup_core::explore::layout::{
    cells, cut_name, footer_path_line, header_line, layout_for_width, path_line, title_left,
    title_line, too_small_lines, ColumnKind, Layout, Tier, TierLayout,
};
use disk_cleanup_core::explore::msg::{PlanPreview, ScanSummary, StopCause};
use disk_cleanup_core::explore::rows::{Badge, Row, RowId, RowKind};
use disk_cleanup_core::explore::tree::{ExploreTree, NodeId, ScanStatus};
use disk_cleanup_core::snapshot_types::FsSnapshot;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::Frame;

use crate::overlays::{
    clean, errors_lines, info_lines, plan_lines, tilde, InfoFacts, ItemCounts, UnreadableEntry,
};
use crate::theme::Theme;

/// What the scan has done so far, for the title.
#[derive(Debug, Clone, Copy)]
pub struct ScanFacts<'a> {
    pub elapsed_tenths: u64,
    pub stop: StopCause,
    pub max_seconds: Option<u64>,
    pub summary: Option<&'a ScanSummary>,
    /// `12 min ago` while a painted cache has not been revalidated (N-1).
    pub cache_age: Option<&'a str>,
}

/// Everything one frame is drawn from. Built by the loop for each frame.
pub struct Screen<'a> {
    pub tree: &'a ExploreTree,
    pub state: &'a ExploreState,
    pub theme: Theme,
    pub tty: bool,
    pub home: Option<&'a Path>,
    pub headroom: Result<&'a FsSnapshot, &'a str>,
    pub headroom_path: &'a Path,
    pub scan: ScanFacts<'a>,
    pub planning: bool,
    pub banner: Option<&'a str>,
    pub plan: Option<&'a PlanPreview>,
    pub scroll_top: usize,
    pub kind_line: Option<String>,
    pub shared: Option<String>,
    pub owner: Option<(u32, u32)>,
    pub now_unix: i64,
    pub unreadable: &'a [UnreadableEntry],
    /// `(another filesystem: /dev/sdb1)` after a new root's path.
    pub root_note: Option<&'a str>,
    /// The path being scanned, shown before the first directory arrives.
    pub scan_root: &'a Path,
    /// Footer text from the loop when the keys said nothing (the first-frame disk note, a stalled directory).
    pub notice: Option<&'a str>,
}

/// The row's path: a file below its directory, a synthetic row at its directory.
pub fn row_path(tree: &ExploreTree, row: &Row) -> PathBuf {
    match &row.id {
        RowId::Dir(id) => tree.path_of(*id),
        RowId::File(dir, name) => tree.path_of(*dir).join(name),
        RowId::Files(dir) | RowId::Smaller(dir) | RowId::Empty(dir) => tree.path_of(*dir),
    }
}

fn node_of(row: &Row) -> Option<NodeId> {
    match &row.id {
        RowId::Dir(id) => Some(*id),
        _ => None,
    }
}

/// The state character, by the precedence `x ! . + ?` (`tui/ux-explorer.md` 3.2).
pub fn state_char(tree: &ExploreTree, row: &Row) -> char {
    let below = node_of(row).is_some_and(|id| tree.unreadable_below(id) > 0);
    if row.boundary {
        'x'
    } else if row.unreadable || below {
        '!'
    } else if !row.complete {
        '.'
    } else {
        match row.badge {
            Badge::Candidate => '+',
            Badge::Possible => '?',
            Badge::None => ' ',
        }
    }
}

/// List rows that fit with the current info-pane state.
pub fn list_capacity(w: usize, h: usize, show_info: bool) -> usize {
    match layout_for_width(w, h) {
        Layout::TooSmall { .. } => 0,
        Layout::Tiered(l) => {
            if show_info {
                l.vertical.list_rows_with_info
            } else {
                l.vertical.list_rows
            }
        }
    }
}

/// The first row shown, moved the least that keeps `cursor` on screen.
pub fn adjust_scroll(top: usize, cursor: usize, capacity: usize, total: usize) -> usize {
    if capacity == 0 || total == 0 {
        return 0;
    }
    let mut top = top.min(total.saturating_sub(capacity));
    if cursor < top {
        top = cursor;
    } else if cursor >= top + capacity {
        top = cursor + 1 - capacity;
    }
    top
}

fn pad_left(s: &str, width: usize) -> String {
    format!("{}{s}", " ".repeat(width.saturating_sub(cells(s))))
}

fn pad_right(s: &str, width: usize) -> String {
    format!("{s}{}", " ".repeat(width.saturating_sub(cells(s))))
}

/// One drawn row (`draw_row` of the layout module, plus the `n/a` cells of an unscanned row and the share
/// column without a bar).
fn draw_main_row(
    l: &TierLayout,
    row: &Row,
    marked: bool,
    state: char,
    cursor: bool,
    parent_usage: u64,
    percent_only: bool,
) -> String {
    let mut out = String::with_capacity(l.width);
    out.push(if cursor { '>' } else { ' ' });
    out.push(if marked { '*' } else { ' ' });
    out.push(state);
    out.push(' ');
    let na = row.boundary;
    for c in &l.columns {
        let cell = match c.kind {
            ColumnKind::Usage if na => pad_left("n/a", c.width),
            ColumnKind::Usage => pad_left(&fmt_size_cell(row.usage), c.width),
            ColumnKind::Apparent if na => pad_left("n/a", c.width),
            ColumnKind::Apparent => pad_left(&fmt_size_cell(row.apparent), c.width),
            ColumnKind::Meta if na => pad_left("n/a", c.width),
            ColumnKind::Meta => pad_left(&row.meta.map_or("n/a".into(), fmt_size_cell), c.width),
            ColumnKind::Items if na => pad_left("n/a", c.width),
            ColumnKind::Items => pad_left(
                &row.items.map_or(
                    "n/a".into(),
                    disk_cleanup_core::explore::fmt::fmt_items_cell,
                ),
                c.width,
            ),
            ColumnKind::Share if na => pad_right("n/a", c.width),
            ColumnKind::Share => {
                let s = share_cell(row.usage, parent_usage.max(row.usage), l.bar_cells);
                if percent_only {
                    pad_right(&format!("{:>4}", s.percent), c.width)
                } else {
                    pad_right(&s.text(), c.width)
                }
            }
            ColumnKind::Name => String::new(),
        };
        out.push_str(&cell);
        out.push_str(&" ".repeat(l.gap));
    }
    out.push_str(&cut_name(&row.display_name(), l.name_width));
    out
}

fn group_thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// The title's right-hand texts, long form first.
fn title_right(s: &Screen<'_>) -> (String, String) {
    if let Some(age) = s.scan.cache_age {
        let t = format!("showing the cache from {age} until revalidated");
        return (t, format!("cache from {age}"));
    }
    let base = scan_title(
        &s.state.input,
        s.tree.status(),
        s.scan.stop,
        s.scan.max_seconds,
        s.scan.elapsed_tenths,
    );
    if matches!(s.tree.status(), ScanStatus::Complete) && !s.state.input.stop_requested {
        let temp = if s.scan.summary.is_some_and(|m| m.dirs_cached > 0) {
            "cache warm"
        } else {
            "cache cold"
        };
        let errors = s.scan.summary.map_or(0, |m| m.errors);
        let long = format!(
            "{base} ({temp}, {} dirs, {errors} unreadable)",
            group_thousands(s.tree.directories() as u64)
        );
        return (long, base);
    }
    (base.clone(), base)
}

fn split_verdict(line: &str) -> (&str, &str) {
    for word in ["OK", "WARN", "CRIT", "unknown"] {
        if let Some(head) = line.strip_suffix(word) {
            if head.ends_with(' ') || head.is_empty() {
                return (head, word);
            }
        }
    }
    (line, "")
}

fn marks_segment(s: &Screen<'_>) -> String {
    s.state
        .marks
        .summary()
        .unwrap_or_else(|| "marks: 0".to_string())
}

fn key_hints(tier: Tier, marked: bool, mode: Mode, info: bool) -> String {
    match mode {
        Mode::FilterEdit => return "type to filter  Enter keep  Esc clear".to_string(),
        Mode::QuitPrompt | Mode::Help | Mode::Errors => return String::new(),
        Mode::Normal => {}
    }
    if info {
        let delete = if marked { "  d delete" } else { "" };
        return format!("i close info  Space mark  p plan{delete}  ? help  q quit");
    }
    let third = if marked { "d delete" } else { "i info" };
    match tier {
        Tier::Wide => format!(
            "j/k move  l open  h back  s a m c n sort  Space mark  p plan  {third}  / filter  ? help  q quit"
        ),
        Tier::Compact => format!(
            "l open  h back  s/a/m/c/n sort  Space mark  p plan  {third}  ? help  q quit"
        ),
        Tier::Narrow => {
            let d = if marked { " d delete" } else { "" };
            format!("l open h back s sort Space mark p plan{d} ? help q quit")
        }
    }
}

fn plain(text: &str, w: usize, style: Style) -> Line<'static> {
    Line::styled(cut_name(&clean(text), w), style)
}

fn centred_too_small(w: usize, h: usize) -> Vec<Line<'static>> {
    let text = too_small_lines(w, h);
    let top = h.saturating_sub(text.len()) / 2;
    let mut out: Vec<Line<'static>> = (0..top).map(|_| Line::raw("")).collect();
    out.extend(text.into_iter().map(Line::raw));
    out
}

fn overlay_lines(lines: Vec<String>, w: usize, h: usize, theme: Theme) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .take(h)
        .enumerate()
        .map(|(i, l)| {
            let style = if i == 0 {
                theme.heading()
            } else {
                Style::default()
            };
            Line::styled(cut_name(&clean(&l), w), style)
        })
        .collect()
}

fn info_facts(s: &Screen<'_>, row: &Row, parent_usage: u64) -> InfoFacts {
    let node = node_of(row);
    let items = match (node, row.items) {
        (Some(id), Some(total)) => {
            let t = s.tree.total(id);
            Some(ItemCounts {
                total,
                files: t.files,
                dirs: t.dirs,
            })
        }
        (None, Some(total)) => Some(ItemCounts {
            total,
            files: total,
            dirs: 0,
        }),
        _ => None,
    };
    let share = share_cell(row.usage, parent_usage.max(row.usage), 5).percent;
    let scan_state = scan_state_text(s.tree, row);
    InfoFacts {
        path: row_path(s.tree, row),
        kind: s
            .kind_line
            .clone()
            .unwrap_or_else(|| "not a cleaner item: deleted here as a plain path".to_string()),
        usage: row.usage,
        apparent: row.apparent,
        meta: row.meta,
        items,
        share_percent: share,
        newest_dir_mtime: node.map(|id| s.tree.newest_mtime(id)),
        now_unix: s.now_unix,
        owner: s.owner,
        shared: s.shared.clone(),
        scan_state,
    }
}

fn scan_state_text(tree: &ExploreTree, row: &Row) -> String {
    if row.boundary {
        return "other filesystem: not scanned (Enter scans it)".to_string();
    }
    let Some(id) = node_of(row) else {
        return "complete, 0 unreadable".to_string();
    };
    if tree.is_unreadable(id) {
        return "unreadable: sizes are at least".to_string();
    }
    let below = tree.unreadable_below(id);
    let base = if row.complete {
        "complete"
    } else if tree.is_at_least(id) {
        "stopped before this directory finished: sizes are at least"
    } else {
        "still scanning: sizes are at least"
    };
    format!(
        "{base}, {below} unreadable{}",
        if below > 0 { " below" } else { "" }
    )
}

/// The footer's second line when no key left a message: what the cursor row's state means, or the marks.
fn row_note(s: &Screen<'_>, row: Option<&Row>) -> Option<String> {
    if let Some(row) = row {
        if let Some(id) = node_of(row) {
            if s.tree.is_unreadable(id) || s.tree.unreadable_below(id) > 0 {
                return Some(format!(
                    "{} has unreadable parts: size is at least. e = errors.",
                    row.display_name()
                ));
            }
        }
        if row.at_least {
            return Some("at least: scan stopped before this directory finished".to_string());
        }
    }
    s.state
        .marks
        .summary()
        .map(|m| format!("{m}. p previews the plan, d reviews and deletes it."))
}

fn location_text(s: &Screen<'_>) -> String {
    let mut t = match s.state.view.dir() {
        Some(dir) => tilde(&s.tree.path_of(dir), s.home),
        None => tilde(s.scan_root, s.home),
    };
    if let Some(note) = s.root_note {
        t.push(' ');
        t.push_str(note);
    }
    t
}

fn size_segment(s: &Screen<'_>, wide: bool) -> String {
    let Some(dir) = s.state.view.dir() else {
        return String::new();
    };
    let total = s.tree.total(dir);
    let usage = fmt_size_cell(total.allocated_bytes);
    if !s.tree.is_complete(dir) && s.tree.status() == ScanStatus::Scanning {
        return format!("at least {usage} so far");
    }
    if s.tree.is_at_least(dir) {
        return format!("at least {usage}");
    }
    if wide {
        format!("{usage} ({} apparent)", fmt_size_cell(total.size_bytes))
    } else {
        usage
    }
}

fn main_lines(s: &Screen<'_>, l: &TierLayout, w: usize, h: usize) -> Vec<Line<'static>> {
    let theme = s.theme;
    let st = s.state;
    let mut out: Vec<Line<'static>> = Vec::with_capacity(h);

    // 1. title
    let (long, short) = title_right(s);
    out.push(plain(
        &title_line(&title_left(w, None), &long, &short, w),
        w,
        Style::default(),
    ));

    // 2. headroom, the verdict word styled
    let head = headroom_line(s.headroom_path, s.headroom, w);
    let (body, verdict) = split_verdict(&head);
    out.push(Line::from(vec![
        Span::raw(clean(body)),
        Span::styled(verdict.to_string(), theme.verdict(verdict)),
    ]));

    // 3. path line
    let mut segs: Vec<String> = Vec::new();
    let size = size_segment(s, l.tier == Tier::Wide);
    if !size.is_empty() {
        segs.push(size);
    }
    segs.extend(st.view.path_segments());
    if let Some(note) = st.view.hidden_column_note(l) {
        segs.push(note);
    }
    segs.push(marks_segment(s));
    let seg_refs: Vec<&str> = segs.iter().map(String::as_str).collect();
    out.push(plain(
        &path_line(&location_text(s), &seg_refs, w),
        w,
        Style::default(),
    ));

    // 4. header
    out.push(plain(
        &header_line(l, st.view.sort_mark(l)),
        w,
        theme.heading(),
    ));

    // body: rows, then the info pane
    let body_h = l.vertical.list_rows;
    let rows = st.view.rows();
    let cursor = st.view.cursor_index();
    let info_open = st.input.show_info;
    let cap = if info_open {
        l.vertical.list_rows_with_info
    } else {
        body_h
    };
    let top = adjust_scroll(s.scroll_top, cursor, cap, rows.len());
    let parent_usage = st.view.dir().map_or(0, |d| s.tree.total(d).allocated_bytes);
    let mut body: Vec<Line<'static>> = Vec::new();
    if !(info_open && l.vertical.info_replaces_list) {
        for (i, row) in rows.iter().enumerate().skip(top).take(cap) {
            let is_cursor = i == cursor;
            let marked = st.marks.contains(&row_path(s.tree, row));
            let mut text = draw_main_row(
                l,
                row,
                marked,
                state_char(s.tree, row),
                is_cursor,
                parent_usage,
                st.input.percent_only,
            );
            let style = if is_cursor {
                text.push_str(&" ".repeat(w.saturating_sub(cells(&text))));
                theme.cursor()
            } else if row.complete && row.kind == RowKind::Dir {
                theme.done_row()
            } else {
                Style::default()
            };
            body.push(Line::styled(clean(&text), style));
        }
    }
    if info_open {
        if let Some(row) = st.view.cursor_row() {
            let facts = info_facts(s, row, parent_usage);
            let room = if l.vertical.info_replaces_list {
                body_h
            } else {
                body_h.saturating_sub(body.len())
            };
            for line in info_lines(&facts, w, s.tty).into_iter().take(room) {
                body.push(Line::raw(clean(&line)));
            }
        }
    }
    body.truncate(body_h);
    while body.len() < body_h {
        body.push(Line::raw(""));
    }
    out.extend(body);

    // footer: path of the cursor row, message, key hints
    let cursor_row = st.view.cursor_row();
    let path_text = cursor_row.map_or(String::new(), |r| {
        escape_os(row_path(s.tree, r).as_os_str(), s.tty)
    });
    out.push(plain(&footer_path_line(&path_text, w), w, Style::default()));
    let message = if let Some(b) = s.banner {
        Some(b.to_string())
    } else if s.planning {
        Some("PLANNING... Esc cancels the plan, q leaves.".to_string())
    } else if st.input.mode == Mode::FilterEdit {
        Some(format!("filter: /{}_", st.view.text_filter()))
    } else {
        st.footer()
            .or_else(|| st.view.footer_note())
            .or_else(|| s.notice.map(str::to_string))
            .or_else(|| row_note(s, cursor_row))
    };
    out.push(plain(&message.unwrap_or_default(), w, Style::default()));
    out.push(plain(
        &key_hints(l.tier, !st.marks.is_empty(), st.input.mode, info_open),
        w,
        Style::default(),
    ));
    out
}

/// The screen as one styled line per terminal row, each at most `w` cells wide; `h` lines at most.
pub fn compose(s: &Screen<'_>, w: usize, h: usize) -> Vec<Line<'static>> {
    let layout = layout_for_width(w, h);
    let Layout::Tiered(l) = layout else {
        return centred_too_small(w, h);
    };
    if let Some(plan) = s.plan {
        return overlay_lines(plan_lines(plan, w, h, s.tty), w, h, s.theme);
    }
    match s.state.input.mode {
        Mode::Help => overlay_lines(help_lines(w), w, h, s.theme),
        Mode::Errors => overlay_lines(errors_lines(s.unreadable, w, h, s.tty), w, h, s.theme),
        _ => {
            let mut v = main_lines(s, &l, w, h);
            v.truncate(h);
            v
        }
    }
}

/// Draws the screen into `frame`.
pub fn render(frame: &mut Frame<'_>, s: &Screen<'_>) {
    let area = frame.area();
    let lines = compose(s, usize::from(area.width), usize::from(area.height));
    let buf = frame.buffer_mut();
    for (i, line) in lines.iter().enumerate() {
        let Ok(y) = u16::try_from(i) else { break };
        if y >= area.height {
            break;
        }
        buf.set_line(area.x, area.y + y, line, area.width);
    }
}
