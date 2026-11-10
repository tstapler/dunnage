#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::many_single_char_names,
    non_snake_case
)]
//! The `Executor` over fakes (Story 1.1.4): plan mode, ordering, audit, abort, the pool, the gate.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use common::{
    build_dir, cache, caches_selection, freed, run, selection, spin_until, FakeTarget, Rig, NOW,
};
use disk_cleanup_core::audit_types::AuditRecord;
use disk_cleanup_core::delete_types::{DeleteError, DeleteFailure, Freed};
use disk_cleanup_core::executor::{changed_since_scan_note, AbortCause, ExitClass};
use disk_cleanup_core::ports::RunGuard as _;
use disk_cleanup_core::target::{Guarded, KindGate};
use disk_cleanup_core::testing::{AuditCall, EventLog, FakeAuditSink, FakeSpaceMeter, MeterCall};
use disk_cleanup_core::{
    AllocSnapshot, AllocatedBytes, ItemKey, SavedSelection, SelectorId, SkipReason, SpaceUse,
    Status,
};

fn three_caches() -> Vec<disk_cleanup_core::Candidate> {
    (1..=3).map(|i| cache(i, 4096 * i)).collect()
}

#[test]
fn plan_mode_should_make_zero_mutating_calls_and_zero_intents_When_three_Will_items() {
    let rig = Rig::new();
    let mut t = FakeTarget::caches(three_caches());
    t.exec = Box::new(|_| panic!("plan mode must never execute"));
    let r = run(&rig, &[&t], &caches_selection(), false, 4, None).unwrap();
    assert_eq!(r.plan.will_count(), 3);
    assert!(r
        .summary
        .outcomes
        .iter()
        .all(|o| o.status != Status::Removed));
    assert_eq!(rig.runner.mutating_calls(), 0);
    assert!(rig.audit.calls().is_empty(), "no run_start, no intent");
    assert_eq!(rig.guard.acquire_calls(), 0);
    assert!(t.executed().is_empty());
    assert!(r.summary.fs_after.is_none());
    assert_eq!(r.exit, ExitClass::Ok);
}

#[test]
fn executor_should_call_run_start_intent_result_intent_result_run_end_and_settle_once_and_acquire_guard_first_When_two_items(
) {
    let log = EventLog::new();
    let rig = Rig::new().with_log(&log);
    let t = FakeTarget::caches(vec![cache(1, 4096), cache(2, 8192)]);
    let r = run(&rig, &[&t], &caches_selection(), true, 1, None).unwrap();
    assert_eq!(
        rig.audit.calls(),
        [
            AuditCall::RunStart,
            AuditCall::Intent,
            AuditCall::Result,
            AuditCall::Intent,
            AuditCall::Result,
            AuditCall::RunEnd
        ]
    );
    let ev = log.events();
    let pos = |name: &str| ev.iter().position(|e| e == name).unwrap();
    assert!(pos("acquire") < pos("audit:Intent"), "{ev:?}");
    assert_eq!(ev.iter().filter(|e| *e == "settle").count(), 1);
    let last_snapshot = ev.iter().rposition(|e| e == "snapshot").unwrap();
    assert!(
        pos("settle") < last_snapshot,
        "settle before the after-snapshot: {ev:?}"
    );
    assert_eq!(r.exit, ExitClass::Ok);
    assert_eq!(r.summary.attributed_bytes, 12288);
    let paths: Vec<_> = rig
        .space
        .calls()
        .into_iter()
        .filter(|c| matches!(c, MeterCall::Settle(_)))
        .collect();
    assert_eq!(paths.len(), 1);
}

#[test]
fn audit_failure_on_second_intent_should_leave_item_1_Removed_items_2_3_unexecuted_exit_2_When_fake_sink_fails(
) {
    let mut rig = Rig::new();
    rig.audit = FakeAuditSink::new().fail_nth(AuditCall::Intent, 2);
    let t = FakeTarget::caches(three_caches());
    let r = run(&rig, &[&t], &caches_selection(), true, 1, None).unwrap();
    let st: Vec<_> = r
        .summary
        .outcomes
        .iter()
        .map(|o| o.status.clone())
        .collect();
    assert_eq!(
        st,
        [
            Status::Removed,
            Status::Skipped(SkipReason::Aborted),
            Status::Skipped(SkipReason::Aborted)
        ]
    );
    assert_eq!(
        t.executed().len(),
        1,
        "item 2 never executes without its intent"
    );
    assert_eq!(r.exit, ExitClass::Fatal);
    assert_eq!(r.exit.code(), 2);
    assert_eq!(r.abort, Some(AbortCause::AuditFailed));
    assert!(r.fatal.is_some());
}

#[test]
fn item_clean_at_plan_dirty_at_execute_should_be_Skipped_Dirty_and_execute_not_called_When_apply() {
    let rig = Rig::new();
    let mut t = FakeTarget::caches(vec![cache(1, 4096)]);
    t.guard = Box::new(|_, n| {
        if n == 0 {
            Guarded::Allow
        } else {
            Guarded::Skip(SkipReason::Dirty)
        }
    });
    let r = run(&rig, &[&t], &caches_selection(), true, 1, None).unwrap();
    assert_eq!(
        r.summary.outcomes[0].status,
        Status::Skipped(SkipReason::Dirty)
    );
    assert!(t.executed().is_empty());
}

#[test]
fn guard_failing_at_delete_time_should_be_Skipped_with_changed_since_scan_counted_apart_from_errors_and_exit_0_When_facts_flip(
) {
    let rig = Rig::new();
    let reasons = [
        SkipReason::Dirty,
        SkipReason::TooRecent,
        SkipReason::VolumeReferenced,
        SkipReason::Symlink,
    ];
    let mut t = FakeTarget::caches((1..=4).map(|i| cache(i, 4096)).collect());
    t.guard = Box::new(move |c, n| {
        if n == 0 {
            return Guarded::Allow;
        }
        let ino = c.ident().ino as usize;
        Guarded::Skip(reasons[ino - 1])
    });
    let r = run(&rig, &[&t], &caches_selection(), true, 1, None).unwrap();
    assert_eq!(r.skipped_by_guard.len(), 4);
    for (o, want) in r.summary.outcomes.iter().zip(reasons) {
        assert_eq!(o.status, Status::Skipped(want));
        assert!(changed_since_scan_note(want).ends_with("(changed since scan)"));
    }
    assert!(!r
        .summary
        .outcomes
        .iter()
        .any(|o| matches!(o.status, Status::Error(_))));
    assert_eq!(r.exit, ExitClass::Ok);
    assert!(t.executed().is_empty());
}

#[test]
fn first_NoSpace_should_set_abort_skip_rest_Aborted_exit_2_and_not_retry_When_A_fails() {
    let rig = Rig::new();
    let mut t = FakeTarget::caches(three_caches());
    t.exec = Box::new(|c| {
        Err(DeleteFailure {
            error: DeleteError::NoSpace,
            freed: freed(c.est_bytes().0 / 2),
        })
    });
    let r = run(&rig, &[&t], &caches_selection(), true, 1, None).unwrap();
    assert_eq!(t.executed().len(), 1, "no retry, B and C never run");
    assert_eq!(
        r.summary.outcomes[0].status,
        Status::Error("no space left on device".into())
    );
    assert_eq!(
        r.summary.outcomes[1].status,
        Status::Skipped(SkipReason::Aborted)
    );
    assert_eq!(
        r.summary.outcomes[2].status,
        Status::Skipped(SkipReason::Aborted)
    );
    assert_eq!(r.abort, Some(AbortCause::NoSpace));
    assert_eq!(r.abort.unwrap().note(), "aborted: ENOSPC");
    assert_eq!(r.exit.code(), 2);
}

#[test]
fn executor_should_sum_partial_freed_of_failed_items_When_one_item_errors() {
    let rig = Rig::new();
    let t0 = cache(1, 8192);
    let t1 = cache(2, 4096);
    let (id1, id2) = (t0.id(), t1.id());
    let mut t = FakeTarget::caches(vec![t0, t1]);
    t.exec = Box::new(move |c| {
        if c.id() == id1 {
            Ok(freed(8192))
        } else {
            assert_eq!(c.id(), id2);
            Err(DeleteFailure {
                error: DeleteError::Io(13),
                freed: freed(4096),
            })
        }
    });
    let r = run(&rig, &[&t], &caches_selection(), true, 1, None).unwrap();
    let (a, b) = (&r.summary.outcomes[0], &r.summary.outcomes[1]);
    assert_eq!(a.status, Status::Removed);
    assert_eq!(a.freed.attributed_bytes, AllocatedBytes(8192));
    assert_eq!(b.status, Status::Error("permission denied (EACCES)".into()));
    assert_eq!(b.freed.attributed_bytes, AllocatedBytes(4096));
    assert_eq!(r.summary.attributed_bytes, 12288);
    let results: Vec<_> = rig
        .audit
        .records()
        .into_iter()
        .filter_map(|r| match r {
            AuditRecord::Result(x) => Some(x.0),
            _ => None,
        })
        .collect();
    assert_eq!(results[1].freed.attributed_bytes, AllocatedBytes(4096));
    assert_eq!(r.exit, ExitClass::PartialFailure);
}

#[test]
fn command_kind_failures_should_render_command_exited_128_with_message_timed_out_permission_denied_EACCES_exited_13_refused_and_not_abort_When_executor_runs(
) {
    let rig = Rig::new();
    let cands: Vec<_> = (1..=5).map(|i| cache(i, 4096)).collect();
    let mut t = FakeTarget::caches(cands);
    t.exec = Box::new(|c| {
        let error = match c.ident().ino {
            1 => DeleteError::CommandFailed {
                status: 128,
                message: "fatal: cannot remove a worktree with submodules".into(),
            },
            2 => DeleteError::Timeout,
            3 => DeleteError::Io(13),
            4 => DeleteError::CommandFailed {
                status: 13,
                message: String::new(),
            },
            _ => DeleteError::Refused,
        };
        Err(DeleteFailure {
            error,
            freed: Freed::default(),
        })
    });
    let r = run(&rig, &[&t], &caches_selection(), true, 1, None).unwrap();
    let texts: Vec<String> = r
        .summary
        .outcomes
        .iter()
        .map(|o| match &o.status {
            Status::Error(t) => t.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        texts,
        [
            "command exited 128: fatal: cannot remove a worktree with submodules",
            "command timed out",
            "permission denied (EACCES)",
            "command exited 13",
            "command refused"
        ]
    );
    assert_eq!(t.executed().len(), 5, "none of them aborts the run");
    assert!(r.abort.is_none());
    assert_eq!(r.exit, ExitClass::PartialFailure);
    let audited: Vec<String> = rig
        .audit
        .records()
        .into_iter()
        .filter_map(|r| match r {
            AuditRecord::Result(x) => match x.0.status {
                Status::Error(t) => Some(t),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(audited, texts);
}

#[test]
fn pool_should_run_4_items_at_once_with_jobs_4_never_more_and_stop_dispatching_after_NoSpace() {
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(60));
        eprintln!("pool test hung: the executor is not running items in parallel");
        std::process::abort();
    });
    // Four parties at a barrier: it releases only if four items are inside `execute` together.
    let rig = Rig::new();
    let barrier = Arc::new(Barrier::new(4));
    let (cur, high) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let mut t = FakeTarget::caches((1..=8).map(|i| cache(i, 4096)).collect());
    let (b, c2, h2) = (barrier.clone(), cur.clone(), high.clone());
    t.exec = Box::new(move |c| {
        let now = c2.fetch_add(1, Ordering::SeqCst) + 1;
        h2.fetch_max(now, Ordering::SeqCst);
        b.wait();
        c2.fetch_sub(1, Ordering::SeqCst);
        Ok(freed(c.est_bytes().0))
    });
    run(&rig, &[&t], &caches_selection(), true, 4, None).unwrap();
    assert_eq!(high.load(Ordering::SeqCst), 4);

    // --jobs 1: high-water 1, outcomes in plan order.
    let rig = Rig::new();
    let (cur, high) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let mut t = FakeTarget::caches((1..=8).map(|i| cache(i, 4096)).collect());
    let (c2, h2) = (cur.clone(), high.clone());
    t.exec = Box::new(move |c| {
        let now = c2.fetch_add(1, Ordering::SeqCst) + 1;
        h2.fetch_max(now, Ordering::SeqCst);
        c2.fetch_sub(1, Ordering::SeqCst);
        Ok(freed(c.est_bytes().0))
    });
    let r = run(&rig, &[&t], &caches_selection(), true, 1, None).unwrap();
    assert_eq!(high.load(Ordering::SeqCst), 1);
    let plan_ids: Vec<_> = r.plan.items.iter().map(|i| i.candidate.id()).collect();
    let out_ids: Vec<_> = r.summary.outcomes.iter().map(|o| o.id).collect();
    assert_eq!(plan_ids, out_ids);
    assert_eq!(t.executed(), plan_ids);

    // NoSpace on the third item with --jobs 4: the other three running items wait for the abort flag, so
    // nothing past the first four can have started before it is set.
    let rig = Rig::new();
    let mut t = FakeTarget::caches((1..=8).map(|i| cache(i, 4096)).collect());
    let guard = rig.guard.clone();
    t.exec = Box::new(move |c| {
        if c.ident().ino == 3 {
            return Err(DeleteFailure {
                error: DeleteError::NoSpace,
                freed: freed(1024),
            });
        }
        spin_until(|| guard.abort_flag().is_set());
        Ok(freed(c.est_bytes().0))
    });
    let r = run(&rig, &[&t], &caches_selection(), true, 4, None).unwrap();
    let ran = t.executed().len();
    assert!(
        (3..=4).contains(&ran),
        "at most the four first items start; ran {ran}"
    );
    let aborted = r
        .summary
        .outcomes
        .iter()
        .filter(|o| o.status == Status::Skipped(SkipReason::Aborted))
        .count();
    assert_eq!(
        aborted,
        8 - ran,
        "every item that did not start is Skipped(Aborted)"
    );
    let sum: u64 = r
        .summary
        .outcomes
        .iter()
        .map(|o| o.freed.attributed_bytes.0)
        .sum();
    assert_eq!(r.summary.attributed_bytes, sum, "the books balance");
    assert_eq!(r.exit.code(), 2);
}

fn old_build_dirs(n: u64) -> Vec<disk_cleanup_core::Candidate> {
    (1..=n).map(|i| build_dir(i, 1 << 20, 70)).collect()
}

#[test]
fn refused_gate_should_leave_lock_audit_and_filesystem_untouched_and_exit_64_When_apply_without_plan_id(
) {
    let rig = Rig::new();
    let t = FakeTarget::new(&[SelectorId::BuildDirs], old_build_dirs(2));
    let sel = selection(&[SelectorId::BuildDirs]);
    let e = run(&rig, &[&t], &sel, true, 2, None).unwrap_err();
    assert_eq!(e.exit, ExitClass::Usage);
    assert_eq!(e.exit.code(), 64);
    assert!(
        e.text
            .starts_with("error: destructive kinds (build-dirs) need --plan-id"),
        "{}",
        e.text
    );
    assert!(rig.audit.calls().is_empty(), "no run_start, no intent");
    assert_eq!(
        rig.guard.acquire_calls(),
        0,
        "the lock is taken after the gate"
    );
    assert!(t.executed().is_empty());
    assert_eq!(rig.runner.mutating_calls(), 0);
}

fn saved(id: &str, kinds: &str, items: &[&str]) -> SavedSelection {
    SavedSelection {
        plan_id: id.into(),
        saved_at: NOW - 3600,
        euid: 1000,
        kinds: kinds.into(),
        flags: String::new(),
        items: items.iter().map(|k| ItemKey::new(k)).collect(),
        items_truncated: false,
    }
}

#[test]
fn apply_should_print_the_docker_down_line_before_the_gate_and_exit_1_when_only_volumes_are_selected(
) {
    let mut rig = Rig::new();
    rig.plans = disk_cleanup_core::testing::FakePlanStore::with_existing(vec![saved(
        "91be04d7",
        "build-dirs,docker-volumes",
        &["node-modules:/home/u/code/p1/node_modules"],
    )]);
    let docker_down = |name: &'static str| {
        let mut d = FakeTarget::new(&[SelectorId::DockerVolumes], vec![]);
        d.name = name;
        d.gate = KindGate::Unavailable(SkipReason::DaemonUnavailable);
        d.detail = Some("connection refused".into());
        d
    };
    // build-dirs with docker-volumes while the daemon is down: availability line first, then the gate.
    let builds = FakeTarget::new(&[SelectorId::BuildDirs], old_build_dirs(1));
    let docker = docker_down("docker");
    let sel = selection(&[SelectorId::BuildDirs, SelectorId::DockerVolumes]);
    let supplied = "91be04d7"
        .parse::<disk_cleanup_core::plan::PlanIdArg>()
        .unwrap()
        .plan_id();
    let e = run(&rig, &[&builds, &docker], &sel, true, 1, Some(supplied)).unwrap_err();
    assert_eq!(e.exit, ExitClass::Usage);
    assert_eq!(
        e.notes,
        ["docker: could not run (daemon not reachable: connection refused)"]
    );
    assert!(
        e.text.contains("docker-volumes was not reachable now, which changes the id (it was in your dry run). Start Docker and re-run the same command, or dry run again without docker-volumes."),
        "{}",
        e.text
    );
    assert!(rig.audit.calls().is_empty());

    // --kinds docker-volumes alone: the run ends at availability, exit 1, the gate never speaks.
    let rig = Rig::new();
    let docker = docker_down("docker");
    let sel = selection(&[SelectorId::DockerVolumes]);
    let e = run(&rig, &[&docker], &sel, true, 1, None).unwrap_err();
    assert_eq!(e.exit, ExitClass::PartialFailure);
    assert!(e.refusal.is_none());
    assert!(!e.text.contains("plan-id"));
    assert!(rig.audit.calls().is_empty());
    assert_eq!(rig.guard.acquire_calls(), 0);
}

#[test]
fn executor_should_plan_100_000_fake_candidates_and_keep_report_output_bounded_to_top_N_When_fake_discovery(
) {
    let rig = Rig::new();
    let t = FakeTarget::caches((1..=100_000).map(|i| cache(i, i * 512)).collect());
    let r = run(&rig, &[&t], &caches_selection(), false, 4, None).unwrap();
    assert_eq!(r.plan.items.len(), 100_000);
    assert_eq!(r.plan.will_count(), 100_000);
    let top = r.plan.top(20);
    assert_eq!(top.len(), 20);
    assert!(top
        .windows(2)
        .all(|w| w[0].candidate.est_bytes() >= w[1].candidate.est_bytes()));
    assert_eq!(top[0].candidate.est_bytes().0, 100_000 * 512);
    assert!(
        r.summary.outcomes.is_empty(),
        "plan mode keeps one row per skipped item only"
    );
}

fn btrfs_snap(data_used: u64) -> disk_cleanup_core::FsSnapshot {
    let mut s = FakeSpaceMeter::plain_snapshot("/home/u", 500, 1000);
    s.fs_type = "btrfs".into();
    let space = SpaceUse {
        bytes_used: data_used,
        total_bytes: 2 * data_used,
    };
    s.btrfs = Some(AllocSnapshot {
        data: space,
        metadata: space,
        system: space,
        unallocated: 1 << 40,
    });
    s
}

#[test]
fn two_pre_delete_samples_should_be_taken_before_the_first_intent_and_flag_a_short_gap_unmeasurable(
) {
    const GIB: u64 = 1 << 30;
    let first = 1000 * GIB;
    let second = first + (0.6 * GIB as f64) as u64;
    let snaps = || {
        vec![
            btrfs_snap(first),
            btrfs_snap(second),
            btrfs_snap(second - 4096),
        ]
    };

    // Discovery takes 6 s of fake time: the pair is measurable.
    let log = EventLog::new();
    let mut rig = Rig::new();
    rig.space = FakeSpaceMeter::new(snaps(), FakeSpaceMeter::stable()).with_log(&log);
    rig.audit = FakeAuditSink::new().with_log(&log);
    let mut t = FakeTarget::caches(vec![cache(1, 4096), cache(2, 4096)]);
    let advance = rig_clock_advancer(&rig);
    t.on_discover = Box::new(move || advance(6));
    let r = run(&rig, &[&t], &caches_selection(), true, 1, None).unwrap();
    assert_eq!(r.summary.pre_delete_data_bytes_used, [first, second]);
    assert!(r.pre_delete.measurable);
    let ev = log.events();
    let snaps_before_intent = ev
        .iter()
        .take_while(|e| *e != "audit:Intent")
        .filter(|e| *e == "snapshot")
        .count();
    assert_eq!(snaps_before_intent, 2, "{ev:?}");
    let between = &ev[ev.iter().position(|e| e == "audit:Intent").unwrap()..];
    assert!(
        between.iter().filter(|e| *e == "snapshot").count() == 1,
        "only the after-snapshot"
    );

    // Discovery under 2 s: stored, but not measurable.
    let mut rig = Rig::new();
    rig.space = FakeSpaceMeter::new(snaps(), FakeSpaceMeter::stable());
    let mut t = FakeTarget::caches(vec![cache(1, 4096)]);
    let advance = rig_clock_advancer(&rig);
    t.on_discover = Box::new(move || advance(1));
    let r = run(&rig, &[&t], &caches_selection(), true, 1, None).unwrap();
    assert!(!r.pre_delete.measurable);
    assert_eq!(r.pre_delete.elapsed_secs, 1);
}

/// A closure that advances the rig's `FakeClock`; the clock lives in the rig, which outlives the run.
fn rig_clock_advancer(rig: &Rig) -> impl Fn(i64) + Send + Sync + 'static {
    let clock = rig.clock.clone();
    move |secs| clock.advance(secs)
}
