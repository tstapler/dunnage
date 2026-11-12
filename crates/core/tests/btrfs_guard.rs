#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! The pure rules of the btrfs balance ladder (Story 4.3.2, Task 4.3.2a). No command is run and no machine
//! state is read.

use std::path::Path;

use dunnage_core::ports::{classify, Class};
use dunnage_core::target::Guarded;
use dunnage_core::targets::btrfs_guard::{
    already_running_note, balance_blocker, balance_cmd, decide_start, next_step, parse_relocated,
    scrub_blocker, timer_blocker, Blocker, Next, StepResult, StopReason, LADDER,
    MIN_UNALLOCATED_FOR_MOVE, TARGET_UNALLOCATED,
};
use dunnage_core::SkipReason;

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

/// Runs the ladder the way the adapter does: `moved(usage)` is how many chunks a step relocates and
/// `unallocated_after(usage)` the re-read value after it.
fn drive(
    start: u64,
    moved: impl Fn(u8) -> u32,
    unallocated_after: impl Fn(u8) -> u64,
) -> (Vec<u8>, StopReason) {
    let mut done: Vec<StepResult> = Vec::new();
    let mut unallocated = start;
    loop {
        match next_step(&done, Some(unallocated)) {
            Next::Run(u) => {
                done.push(StepResult {
                    usage: u,
                    relocated: Some(moved(u)),
                });
                unallocated = unallocated_after(u);
            }
            Next::Stop(r) => return (done.iter().map(|s| s.usage).collect(), r),
        }
    }
}

#[test]
fn ladder_should_run_dusage_0_5_10_20_40_with_reread_and_stop_at_zero_relocated_When_3gib_unallocated(
) {
    // every step moves chunks and gives 1 GiB back: the whole ladder runs
    let (steps, stop) = drive(3 * GIB, |_| 2, |u| 3 * GIB + u64::from(u) * 10 * MIB);
    assert_eq!(steps, LADDER.to_vec());
    assert_eq!(stop, StopReason::Finished);

    // -dusage=10 relocates nothing: 20 and 40 never run
    let (steps, stop) = drive(3 * GIB, |u| u32::from(u != 10), |_| 3 * GIB);
    assert_eq!(steps, vec![0, 5, 10]);
    assert_eq!(stop, StopReason::NothingToRelocate(10));
}

#[test]
fn ladder_should_go_on_to_dusage_5_When_dusage_0_found_no_empty_chunk() {
    let (steps, _) = drive(3 * GIB, |u| u32::from(u == 5), |_| 3 * GIB);
    assert_eq!(steps[..2], [0, 5]);
}

#[test]
fn ladder_should_stop_when_the_reread_unallocated_reaches_the_target() {
    let (steps, stop) = drive(
        3 * GIB,
        |_| 4,
        |u| if u == 5 { TARGET_UNALLOCATED } else { 3 * GIB },
    );
    assert_eq!(steps, vec![0, 5]);
    assert_eq!(stop, StopReason::TargetReached);
}

#[test]
fn ladder_should_run_nothing_When_unallocated_is_already_at_the_target() {
    let (steps, stop) = drive(TARGET_UNALLOCATED + GIB, |_| 1, |_| 0);
    assert!(steps.is_empty());
    assert_eq!(stop, StopReason::TargetReached);
}

#[test]
fn non_zero_dusage_should_not_run_and_say_why_When_unallocated_400mib() {
    let (steps, stop) = drive(400 * MIB, |_| 1, |_| 400 * MIB);
    assert_eq!(steps, vec![0]);
    assert_eq!(
        stop,
        StopReason::TooLittleUnallocated {
            next: 5,
            unallocated: Some(400 * MIB)
        }
    );
    let note = stop.note();
    assert!(note.contains("-dusage=5"), "{note}");
    assert!(note.contains("400.0 MiB"), "{note}");
    assert!(note.contains("1.0 GiB"), "{note}");
    assert!(note.contains("only -dusage=0 runs"), "{note}");
}

#[test]
fn non_zero_dusage_should_become_possible_When_dusage_0_freed_chunks_above_the_threshold() {
    let (steps, _) = drive(400 * MIB, |_| 1, |_| 2 * GIB);
    assert_eq!(steps[..2], [0, 5]);
}

#[test]
fn the_threshold_is_exactly_one_gib() {
    assert_eq!(MIN_UNALLOCATED_FOR_MOVE, GIB);
    assert_eq!(
        next_step(&[done(0, 1)], Some(GIB - 1)),
        Next::Stop(StopReason::TooLittleUnallocated {
            next: 5,
            unallocated: Some(GIB - 1)
        })
    );
    assert_eq!(next_step(&[done(0, 1)], Some(GIB)), Next::Run(5));
}

#[test]
fn an_unreadable_unallocated_should_allow_only_dusage_0() {
    assert_eq!(next_step(&[], None), Next::Run(0));
    assert!(matches!(
        next_step(&[done(0, 3)], None),
        Next::Stop(StopReason::TooLittleUnallocated {
            next: 5,
            unallocated: None
        })
    ));
    assert!(StopReason::TooLittleUnallocated {
        next: 5,
        unallocated: None
    }
    .note()
    .contains("unknown"));
}

#[test]
fn a_step_whose_output_has_no_count_should_stop_the_ladder() {
    let s = StepResult {
        usage: 5,
        relocated: None,
    };
    assert_eq!(
        next_step(&[done(0, 1), s], Some(5 * GIB)),
        Next::Stop(StopReason::UnrecognisedOutput(5))
    );
}

fn done(usage: u8, relocated: u32) -> StepResult {
    StepResult {
        usage,
        relocated: Some(relocated),
    }
}

#[test]
fn parse_relocated_should_read_the_done_line_and_nothing_else() {
    assert_eq!(
        parse_relocated("Done, had to relocate 3 out of 120 chunks\n"),
        Some(3)
    );
    assert_eq!(
        parse_relocated("WARNING: x\nDone, had to relocate 0 out of 0 chunks"),
        Some(0)
    );
    assert_eq!(parse_relocated(""), None);
    assert_eq!(
        parse_relocated("Done, had to relocate many out of 3 chunks"),
        None
    );
    assert_eq!(parse_relocated("Done, had to relocate 3"), None);
    assert_eq!(parse_relocated("had to relocate 3 out of 4 chunks"), None);
}

#[test]
fn every_step_cmd_should_be_a_row_of_the_allowlist_and_every_non_zero_one_carry_limit_8() {
    for u in LADDER {
        let cmd = balance_cmd(u, Path::new("/"));
        assert_eq!(classify(&cmd), Some(Class::Mutating), "usage {u}");
        let rendered = dunnage_core::ports::render_argv(&cmd.program, &cmd.args);
        if u == 0 {
            assert_eq!(rendered, "btrfs balance start -dusage=0 /");
        } else {
            assert_eq!(
                rendered,
                format!("btrfs balance start -dusage={u},limit=8 /")
            );
        }
    }
    // a step off the ladder is not a row
    assert_eq!(classify(&balance_cmd(50, Path::new("/"))), None);
    assert_eq!(classify(&balance_cmd(100, Path::new("/"))), None);
}

#[test]
fn balance_should_Skip_AlreadyRunning_naming_balance_scrub_or_timer_When_active() {
    let idle = "No balance found on '/'\n";
    assert_eq!(balance_blocker(idle), None);
    assert_eq!(
        balance_blocker("Balance on '/' is running\n3 out of about 120 chunks balanced (4 considered),  98% left\n"),
        Some(Blocker::BalanceRunning)
    );
    assert_eq!(
        balance_blocker("Balance on '/' is paused\n"),
        Some(Blocker::BalanceRunning)
    );
    assert_eq!(
        balance_blocker("garbage"),
        Some(Blocker::BalanceStatusUnknown)
    );
    assert_eq!(balance_blocker(""), Some(Blocker::BalanceStatusUnknown));

    assert_eq!(
        scrub_blocker("UUID: x\nStatus:           running\nDuration: 0:00:01\n"),
        Some(Blocker::ScrubRunning)
    );
    assert_eq!(scrub_blocker("Status:           finished\n"), None);
    assert_eq!(scrub_blocker("no stats available\n"), None);

    assert_eq!(timer_blocker("active\n"), Some(Blocker::TimerActive));
    assert_eq!(timer_blocker("inactive\n"), None);
    assert_eq!(timer_blocker(""), None);

    assert_eq!(decide_start(0, &[]), Guarded::Allow);
    for b in [
        Blocker::BalanceRunning,
        Blocker::BalanceStatusUnknown,
        Blocker::ScrubRunning,
        Blocker::TimerActive,
    ] {
        assert_eq!(
            decide_start(0, &[b]),
            Guarded::Skip(SkipReason::AlreadyRunning)
        );
    }
    let note = already_running_note(&[Blocker::ScrubRunning, Blocker::TimerActive]);
    assert!(
        note.contains("scrub") && note.contains("btrfs-balance.timer"),
        "{note}"
    );
    assert!(already_running_note(&[Blocker::BalanceRunning]).contains("balance is already running"));
}

#[test]
fn decide_start_should_say_NeedsRoot_before_anything_else_When_not_root() {
    assert_eq!(
        decide_start(1000, &[]),
        Guarded::Skip(SkipReason::NeedsRoot)
    );
    assert_eq!(
        decide_start(1000, &[Blocker::BalanceRunning]),
        Guarded::Skip(SkipReason::NeedsRoot)
    );
}

// ---------------------------------------------------------------------------------------------- trim

#[test]
fn parse_trimmed_should_read_both_fstrim_forms_and_nothing_else() {
    use dunnage_core::targets::btrfs_guard::parse_trimmed;
    assert_eq!(
        parse_trimmed("/: 1.2 TiB (1319011106816 bytes) trimmed on /dev/x\n"),
        Some(1_319_011_106_816)
    );
    assert_eq!(parse_trimmed("trimmed 120 GiB\n"), Some(120 * GIB));
    assert_eq!(parse_trimmed("trimmed 1.5 MiB"), Some(MIB + MIB / 2));
    assert_eq!(parse_trimmed("/: 0 B (0 bytes) trimmed\n"), Some(0));
    assert_eq!(parse_trimmed("fstrim: /: FITRIM ioctl failed\n"), None);
    assert_eq!(parse_trimmed(""), None);
}

#[test]
fn trim_cmd_should_be_the_allowlisted_fstrim_v_mount_and_decide_trim_should_need_root() {
    use dunnage_core::targets::btrfs_guard::{decide_trim, trim_cmd};
    let cmd = trim_cmd(Path::new("/"));
    assert_eq!(classify(&cmd), Some(Class::Mutating));
    assert_eq!(decide_trim(0), Guarded::Allow);
    assert_eq!(decide_trim(1000), Guarded::Skip(SkipReason::NeedsRoot));
}
