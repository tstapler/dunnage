//! `disk-cleanup kinds` (Story 1.2.4, Task 1.2.4e, `design/ux.md` S5): the table of selectors with their
//! availability in this build. Exit 0 whatever the availability.

use disk_cleanup_core::SelectorId;

use crate::render::render_kinds;
use crate::render_json::{kind_rows_with, kinds_doc, to_text};
use crate::terminal::Terminal;

/// `available` is the AVAILABLE cell of one selector (`KindGate::available_label`, or `not available in
/// this build`).
pub fn run(
    available: &dyn Fn(SelectorId) -> String,
    extra: &[SelectorId],
    json: bool,
    term: &Terminal,
) -> String {
    let rows = kind_rows_with(available, extra);
    if json {
        to_text(&kinds_doc(&rows))
    } else {
        render_kinds(&rows, term)
    }
}
