#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `run_explorer` over fakes (Task 6.5.2b; UX-T9, UX-T12, UX-T15, UX-T22 to UX-T24, O-2): drain budget,
//! redraw throttle, signals, planning, rescan and nested roots.

mod common;

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use disk_cleanup_core::explore::keymap::Key;
use disk_cleanup_core::explore::msg::{
    NodeEvent, ScanMode, ScanMsg, ScanSummary, StopCause, NO_PARENT,
};
use disk_cleanup_tui::app::{CLOSED_LINE, CLOSED_PARTIAL_LINE};
use disk_cleanup_tui::event_loop::Input;
use disk_cleanup_tui::terminal::RESTORE_FAILED;

fn first_line_with<'a>(screen: &'a str, needle: &str) -> Option<&'a str> {
    screen.lines().find(|l| l.contains(needle))
}

#[test]
fn the_first_frame_with_scanning_is_drawn_before_the_first_wait_for_input() {
    let r = Run::virtual_time(vec![(100, ch('q'))]);
    let out = run(r);
    let (first_timeout, at, frames_then) = out.polls[0];
    assert!(
        frames_then >= 1,
        "no frame before the first poll (timeout {first_timeout:?} at {at} ms)"
    );
    assert!(
        first_line_with(&out.screen, "SCANNING").is_some(),
        "{}",
        out.screen
    );
    assert_eq!(out.exit.code, 0);
}

#[test]
fn scan_driven_redraws_are_at_most_four_a_second_and_keys_are_handled_while_batches_flow() {
    let mut r = Run::real_time(vec![(1_000, ch('j')), (1_500, ch('j')), (3_800, ch('q'))]);
    // the feeder's 3,000 sends can take longer than 3.8 s on a busy host: `q` waits for the end of the scan
    r.gates = vec![(2, "!SCANNING")];
    let source = Arc::clone(&r.source);
    let feeder = std::thread::spawn(move || {
        // wait for the loop to start the scan
        while source.count() == 0 {
            std::thread::sleep(Duration::from_millis(1));
        }
        source.send(0, root_batch(&[("a", 4096)], true));
        for i in 0..3_000u64 {
            let ev = dir_ev(100 + i, 0, &format!("d{i}"), stats(1, 0, 4096, 10), 0, 0);
            source.send(
                0,
                ScanMsg::Batch {
                    generation: 0,
                    mode: ScanMode::Fresh,
                    events: vec![ev],
                },
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        source.send(0, finished(0, false, StopCause::None));
    });
    let out = run(r);
    feeder.join().unwrap();
    let secs = out.elapsed_ms as f64 / 1000.0;
    assert!(secs >= 3.0, "the run lasted {secs}s");
    // scan-driven redraws (4/s) plus the three key redraws and the first frame
    assert!(
        out.frames as f64 <= 4.0 * secs + 6.0,
        "{} frames in {secs:.1}s",
        out.frames
    );
    assert!(out.frames >= 6, "{} frames", out.frames);
    // every key press is followed by a frame before the loop waits again (one loop iteration)
    for t in [1_000u64, 1_500] {
        let before = out.polls.iter().rev().find(|p| p.1 < t).map_or(0, |p| p.2);
        let after = out
            .polls
            .iter()
            .find(|p| p.1 > t + 60)
            .map_or(usize::MAX, |p| p.2);
        assert!(after > before, "no frame after the key at {t} ms");
    }
    // the cursor moved down twice while batches were arriving and keeps its row through the re-sorts
    let at: Vec<usize> = out
        .screen
        .lines()
        .enumerate()
        .filter(|(_, l)| l.starts_with('>'))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(at.len(), 1, "one cursor row: {}", out.screen);
    assert!(at[0] > 4, "the cursor left the first row: {}", out.screen);
    assert!(
        first_line_with(&out.screen, "SCANNING").is_none(),
        "finished: {}",
        out.screen
    );
}

#[test]
fn esc_stops_the_scan_and_a_truncated_finish_says_stopped_by_you() {
    let mut r = Run::real_time(vec![(200, key(Key::Esc)), (900, ch('q'))]);
    let source = Arc::clone(&r.source);
    r.tweak = Box::new(|c| c.scan_opts.max_seconds = Some(7));
    let feeder = std::thread::spawn(move || {
        while source.count() == 0 {
            std::thread::sleep(Duration::from_millis(1));
        }
        source.send(0, root_batch(&[("a", 4096)], true));
        let cancel = Arc::clone(&source.starts.lock().unwrap()[0].cancel);
        while !cancel.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1));
        }
        source.send(0, finished(0, true, StopCause::Cancelled));
    });
    let out = run(r);
    feeder.join().unwrap();
    assert!(
        first_line_with(&out.screen, "PARTIAL SCAN (stopped by you)").is_some(),
        "{}",
        out.screen
    );
}

#[test]
fn a_deadline_finish_names_max_seconds() {
    let mut r = Run::virtual_time(vec![(500, ch('q'))]);
    r.tweak = Box::new(|c| c.scan_opts.max_seconds = Some(7));
    {
        let mut pre = r.source.prefill.lock().unwrap();
        pre.push(root_batch(&[("a", 4096)], true));
        pre.push(finished(0, true, StopCause::Deadline));
    }
    let out = run(r);
    assert!(
        first_line_with(&out.screen, "PARTIAL SCAN (hit --max-seconds 7)").is_some(),
        "{}",
        out.screen
    );
}

/// Nanoseconds this thread has spent running (the first field of `/proc/thread-self/schedstat`).
fn thread_cpu_ns() -> Option<u64> {
    std::fs::read_to_string("/proc/thread-self/schedstat")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn big_backlog(per_batch: usize) -> (Vec<ScanMsg>, u64) {
    // 1 root + 100 directories + leaves = 676,296 events
    let total: u64 = 676_296;
    let mids = 100u64;
    let mut events: Vec<NodeEvent> = Vec::with_capacity(total as usize);
    events.push(dir_ev(
        0,
        NO_PARENT,
        "/h/code/app",
        stats(0, mids, 0, 0),
        1_000_000,
        0,
    ));
    for m in 1..=mids {
        events.push(dir_ev(
            m,
            0,
            &format!("m{m}"),
            stats(0, 0, 4096, 10),
            1_000_000,
            0,
        ));
    }
    let mut id = mids + 1;
    while (events.len() as u64) < total {
        let parent = 1 + (id % mids);
        events.push(dir_ev(
            id,
            parent,
            &format!("l{id}"),
            stats(0, 0, 4096, 10),
            0,
            0,
        ));
        id += 1;
    }
    let msgs = events
        .chunks(per_batch)
        .map(|c| ScanMsg::Batch {
            generation: 0,
            mode: ScanMode::Fresh,
            events: c.to_vec(),
        })
        .collect();
    (msgs, total)
}

#[test]
fn a_prefilled_backlog_drains_with_a_zero_poll_timeout_at_a_quarter_million_events_a_second() {
    let (msgs, total) = big_backlog(64);
    let mut r = Run::real_time(vec![(4_000, ch('q'))]);
    *r.source.prefill.lock().unwrap() = msgs;
    let source = Arc::clone(&r.source);
    // end the run once the backlog is through: finish, then a key shortly after
    let feeder = std::thread::spawn(move || {
        while source.count() == 0 {
            std::thread::sleep(Duration::from_millis(1));
        }
        source.send(0, finished(0, false, StopCause::None));
    });
    // `q` waits for the frame that follows the end of the scan: `finished` is queued behind the backlog, so a
    // screen that no longer says SCANNING has applied every event, however long a preempted thread took to get
    // there
    r.script = vec![(100, ch('q'))];
    r.gates = vec![(0, "!SCANNING")];
    let cpu_before = thread_cpu_ns();
    let out = run(r);
    let cpu = thread_cpu_ns().zip(cpu_before).map(|(a, b)| a - b);
    feeder.join().unwrap();
    assert_eq!(out.exit.stats.node_events, total, "every event was applied");
    let wall_rate = out.exit.stats.node_events as f64 / out.exit.stats.apply_time.as_secs_f64();
    eprintln!(
        "drained {total} events in {:?}: {wall_rate:.0}/s (thread cpu {cpu:?} ns)",
        out.exit.stats.apply_time
    );
    // The bound is a speed of the loop, so it is judged on the CPU time of the loop's thread for the whole run
    // (apply, redraws and polls included): a machine that preempts the thread, as a saturated CI host does,
    // stretches the wall-clock `apply_time` without making the loop slower. Without /proc/thread-self/schedstat
    // the wall-clock rate is used.
    let rate = cpu.map_or(wall_rate, |ns| total as f64 / (ns.max(1) as f64 / 1e9));
    assert!(
        rate >= 250_000.0,
        "{rate:.0} events/s (wall {wall_rate:.0})"
    );
    // zero timeouts while data remained, the tick only once the channel was empty
    let timeouts: Vec<Duration> = out.polls.iter().map(|p| p.0).collect();
    let first_nonzero = timeouts
        .iter()
        .position(|t| !t.is_zero())
        .unwrap_or(timeouts.len());
    assert!(first_nonzero >= 3, "{first_nonzero} zero polls");
    assert!(timeouts[first_nonzero..]
        .iter()
        .all(|t| !t.is_zero() && *t <= Duration::from_millis(50)));
}

#[test]
fn a_key_pressed_during_the_backlog_is_handled_within_one_iteration() {
    let (msgs, _) = big_backlog(64);
    let r = Run::real_time(vec![(30, ch('?')), (400, key(Key::Esc)), (500, ch('q'))]);
    *r.source.prefill.lock().unwrap() = msgs;
    let out = run(r);
    // the help overlay was drawn: a frame appears between the key's time and 100 ms later
    let at = |t: u64| out.polls.iter().find(|p| p.1 >= t).map(|p| p.2);
    assert!(
        at(30).unwrap() < at(150).unwrap(),
        "no frame after the key: {:?}",
        out.polls.len()
    );
    assert!(out.elapsed_ms < 1_500, "q took {} ms", out.elapsed_ms);
    assert!(out.exit.stats.node_events > 0);
    assert_eq!(out.exit.code, 0);
}

#[test]
fn signals_and_ctrl_c_map_to_their_exit_codes_with_no_plan() {
    for (sig, want) in [(2, 130), (15, 143), (1, 143), (3, 143)] {
        let mut r = Run::virtual_time(vec![(60_000, ch('q'))]);
        r.signals = LogSignals::at(&r.log, 3, sig);
        let out = run(r);
        assert_eq!(out.exit.code, want, "signal {sig}");
        assert!(out.exit.stdout_lines.is_empty(), "no plan on a signal");
        assert_eq!(out.log, ["restore"], "restored once");
    }
    let out = run(Run::virtual_time(vec![(100, key(Key::Ctrl('c')))]));
    assert_eq!(out.exit.code, 130);
    assert!(out.exit.stdout_lines.is_empty());
}

#[test]
fn a_signal_code_wins_over_a_failed_restore_and_a_plain_exit_reports_74() {
    let mut r = Run::virtual_time(vec![(60_000, ch('q'))]);
    r.signals = LogSignals::at(&r.log, 3, 15);
    r.fail_restore = true;
    let out = run(r);
    assert_eq!(
        out.exit.code, 143,
        "EIO on the restore is not reported as 74"
    );
    assert!(!out.exit.stderr_lines.iter().any(|l| l == RESTORE_FAILED));

    let mut r = Run::virtual_time(vec![(100, ch('q'))]);
    r.fail_restore = true;
    let out = run(r);
    assert_eq!(out.exit.code, 74);
    assert!(
        out.exit.stderr_lines.contains(&RESTORE_FAILED.to_string()),
        "{:?}",
        out.exit.stderr_lines
    );
}

#[test]
fn ctrl_z_and_an_external_sigtstp_restore_suspend_reenter_and_redraw() {
    let r = Run::virtual_time(vec![(100, key(Key::Ctrl('z'))), (400, ch('q'))]);
    let out = run(r);
    assert_eq!(
        out.log,
        ["restore", "suspend", "reenter", "restore"],
        "Ctrl-Z, then the exit restore"
    );
    let frames_after = out.polls.last().unwrap().2;
    assert!(frames_after >= 2, "redrawn after the resume");

    let r = Run::virtual_time(vec![(100, ch('q'))]);
    r.signals.suspend.store(true, Ordering::SeqCst);
    let out = run(r);
    assert_eq!(&out.log[..3], ["restore", "suspend", "reenter"]);
}

#[test]
fn q_returns_within_a_second_with_a_scan_thread_blocked_and_clears_the_save_gate() {
    let r = Run::virtual_time(vec![(100, ch('q'))]);
    let source = Arc::clone(&r.source);
    let t0 = std::time::Instant::now();
    let out = run(r);
    assert!(t0.elapsed() < Duration::from_secs(1));
    let rec = &source.starts.lock().unwrap()[0];
    assert!(!rec.save.load(Ordering::SeqCst), "the save gate is cleared");
    assert!(rec.cancel.load(Ordering::SeqCst));
    assert_eq!(out.exit.code, 0);
    assert_eq!(out.exit.stdout_lines, [OsString::from(CLOSED_PARTIAL_LINE)]);
}

#[test]
fn q_after_a_finished_scan_prints_the_closing_line_and_diagnostics_go_to_stderr_escaped() {
    let r = Run::virtual_time(vec![(300, ch('q'))]);
    {
        let mut pre = r.source.prefill.lock().unwrap();
        pre.push(root_batch(&[("a", 4096)], false));
        pre.push(ScanMsg::Finished(ScanSummary {
            notes: vec!["cache not saved to /tmp/\u{1b}[31mx: disk full".into()],
            ..ScanSummary::default()
        }));
    }
    let out = run(r);
    assert_eq!(out.exit.stdout_lines, [OsString::from(CLOSED_LINE)]);
    assert_eq!(
        out.exit.stderr_lines.len(),
        1,
        "{:?}",
        out.exit.stderr_lines
    );
    assert!(out.exit.stderr_lines[0].contains("cache not saved"));
    assert!(!out.exit.stderr_lines[0].contains('\u{1b}'));
    assert!(
        !out.screen.contains("cache not saved"),
        "nothing is printed during the session"
    );
}

fn with_kids() -> Run {
    let r = Run::virtual_time(vec![]);
    r.source.prefill.lock().unwrap().push(root_batch(
        &[("node_modules", 5 << 30), ("src", 4096)],
        false,
    ));
    r
}

#[test]
fn planning_does_not_freeze_the_screen_and_signals_end_the_run_at_once() {
    let mut r = with_kids();
    r.actions = Arc::new(SlowActions {
        delay: Duration::from_secs(5),
        plan: plan_fixture(),
        calls: Log::default(),
    });
    r.time = Time::Real(Arc::new(
        disk_cleanup_tui::event_loop::SystemClock::default(),
    ));
    r.script = vec![(100, ch(' ')), (200, ch('p'))];
    r.signals = LogSignals::never(&r.log);
    let sig = Arc::clone(&r.signals);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(600));
        *sig.signal.lock().unwrap() = Some((0, 15));
    });
    let t0 = std::time::Instant::now();
    let out = run(r);
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "SIGTERM waited for the planner: {:?}",
        t0.elapsed()
    );
    assert_eq!(out.exit.code, 143);
    assert!(
        first_line_with(&out.screen, "PLANNING...").is_some(),
        "{}",
        out.screen
    );
}

#[test]
fn esc_cancels_the_plan_and_q_leaves_while_it_runs() {
    let mut r = with_kids();
    r.time = Time::Real(Arc::new(
        disk_cleanup_tui::event_loop::SystemClock::default(),
    ));
    r.actions = Arc::new(SlowActions {
        delay: Duration::from_secs(5),
        plan: plan_fixture(),
        calls: Log::default(),
    });
    r.script = vec![
        (100, ch(' ')),
        (200, ch('p')),
        (400, key(Key::Esc)),
        (500, ch('p')),
        (700, ch('q')),
    ];
    let t0 = std::time::Instant::now();
    let out = run(r);
    assert!(t0.elapsed() < Duration::from_secs(2));
    assert_eq!(out.exit.code, 0);
    assert!(
        out.exit.stdout_lines.contains(&OsString::from(CLOSED_LINE))
            || out
                .exit
                .stdout_lines
                .contains(&OsString::from(CLOSED_PARTIAL_LINE))
    );
}

#[test]
fn a_finished_plan_shows_the_preview_with_no_next_line_and_esc_returns_to_the_list() {
    // The planner and the review answer from worker threads, so the clock is the real one: with virtual
    // time the eight loop iterations between `p` and `q` can pass before a thread has run.
    let mut r = with_kids();
    r.time = Time::Real(Arc::new(
        disk_cleanup_tui::event_loop::SystemClock::default(),
    ));
    r.script = vec![(100, ch(' ')), (200, ch('p')), (600, ch('q'))];
    let open = run(r);
    assert!(
        open.screen.contains("PLAN PREVIEW - nothing is deleted."),
        "{}",
        open.screen
    );
    assert!(open.screen.contains("d      delete these items"));
    assert!(!open.screen.contains("Next:"));

    let mut r = with_kids();
    r.time = Time::Real(Arc::new(
        disk_cleanup_tui::event_loop::SystemClock::default(),
    ));
    r.script = vec![
        (100, ch(' ')),
        (200, ch('p')),
        (600, key(Key::Esc)),
        (900, ch('x')),
        (1000, ch('q')),
    ];
    let out = run(r);
    assert_eq!(out.exit.code, 0);
    assert!(
        first_line_with(&out.screen, "marks: 0").is_some(),
        "{}",
        out.screen
    );
}

#[test]
fn the_quit_prompt_enter_runs_the_planner_off_thread_and_sigint_still_exits_130() {
    let mut r = with_kids();
    r.time = Time::Real(Arc::new(
        disk_cleanup_tui::event_loop::SystemClock::default(),
    ));
    r.actions = Arc::new(SlowActions {
        delay: Duration::from_secs(5),
        plan: plan_fixture(),
        calls: Log::default(),
    });
    r.script = vec![(100, ch(' ')), (200, ch('q')), (300, key(Key::Enter))];
    r.signals = LogSignals::never(&r.log);
    let sig = Arc::clone(&r.signals);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(700));
        *sig.signal.lock().unwrap() = Some((0, 2));
    });
    let t0 = std::time::Instant::now();
    let out = run(r);
    assert!(t0.elapsed() < Duration::from_secs(2));
    assert_eq!(out.exit.code, 130);
    assert!(
        first_line_with(&out.screen, "PLANNING...").is_some(),
        "{}",
        out.screen
    );
}

#[test]
fn the_quit_prompt_enter_hands_the_planners_lines_and_code_to_the_caller() {
    let mut r = with_kids();
    r.script = vec![
        (100, ch(' ')),
        (200, ch('q')),
        (300, key(Key::Enter)),
        (60_000, ch('q')),
    ];
    let out = run(r);
    assert_eq!(
        out.exit.stdout_lines,
        [OsString::from("DRY RUN - nothing was deleted.")]
    );
    assert_eq!(out.exit.code, 0);
}

fn batch_gen(generation: u32, id: u64, parent: u64, name: &str, mode: ScanMode) -> ScanMsg {
    ScanMsg::Batch {
        generation,
        mode,
        events: vec![dir_ev(id, parent, name, stats(1, 0, 4096, 10), 0, 0)],
    }
}

#[test]
fn rescan_restarts_the_source_at_the_directory_drops_the_old_receiver_and_ignores_old_generations()
{
    let r = with_kids();
    let source = Arc::clone(&r.source);
    let mut r = r;
    r.script = vec![(100, ch('r')), (400, ch('q'))];
    let out = run(r);
    let starts = source.starts.lock().unwrap();
    assert_eq!(starts.len(), 2, "the rescan started a second scan");
    assert_eq!(starts[1].roots, [PathBuf::from("/h/code/app")]);
    assert_eq!(
        starts[1].opts.generation, 1,
        "begin_rescan gave a new generation"
    );
    assert!(!starts[1].opts.paint_from_cache);
    assert!(
        starts[0].cancel.load(Ordering::SeqCst),
        "the old source is cancelled"
    );
    assert!(
        starts[0]
            .tx
            .send(finished(0, false, StopCause::None))
            .is_err(),
        "the old receiver is dropped"
    );
    // the rows of the rescanned directory read `.` and the title says SCANNING again
    assert!(
        first_line_with(&out.screen, "SCANNING").is_some(),
        "{}",
        out.screen
    );
    assert!(
        !out.screen.contains("node_modules"),
        "the subtree was dropped until the new batches arrive"
    );
    drop(starts);
    let _ = batch_gen(0, 1, 0, "stale", ScanMode::Fresh);
}

#[test]
fn rescan_all_bypasses_the_cache_and_keeps_the_roots() {
    let mut r = with_kids();
    r.tweak = Box::new(|c| c.scan_opts.paint_from_cache = true);
    let source = Arc::clone(&r.source);
    r.script = vec![(100, ch('R')), (400, ch('q'))];
    let _ = run(r);
    let starts = source.starts.lock().unwrap();
    assert_eq!(starts.len(), 2);
    assert!(starts[0].opts.paint_from_cache);
    assert!(
        !starts[1].opts.paint_from_cache,
        "R is --rescan: the cache is bypassed"
    );
    assert_eq!(starts[1].roots, starts[0].roots);
}

#[test]
fn enter_on_an_x_row_pushes_a_root_h_pops_it_and_a_fifth_push_is_refused() {
    let boundary = |id, name: &str| NodeEvent::Boundary {
        id,
        parent: 0,
        name: name.into(),
        dev: 7,
    };
    let root = dir_ev(0, NO_PARENT, "/h/code/app", stats(0, 1, 4096, 0), 1, 0);
    let mk = |kids: Vec<NodeEvent>| {
        let mut ev = vec![root.clone()];
        ev.extend(kids);
        ScanMsg::Batch {
            generation: 0,
            mode: ScanMode::Fresh,
            events: ev,
        }
    };
    let r = Run::virtual_time(vec![]);
    r.source
        .prefill
        .lock()
        .unwrap()
        .push(mk(vec![boundary(1, "mnt")]));
    let source = Arc::clone(&r.source);
    let mut r = r;
    r.script = vec![
        (100, ch('j')),
        (150, key(Key::Enter)),
        (300, key(Key::Left)),
        (500, ch('q')),
    ];
    let out = run(r);
    let starts = source.starts.lock().unwrap();
    assert_eq!(
        starts.len(),
        2,
        "h at the pushed root restores without a rescan"
    );
    assert_eq!(starts[1].roots, [PathBuf::from("/h/code/app/mnt")]);
    assert_eq!(starts[1].opts.generation, 0);
    assert!(!starts[1].opts.cross_fs);
    assert!(starts[0].cancel.load(Ordering::SeqCst));
    assert!(
        first_line_with(&out.screen, "mnt").is_some(),
        "{}",
        out.screen
    );
    assert!(
        first_line_with(&out.screen, "(another filesystem").is_none(),
        "popped: {}",
        out.screen
    );
    drop(starts);

    // the pushed root: title SCANNING and the path line names the other filesystem
    let r2 = Run::virtual_time(vec![(100, ch('j')), (150, key(Key::Enter)), (400, ch('q'))]);
    r2.source
        .prefill
        .lock()
        .unwrap()
        .push(mk(vec![boundary(1, "mnt")]));
    let out2 = run(r2);
    assert!(
        first_line_with(&out2.screen, "SCANNING").is_some(),
        "{}",
        out2.screen
    );
    assert!(
        first_line_with(&out2.screen, "(another filesystem").is_some(),
        "{}",
        out2.screen
    );
}

#[test]
fn a_fifth_nested_scan_gives_the_footer_four_nested_scans() {
    let boundary = NodeEvent::Boundary {
        id: 1,
        parent: 0,
        name: "mnt".into(),
        dev: 7,
    };
    let root = dir_ev(0, NO_PARENT, "/h/code/app", stats(0, 1, 4096, 0), 1, 0);
    let r = Run::virtual_time(vec![]);
    let batch = ScanMsg::Batch {
        generation: 0,
        mode: ScanMode::Fresh,
        events: vec![root, boundary],
    };
    // each pushed root answers with the same shape
    let source = Arc::clone(&r.source);
    let feeder = std::thread::spawn(move || {
        let mut fed = 0;
        loop {
            let n = source.count();
            while fed < n {
                source.send(fed, batch.clone());
                fed += 1;
            }
            if n >= 5 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    let mut r = r;
    r.time = Time::Real(Arc::new(
        disk_cleanup_tui::event_loop::SystemClock::default(),
    ));
    r.script = (0..6)
        .flat_map(|i| [(300 + i * 300, ch('j')), (330 + i * 300, key(Key::Enter))])
        .chain([(2_500, ch('q'))])
        .collect();
    let src2 = Arc::clone(&r.source);
    let out = run(r);
    feeder.join().ok();
    assert_eq!(
        src2.count(),
        5,
        "the original and four nested scans, the fifth push refused"
    );
    assert!(
        first_line_with(&out.screen, "4 nested scans: go back with h").is_some(),
        "{}",
        out.screen
    );
}

#[test]
fn resize_events_coalesce_to_one_query_per_tick_and_relayout_keeping_the_cursor() {
    let mut r = with_kids();
    r.script = (0..20)
        .map(|_| (200, Input::Resize))
        .chain([(260, ch('j')), (500, ch('q'))])
        .collect();
    r.resize_to = Some((70, 24));
    let out = run(r);
    assert_eq!(out.size_queries, 1, "a storm of 20 resize events asks once");
    // the narrow tier: no APPARENT column
    assert!(!out.screen.contains("APPARENT"), "{}", out.screen);
    assert!(out.screen.contains("USAGE"));
}

#[test]
fn a_failed_size_query_mid_session_is_too_small_not_a_crash() {
    let mut r = with_kids();
    r.script = vec![(200, Input::Resize), (400, ch('q'))];
    r.resize_to = None;
    let out = run(r);
    assert_eq!(out.exit.code, 0);
}
