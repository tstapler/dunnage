//! The explorer's layout rule (`tui/ux-explorer.md` 3.1 to 3.3): tiers and columns by terminal width, the
//! vertical budget, the too-small message, name cutting, and the drawn title, header, row, path and footer
//! lines. Pure: every function maps numbers and text to text, so no drawn line is ever wider than the
//! terminal and the same lines serve the TUI and its tests.
//!
//! Text handed in is already escaped (`crate::escape`), so it holds no control character; widths are the
//! `unicode-width` cell widths and a name is cut only between atoms (one escape sequence, or one character
//! with its zero-width followers).

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::fmt::{fmt_items_cell, fmt_size_cell, share_cell};

/// Narrowest and shortest terminal that draws the list (UX-T14).
pub const MIN_WIDTH: usize = 60;
pub const MIN_HEIGHT: usize = 12;
/// The row prefix: cursor, mark, state, one space.
pub const PREFIX_WIDTH: usize = 4;
/// Lines the info pane takes from the list, and the height below which it replaces the list.
pub const INFO_LINES: usize = 8;
pub const INFO_REPLACES_BELOW: usize = 20;
/// Title, headroom, path line, header and the three footer lines.
const CHROME_LINES: usize = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Wide,
    Compact,
    Narrow,
}

/// A column, also the sort target (`Name` is never in a tier's column list).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnKind {
    Usage,
    Apparent,
    Meta,
    Items,
    Share,
    Name,
}

impl ColumnKind {
    pub fn label(self) -> &'static str {
        match self {
            ColumnKind::Usage => "USAGE",
            ColumnKind::Apparent => "APPARENT",
            ColumnKind::Meta => "META",
            ColumnKind::Items => "ITEMS",
            ColumnKind::Share => "SHARE",
            ColumnKind::Name => "NAME",
        }
    }

    fn right_aligned(self) -> bool {
        !matches!(self, ColumnKind::Share | ColumnKind::Name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Column {
    pub kind: ColumnKind,
    pub width: usize,
}

/// The rows of the screen that depend on its height (G-7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vertical {
    pub list_rows: usize,
    pub info_lines: usize,
    /// Below 20 rows `i` shows the pane instead of the list.
    pub info_replaces_list: bool,
    /// List rows while the pane is open beside the list (0 when it replaces the list).
    pub list_rows_with_info: usize,
}

/// `h` terminal rows: 7 are chrome, the rest is the list.
pub fn vertical_budget(h: usize) -> Vertical {
    let list_rows = h.saturating_sub(CHROME_LINES);
    let replaces = h < INFO_REPLACES_BELOW;
    Vertical {
        list_rows,
        info_lines: INFO_LINES,
        info_replaces_list: replaces,
        list_rows_with_info: if replaces {
            0
        } else {
            list_rows.saturating_sub(INFO_LINES)
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierLayout {
    pub tier: Tier,
    pub width: usize,
    pub columns: Vec<Column>,
    /// Characters between two columns.
    pub gap: usize,
    pub bar_cells: usize,
    /// Prefix, columns and the gaps up to NAME: 65, 55 or 35.
    pub fixed: usize,
    pub name_width: usize,
    pub vertical: Vertical,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Layout {
    Tiered(TierLayout),
    /// Below 60 columns or 12 rows (or a 0x0 size): only [`too_small_lines`] is drawn.
    TooSmall {
        width: usize,
        height: usize,
    },
}

/// The tier of `ux-explorer.md` 3.2 for a `w` x `h` terminal.
pub fn layout_for_width(w: usize, h: usize) -> Layout {
    if w < MIN_WIDTH || h < MIN_HEIGHT {
        return Layout::TooSmall {
            width: w,
            height: h,
        };
    }
    let col = |kind, width| Column { kind, width };
    let (tier, gap, bar_cells, columns) = if w >= 100 {
        (
            Tier::Wide,
            2,
            10,
            vec![
                col(ColumnKind::Usage, 9),
                col(ColumnKind::Apparent, 9),
                col(ColumnKind::Meta, 9),
                col(ColumnKind::Items, 7),
                col(ColumnKind::Share, 17),
            ],
        )
    } else if w >= 80 {
        (
            Tier::Compact,
            1,
            5,
            vec![
                col(ColumnKind::Usage, 9),
                col(ColumnKind::Apparent, 9),
                col(ColumnKind::Meta, 9),
                col(ColumnKind::Items, 7),
                col(ColumnKind::Share, 12),
            ],
        )
    } else {
        (
            Tier::Narrow,
            1,
            5,
            vec![
                col(ColumnKind::Usage, 9),
                col(ColumnKind::Items, 7),
                col(ColumnKind::Share, 12),
            ],
        )
    };
    let fixed = PREFIX_WIDTH + columns.iter().map(|c| c.width).sum::<usize>() + gap * columns.len();
    Layout::Tiered(TierLayout {
        tier,
        width: w,
        columns,
        gap,
        bar_cells,
        fixed,
        name_width: w - fixed,
        vertical: vertical_budget(h),
    })
}

/// Largest figure of a too-small message, so its lines stay within 36 columns.
const DIM_CAP: usize = 999;

/// The 3-line message (UX-T14), each line at most 36 columns, centred and clipped to `w` columns and
/// `h` rows. Never panics for any size, including 0x0.
pub fn too_small_lines(w: usize, h: usize) -> Vec<String> {
    let lines = [
        format!(
            "Too small: need {MIN_WIDTH}x{MIN_HEIGHT}, have {}x{}.",
            w.min(DIM_CAP),
            h.min(DIM_CAP)
        ),
        "Resize the window, press q to quit,".to_string(),
        "or run: disk-cleanup explore --text".to_string(),
    ];
    lines
        .into_iter()
        .take(h)
        .map(|l| {
            let clipped: String = l.chars().take(w).collect();
            let pad = (w - clipped.chars().count()) / 2;
            format!("{}{clipped}", " ".repeat(pad))
        })
        .collect()
}

/// Display width in terminal cells.
pub fn cells(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Length in bytes of the escape sequence `escape` wrote at the start of `s`, if one starts there.
fn escape_len(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    if b.first() != Some(&b'\\') {
        return None;
    }
    match b.get(1)? {
        b'\\' | b'n' | b't' | b'r' => Some(2),
        b'x' if b.len() >= 4 && b[2..4].iter().all(u8::is_ascii_hexdigit) => Some(4),
        b'u' if b.get(2) == Some(&b'{') => {
            let close = s.find('}')?;
            let body = &s[3..close];
            (!body.is_empty() && body.bytes().all(|c| c.is_ascii_hexdigit())).then_some(close + 1)
        }
        _ => None,
    }
}

/// The units a name may be cut between: one escape sequence, or one character with the zero-width
/// characters that follow it.
pub fn split_atoms(s: &str) -> Vec<&str> {
    let mut atoms = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let rest = &s[i..];
        let mut n =
            escape_len(rest).unwrap_or_else(|| rest.chars().next().map_or(1, char::len_utf8));
        if escape_len(rest).is_none() {
            for c in rest[n..].chars() {
                if c == '\\' || UnicodeWidthChar::width(c) != Some(0) {
                    break;
                }
                n += c.len_utf8();
            }
        }
        atoms.push(&rest[..n]);
        i += n;
    }
    atoms
}

/// `name` cut to at most `width` cells; a longer name ends in `...`. Cut between atoms only.
pub fn cut_name(name: &str, width: usize) -> String {
    if cells(name) <= width {
        return name.to_string();
    }
    if width <= 3 {
        return ".".repeat(width);
    }
    let mut out = String::new();
    let mut used = 0;
    for a in split_atoms(name) {
        let w = cells(a);
        if used + w > width - 3 {
            break;
        }
        out.push_str(a);
        used += w;
    }
    out.push_str("...");
    out
}

/// `name` cut from the left to at most `width` cells; a longer name starts with `...`.
pub fn cut_name_left(name: &str, width: usize) -> String {
    if cells(name) <= width {
        return name.to_string();
    }
    if width <= 3 {
        return ".".repeat(width);
    }
    let mut kept: Vec<&str> = Vec::new();
    let mut used = 0;
    for a in split_atoms(name).into_iter().rev() {
        let w = cells(a);
        if used + w > width - 3 {
            break;
        }
        kept.push(a);
        used += w;
    }
    kept.reverse();
    format!("...{}", kept.concat())
}

fn pad_left(s: &str, width: usize) -> String {
    format!("{}{s}", " ".repeat(width.saturating_sub(cells(s))))
}

fn pad_right(s: &str, width: usize) -> String {
    format!("{s}{}", " ".repeat(width.saturating_sub(cells(s))))
}

/// Left text `Explore` title (`3.1`): the long form from 80 columns, `EXPLORE (d deletes)` below, and the
/// delete progress while one runs.
pub fn title_left(w: usize, deleting: Option<(usize, usize)>) -> String {
    match deleting {
        Some((done, of)) => format!("DELETING {done} of {of}"),
        None if w < 80 => "EXPLORE (d deletes)".to_string(),
        None => "EXPLORE - nothing is deleted until you confirm.".to_string(),
    }
}

/// Line 1: `left` flush left and the scan state flush right. The long right-hand text is used only when
/// `left + right + 2 <= w`; otherwise the short one if it fits, otherwise the left text alone (cut).
pub fn title_line(left: &str, right_long: &str, right_short: &str, w: usize) -> String {
    let l = cells(left);
    for right in [right_long, right_short] {
        let r = cells(right);
        if l + r + 2 <= w {
            return format!("{left}{}{right}", " ".repeat(w - l - r));
        }
    }
    cut_name(left, w)
}

/// Line 3: `location` then each segment after three spaces, dropping trailing segments (last first) until
/// the line fits; the location alone is cut when it does not fit by itself.
pub fn path_line(location: &str, segments: &[&str], w: usize) -> String {
    for keep in (0..=segments.len()).rev() {
        let mut line = location.to_string();
        for s in &segments[..keep] {
            line.push_str("   ");
            line.push_str(s);
        }
        if cells(&line) <= w {
            return line;
        }
    }
    cut_name(location, w)
}

/// Footer line 1: `Path: ` and the full escaped path of the cursor row, cut from the left with a `...`
/// prefix of the path when the line is longer than `w`.
pub fn footer_path_line(escaped_path: &str, w: usize) -> String {
    const LABEL: &str = "Path: ";
    if cells(LABEL) + cells(escaped_path) <= w {
        return format!("{LABEL}{escaped_path}");
    }
    let room = w.saturating_sub(cells(LABEL));
    if room < 4 {
        return cut_name(LABEL.trim_end(), w);
    }
    format!("{LABEL}{}", cut_name_left(escaped_path, room))
}

/// The sort mark of a header: which column, and whether the order is descending (`v`) or ascending (`^`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortMark {
    pub column: ColumnKind,
    pub descending: bool,
}

/// The header line: labels aligned over their columns, the sorted one followed by `v` or `^`. Not padded.
pub fn header_line(l: &TierLayout, sort: Option<SortMark>) -> String {
    let label = |kind: ColumnKind| -> String {
        match sort {
            Some(m) if m.column == kind => {
                format!("{}{}", kind.label(), if m.descending { 'v' } else { '^' })
            }
            _ => kind.label().to_string(),
        }
    };
    let mut out = " ".repeat(PREFIX_WIDTH);
    for c in &l.columns {
        let text = label(c.kind);
        out.push_str(&if c.kind.right_aligned() {
            pad_left(&text, c.width)
        } else {
            pad_right(&text, c.width)
        });
        out.push_str(&" ".repeat(l.gap));
    }
    out.push_str(&cut_name(&label(ColumnKind::Name), l.name_width));
    out.trim_end().to_string()
}

/// One list row's data. Sizes are raw; the drawing applies the cell rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowCells {
    pub cursor: bool,
    pub marked: bool,
    /// `.`, `!`, `x`, `+`, `?`, `p` or a space (`3.2`).
    pub state: char,
    pub usage: u64,
    pub apparent: u64,
    /// `None` reads `n/a` (off btrfs, or a file).
    pub meta: Option<u64>,
    /// `None` reads `n/a` (a file).
    pub items: Option<u64>,
    pub parent_usage: u64,
    /// Escaped; directories end with `/`.
    pub name: String,
}

/// One drawn row, at most `l.width` cells wide.
pub fn draw_row(l: &TierLayout, r: &RowCells) -> String {
    let mut out = String::with_capacity(l.width);
    out.push(if r.cursor { '>' } else { ' ' });
    out.push(if r.marked { '*' } else { ' ' });
    out.push(r.state);
    out.push(' ');
    for c in &l.columns {
        let cell = match c.kind {
            ColumnKind::Usage => pad_left(&fmt_size_cell(r.usage), c.width),
            ColumnKind::Apparent => pad_left(&fmt_size_cell(r.apparent), c.width),
            ColumnKind::Meta => pad_left(&r.meta.map_or("n/a".into(), fmt_size_cell), c.width),
            ColumnKind::Items => pad_left(&r.items.map_or("n/a".into(), fmt_items_cell), c.width),
            ColumnKind::Share => pad_right(
                &share_cell(r.usage, r.parent_usage, l.bar_cells).text(),
                c.width,
            ),
            ColumnKind::Name => String::new(),
        };
        out.push_str(&cell);
        out.push_str(&" ".repeat(l.gap));
    }
    out.push_str(&cut_name(&r.name, l.name_width));
    out
}
