//! Path and name escaping lives in `disk_cleanup_core::escape` and shell quoting in
//! `disk_cleanup_core::shell_quote` (Tasks 6.2.3a and 6.2.3d); this module re-exports both for the
//! report, `--text` and `--json` renderers.

pub use disk_cleanup_core::escape::{escape, escape_os, needs_escape, path_hex, unescape};
pub use disk_cleanup_core::shell_quote::shell_quote;
