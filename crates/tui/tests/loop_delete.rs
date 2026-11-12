#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `run_explorer` over a scripted delete (Task 6.6.4c; UX-T34, UX-T35, UX-T38): the reviewed-set rule, the
//! prompt, the progress and result screens, the rescan, the marks, the closing line and the exits.
//!
//! These runs use the real clock: the plan, the review and the delete answer from worker threads.

mod common;

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use disk_cleanup_core::explore::keymap::Key;
use disk_cleanup_core::explore::msg::{
    DeleteItemOutcome, DeleteItemResult, DeleteMsg, DeleteSummary,
};
use disk_cleanup_core::explore::ports::{DELETE_NOT_IN_BUILD, NEEDS_TERMINAL};
use disk_cleanup_tui::event_loop::SystemClock;

/// Later than every scripted key of every test; only a run that stalls reaches it.
const WATCHDOG_MS: u64 = 25_000;
const AUDIT: &str = "/h/.local/state/disk-cleanup/audit.jsonl";
const NODE_MODULES: &str = "/h/code/app/node_modules";
const SRC: &str = "/h/code/app/src";

fn base(
    actions: &Arc<ControlActions>,
    script: Vec<(u64, disk_cleanup_tui::event_loop::Input)>,
) -> Run {
    let mut r = Run::real_time(script);
    r.time = Time::Real(Arc::new(SystemClock::default()));
    r.source.prefill.lock().unwrap().push(root_batch(
        &[("node_modules", 5 << 30), ("src", 1 << 20)],
        false,
    ));
    r.source.prefill.lock().unwrap().push(finished(
        0,
        false,
        disk_cleanup_core::explore::msg::StopCause::None,
    ));
    r.actions = actions.clone();
    r.signals = LogSignals::never(&r.log);
    // A watchdog: a run whose script went wrong ends here instead of waiting for a key that never comes.
    r.script.push((WATCHDOG_MS, key(Key::Ctrl('c'))));
    r.tweak = Box::new(|c| c.audit_log = Some(PathBuf::from(AUDIT)));
    r
}

fn mark_both() -> Vec<(u64, disk_cleanup_tui::event_loop::Input)> {
    vec![(100, ch(' ')), (150, ch('j')), (200, ch(' '))]
}

fn at(
    script: &mut Vec<(u64, disk_cleanup_tui::event_loop::Input)>,
    t: u64,
    k: disk_cleanup_tui::event_loop::Input,
) {
    script.push((t, k));
}

fn closing(out: &Outcome) -> Vec<String> {
    out.exit
        .stdout_lines
        .iter()
        .map(|l| l.to_string_lossy().into_owned())
        .collect()
}

// ----- the reviewed-set rule (UX-T34) -----

#[test]
fn d_with_no_marks_says_so_and_calls_nothing() {
    let a = Arc::new(ControlActions::new());
    let r = base(&a, vec![(100, ch('d')), (300, ch('q'))]);
    let out = run(r);
    assert!(a.calls.get().is_empty(), "{:?}", a.calls.get());
    assert_eq!(out.exit.code, 0);
    // the message shows until the next key; the run ended on q, so the last frame is the one after d
    assert!(
        out.screen
            .contains("Nothing is marked. Space marks an item."),
        "{}",
        out.screen
    );
}

#[test]
fn d_without_a_plan_plans_shows_the_preview_and_the_review_line_and_deletes_nothing() {
    let a = Arc::new(ControlActions::new());
    let mut s = mark_both();
    at(&mut s, 400, ch('d'));
    at(&mut s, 900, key(Key::Ctrl('c')));
    let out = run(base(&a, s));
    assert_eq!(a.count("preview"), 1);
    assert_eq!(a.count("review"), 1);
    assert_eq!(a.count("delete"), 0);
    assert!(
        out.screen.contains("PLAN PREVIEW - nothing is deleted."),
        "{}",
        out.screen
    );
    assert!(
        out.screen.contains("Review the plan, then press d again."),
        "{}",
        out.screen
    );
    assert_eq!(out.exit.code, 130);
}

#[test]
fn d_again_on_the_same_marks_measures_afresh_and_opens_the_prompt_with_the_fresh_figures() {
    let a = Arc::new(ControlActions::new());
    let mut s = mark_both();
    at(&mut s, 400, ch('d'));
    at(&mut s, 900, ch('d'));
    at(&mut s, 1400, key(Key::Ctrl('c')));
    let out = run(base(&a, s));
    assert_eq!(a.count("review"), 2, "the figures are measured again at d");
    assert_eq!(a.count("preview"), 1);
    assert_eq!(a.count("delete"), 0, "the prompt deletes nothing by itself");
    assert!(
        out.screen.contains("DELETE - this cannot be undone"),
        "{}",
        out.screen
    );
    assert!(
        out.screen
            .contains("Type 2 and press Enter to delete, Esc to cancel: _"),
        "{}",
        out.screen
    );
    assert!(
        out.screen
            .contains("Audit log: ~/.local/state/disk-cleanup/audit.jsonl"),
        "{}",
        out.screen
    );
    assert_eq!(out.exit.code, 130);
    assert!(a.confirmed.lock().unwrap().is_empty());
}

#[test]
fn safety_after_one_mark_changes_d_is_back_to_planning_and_no_prompt_opens() {
    let a = Arc::new(ControlActions::new());
    let mut s = mark_both();
    at(&mut s, 400, ch('p'));
    at(&mut s, 900, key(Key::Esc));
    at(&mut s, 1000, ch(' ')); // unmark the second item
    at(&mut s, 1100, ch('d'));
    at(&mut s, 1700, key(Key::Ctrl('c')));
    let out = run(base(&a, s));
    assert_eq!(a.count("preview"), 2, "a changed set plans again");
    assert_eq!(a.count("review"), 2);
    assert!(
        !out.screen.contains("DELETE - this cannot be undone"),
        "{}",
        out.screen
    );
    assert!(
        out.screen.contains("Review the plan, then press d again."),
        "{}",
        out.screen
    );
    assert_eq!(a.count("delete"), 0);
}

#[test]
fn safety_a_plan_whose_items_changed_at_the_fresh_measure_opens_no_prompt() {
    let mut a = ControlActions::new();
    // the second review finds another directory under the same name: other identities, another plan-id
    a.review_fn = Box::new(|n, marks| {
        let mut m = marks.to_vec();
        if n >= 2 {
            m[0] = PathBuf::from("/h/code/app/other");
        }
        Ok(disk_cleanup_core::explore::testing::reviewed_of(&m, GIB))
    });
    let a = Arc::new(a);
    let mut s = mark_both();
    at(&mut s, 400, ch('p'));
    at(&mut s, 900, ch('d'));
    at(&mut s, 1500, ch('d'));
    at(&mut s, 2100, key(Key::Ctrl('c')));
    let out = run(base(&a, s));
    assert!(
        !out.screen.contains("DELETE - this cannot be undone"),
        "{}",
        out.screen
    );
    assert_eq!(a.count("delete"), 0);
    // the held plan is gone: the d that follows plans and previews again
    assert!(out.screen.contains("PLAN PREVIEW"), "{}", out.screen);
    assert_eq!(a.count("preview"), 2);
}

#[test]
fn a_plan_that_could_not_be_saved_leaves_the_prompt_shut_and_says_so() {
    let mut a = ControlActions::new();
    a.review_fn = Box::new(|_, _| {
        Err("plan-id NOT usable: the plan could not be saved. Nothing was deleted.".to_string())
    });
    let a = Arc::new(a);
    let mut s = mark_both();
    at(&mut s, 400, ch('d'));
    at(&mut s, 900, ch('d'));
    at(&mut s, 1400, key(Key::Ctrl('c')));
    let out = run(base(&a, s));
    assert_eq!(a.count("delete"), 0);
    assert!(out.screen.contains("plan-id NOT usable"), "{}", out.screen);
    assert!(!out.screen.contains("DELETE - this cannot be undone"));
}

#[test]
fn in_a_build_without_deleting_d_shows_the_refusal_and_p_still_previews() {
    let mut a = ControlActions::new();
    a.review_fn = Box::new(|_, _| Err(DELETE_NOT_IN_BUILD.to_string()));
    let a = Arc::new(a);
    let mut s = mark_both();
    at(&mut s, 400, ch('d'));
    at(&mut s, 900, key(Key::Ctrl('c')));
    let out = run(base(&a, s));
    assert!(out.screen.contains(DELETE_NOT_IN_BUILD), "{}", out.screen);
    assert_eq!(a.count("delete"), 0);
    assert!(
        !out.screen.contains("PLAN PREVIEW"),
        "d shows the refusal, not a preview"
    );

    let mut a = ControlActions::new();
    a.review_fn = Box::new(|_, _| Err(DELETE_NOT_IN_BUILD.to_string()));
    let a = Arc::new(a);
    let mut s = mark_both();
    at(&mut s, 400, ch('p'));
    at(&mut s, 900, key(Key::Ctrl('c')));
    let out = run(base(&a, s));
    assert!(
        out.screen.contains("PLAN PREVIEW - nothing is deleted."),
        "{}",
        out.screen
    );
    assert!(out.screen.contains(DELETE_NOT_IN_BUILD), "{}", out.screen);
}

#[test]
fn without_a_terminal_d_is_refused_before_anything_is_planned() {
    let a = Arc::new(ControlActions::new());
    let mut s = mark_both();
    at(&mut s, 400, ch('d'));
    at(&mut s, 700, key(Key::Ctrl('c')));
    let mut r = base(&a, s);
    r.terminal = false;
    let out = run(r);
    assert!(a.calls.get().is_empty(), "{:?}", a.calls.get());
    assert!(out.screen.contains(NEEDS_TERMINAL), "{}", out.screen);
}

// ----- the prompt (UX-T35) -----

fn to_prompt() -> Vec<(u64, disk_cleanup_tui::event_loop::Input)> {
    let mut s = mark_both();
    at(&mut s, 400, ch('d'));
    at(&mut s, 900, ch('d'));
    s
}

#[test]
fn a_wrong_entry_keeps_the_prompt_and_calls_nothing_and_the_right_count_deletes_once() {
    let a = Arc::new(ControlActions::new());
    let mut s = to_prompt();
    at(&mut s, 1400, ch('3'));
    at(&mut s, 1500, key(Key::Enter));
    at(&mut s, 1900, key(Key::Ctrl('c')));
    let out = run(base(&a, s));
    assert_eq!(
        a.count("delete"),
        0,
        "a wrong count never reaches the delete"
    );
    assert!(
        out.screen.contains("That is not 2. Nothing was deleted."),
        "{}",
        out.screen
    );
    assert!(out.screen.contains("Esc to cancel: _"), "{}", out.screen);

    let a = Arc::new(ControlActions::new());
    let mut s = to_prompt();
    at(&mut s, 1400, ch('3'));
    at(&mut s, 1500, key(Key::Enter));
    at(&mut s, 1600, ch('2'));
    at(&mut s, 1700, key(Key::Enter));
    at(&mut s, 2600, key(Key::Esc));
    at(&mut s, 2700, ch('q'));
    let out = run(base(&a, s));
    assert_eq!(a.count("delete"), 1);
    let conf = a.confirmed.lock().unwrap();
    assert_eq!(conf.len(), 1);
    assert_eq!(conf[0].typed, "2");
    assert_eq!(conf[0].count, 2);
    assert_eq!(out.exit.code, 0);
}

#[test]
fn safety_the_prompt_ignores_d_y_j_space_and_q() {
    let a = Arc::new(ControlActions::new());
    let mut s = to_prompt();
    for (i, c) in ['d', 'y', 'D', 'j', ' ', 'q', 'x', 'p']
        .into_iter()
        .enumerate()
    {
        at(&mut s, 1400 + 50 * i as u64, ch(c));
    }
    at(&mut s, 2000, key(Key::Ctrl('c')));
    let out = run(base(&a, s));
    assert_eq!(a.count("delete"), 0);
    assert_eq!(a.count("review"), 2, "no key of the main view ran");
    assert!(
        out.screen
            .contains("Type 2 and press Enter to delete, Esc to cancel: _"),
        "{}",
        out.screen
    );
}

#[test]
fn esc_at_the_prompt_keeps_the_marks_and_the_review_and_ctrl_c_leaves_with_130() {
    let a = Arc::new(ControlActions::new());
    let mut s = to_prompt();
    at(&mut s, 1400, key(Key::Esc));
    at(&mut s, 1600, ch('d')); // the review was kept: no preview, a fresh measure, the prompt again
    at(&mut s, 2200, key(Key::Ctrl('c')));
    let out = run(base(&a, s));
    assert_eq!(a.count("preview"), 1);
    assert_eq!(a.count("review"), 3);
    assert_eq!(a.count("delete"), 0);
    assert_eq!(out.exit.code, 130);
    assert!(
        out.screen.contains("Type 2 and press Enter"),
        "{}",
        out.screen
    );
    assert!(
        closing(&out).is_empty(),
        "no delete ran, so nothing is printed: {:?}",
        closing(&out)
    );
}

// ----- DELETING and the result (UX-T8, UX-T35) -----

fn item(path: &str, outcome: DeleteItemOutcome) -> DeleteMsg {
    DeleteMsg::Item(DeleteItemResult {
        path: PathBuf::from(path),
        outcome,
    })
}

fn typed_two(
    mut s: Vec<(u64, disk_cleanup_tui::event_loop::Input)>,
) -> Vec<(u64, disk_cleanup_tui::event_loop::Input)> {
    at(&mut s, 1400, ch('2'));
    at(&mut s, 1500, key(Key::Enter));
    s
}

#[test]
fn the_result_list_shows_each_item_and_esc_returns_to_a_list_that_dropped_the_marks() {
    let a = Arc::new(ControlActions::new());
    let mut s = typed_two(to_prompt());
    at(&mut s, 2300, key(Key::Esc));
    at(&mut s, 2600, ch('q'));
    let r = base(&a, s);
    let source = Arc::clone(&r.source);
    let out = run(r);
    assert!(out.screen.contains("marks: 0"), "{}", out.screen);
    // the parents are rescanned: the second scan starts at the common parent
    assert_eq!(source.count(), 2, "one rescan");
    assert_eq!(
        source.starts.lock().unwrap()[1].roots,
        vec![PathBuf::from("/h/code/app")]
    );
    assert_eq!(
        closing(&out),
        vec![format!(
            "EXPLORE CLOSED - deleted 2 items, 2.0 GiB measured. Audit log: {AUDIT}"
        )]
    );
    assert_eq!(out.exit.code, 0);
}

#[test]
fn a_result_with_removed_skipped_and_error_keeps_only_the_marks_of_the_failed_items() {
    let mut a = ControlActions::new();
    a.on_delete = Box::new(|_paths, tx, _abort| {
        std::thread::spawn(move || {
            let _ = tx.send(DeleteMsg::Progress { index: 1, total: 2 });
            let _ = tx.send(item(
                NODE_MODULES,
                DeleteItemOutcome::Removed { bytes: GIB },
            ));
            let _ = tx.send(DeleteMsg::Progress { index: 2, total: 2 });
            let _ = tx.send(item(
                SRC,
                DeleteItemOutcome::Failed {
                    reason: "permission denied".into(),
                },
            ));
            let _ = tx.send(DeleteMsg::Done(DeleteSummary {
                removed: 1,
                bytes: GIB,
                interrupted: false,
            }));
        });
    });
    let a = Arc::new(a);
    let mut s = typed_two(to_prompt());
    at(&mut s, 2300, key(Key::Esc));
    at(&mut s, 2500, ch('d')); // the failed item is still marked: d plans again
    at(&mut s, 3100, key(Key::Ctrl('c')));
    let out = run(base(&a, s));
    assert_eq!(a.count("preview"), 2, "a delete consumes the plan");
    assert!(
        out.screen.contains("marks: 1") || out.screen.contains("1 marked"),
        "{}",
        out.screen
    );
    assert_eq!(
        closing(&out),
        vec![format!(
            "EXPLORE CLOSED - deleted 1 item, 1.0 GiB measured. Audit log: {AUDIT}"
        )]
    );
    assert_eq!(out.exit.code, 130);
}

#[test]
fn an_err_item_makes_a_normal_quit_exit_1() {
    let mut a = ControlActions::new();
    a.on_delete = Box::new(|_paths, tx, _abort| {
        std::thread::spawn(move || {
            let _ = tx.send(item(
                NODE_MODULES,
                DeleteItemOutcome::Failed {
                    reason: "i/o error".into(),
                },
            ));
            let _ = tx.send(item(SRC, DeleteItemOutcome::Removed { bytes: 5 }));
            let _ = tx.send(DeleteMsg::Done(DeleteSummary {
                removed: 1,
                bytes: 5,
                interrupted: false,
            }));
        });
    });
    let a = Arc::new(a);
    let mut s = typed_two(to_prompt());
    at(&mut s, 2300, key(Key::Esc));
    at(&mut s, 2400, ch('x')); // forget the remaining mark
    at(&mut s, 2500, ch('q'));
    let out = run(base(&a, s));
    assert_eq!(out.exit.code, 1);
    assert!(closing(&out)[0].starts_with("EXPLORE CLOSED - deleted 1 item, 5 B measured."));
}

#[test]
fn every_refusal_text_is_shown_and_keeps_the_marks_and_the_closing_line_stays_the_old_one() {
    for text in [
        "another disk-cleanup apply is running (pid 4242). Nothing was deleted.",
        "Changed since you confirmed: 40.1 GiB then, 55.0 GiB now. Nothing was deleted. Press d to review again.",
        "Deleting is disabled in this build: run 'make gate' and 'make install-gated'. Nothing was deleted.",
    ] {
        let mut a = ControlActions::new();
        let t = text.to_string();
        a.on_delete = Box::new(move |_p, tx, _a| {
            let _ = tx.send(DeleteMsg::Refused(t.clone()));
        });
        let a = Arc::new(a);
        let s = typed_two(to_prompt());
        let mut s2 = s;
        at(&mut s2, 2200, ch('z')); // ignored on the result screen
        at(&mut s2, 2300, key(Key::Esc));
        at(&mut s2, 2500, ch('x'));
        at(&mut s2, 2600, ch('q'));
        let out = run(base(&a, s2));
        assert_eq!(closing(&out), vec![CLOSED.to_string()], "{text}");
        assert_eq!(out.exit.code, 0, "{text}");

        let s = typed_two(to_prompt());
        let mut s3 = s;
        at(&mut s3, 2300, key(Key::Ctrl('c')));
        let out = run(base(&a, s3));
        let flat = out.screen.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(flat.contains(text), "{text}\n{}", out.screen);
        assert!(out.screen.starts_with("DELETE REFUSED - nothing was deleted."), "{}", out.screen);
    }
}

#[test]
fn the_title_reads_deleting_while_a_delete_runs_and_a_key_draws_the_stopping_frame() {
    let mut a = ControlActions::new();
    a.on_delete = Box::new(|_paths, tx, abort| {
        std::thread::spawn(move || {
            let _ = tx.send(DeleteMsg::Progress { index: 1, total: 2 });
            while !abort.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            std::thread::sleep(Duration::from_millis(400));
            let _ = tx.send(DeleteMsg::Done(DeleteSummary {
                removed: 0,
                bytes: 0,
                interrupted: true,
            }));
        });
    });
    let a = Arc::new(a);
    let mut s = typed_two(to_prompt());
    at(&mut s, 2000, key(Key::Ctrl('c')));
    let out = run(base(&a, s));
    assert!(out.screen.starts_with("DELETING 1 of 2"), "{}", out.screen);
    assert!(
        out.screen.contains("stopping at the next item boundary"),
        "{}",
        out.screen
    );
    assert!(!out.screen.contains("APPLYING") && !out.screen.contains("DRY RUN"));
    assert_eq!(out.exit.code, 130);
}

#[test]
fn keys_other_than_ctrl_c_are_ignored_while_deleting() {
    let mut a = ControlActions::new();
    a.on_delete = Box::new(|_paths, tx, abort| {
        std::thread::spawn(move || {
            let _ = tx.send(DeleteMsg::Progress { index: 1, total: 2 });
            while !abort.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            let _ = tx.send(DeleteMsg::Done(DeleteSummary::default()));
        });
    });
    let a = Arc::new(a);
    let mut s = typed_two(to_prompt());
    for (i, c) in ['q', 'd', 'y', 'x', ' ', 'p'].into_iter().enumerate() {
        at(&mut s, 2000 + 50 * i as u64, ch(c));
    }
    at(&mut s, 2600, key(Key::Esc));
    at(&mut s, 2800, key(Key::Ctrl('c')));
    let out = run(base(&a, s));
    assert_eq!(a.count("delete"), 1, "no second delete");
    assert!(a.aborts.lock().unwrap()[0].load(Ordering::SeqCst));
    assert_eq!(out.exit.code, 130);
}

// ----- interrupts mid-delete (UX-T38) -----

/// A delete that blocks until aborted, then logs, takes `finish_ms`, logs again and reports.
fn slow_abort(log: Log, finish_ms: u64) -> OnDelete {
    Box::new(move |_paths, tx, abort| {
        let log = log.clone();
        std::thread::spawn(move || {
            let _ = tx.send(DeleteMsg::Progress { index: 1, total: 2 });
            while !abort.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            log.push("abort_seen");
            std::thread::sleep(Duration::from_millis(finish_ms));
            log.push("worker_done");
            let _ = tx.send(item(
                NODE_MODULES,
                DeleteItemOutcome::Removed { bytes: GIB },
            ));
            let _ = tx.send(DeleteMsg::Done(DeleteSummary {
                removed: 1,
                bytes: GIB,
                interrupted: true,
            }));
        });
    })
}

#[test]
fn each_signal_mid_delete_aborts_waits_for_the_worker_restores_then_exits_130_or_143() {
    for (sig, code) in [(2, 130), (15, 143), (1, 143), (3, 143)] {
        let mut a = ControlActions::new();
        let log = Log::default();
        a.on_delete = slow_abort(log.clone(), 400);
        let a = Arc::new(a);
        let s = typed_two(to_prompt());
        let r = base(&a, s);
        let signals = Arc::clone(&r.signals);
        let trigger = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(2200));
            *signals.signal.lock().unwrap() = Some((0, sig));
        });
        let out = run(r);
        trigger.join().unwrap();
        assert_eq!(out.exit.code, code, "signal {sig}");
        assert!(
            a.aborts.lock().unwrap()[0].load(Ordering::SeqCst),
            "signal {sig}"
        );
        let worker = log.get();
        assert_eq!(worker, vec!["abort_seen", "worker_done"], "signal {sig}");
        let restore_at = out.log.iter().position(|e| *e == "restore").unwrap();
        assert_eq!(restore_at, out.log.len() - 1, "{:?}", out.log);
        assert_eq!(
            closing(&out),
            vec![format!(
                "EXPLORE CLOSED - deleted 1 item, 1.0 GiB measured. Audit log: {AUDIT}"
            )],
            "the closing line follows the abort (signal {sig})"
        );
    }
}

#[test]
fn the_terminal_is_restored_only_after_the_aborted_delete_reported() {
    let mut a = ControlActions::new();
    let log = Log::default();
    a.on_delete = slow_abort(log.clone(), 600);
    let a = Arc::new(a);
    let r = base(&a, typed_two(to_prompt()));
    let (rlog, wlog) = (r.log.clone(), log.clone());
    let signals = Arc::clone(&r.signals);
    let watcher = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(2200));
        *signals.signal.lock().unwrap() = Some((0, 15));
        // sampled while the worker is still finishing: the restore has not happened
        std::thread::sleep(Duration::from_millis(300));
        (rlog.get().contains(&"restore"), wlog.get())
    });
    let out = run(r);
    let (restored_early, worker_then) = watcher.join().unwrap();
    assert!(
        !restored_early,
        "restored while the delete was still running"
    );
    assert_eq!(worker_then, vec!["abort_seen"]);
    assert_eq!(out.exit.code, 143);
}

#[test]
fn ctrl_c_mid_delete_aborts_and_exits_130_after_the_worker_reported() {
    let mut a = ControlActions::new();
    let log = Log::default();
    a.on_delete = slow_abort(log.clone(), 300);
    let a = Arc::new(a);
    let mut s = typed_two(to_prompt());
    at(&mut s, 2000, key(Key::Ctrl('c')));
    let out = run(base(&a, s));
    assert_eq!(out.exit.code, 130);
    assert_eq!(log.get(), vec!["abort_seen", "worker_done"]);
    assert!(closing(&out)[0].starts_with("EXPLORE CLOSED - deleted 1 item, 1.0 GiB measured."));
}

#[test]
fn the_wait_for_an_aborted_delete_is_bounded_and_the_exit_code_stays_the_signals() {
    let mut a = ControlActions::new();
    a.on_delete = Box::new(|_paths, tx, _abort| {
        std::thread::spawn(move || {
            let _ = tx.send(DeleteMsg::Progress { index: 1, total: 2 });
            std::thread::sleep(Duration::from_secs(30)); // never reports
            drop(tx);
        });
    });
    let a = Arc::new(a);
    let mut s = typed_two(to_prompt());
    at(&mut s, 2000, key(Key::Ctrl('c')));
    let mut r = base(&a, s);
    r.tweak = Box::new(|c| {
        c.audit_log = Some(PathBuf::from(AUDIT));
        c.abort_wait_ms = 400;
    });
    let t0 = Instant::now();
    let out = run(r);
    let took = t0.elapsed();
    assert!(took < Duration::from_secs(6), "waited {took:?}");
    assert_eq!(out.exit.code, 130);
    assert_eq!(out.log.last(), Some(&"restore"));
    assert!(
        closing(&out)[0].starts_with("EXPLORE CLOSED - deleted 0 items"),
        "a lost run still leaves its closing line: {:?}",
        closing(&out)
    );
}

#[test]
fn a_worker_that_vanishes_mid_delete_is_reported_as_lost_not_hung() {
    let mut a = ControlActions::new();
    a.on_delete = Box::new(|_paths, tx, _abort| {
        let _ = tx.send(DeleteMsg::Progress { index: 1, total: 2 });
        drop(tx);
    });
    let a = Arc::new(a);
    let mut s = typed_two(to_prompt());
    at(&mut s, 2300, key(Key::Ctrl('c')));
    let out = run(base(&a, s));
    assert!(out.screen.contains("Check the audit log"), "{}", out.screen);
}

#[test]
fn a_signal_at_the_prompt_leaves_at_once_without_a_delete() {
    let a = Arc::new(ControlActions::new());
    let r = base(&a, to_prompt());
    let signals = Arc::clone(&r.signals);
    let trigger = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1600));
        *signals.signal.lock().unwrap() = Some((0, 15));
    });
    let out = run(r);
    trigger.join().unwrap();
    assert_eq!(out.exit.code, 143);
    assert_eq!(a.count("delete"), 0);
    assert_eq!(out.exit.stdout_lines, Vec::<OsString>::new());
}

// ----- the old exit paths still hold -----

#[test]
fn the_quit_prompt_hand_off_after_a_delete_ends_with_the_closing_line_not_the_old_one() {
    // The first item is removed and the second fails, so one mark stays and `q` offers the hand-off.
    let mut a = ControlActions::new();
    a.on_delete = Box::new(|paths, tx, _abort| {
        std::thread::spawn(move || {
            let _ = tx.send(removed_item(&paths[0], GIB));
            let _ = tx.send(item(
                paths[1].to_str().unwrap(),
                DeleteItemOutcome::Failed {
                    reason: "i/o error".into(),
                },
            ));
            let _ = tx.send(DeleteMsg::Done(DeleteSummary {
                removed: 1,
                bytes: GIB,
                interrupted: false,
            }));
        });
    });
    let a = Arc::new(a);
    let mut s = typed_two(to_prompt());
    at(&mut s, 2300, key(Key::Esc));
    at(&mut s, 2500, ch('q'));
    at(&mut s, 2600, key(Key::Enter));
    let out = run(base(&a, s));
    assert_eq!(a.count("finish"), 1, "the hand-off ran");
    let lines = closing(&out);
    assert_eq!(
        lines.len(),
        2,
        "the hand-off's own lines are kept: {lines:?}"
    );
    assert_eq!(lines[0], "DRY RUN - nothing was deleted.");
    assert_eq!(
        lines[1],
        format!("EXPLORE CLOSED - deleted 1 item, 1.0 GiB measured. Audit log: {AUDIT}")
    );
    assert_eq!(out.exit.code, 1, "an item ended in error");
}

// ----- a plan that was cancelled stays cancelled -----

#[test]
fn a_review_that_answers_after_its_plan_was_cancelled_is_dropped_not_taken_for_the_next_plan() {
    let mut a = ControlActions::new();
    a.review_fn = Box::new(|n, marks| {
        if n == 1 {
            std::thread::sleep(Duration::from_millis(700));
            Err("STALE FIRST REVIEW".to_string())
        } else {
            std::thread::sleep(Duration::from_millis(1500));
            Ok(disk_cleanup_core::explore::testing::reviewed_of(marks, GIB))
        }
    });
    let a = Arc::new(a);
    let mut s = mark_both();
    at(&mut s, 400, ch('p')); // the first plan: its review answers at about 1100 ms
    at(&mut s, 500, key(Key::Esc)); // cancelled
    at(&mut s, 600, ch('p')); // the second plan: its review answers at about 2100 ms
    at(&mut s, 3500, key(Key::Ctrl('c')));
    let out = run(base(&a, s));
    assert!(
        !out.screen.contains("STALE FIRST REVIEW"),
        "the cancelled plan's answer was taken:\n{}",
        out.screen
    );
    assert!(out.screen.contains("PLAN PREVIEW"), "{}", out.screen);
    assert_eq!(a.count("review"), 2);
}

#[test]
fn d_while_a_plan_is_in_flight_does_nothing() {
    let mut a = ControlActions::new();
    a.review_fn = Box::new(|_, marks| {
        std::thread::sleep(Duration::from_millis(900));
        Ok(disk_cleanup_core::explore::testing::reviewed_of(marks, GIB))
    });
    let a = Arc::new(a);
    let mut s = mark_both();
    at(&mut s, 400, ch('p'));
    at(&mut s, 700, ch('d')); // the review is still running
    at(&mut s, 2500, key(Key::Ctrl('c')));
    let out = run(base(&a, s));
    assert_eq!(a.count("review"), 1, "d started a second plan");
    assert_eq!(a.count("preview"), 1);
    assert!(out.screen.contains("PLAN PREVIEW"), "{}", out.screen);
}

#[test]
fn a_plan_made_for_marks_that_changed_while_it_ran_is_not_the_review_of_the_new_marks() {
    let mut a = ControlActions::new();
    a.review_fn = Box::new(|n, marks| {
        if n == 1 {
            std::thread::sleep(Duration::from_millis(900));
        }
        Ok(disk_cleanup_core::explore::testing::reviewed_of(marks, GIB))
    });
    let a = Arc::new(a);
    let mut s = mark_both();
    at(&mut s, 400, ch('p')); // plans both marks; the review answers at about 1300 ms
    at(&mut s, 700, ch(' ')); // the second mark goes while it runs
    at(&mut s, 1800, key(Key::Esc));
    at(&mut s, 2000, ch('d')); // the plan was for two marks: this is a first d again
    at(&mut s, 3000, key(Key::Ctrl('c')));
    let out = run(base(&a, s));
    assert_eq!(
        a.count("preview"),
        2,
        "d must plan again, not measure afresh"
    );
    assert!(
        out.screen.contains("Review the plan, then press d again."),
        "{}",
        out.screen
    );
    assert!(!out.screen.contains("DELETE - this cannot be undone"));
}

#[test]
fn a_signal_after_a_finished_delete_still_prints_the_closing_line() {
    let a = Arc::new(ControlActions::new());
    let s = typed_two(to_prompt());
    let r = base(&a, s);
    let signals = Arc::clone(&r.signals);
    let trigger = std::thread::spawn(move || {
        // the delete is done and the result list is up by 2.5 s
        std::thread::sleep(Duration::from_millis(3000));
        *signals.signal.lock().unwrap() = Some((0, 15));
    });
    let out = run(r);
    trigger.join().unwrap();
    assert_eq!(out.exit.code, 143);
    assert_eq!(
        closing(&out),
        vec![format!(
            "EXPLORE CLOSED - deleted 2 items, 2.0 GiB measured. Audit log: {AUDIT}"
        )]
    );
}

// ----- the rescan after a delete and the redraw when it ends -----

/// `/h/code/app/node_modules/pkg` below the usual two directories.
fn with_nested_dir(r: &Run) {
    let extra = disk_cleanup_core::explore::msg::ScanMsg::Batch {
        generation: 0,
        mode: disk_cleanup_core::explore::msg::ScanMode::Fresh,
        events: vec![dir_ev(
            10,
            1,
            "pkg",
            stats(1, 0, 1 << 20, 1 << 20),
            0,
            1_700_000_000,
        )],
    };
    r.source.prefill.lock().unwrap().insert(1, extra);
}

fn location_line(screen: &str) -> &str {
    screen.lines().nth(2).unwrap_or("")
}

#[test]
fn the_rescan_after_a_delete_starts_at_the_parent_of_what_was_removed_and_the_view_stays_inside_it()
{
    let a = Arc::new(ControlActions::new());
    // open node_modules, mark the directory in it, plan, confirm: the parent is node_modules, which the
    // view already shows
    let mut s = vec![(100, ch('l')), (200, ch('j')), (300, ch(' '))];
    at(&mut s, 500, ch('d'));
    at(&mut s, 1100, ch('d'));
    at(&mut s, 1700, ch('1'));
    at(&mut s, 1800, key(Key::Enter));
    at(&mut s, 2600, key(Key::Esc));
    at(&mut s, 3200, key(Key::Ctrl('c')));
    let r = base(&a, s);
    with_nested_dir(&r);
    let source = Arc::clone(&r.source);
    let out = run(r);
    assert_eq!(a.count("delete"), 1, "{}", out.screen);
    assert_eq!(
        source.starts.lock().unwrap()[1].roots,
        vec![PathBuf::from("/h/code/app/node_modules")]
    );
    assert!(
        location_line(&out.screen).starts_with("~/code/app/node_modules"),
        "the view moved away:\n{}",
        out.screen
    );
}

#[test]
fn the_view_moves_to_the_rescanned_directory_when_it_was_somewhere_else() {
    let a = Arc::new(ControlActions::new());
    // mark pkg in node_modules, go up and into src, delete: the parent node_modules is not where the view is
    let mut s = vec![
        (100, ch('l')),
        (200, ch('j')),
        (300, ch(' ')),
        (400, ch('h')),
        (550, ch('j')),
        (700, ch('l')),
    ];
    at(&mut s, 900, ch('d'));
    at(&mut s, 1500, ch('d'));
    at(&mut s, 2100, ch('1'));
    at(&mut s, 2200, key(Key::Enter));
    at(&mut s, 3000, key(Key::Esc));
    at(&mut s, 3600, key(Key::Ctrl('c')));
    let r = base(&a, s);
    with_nested_dir(&r);
    let out = run(r);
    assert_eq!(a.count("delete"), 1, "{}", out.screen);
    assert!(
        location_line(&out.screen).starts_with("~/code/app/node_modules"),
        "the view should follow the rescan:\n{}",
        out.screen
    );
}

#[test]
fn the_result_is_drawn_when_the_delete_ends_without_any_key() {
    // Nothing is removed, so there is no rescan whose progress would redraw the screen anyway.
    let mut a = ControlActions::new();
    a.on_delete = Box::new(|paths, tx, _abort| {
        std::thread::spawn(move || {
            let _ = tx.send(DeleteMsg::Progress { index: 1, total: 2 });
            std::thread::sleep(Duration::from_millis(700));
            for p in &paths {
                let _ = tx.send(item(
                    p.to_str().unwrap(),
                    DeleteItemOutcome::Failed {
                        reason: "i/o error".into(),
                    },
                ));
            }
            let _ = tx.send(DeleteMsg::Done(DeleteSummary::default()));
        });
    });
    let a = Arc::new(a);
    let mut s = typed_two(to_prompt());
    at(&mut s, 3500, key(Key::Ctrl('c'))); // the only key after the Enter that started the delete
    let out = run(base(&a, s));
    assert!(
        out.screen
            .starts_with("DELETE RESULT - 0 removed, 0 skipped, 2 errors"),
        "{}",
        out.screen
    );
}
