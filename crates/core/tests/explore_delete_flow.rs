//! Task 6.6.4a: the pure delete flow (UX-T34, UX-T35, UX-T38): the reviewed-set rule, the prompt's input
//! rules, the stream of a delete and the closing line.
#![allow(clippy::unwrap_used, non_snake_case)]

use std::path::{Path, PathBuf};

use disk_cleanup_core::explore::confirm::Confirm;
use disk_cleanup_core::explore::delete_flow::{
    rescan_target, DPress, DeleteRun, Prompt, PromptKey, Remeasured, ReviewedSet, RunEnd,
    Submitted, Tally, ENTRY_MAX, MSG_WORKER_LOST,
};
use disk_cleanup_core::explore::keymap::Key;
use disk_cleanup_core::explore::marks::{Mark, Marks};
use disk_cleanup_core::explore::msg::{
    DeleteItemOutcome, DeleteItemResult, DeleteMsg, DeleteSummary,
};
use disk_cleanup_core::explore::testing::reviewed_of;
use disk_cleanup_core::FatalError;

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

fn paths(m: &Marks) -> Vec<PathBuf> {
    m.iter().map(|x| x.path.clone()).collect()
}

fn prompt_for(n: usize) -> Prompt {
    let names: Vec<String> = (0..n).map(|i| format!("/h/code/a{i}/dist")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let marks = marks_of(&refs);
    Prompt::new(
        Confirm::new(reviewed_of(&paths(&marks), 1 << 30)),
        marks.identity(),
    )
}

fn type_in(p: &mut Prompt, text: &str) {
    for c in text.chars() {
        assert_eq!(p.key(Key::Char(c)), PromptKey::Edited, "{c}");
    }
}

// ----- the reviewed-set rule (UX-T34) -----

#[test]
fn press_d_should_say_nothing_is_marked_When_there_are_no_marks() {
    assert_eq!(
        ReviewedSet::new().press_d(&Marks::new()),
        DPress::NothingMarked
    );
}

#[test]
fn press_d_should_plan_first_When_no_plan_covers_exactly_this_set() {
    let set = ReviewedSet::new();
    assert_eq!(set.press_d(&marks_of(&["/h/code/a"])), DPress::ReviewFirst);
}

#[test]
fn press_d_should_remeasure_When_the_held_plan_covers_exactly_the_marks() {
    let marks = marks_of(&["/h/code/b", "/h/code/a"]);
    let mut set = ReviewedSet::new();
    set.hold(&marks, reviewed_of(&paths(&marks), 10));
    assert_eq!(set.press_d(&marks), DPress::Remeasure);
    // the same paths in another order are the same set
    assert_eq!(
        set.press_d(&marks_of(&["/h/code/a", "/h/code/b"])),
        DPress::Remeasure
    );
}

#[test]
fn safety_press_d_should_plan_again_When_one_mark_is_added_or_removed_or_replaced() {
    let marks = marks_of(&["/h/code/a", "/h/code/b"]);
    let mut set = ReviewedSet::new();
    set.hold(&marks, reviewed_of(&paths(&marks), 10));
    for other in [
        marks_of(&["/h/code/a"]),
        marks_of(&["/h/code/a", "/h/code/b", "/h/code/c"]),
        marks_of(&["/h/code/a", "/h/code/c"]),
    ] {
        assert_eq!(
            set.press_d(&other),
            DPress::ReviewFirst,
            "{:?}",
            paths(&other)
        );
    }
}

#[test]
fn safety_a_plan_should_authorise_nothing_When_a_fresh_review_has_other_items() {
    let marks = marks_of(&["/h/code/a", "/h/code/b"]);
    let mut set = ReviewedSet::new();
    set.hold(&marks, reviewed_of(&paths(&marks), 10));
    // a fresh review whose identities differ (a marked directory replaced by another with the same name)
    let other = reviewed_of(
        &[PathBuf::from("/h/code/a"), PathBuf::from("/h/code/zz")],
        10,
    );
    let got = set.remeasured(&marks.identity(), &marks, other);
    assert!(matches!(got, Remeasured::Changed));
    assert!(set.is_empty(), "the old plan must be gone");
    assert_eq!(set.press_d(&marks), DPress::ReviewFirst);
}

#[test]
fn remeasured_should_open_the_prompt_with_the_fresh_figures_When_the_plan_id_is_the_same() {
    let marks = marks_of(&["/h/code/a", "/h/code/b"]);
    let mut set = ReviewedSet::new();
    set.hold(&marks, reviewed_of(&paths(&marks), 10));
    let fresh = reviewed_of(&paths(&marks), 7_000);
    let Remeasured::Prompt(p) = set.remeasured(&marks.identity(), &marks, fresh) else {
        panic!("expected the prompt");
    };
    assert_eq!(p.count(), 2);
    assert_eq!(p.bytes(), 14_000, "the prompt shows the fresh measure");
    assert!(set.is_empty(), "the held plan is consumed by the prompt");
}

#[test]
fn safety_remeasured_should_not_open_the_prompt_When_the_marks_changed_during_the_measure() {
    let marks = marks_of(&["/h/code/a"]);
    let mut set = ReviewedSet::new();
    set.hold(&marks, reviewed_of(&paths(&marks), 10));
    let asked = marks.identity();
    let now = marks_of(&["/h/code/a", "/h/code/b"]);
    let got = set.remeasured(&asked, &now, reviewed_of(&paths(&marks), 10));
    assert!(matches!(got, Remeasured::Changed));
}

#[test]
fn safety_remeasured_should_not_open_the_prompt_When_no_plan_is_held() {
    let marks = marks_of(&["/h/code/a"]);
    let mut set = ReviewedSet::new();
    let got = set.remeasured(&marks.identity(), &marks, reviewed_of(&paths(&marks), 10));
    assert!(matches!(got, Remeasured::Changed));
}

#[test]
fn cancel_should_keep_the_review_of_the_set_it_was_made_for() {
    let marks = marks_of(&["/h/code/a"]);
    let mut set = ReviewedSet::new();
    set.hold(&marks, reviewed_of(&paths(&marks), 10));
    let Remeasured::Prompt(p) =
        set.remeasured(&marks.identity(), &marks, reviewed_of(&paths(&marks), 10))
    else {
        panic!("prompt");
    };
    let (id, plan) = p.cancel();
    set.restore(id, plan);
    assert!(set.covers(&marks));
    assert_eq!(set.press_d(&marks), DPress::Remeasure);
}

// ----- the prompt's input rules (UX-T35) -----

#[test]
fn the_prompt_should_name_the_count_When_it_opens() {
    let p = prompt_for(2);
    assert_eq!(
        p.prompt_line(),
        "Type 2 and press Enter to delete, Esc to cancel:"
    );
    assert_eq!(p.entry(), "");
    assert_eq!(p.message(), None);
}

#[test]
fn the_exact_count_and_enter_should_give_the_confirmed_plan() {
    let mut p = prompt_for(2);
    type_in(&mut p, "2");
    assert_eq!(p.key(Key::Enter), PromptKey::Submit);
    let Submitted::Accepted(c) = p.submit() else {
        panic!("accepted");
    };
    assert_eq!(c.typed(), "2");
    assert_eq!(c.count(), 2);
}

#[test]
fn safety_a_wrong_count_should_keep_the_prompt_and_say_nothing_was_deleted() {
    let mut p = prompt_for(2);
    type_in(&mut p, "3");
    let Submitted::Refused(mut p) = p.submit() else {
        panic!("refused");
    };
    assert_eq!(p.message(), Some("That is not 2. Nothing was deleted."));
    assert_eq!(p.entry(), "", "the entry is emptied for the next try");
    // the message clears with the next key, and the right count then goes through
    type_in(&mut p, "2");
    assert_eq!(p.message(), None);
    assert!(matches!(p.submit(), Submitted::Accepted(_)));
}

#[test]
fn safety_enter_on_an_empty_entry_should_not_confirm() {
    let p = prompt_for(1);
    assert!(matches!(p.submit(), Submitted::Refused(_)));
}

#[test]
fn safety_near_misses_should_not_confirm() {
    for bad in ["02", "20", "22", "1", "0"] {
        let mut p = prompt_for(2);
        type_in(&mut p, bad);
        assert!(matches!(p.submit(), Submitted::Refused(_)), "{bad}");
    }
}

#[test]
fn the_prompt_should_ignore_every_key_but_digits_backspace_enter_esc_and_ctrl_c() {
    let mut p = prompt_for(2);
    for k in [
        Key::Char('d'),
        Key::Char('y'),
        Key::Char('D'),
        Key::Char('j'),
        Key::Char(' '),
        Key::Char('q'),
        Key::Char('+'),
        Key::Char('٣'),
        Key::Down,
        Key::Up,
        Key::Delete,
        Key::Ctrl('z'),
        Key::Ctrl('d'),
    ] {
        assert_eq!(p.key(k), PromptKey::Ignored, "{k:?}");
    }
    assert_eq!(p.entry(), "");
    assert_eq!(
        p.key(Key::Backspace),
        PromptKey::Ignored,
        "nothing to erase"
    );
}

#[test]
fn backspace_should_erase_the_last_digit() {
    let mut p = prompt_for(12);
    type_in(&mut p, "13");
    assert_eq!(p.key(Key::Backspace), PromptKey::Edited);
    assert_eq!(p.entry(), "1");
    type_in(&mut p, "2");
    assert!(matches!(p.submit(), Submitted::Accepted(_)));
}

#[test]
fn esc_and_ctrl_c_should_cancel_and_interrupt() {
    let mut p = prompt_for(2);
    assert_eq!(p.key(Key::Esc), PromptKey::Cancel);
    assert_eq!(p.key(Key::Ctrl('c')), PromptKey::Interrupt);
}

#[test]
fn the_entry_should_stop_at_the_digit_limit() {
    let mut p = prompt_for(2);
    for _ in 0..ENTRY_MAX {
        assert_eq!(p.key(Key::Char('7')), PromptKey::Edited);
    }
    assert_eq!(p.key(Key::Char('7')), PromptKey::Ignored);
    assert_eq!(p.entry().len(), ENTRY_MAX);
}

// ----- a delete in progress and its result (UX-T8, UX-T35) -----

fn removed(path: &str, bytes: u64) -> DeleteMsg {
    DeleteMsg::Item(DeleteItemResult {
        path: PathBuf::from(path),
        outcome: DeleteItemOutcome::Removed { bytes },
    })
}

fn skipped(path: &str) -> DeleteMsg {
    DeleteMsg::Item(DeleteItemResult {
        path: PathBuf::from(path),
        outcome: DeleteItemOutcome::Skipped {
            reason: "changed since the plan".into(),
        },
    })
}

fn failed(path: &str) -> DeleteMsg {
    DeleteMsg::Item(DeleteItemResult {
        path: PathBuf::from(path),
        outcome: DeleteItemOutcome::Failed {
            reason: "permission denied".into(),
        },
    })
}

fn done(removed: usize, bytes: u64, interrupted: bool) -> DeleteMsg {
    DeleteMsg::Done(DeleteSummary {
        removed,
        bytes,
        interrupted,
    })
}

#[test]
fn a_run_should_title_its_progress_from_the_first_item() {
    let mut r = DeleteRun::new(2);
    assert_eq!(r.progress(), (1, 2), "before the first Progress");
    r.apply(DeleteMsg::Progress { index: 2, total: 2 });
    assert_eq!(r.progress(), (2, 2));
    r.apply(DeleteMsg::Progress { index: 9, total: 2 });
    assert_eq!(r.progress(), (2, 2), "never past the total");
    assert!(!r.is_finished());
}

#[test]
fn a_run_should_list_each_item_and_add_up_the_measured_bytes() {
    let mut r = DeleteRun::new(3);
    r.apply(removed("/h/a", 100));
    r.apply(skipped("/h/b"));
    r.apply(failed("/h/c"));
    r.apply(done(1, 100, false));
    assert!(r.is_finished());
    assert_eq!((r.removed(), r.failed(), r.freed()), (1, 1, 100));
    assert_eq!(r.items().len(), 3);
    assert!(!r.interrupted());
}

#[test]
fn nothing_should_change_a_run_after_it_ended() {
    let mut r = DeleteRun::new(1);
    r.apply(DeleteMsg::Refused(
        "another disk-cleanup apply is running (pid 4). Nothing was deleted.".into(),
    ));
    r.apply(removed("/h/a", 5));
    r.apply(done(1, 5, false));
    assert_eq!(r.items().len(), 0);
    assert!(matches!(r.end(), RunEnd::Refused(_)));
    assert_eq!(
        r.refused(),
        Some("another disk-cleanup apply is running (pid 4). Nothing was deleted.")
    );
}

#[test]
fn a_run_whose_channel_closed_should_be_lost_and_say_so() {
    let mut r = DeleteRun::new(1);
    r.apply(removed("/h/a", 5));
    r.lose();
    assert_eq!(r.refused(), Some(MSG_WORKER_LOST));
    r.lose();
    let mut ended = DeleteRun::new(1);
    ended.apply(done(0, 0, false));
    ended.lose();
    assert!(
        matches!(ended.end(), RunEnd::Done(_)),
        "lose() must not overwrite an end"
    );
}

#[test]
fn safety_only_failed_items_should_keep_their_marks() {
    let mut r = DeleteRun::new(4);
    r.apply(removed("/h/a", 1));
    r.apply(skipped("/h/b"));
    r.apply(failed("/h/c"));
    r.apply(done(1, 1, false));
    let settled: Vec<&Path> = r.settled();
    assert_eq!(settled, vec![Path::new("/h/a"), Path::new("/h/b")]);
    assert_eq!(r.removed_paths(), vec![PathBuf::from("/h/a")]);
}

#[test]
fn rescan_target_should_be_the_common_parent_of_everything_removed() {
    let p = |s: &str| PathBuf::from(s);
    assert_eq!(rescan_target(&[]), None);
    assert_eq!(
        rescan_target(&[p("/h/code/app/node_modules")]),
        Some(p("/h/code/app"))
    );
    assert_eq!(
        rescan_target(&[p("/h/code/app/dist"), p("/h/code/app/target")]),
        Some(p("/h/code/app"))
    );
    assert_eq!(
        rescan_target(&[p("/h/code/a/dist"), p("/h/code/b/dist")]),
        Some(p("/h/code"))
    );
    assert_eq!(
        rescan_target(&[
            p("/h/code/a/dist"),
            p("/h/code/a/dist2"),
            p("/h/code/a/x/y")
        ]),
        Some(p("/h/code/a"))
    );
    // a sibling that only shares a name prefix is not below the parent
    assert_eq!(
        rescan_target(&[p("/h/code/a/x"), p("/h/code/ab/y")]),
        Some(p("/h/code"))
    );
}

// ----- the closing line and the exit code (UX-T38) -----

fn tally_of(runs: &[Vec<DeleteMsg>], planned: usize) -> Tally {
    let mut t = Tally::new();
    for msgs in runs {
        let mut r = DeleteRun::new(planned);
        for m in msgs {
            r.apply(m.clone());
        }
        t.add(&r);
    }
    t
}

#[test]
fn the_closing_line_should_count_items_and_measured_bytes_and_quote_the_log_path() {
    let t = tally_of(
        &[vec![
            removed("/h/a", 21 * (1 << 30)),
            removed("/h/b", 1 << 30),
            done(2, 22 * (1 << 30), false),
        ]],
        2,
    );
    assert_eq!(
        t.closing_line(Path::new("/home/u/.local/state/disk-cleanup/audit.jsonl"), true)
            .unwrap(),
        "EXPLORE CLOSED - deleted 2 items, 22.0 GiB measured. Audit log: /home/u/.local/state/disk-cleanup/audit.jsonl"
    );
    let one = tally_of(&[vec![removed("/h/a", 5), done(1, 5, false)]], 1);
    assert!(one
        .closing_line(Path::new("/s/it's here/audit.jsonl"), true)
        .unwrap()
        .starts_with("EXPLORE CLOSED - deleted 1 item, 5 B measured. Audit log: '"));
}

#[test]
fn the_closing_line_should_add_up_every_delete_of_the_session() {
    let t = tally_of(
        &[
            vec![removed("/h/a", 10), done(1, 10, false)],
            vec![removed("/h/b", 30), removed("/h/c", 5), done(2, 35, false)],
        ],
        2,
    );
    assert_eq!((t.removed(), t.bytes()), (3, 45));
}

#[test]
fn there_should_be_no_closing_line_When_no_delete_began() {
    let refused = tally_of(
        &[vec![DeleteMsg::Refused(
            "Deleting is not available in this build. Nothing was deleted.".into(),
        )]],
        1,
    );
    assert!(!refused.attempted());
    assert_eq!(refused.closing_line(Path::new("/a"), true), None);
    assert_eq!(Tally::new().closing_line(Path::new("/a"), true), None);
}

#[test]
fn a_delete_that_began_and_removed_nothing_should_still_close_with_the_log() {
    let t = tally_of(&[vec![failed("/h/a"), done(0, 0, false)]], 1);
    assert!(t.attempted());
    assert_eq!(
        t.closing_line(Path::new("/a/audit.jsonl"), true).unwrap(),
        "EXPLORE CLOSED - deleted 0 items, 0 B measured. Audit log: /a/audit.jsonl"
    );
}

#[test]
fn the_exit_code_should_be_1_for_an_err_item_and_3_for_a_starved_refusal() {
    let ok = tally_of(&[vec![removed("/h/a", 1), done(1, 1, false)]], 1);
    assert_eq!(ok.exit_code(0), 0);
    let err = tally_of(&[vec![failed("/h/a"), done(0, 0, false)]], 1);
    assert_eq!(err.exit_code(0), 1);
    let text = FatalError::Starved {
        unallocated_mib: 100,
        metadata_headroom_mib: 10,
    }
    .text();
    let starved = tally_of(&[vec![DeleteMsg::Refused(text)]], 1);
    assert_eq!(
        starved.exit_code(0),
        3,
        "STARVED_MARK must match the real text"
    );
    assert!(!starved.attempted());
    let lost = tally_of(&[vec![removed("/h/a", 1)]], 1);
    let mut lost_run = DeleteRun::new(1);
    lost_run.apply(removed("/h/a", 1));
    lost_run.lose();
    let mut t = Tally::new();
    t.add(&lost_run);
    assert_eq!(t.exit_code(0), 1, "a lost worker is an error");
    assert!(!lost.attempted(), "a run still running adds nothing");
}

#[test]
fn a_signal_exit_code_should_win_over_the_tally() {
    let err = tally_of(&[vec![failed("/h/a"), done(0, 0, false)]], 1);
    assert_eq!(err.exit_code(130), 130);
    assert_eq!(err.exit_code(143), 143);
}
