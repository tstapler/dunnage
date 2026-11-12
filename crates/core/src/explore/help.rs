//! The help overlay (Task 6.2.4b; UX-T29). The key lines are generated from `KEYMAP`, so the overlay cannot
//! list a key the machine does not have or miss one it does. The sentences about deleting, the badges and
//! the screen-reader limit are fixed text.

use super::keymap::{Group, KEYMAP};
use super::layout::cells;

/// First line of the overlay.
pub const TITLE: &str = "EXPLORE HELP - nothing is deleted until you confirm.";
pub const CLOSE_HINT: &str = "Esc or ? closes";
/// The two sentences about the one deleting key.
pub const DELETE_RULE: &str =
    "d deletes after a plan and a typed count. D, Delete and y do nothing.";
pub const DELETE_STEPS: &str =
    "Deleting is permanent: mark, press p, read the plan, press d, type the count";
pub const SCREEN_READER: &str =
    "This view is not screen-reader friendly: use 'dunnage explore --text'.";

const LABEL_WIDTH: usize = 10;

/// Lays `atoms` out after `label`: entries sit on a line three spaces apart and move whole to the next line
/// when they do not fit; an entry wider than the line is broken at its spaces. Continuation lines are indented.
fn wrap(label: &str, atoms: &[String], width: usize) -> Vec<String> {
    let indent = " ".repeat(LABEL_WIDTH);
    let room = width.saturating_sub(LABEL_WIDTH).max(1);
    let mut pieces: Vec<(String, &str)> = Vec::new();
    for a in atoms {
        if cells(a) <= room {
            pieces.push((a.clone(), "   "));
        } else {
            for (i, w) in a.split(' ').enumerate() {
                pieces.push((w.to_string(), if i == 0 { "   " } else { " " }));
            }
        }
    }
    let mut lines = Vec::new();
    let mut line = format!("{label:<LABEL_WIDTH$}");
    let mut used = 0usize;
    for (text, sep) in pieces {
        let need = if used == 0 {
            cells(&text)
        } else {
            cells(sep) + cells(&text)
        };
        if used > 0 && used + need > room {
            lines.push(std::mem::replace(&mut line, indent.clone()));
            used = 0;
        }
        if used > 0 {
            line.push_str(sep);
            used += cells(sep);
        }
        line.push_str(&text);
        used += cells(&text);
    }
    lines.push(line);
    lines
}

/// Plain sentences broken at spaces to fit `width`.
fn prose(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        if !line.is_empty() && cells(&line) + 1 + cells(word) > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    lines.push(line);
    lines
}

/// The key lines, one block per [`Group`].
pub fn key_lines(width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for g in Group::ALL {
        let entries: Vec<String> = KEYMAP
            .iter()
            .filter(|d| d.group == g)
            .map(|d| format!("{} {}", d.label, d.help))
            .collect();
        if !entries.is_empty() {
            out.extend(wrap(g.name(), &entries, width));
        }
    }
    out
}

/// The state and column legend.
pub fn legend_lines(width: usize) -> Vec<String> {
    let state = [
        "'.' still scanning (size is at least)",
        "'!' unreadable (at least)",
        "'x' other filesystem (Enter scans it)",
        "'+' candidate: the cleaner lists it",
        "'?' possible candidate: p checks it with git or Steam data the explorer does not have",
    ];
    let columns = [
        "USAGE bytes on disk",
        "APPARENT file sizes",
        "META est. btrfs metadata",
        "SHARE of parent",
    ];
    let mut out = wrap("State", &state.map(String::from), width);
    out.extend(wrap("Headers", &columns.map(String::from), width));
    out.extend(wrap("Access", &[SCREEN_READER.to_string()], width));
    out
}

/// The whole overlay for a screen `width` columns wide (60 or more).
pub fn help_lines(width: usize) -> Vec<String> {
    let (t, c) = (cells(TITLE), cells(CLOSE_HINT));
    let mut out = if t + c + 2 <= width {
        vec![format!("{TITLE}{}{CLOSE_HINT}", " ".repeat(width - t - c))]
    } else {
        vec![TITLE.to_string(), CLOSE_HINT.to_string()]
    };
    out.push(String::new());
    out.extend(key_lines(width));
    out.push(String::new());
    out.extend(prose(DELETE_RULE, width));
    out.extend(prose(DELETE_STEPS, width));
    out.push(String::new());
    out.extend(legend_lines(width));
    out
}
