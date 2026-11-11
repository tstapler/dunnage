//! Path and name escaping (UX-57, `design/ux.md` 3.6), the one implementation for the report, `--text`,
//! `--json` and the explorer. Relocated from `cli` by Task 6.2.3a; `cli::escape` re-exports it.

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::os::unix::ffi::OsStrExt;

/// Unicode 16.0.0 format (Cf), line and paragraph separator (Zl, Zp) characters: bidi overrides, zero-width
/// characters and the like, as 21 ranges. Listed because `std` has no category tables; a golden test pins it.
pub const FORMAT_RANGES: &[(u32, u32)] = &[
    (0x00AD, 0x00AD),
    (0x0600, 0x0605),
    (0x061C, 0x061C),
    (0x06DD, 0x06DD),
    (0x070F, 0x070F),
    (0x0890, 0x0891),
    (0x08E2, 0x08E2),
    (0x180E, 0x180E),
    (0x200B, 0x200F),
    (0x2028, 0x202E),
    (0x2060, 0x2064),
    (0x2066, 0x206F),
    (0xFEFF, 0xFEFF),
    (0xFFF9, 0xFFFB),
    (0x110BD, 0x110BD),
    (0x110CD, 0x110CD),
    (0x13430, 0x1343F),
    (0x1BCA0, 0x1BCA3),
    (0x1D173, 0x1D17A),
    (0xE0001, 0xE0001),
    (0xE0020, 0xE007F),
];

fn is_format(c: char) -> bool {
    let u = u32::from(c);
    FORMAT_RANGES.iter().any(|&(lo, hi)| (lo..=hi).contains(&u))
}

/// A character that must never reach a terminal as itself: C0, DEL, C1 and the Cf, Zl and Zp categories.
pub fn needs_escape(c: char) -> bool {
    c.is_control() || is_format(c)
}

fn push_char(out: &mut String, c: char, tty: bool) {
    use std::fmt::Write as _;
    match c {
        '\\' => out.push_str("\\\\"),
        '\n' => out.push_str("\\n"),
        '\t' => out.push_str("\\t"),
        '\r' => out.push_str("\\r"),
        c if c.is_ascii() && !c.is_ascii_control() => out.push(c),
        c if c.is_ascii() => {
            let _ = write!(out, "\\x{:02x}", u32::from(c));
        }
        c if needs_escape(c) || !tty => {
            let _ = write!(out, "\\u{{{:x}}}", u32::from(c));
        }
        c => out.push(c),
    }
}

/// The reversible escaping of one name. On a TTY a valid non-ASCII character prints as itself; otherwise it
/// prints as `\u{HEX}` so the output is ASCII only. Never truncates.
pub fn escape(bytes: &[u8], tty: bool) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut rest = bytes;
    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(s) => {
                s.chars().for_each(|c| push_char(&mut out, c, tty));
                break;
            }
            Err(e) => {
                let (good, after) = rest.split_at(e.valid_up_to());
                std::str::from_utf8(good)
                    .unwrap_or_default()
                    .chars()
                    .for_each(|c| push_char(&mut out, c, tty));
                let bad = e.error_len().unwrap_or(after.len());
                for b in &after[..bad] {
                    let _ = write!(out, "\\x{b:02x}");
                }
                rest = &after[bad..];
            }
        }
    }
    out
}

/// [`escape`] of an `OsStr`.
pub fn escape_os(s: &OsStr, tty: bool) -> String {
    escape(s.as_bytes(), tty)
}

/// One escape sequence at the start of `s` (which begins with `\`): its bytes and its length in chars.
fn decode_escape(s: &[char]) -> Option<(Vec<u8>, usize)> {
    match s.get(1)? {
        '\\' => Some((vec![b'\\'], 2)),
        'n' => Some((vec![b'\n'], 2)),
        't' => Some((vec![b'\t'], 2)),
        'r' => Some((vec![b'\r'], 2)),
        'x' if s.len() >= 4 => {
            let hex: String = s[2..4].iter().collect();
            u8::from_str_radix(&hex, 16).ok().map(|b| (vec![b], 4))
        }
        'u' if s.get(2) == Some(&'{') => {
            let close = s.iter().position(|c| *c == '}')?;
            let hex: String = s[3..close].iter().collect();
            let c = char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?;
            Some((c.to_string().into_bytes(), close + 1))
        }
        _ => None,
    }
}

/// The exact bytes of a name, recovered from its escaped form.
pub fn unescape(s: &str) -> Vec<u8> {
    let chars: Vec<char> = s.chars().collect();
    let mut out: Vec<u8> = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let decoded = (chars[i] == '\\')
            .then(|| decode_escape(&chars[i..]))
            .flatten();
        if let Some((bytes, used)) = decoded {
            out.extend_from_slice(&bytes);
            i += used;
        } else {
            out.extend_from_slice(chars[i].to_string().as_bytes());
            i += 1;
        }
    }
    out
}

/// `path_hex` of the JSON documents: the exact bytes, present only when the name is not valid UTF-8.
pub fn path_hex(s: &OsStr) -> Option<String> {
    let b = s.as_bytes();
    std::str::from_utf8(b).is_err().then(|| {
        b.iter().fold(String::new(), |mut acc, x| {
            let _ = write!(acc, "{x:02x}");
            acc
        })
    })
}
