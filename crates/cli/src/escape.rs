//! Path and name escaping lives in `disk_cleanup_core::escape` (Task 6.2.3a); this module re-exports it and
//! keeps `shell_quote` until Task 6.2.3d moves that too.

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::os::unix::ffi::OsStrExt;

pub use disk_cleanup_core::escape::{escape, escape_os, needs_escape, path_hex, unescape};

fn is_bare_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"_/.+:,@".contains(&b)
}

/// One printed argument, quoted for the POSIX sh family and zsh: bare when it matches
/// `^[A-Za-z0-9_/.+:,@][A-Za-z0-9_/.+:,@-]*$`, `''` when empty, single-quoted with `'\''` when plain text,
/// otherwise `$'...'` with `\xNN`. The sequence `\'` is never produced. `ascii_only` is true whenever
/// stdout is not a TTY.
pub fn shell_quote(path: &OsStr, ascii_only: bool) -> String {
    let bytes = path.as_bytes();
    if bytes.is_empty() {
        return "''".to_string();
    }
    if is_bare_byte(bytes[0]) && bytes[1..].iter().all(|&b| is_bare_byte(b) || b == b'-') {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    if let Ok(t) = std::str::from_utf8(bytes) {
        let plain = !t.chars().any(needs_escape) && (!ascii_only || t.is_ascii());
        if plain {
            return format!("'{}'", t.replace('\'', "'\\''"));
        }
    }
    let mut out = String::from("$'");
    for &b in bytes {
        if b == b'\\' {
            out.push_str("\\\\");
        } else if (0x20..0x7f).contains(&b) && b != b'\'' && b != b'!' {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "\\x{b:02x}");
        }
    }
    out.push('\'');
    out
}

/// [`shell_quote`] of a `str`.
pub fn shell_quote_str(s: &str, ascii_only: bool) -> String {
    shell_quote(OsStr::new(s), ascii_only)
}
