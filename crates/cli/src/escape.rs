//! Path and name escaping lives in `dunnage_core::escape` and shell quoting in
//! `dunnage_core::shell_quote` (Tasks 6.2.3a and 6.2.3d); this module re-exports both for the
//! report, `--text` and `--json` renderers.

pub use dunnage_core::escape::{escape, escape_os, needs_escape, path_hex, unescape};
pub use dunnage_core::shell_quote::shell_quote;
