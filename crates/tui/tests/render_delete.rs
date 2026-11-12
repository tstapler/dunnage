#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The delete screens (Task 6.6.4b; UX-T8, UX-T29, UX-T35, UX-T38): the confirmation with its typed entry,
//! `DELETING 1 of 2`, the result list and the refusal texts, as `TestBackend` snapshots.

mod common;

use std::path::PathBuf;

use common::*;
use dunnage_core::explore::confirm::Confirm;
use dunnage_core::explore::delete_flow::{DeleteRun, Prompt, Submitted};
use dunnage_core::explore::help::help_lines;
use dunnage_core::explore::keymap::Key;
use dunnage_core::explore::marks::{Mark, Marks};
use dunnage_core::explore::msg::{
    DeleteItemOutcome, DeleteItemResult, DeleteMsg, DeleteSummary, PlanPreview,
};
use dunnage_core::explore::ports::{DELETE_NOT_IN_BUILD, NEEDS_TERMINAL};
use dunnage_core::explore::testing::reviewed_of;
use dunnage_tui::view::DeleteScreen;
use std::path::Path;

const AUDIT: &str = "/home/u/.local/state/dunnage/audit.jsonl";

fn marks_of(paths: &[&str]) -> Marks {
    let mut m = Marks::new();
    for p in paths {
        m.add(Mark {
            path: PathBuf::from(p),
            est_bytes: 1,
            cleaner_item: false,
        })
        .unwrap();
    }
    m
}

fn prompt_of(paths: &[&str], bytes_each: u64) -> Prompt {
    let marks = marks_of(paths);
    let list: Vec<PathBuf> = marks.iter().map(|m| m.path.clone()).collect();
    Prompt::new(
        Confirm::new(reviewed_of(&list, bytes_each)),
        marks.identity(),
    )
}

fn two() -> Prompt {
    prompt_of(
        &["/home/u/code/app/node_modules", "/home/u/code/app/dist.old"],
        114 * GIB / 10,
    )
}

fn fixture() -> Fixture {
    let mut f = Fixture::new(sample_tree(true));
    f.home = Some(PathBuf::from("/home/u"));
    f
}

fn screen_of<'a>(fx: &'a Fixture, which: DeleteScreen<'a>) -> dunnage_tui::view::Screen<'a> {
    let mut screen = fx.screen();
    screen.delete = Some(which);
    screen.audit_log = Some(Path::new(AUDIT));
    screen
}

fn drawn(which: DeleteScreen<'_>, cols: u16, rows: u16) -> String {
    let fx = fixture();
    let screen = screen_of(&fx, which);
    let text = draw_text(&screen, cols, rows);
    assert_fits(&text, usize::from(cols));
    text
}

fn item(path: &str, outcome: DeleteItemOutcome) -> DeleteMsg {
    DeleteMsg::Item(DeleteItemResult {
        path: PathBuf::from(path),
        outcome,
    })
}

fn run_of(planned: usize, msgs: Vec<DeleteMsg>) -> DeleteRun {
    let mut r = DeleteRun::new(planned);
    for m in msgs {
        r.apply(m);
    }
    r
}

// ----- the prompt (UX-T35, E6) -----

#[test]
fn the_prompt_states_the_count_the_items_the_total_the_filesystem_and_the_audit_log() {
    let p = two();
    let text = drawn(DeleteScreen::Prompt(&p), 80, 24);
    insta::assert_snapshot!("delete_prompt_80x24", text);
    for needle in [
        "nothing goes to a trash",
        "covers exactly 2 marked items",
        "~/code/app/node_modules",
        "~/code/app/dist.old",
        "Total 22.8 GiB (attributed) on / (btrfs).",
        "Audit log: ~/.local/state/dunnage/audit.jsonl",
        "Type 2 and press Enter to delete, Esc to cancel: _",
    ] {
        assert!(text.contains(needle), "{needle}\n{text}");
    }
}

#[test]
fn the_prompt_shows_what_was_typed() {
    let mut p = two();
    p.key(Key::Char('1'));
    let text = drawn(DeleteScreen::Prompt(&p), 80, 24);
    assert!(
        text.contains("Type 2 and press Enter to delete, Esc to cancel: 1_"),
        "{text}"
    );
}

#[test]
fn a_wrong_entry_prints_that_is_not_n_and_keeps_the_prompt() {
    let mut p = two();
    p.key(Key::Char('3'));
    let Submitted::Refused(p) = p.submit() else {
        panic!("refused")
    };
    let text = drawn(DeleteScreen::Prompt(&p), 80, 24);
    insta::assert_snapshot!("delete_prompt_wrong_80x24", text);
    assert!(
        text.contains("That is not 2. Nothing was deleted."),
        "{text}"
    );
    assert!(
        text.contains("Esc to cancel: _"),
        "the entry is empty again: {text}"
    );
}

#[test]
fn the_prompt_names_one_item_in_the_singular() {
    let p = prompt_of(&["/home/u/code/app/dist"], GIB);
    let text = drawn(DeleteScreen::Prompt(&p), 80, 24);
    assert!(text.contains("covers exactly 1 marked item:"), "{text}");
    assert!(text.contains("Type 1 and press Enter"), "{text}");
}

#[test]
fn the_prompt_fits_a_narrow_short_screen_and_counts_the_items_it_had_no_room_for() {
    let names: Vec<String> = (0..12)
        .map(|i| format!("/home/u/code/app/some-long-directory-name-{i}/node_modules"))
        .collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let p = prompt_of(&refs, GIB);
    for (w, h) in [(60u16, 14u16), (80, 24), (120, 30)] {
        let text = drawn(DeleteScreen::Prompt(&p), w, h);
        assert!(text.lines().count() <= usize::from(h), "{w}x{h}\n{text}");
        // the question is never cut off, whatever the height
        assert!(
            text.contains("Type 12 and press Enter to delete, Esc to cancel: _"),
            "{w}x{h}\n{text}"
        );
        assert!(text.contains("Audit log:"), "{w}x{h}\n{text}");
        assert!(text.contains("nothing goes to a trash"), "{w}x{h}\n{text}");
        if h < 24 {
            assert!(text.contains("more items"), "{w}x{h}\n{text}");
        }
    }
    insta::assert_snapshot!(
        "delete_prompt_many_60x14",
        drawn(DeleteScreen::Prompt(&p), 60, 14)
    );
}

#[test]
fn control_bytes_in_a_marked_name_never_reach_the_screen_raw() {
    let p = prompt_of(&["/home/u/code/a\u{1b}[31mb\nc/dist"], GIB);
    let text = drawn(DeleteScreen::Prompt(&p), 80, 24);
    assert!(!text.contains('\u{1b}'), "{text:?}");
    assert!(text.contains("\\x1b") || text.contains("\\u{1b}"), "{text}");
}

// ----- DELETING (UX-T8) -----

#[test]
fn the_title_reads_deleting_n_of_m_while_it_runs() {
    let run = run_of(
        2,
        vec![
            DeleteMsg::Progress { index: 1, total: 2 },
            item(
                "/home/u/code/app/node_modules",
                DeleteItemOutcome::Removed { bytes: 21 * GIB },
            ),
            DeleteMsg::Progress { index: 2, total: 2 },
        ],
    );
    let text = drawn(
        DeleteScreen::Running {
            run: &run,
            stopping: false,
        },
        80,
        24,
    );
    insta::assert_snapshot!("delete_running_80x24", text);
    assert!(
        text.lines().next().unwrap().starts_with("DELETING 2 of 2"),
        "{text}"
    );
    let stopping = drawn(
        DeleteScreen::Running {
            run: &run,
            stopping: true,
        },
        80,
        24,
    );
    assert!(
        stopping.contains("stopping at the next item boundary"),
        "{stopping}"
    );
}

#[test]
fn the_first_item_counts_from_one_before_any_progress_arrives() {
    let run = run_of(3, vec![]);
    let text = drawn(
        DeleteScreen::Running {
            run: &run,
            stopping: false,
        },
        80,
        24,
    );
    assert!(text.starts_with("DELETING 1 of 3"), "{text}");
}

// ----- the result list (UX-T8, UX-T35) -----

fn mixed() -> DeleteRun {
    run_of(
        3,
        vec![
            DeleteMsg::Progress { index: 1, total: 3 },
            item(
                "/home/u/code/app/node_modules",
                DeleteItemOutcome::Removed {
                    bytes: 217 * GIB / 10,
                },
            ),
            DeleteMsg::Progress { index: 2, total: 3 },
            item(
                "/home/u/code/app/dist.old",
                DeleteItemOutcome::Skipped {
                    reason: "changed since the plan".into(),
                },
            ),
            DeleteMsg::Progress { index: 3, total: 3 },
            item(
                "/home/u/code/app/target",
                DeleteItemOutcome::Failed {
                    reason: "permission denied".into(),
                },
            ),
            DeleteMsg::Done(DeleteSummary {
                removed: 1,
                bytes: 217 * GIB / 10,
                interrupted: false,
            }),
        ],
    )
}

#[test]
fn the_result_list_shows_removed_skipped_and_error_with_the_freed_figure() {
    let run = mixed();
    let text = drawn(DeleteScreen::Result(&run), 80, 24);
    insta::assert_snapshot!("delete_result_80x24", text);
    for needle in [
        "DELETE RESULT - 1 removed, 1 skipped, 1 error",
        "removed   21.7 GiB  ~/code/app/node_modules",
        "skipped: changed since the plan",
        "~/code/app/dist.old",
        "error: permission denied",
        "~/code/app/target",
        "Freed 21.7 GiB (measured per item)",
        "Esc returns to the list",
    ] {
        assert!(text.contains(needle), "{needle}\n{text}");
    }
}

#[test]
fn the_result_list_of_an_interrupted_run_says_what_stays_marked() {
    let run = run_of(
        3,
        vec![
            item(
                "/home/u/code/app/node_modules",
                DeleteItemOutcome::Removed { bytes: GIB },
            ),
            DeleteMsg::Done(DeleteSummary {
                removed: 1,
                bytes: GIB,
                interrupted: true,
            }),
        ],
    );
    let text = drawn(DeleteScreen::Result(&run), 80, 24);
    assert!(text.contains("items not reached stay marked"), "{text}");
}

#[test]
fn a_long_result_list_keeps_its_end_and_the_freed_line_on_a_short_screen() {
    let mut msgs = Vec::new();
    for i in 0..15 {
        msgs.push(item(
            &format!("/home/u/code/app/d{i}"),
            DeleteItemOutcome::Removed { bytes: GIB },
        ));
    }
    msgs.push(DeleteMsg::Done(DeleteSummary {
        removed: 15,
        bytes: 15 * GIB,
        interrupted: false,
    }));
    let run = run_of(15, msgs);
    let text = drawn(DeleteScreen::Result(&run), 80, 14);
    assert!(text.lines().count() <= 14, "{text}");
    assert!(text.contains("Freed 15.0 GiB"), "{text}");
    assert!(
        text.contains("~/code/app/d14"),
        "the last item is shown: {text}"
    );
    assert!(text.contains("earlier lines"), "{text}");
}

#[test]
fn every_refusal_shows_its_ready_made_text_and_says_nothing_was_deleted() {
    let texts = [
        (
            "lock",
            "another dunnage apply is running (pid 4242). Nothing was deleted.",
        ),
        (
            "changed",
            "Changed since you confirmed: 40.1 GiB then, 55.0 GiB now. Nothing was deleted. Press d to review again.",
        ),
        (
            "audit",
            "The audit log cannot be written (/home/u/.local/state/dunnage/audit.jsonl: No space left on device). Nothing was deleted.",
        ),
        ("not_in_build", DELETE_NOT_IN_BUILD),
        ("needs_terminal", NEEDS_TERMINAL),
    ];
    for (name, t) in texts {
        let run = run_of(2, vec![DeleteMsg::Refused(t.to_string())]);
        let text = drawn(DeleteScreen::Result(&run), 80, 24);
        insta::assert_snapshot!(format!("delete_refused_{name}_80x24"), text);
        assert!(
            text.starts_with("DELETE REFUSED - nothing was deleted."),
            "{text}"
        );
        assert!(text.contains("Nothing was deleted."), "{text}");
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(flat.contains(t), "the whole text is shown: {t}\n{text}");
    }
}

#[test]
fn a_run_whose_worker_vanished_says_to_check_the_audit_log() {
    let mut run = run_of(1, vec![]);
    run.lose();
    let text = drawn(DeleteScreen::Result(&run), 80, 24);
    assert!(text.contains("Check the audit log"), "{text}");
}

// ----- the preview's line, the help, and the words that must be gone (UX-T8, UX-T29) -----

#[test]
fn the_plan_preview_carries_the_review_line_above_its_keys() {
    let f = fixture();
    let p = PlanPreview {
        plan_id: "5d2e77a1".into(),
        saved: true,
        rows: vec![],
        est_bytes: 0,
        scope_line: "2 marked items".into(),
        guards_line: "Guards run now.".into(),
    };
    let mut s = f.screen();
    s.plan = Some(&p);
    s.plan_note = Some("Review the plan, then press d again.");
    let text = draw_text(&s, 80, 24);
    insta::assert_snapshot!("delete_plan_note_80x24", text);
    let at = |needle: &str| text.lines().position(|l| l.contains(needle)).unwrap();
    assert!(at("Review the plan, then press d again.") < at("d      delete these items"));
}

#[test]
fn the_help_lists_d_with_the_plan_and_the_typed_count() {
    let flat = help_lines(80).join(" ");
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains("d delete the marked items after a plan and a typed count"),
        "{flat}"
    );
}

#[test]
fn no_screen_says_read_only_never_deletes_applying_or_dry_run() {
    let f = fixture();
    let p = two();
    let run = mixed();
    let running = run_of(2, vec![DeleteMsg::Progress { index: 1, total: 2 }]);
    let mut texts = vec![
        draw_text(&screen_of(&f, DeleteScreen::Prompt(&p)), 80, 24),
        draw_text(
            &screen_of(
                &f,
                DeleteScreen::Running {
                    run: &running,
                    stopping: false,
                },
            ),
            80,
            24,
        ),
        draw_text(&screen_of(&f, DeleteScreen::Result(&run)), 80, 24),
        draw_text(&f.screen(), 80, 24),
        draw_text(&f.screen(), 60, 14),
    ];
    texts.push(help_lines(80).join("\n"));
    texts.push(help_lines(60).join("\n"));
    for t in texts {
        for banned in ["read-only", "never deletes", "APPLYING", "DRY RUN"] {
            assert!(!t.contains(banned), "{banned} in\n{t}");
        }
    }
}
