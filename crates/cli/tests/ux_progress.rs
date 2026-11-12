#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 1.2.4, Task 1.2.4b: progress on a shared terminal (UX-59, gap G-17). The decision is a pure function
//! over its whole truth table; the lines are checked against a clock the test advances by hand.

use dunnage_cli::progress::{
    progress_mode, Progress, ProgressMode, MIN_LINE_GAP_MS, STALL_AFTER_MS,
};

#[test]
fn progress_mode_should_follow_the_whole_truth_table() {
    for bits in 0..32_u8 {
        let [stdout_tty, stderr_tty, quiet, json, no_progress] =
            [0, 1, 2, 3, 4].map(|i| bits & (1 << i) != 0);
        let m = progress_mode(stdout_tty, stderr_tty, quiet, json, no_progress);
        let silenced = quiet || json || no_progress;
        assert_eq!(
            m.per_item,
            !stdout_tty && stderr_tty && !silenced,
            "per-item lines for {bits:05b}"
        );
        assert_eq!(
            m.scanning,
            stderr_tty && !silenced,
            "scanning lines for {bits:05b}"
        );
    }
    // The three cases named by the plan.
    assert!(progress_mode(false, true, false, false, false).per_item);
    assert!(
        !progress_mode(true, true, false, false, false).per_item,
        "one shared terminal"
    );
    assert!(progress_mode(true, true, false, false, false).scanning);
}

fn lines_of(mode: ProgressMode, steps: &[(u64, usize)]) -> String {
    let mut sink: Vec<u8> = Vec::new();
    {
        let mut p = Progress::new(mode, &mut sink, 0);
        for (t, n) in steps {
            p.removing(*t, *n, 3, "~/.cache/x");
        }
    }
    String::from_utf8(sink).unwrap()
}

#[test]
fn per_item_lines_should_be_plain_at_most_one_per_two_seconds_and_never_use_a_carriage_return() {
    let on = ProgressMode {
        per_item: true,
        scanning: true,
    };
    let out = lines_of(
        on,
        &[(0, 1), (500, 2), (1_999, 3), (2_000 + MIN_LINE_GAP_MS, 3)],
    );
    assert_eq!(
        out,
        "[1/3] removing ~/.cache/x\n[3/3] removing ~/.cache/x\n"
    );
    assert!(!out.contains('\r'));
    assert_eq!(
        lines_of(
            ProgressMode {
                per_item: false,
                scanning: true
            },
            &[(0, 1)]
        ),
        ""
    );
}

#[test]
fn scanning_lines_should_obey_the_same_gap_and_the_mode() {
    let mut sink: Vec<u8> = Vec::new();
    {
        let mut p = Progress::new(
            ProgressMode {
                per_item: false,
                scanning: true,
            },
            &mut sink,
            0,
        );
        p.scanning(0, "~/code");
        p.scanning(100, "~/dotfiles");
        p.scanning(2_100, "~/Programming");
    }
    assert_eq!(
        String::from_utf8(sink).unwrap(),
        "scanning ~/code\nscanning ~/Programming\n"
    );
}

#[test]
fn the_watchdog_should_warn_once_per_stall_after_15_seconds_even_when_progress_lines_are_off() {
    let mut sink: Vec<u8> = Vec::new();
    let off = ProgressMode {
        per_item: false,
        scanning: false,
    };
    let mut p = Progress::new(off, &mut sink, 0);
    assert!(!p.watchdog(STALL_AFTER_MS - 1));
    assert!(p.watchdog(STALL_AFTER_MS));
    assert!(!p.watchdog(STALL_AFTER_MS + 5_000), "once per stall");
    p.removing(STALL_AFTER_MS + 6_000, 1, 3, "x");
    assert!(
        p.watchdog(STALL_AFTER_MS * 2 + 6_000),
        "progress re-arms it"
    );
    let text = String::from_utf8(sink).unwrap();
    assert_eq!(
        text.matches("warning: no progress for 15s").count(),
        2,
        "{text}"
    );
    assert!(!text.contains('\r'));
}
