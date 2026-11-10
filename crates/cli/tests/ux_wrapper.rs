#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 1.2.3: the wrapper NOTE survives a pipe (UX-50, T-449).

mod support;

use disk_cleanup_cli::apply::render_refused;
use disk_cleanup_cli::render::{render_plan, wrapper_note};
use disk_cleanup_cli::render_json::plan_doc;
use disk_cleanup_cli::view::PlanView;
use disk_cleanup_core::plan::PlanId;
use support::*;

#[test]
fn wrapper_note_should_appear_on_report_line_2_and_in_json_notes_and_stderr_and_never_on_a_refusal_stdout(
) {
    let fx = Fixture::s1();
    let dash_n = wrapper_note(Some("disk-cleanup"), true).unwrap();
    let bare = wrapper_note(Some("disk-cleanup"), false).unwrap();
    assert!(dash_n.starts_with(
        "NOTE: dry-run is now the default; -n is a no-op. Copy a Next: line to delete"
    ));
    assert_eq!(bare, "NOTE: disk-cleanup no longer deletes by default; this was a dry run. Copy a Next: line from the report to delete.");
    for note in [dash_n, bare] {
        let mut input = fx.input();
        input.wrapper_note = Some(note.clone());
        let view = PlanView::build(&input);
        let out = render_plan(&input, &view, &pipe(80));
        let ls: Vec<&str> = out.lines().collect();
        assert!(ls[0].starts_with("DRY RUN - nothing will be deleted."));
        assert_eq!(ls[1], note);
        let doc = plan_doc(&input, &view, 0);
        assert!(doc["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n == &serde_json::Value::String(note.clone())));
    }
    let none = render_plan(&fx.input(), &PlanView::build(&fx.input()), &pipe(80));
    assert!(!none.contains("NOTE:"));
    assert!(wrapper_note(None, false).is_none());
    let refused = render_refused(PlanId::from_hex("91be04d7").unwrap(), None, 2, &pipe(80));
    assert!(!refused.contains("NOTE"));
}
