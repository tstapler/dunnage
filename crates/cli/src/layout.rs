//! The row layout of `design/ux.md` 3.6, ported from `implementation/prototypes/layout_rows.py`.
//!
//! W is the terminal width (at least 20). A row prints on one line when its fixed columns, path and reason
//! fit in W; otherwise it is a block: the fixed columns, then the reason word-wrapped, then the path
//! hard-wrapped, each indented two columns and never split inside one escape sequence. No line is longer
//! than W and nothing is truncated.

use crate::terminal::MIN_WIDTH;

/// Longest escape sequence `escape` writes: `\u{10ffff}`.
const LONGEST_ESCAPE: usize = 10;

fn chars(s: &str) -> usize {
    s.chars().count()
}

/// Length in chars of the escape sequence that starts at `s[i..]`, if one does.
fn escape_len(cs: &[char], i: usize) -> Option<usize> {
    if cs[i] != '\\' {
        return None;
    }
    match cs.get(i + 1)? {
        '\\' | 'n' | 't' | 'r' => Some(2),
        'x' if cs.len() >= i + 4
            && cs[i + 2..i + 4]
                .iter()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()) =>
        {
            Some(4)
        }
        'u' if cs.get(i + 2) == Some(&'{') => {
            let close = cs[i..].iter().position(|c| *c == '}')?;
            let body = &cs[i + 3..i + close];
            (!body.is_empty() && body.iter().all(char::is_ascii_hexdigit)).then_some(close + 1)
        }
        _ => None,
    }
}

/// Units that must not be broken: an escape sequence or one character.
fn atoms(s: &str) -> Vec<String> {
    let cs: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < cs.len() {
        let n = escape_len(&cs, i).unwrap_or(1);
        out.push(cs[i..i + n].iter().collect());
        i += n;
    }
    out
}

/// Cut `s` into lines of at most `width` characters, never inside an escape sequence.
pub fn hard_wrap(s: &str, width: usize) -> Vec<String> {
    let width = width.max(LONGEST_ESCAPE);
    let mut lines = Vec::new();
    let mut cur = String::new();
    let mut len = 0;
    for a in atoms(s) {
        let n = chars(&a);
        if len + n > width {
            lines.push(std::mem::take(&mut cur));
            len = 0;
        }
        cur.push_str(&a);
        len += n;
    }
    lines.push(cur);
    lines
}

/// Word-wrap to `width`; a word longer than `width` is hard-wrapped.
pub fn word_wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(LONGEST_ESCAPE);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split(' ') {
        let pieces = if chars(word) > width {
            hard_wrap(word, width)
        } else {
            vec![word.to_string()]
        };
        for piece in pieces {
            if cur.is_empty() {
                cur = piece;
            } else if chars(&cur) + 1 + chars(&piece) <= width {
                cur.push(' ');
                cur.push_str(&piece);
            } else {
                lines.push(std::mem::replace(&mut cur, piece));
            }
        }
    }
    lines.push(cur);
    lines
}

/// A non-row line wrapped to `width`: the first line starts with `first`, later lines with `rest`.
pub fn wrap_line(first: &str, rest: &str, text: &str, width: usize) -> Vec<String> {
    if chars(first) + chars(text) <= width {
        return vec![format!("{first}{text}")];
    }
    // Leading spaces of the text are indentation, kept on every line.
    let lead = text.len() - text.trim_start_matches(' ').len();
    if lead > 0 {
        let pad = " ".repeat(lead);
        return wrap_line(
            &format!("{first}{pad}"),
            &format!("{rest}{pad}"),
            &text[lead..],
            width,
        );
    }
    let inner = width.saturating_sub(chars(first).max(chars(rest)));
    let mut out = Vec::new();
    for (i, l) in word_wrap(text, inner).into_iter().enumerate() {
        out.push(format!("{}{l}", if i == 0 { first } else { rest }));
    }
    out
}

/// One optional fixed column after STATUS and KIND.
#[derive(Debug, Clone, Copy)]
pub struct Col {
    pub label: &'static str,
    pub width: usize,
    pub right: bool,
    /// Shown only when the terminal is at least this wide.
    pub min_width: usize,
}

/// The plan table: `STATUS KIND [TIER] SIZE [AGE] ITEM`.
pub const PLAN_COLS: [Col; 3] = [
    Col {
        label: "TIER",
        width: 11,
        right: false,
        min_width: 90,
    },
    Col {
        label: "SIZE",
        width: 9,
        right: true,
        min_width: 0,
    },
    Col {
        label: "AGE",
        width: 4,
        right: true,
        min_width: 60,
    },
];

/// The apply table: `STATUS KIND [TIME] FREED ITEM`.
pub const APPLY_COLS: [Col; 2] = [
    Col {
        label: "TIME",
        width: 5,
        right: true,
        min_width: 60,
    },
    Col {
        label: "FREED",
        width: 9,
        right: true,
        min_width: 0,
    },
];

/// One row: its fixed cells (one per column of the table, in order), then the escaped path and the reason.
#[derive(Debug, Clone)]
pub struct RowCells {
    pub status: String,
    pub kind: String,
    pub cells: Vec<String>,
    pub path: String,
    pub reason: String,
}

/// A table at one width. `key` is the index of the column kept on line 1 of a stacked (narrow) row.
#[derive(Debug, Clone)]
pub struct Table {
    pub width: usize,
    pub kind_width: usize,
    pub cols: Vec<Col>,
    pub key: usize,
}

const STATUS_W: usize = 5;
const STACK_BELOW: usize = 40;

fn pad(s: &str, w: usize, right: bool) -> String {
    let n = chars(s);
    if n >= w {
        return s.to_string();
    }
    let fill = " ".repeat(w - n);
    if right {
        format!("{fill}{s}")
    } else {
        format!("{s}{fill}")
    }
}

impl Table {
    /// `kind_width` is the longest KIND shown, capped at 18.
    pub fn new(width: usize, kind_width: usize, cols: &[Col], key: usize) -> Table {
        Table {
            width: width.max(MIN_WIDTH),
            kind_width: kind_width.clamp(4, 18),
            cols: cols.to_vec(),
            key,
        }
    }

    fn stacked(&self) -> bool {
        self.width < STACK_BELOW
    }

    fn visible(&self) -> Vec<usize> {
        (0..self.cols.len())
            .filter(|i| self.width >= self.cols[*i].min_width)
            .collect()
    }

    fn fixed(&self, status: &str, kind: &str, cell: &dyn Fn(usize) -> String) -> Vec<String> {
        if self.stacked() {
            let key = &self.cols[self.key];
            return vec![
                format!(
                    "{}  {}",
                    pad(status, STATUS_W, false),
                    pad(&cell(self.key), key.width, key.right)
                ),
                format!("  {kind}"),
            ];
        }
        let mut cols = vec![
            pad(status, STATUS_W, false),
            pad(kind, self.kind_width, false),
        ];
        for i in self.visible() {
            cols.push(pad(&cell(i), self.cols[i].width, self.cols[i].right));
        }
        vec![cols.join("  ")]
    }

    /// The header line(s) of the table.
    pub fn header(&self) -> Vec<String> {
        if self.stacked() {
            return vec![
                format!("STATUS  {}", self.cols[self.key].label),
                "  KIND".into(),
            ];
        }
        let mut head = self.fixed("STATUS", "KIND", &|i| self.cols[i].label.to_string());
        // `STATUS` is one wider than its column: pull the first separator in so the columns line up.
        for l in &mut head {
            *l = l.replacen("STATUS  ", "STATUS ", 1);
        }
        if let Some(first) = head.first_mut() {
            first.push_str("  ITEM");
        }
        head
    }

    /// The line(s) of one row.
    pub fn row(&self, r: &RowCells) -> Vec<String> {
        let head = self.fixed(&r.status, &r.kind, &|i| {
            r.cells.get(i).cloned().unwrap_or_default()
        });
        let item = if r.reason.is_empty() {
            r.path.clone()
        } else {
            format!("{}  {}", r.path, r.reason)
        };
        let single = format!("{}  {item}", head[0]);
        if head.len() == 1 && chars(&single) <= self.width {
            return vec![single];
        }
        let inner = self.width - 2;
        let mut out = head;
        if !r.reason.is_empty() {
            out.extend(
                word_wrap(&r.reason, inner)
                    .into_iter()
                    .map(|l| format!("  {l}")),
            );
        }
        out.extend(
            hard_wrap(&r.path, inner)
                .into_iter()
                .map(|l| format!("  {l}")),
        );
        out
    }
}
