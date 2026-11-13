#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cloned_ref_to_slice_refs,
    clippy::redundant_closure_for_method_calls,
    non_snake_case
)]
//! Task 6.6.3b: the explorer's apply as a tested sequence over a real fixture tree (real `lstat`, mount table
//! and fd-relative directory operations; fake audit sink, lock, plan store and clock). Every case ends with a
//! comparison of the whole fixture with what it was before: nothing but the reviewed directories may go.

mod explore_rig;

use std::collections::HashMap;
use std::path::PathBuf;

use dunnage_adapters::testing::{Event, FaultDirOps};
use dunnage_adapters::RealDirOps;
use dunnage_cli::explore::apply::{changed_text, exit_for_signal, grew_too_much, skip_text};
use dunnage_core::testing::{AuditCall, FakeAuditSink, FakeRunGuard};
use dunnage_core::{AuditRecord, SkipReason, Status};
use explore_rig::{kinds_of, manifest, Ex, FixedSizer, Knobs, Ran, NOW};

const ORDER: [&str; 10] = [
    "check_plan_id",
    "run_lock",
    "audit_open",
    "explorer_confirm",
    "remeasure",
    "resolve_roots",
    "mint",
    "execute",
    "settle",
    "run_end",
];

fn two_marks(ex: &Ex) -> (PathBuf, PathBuf) {
    let a = ex.tree("app/dist", 3, 4000);
    let b = ex.tree("app/out", 2, 4000);
    ex.tree("app/keep", 1, 100);
    (a, b)
}

fn result_statuses(ex: &Ex) -> Vec<Status> {
    ex.audit
        .records()
        .into_iter()
        .filter_map(|r| match r {
            AuditRecord::Result(o) => Some(o.0.status),
            _ => None,
        })
        .collect()
}

/// Runs `confirmed` after `change` was made: before the apply (the plan-id gate recomputes from what is on disk
/// and refuses the whole set) or in the window after the gate (only the per-item identity compare can see it).
fn run_swapped(
    ex: &Ex,
    confirmed: dunnage_core::explore::confirm::ConfirmedPlan,
    change: impl FnOnce() + Send + 'static,
    after_gate: bool,
) -> Ran {
    if after_gate {
        ex.run_with(confirmed, ex.confined_ops(), &Knobs::after_gate(change))
    } else {
        change();
        ex.run(confirmed, ex.confined_ops())
    }
}

/// The whole set was refused at step 1: exit 64, the plan-id text, nothing audited, no capability.
fn assert_gate_refused(ex: &Ex, ran: &Ran) {
    assert_eq!(ran.report.exit, 64, "{:?}", ran.report);
    assert!(
        ran.report.refused.as_deref().is_some_and(|t| t.contains(
            "changed since the plan was made. Review the plan, then press d again. Nothing was deleted."
        )),
        "{:?}",
        ran.report.refused
    );
    assert_eq!(ran.steps, ["check_plan_id"]);
    assert_eq!(ran.minted, 0);
    assert!(kinds_of(&ex.audit).is_empty(), "{:?}", kinds_of(&ex.audit));
    assert_eq!(ran.report.summary.removed, 0);
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "explorer delete fails closed without the kernel mount id (statx STATX_MNT_ID): Linux only"
)]
fn safety_apply_should_delete_only_the_reviewed_directories_in_the_documented_order_When_nothing_changed(
) {
    let ex = Ex::new();
    let (a, b) = two_marks(&ex);
    let keep_before = manifest(&ex.code.join("app/keep"));
    let reviewed = ex.review(&[a.clone(), b.clone()]);
    let idents: Vec<_> = reviewed
        .items()
        .iter()
        .map(|i| (i.path.clone(), i.identity))
        .collect();
    let ran = ex.run(Ex::confirm(reviewed), ex.confined_ops());
    assert_eq!(ran.steps, ORDER);
    assert_eq!(ran.minted, 1);
    assert_eq!(ran.report.exit, 0, "{:?}", ran.report);
    assert!(!a.exists() && !b.exists());
    assert_eq!(manifest(&ex.code.join("app/keep")), keep_before);
    assert_eq!(
        kinds_of(&ex.audit),
        [
            "run_start",
            "explorer_confirm",
            "intent",
            "result",
            "intent",
            "result",
            "run_end"
        ]
    );
    let confirm = ex
        .audit
        .records()
        .into_iter()
        .find_map(|r| match r {
            AuditRecord::ExplorerConfirm(c) => Some(c),
            _ => None,
        })
        .unwrap();
    assert_eq!((confirm.count, confirm.typed.as_str()), (2, "2"));
    assert_eq!(confirm.items.len(), 2);
    for (path, id) in idents {
        let item = confirm.items.iter().find(|i| i.path == path).unwrap();
        assert_eq!((item.dev, item.ino), (id.dev, id.ino));
    }
    assert!(result_statuses(&ex).iter().all(|s| *s == Status::Removed));
}

#[test]
fn apply_should_refuse_before_any_state_or_capability_When_no_plan_was_saved() {
    let mut ex = Ex::new();
    let (a, _) = two_marks(&ex);
    let confirmed = ex.confirmed(&[a.clone()]);
    ex.plans = dunnage_core::testing::FakePlanStore::new();
    let before = manifest(&ex.code);
    let ran = ex.run(confirmed, ex.confined_ops());
    assert_eq!(ran.steps, ["check_plan_id"]);
    assert_eq!(ran.minted, 0);
    assert_eq!(ran.report.exit, 64);
    assert!(ran.report.refused.unwrap().contains("no saved plan"));
    assert_eq!(manifest(&ex.code), before);
    assert!(ex.audit.records().is_empty());
    assert_eq!(ex.guard.acquire_calls(), 0);
}

#[test]
fn apply_should_refuse_When_the_saved_plan_is_older_than_a_day_or_dated_in_the_future() {
    for shift in [86_400 + 1, -3600] {
        let ex = Ex::new();
        let (a, _) = two_marks(&ex);
        let confirmed = ex.confirmed(&[a.clone()]);
        ex.clock.set(NOW + shift);
        let ran = ex.run(confirmed, ex.confined_ops());
        assert_eq!(ran.minted, 0, "shift {shift}");
        assert_eq!(ran.steps, ["check_plan_id"], "shift {shift}");
        assert!(a.exists(), "shift {shift}");
        assert!(
            ran.report.refused.unwrap().contains("24 h"),
            "shift {shift}"
        );
    }
}

#[test]
fn apply_should_refuse_When_the_plan_in_the_store_is_for_other_marks() {
    let mut ex = Ex::new();
    let (a, _) = two_marks(&ex);
    let confirmed_for_a = ex.confirmed(&[a.clone()]);
    // The store now holds only the plan of `out`: a's confirmation finds none of its own.
    ex.plans = dunnage_core::testing::FakePlanStore::new();
    let _only_out = ex.review(&[ex.code.join("app/out")]);
    let ran = ex.run(confirmed_for_a, ex.confined_ops());
    assert_eq!(ran.minted, 0);
    assert!(a.exists());
    assert!(ran.report.refused.is_some());
}

#[test]
fn apply_should_name_the_holder_and_delete_nothing_When_the_run_lock_is_held() {
    let mut ex = Ex::new();
    let (a, _) = two_marks(&ex);
    let confirmed = ex.confirmed(&[a.clone()]);
    ex.guard = FakeRunGuard::lock_held_by(4242);
    let ran = ex.run(confirmed, ex.confined_ops());
    assert_eq!(ran.steps, ["check_plan_id", "run_lock"]);
    assert_eq!(ran.minted, 0);
    assert_eq!(
        ran.report.refused.as_deref(),
        Some("another dunnage apply is running (pid 4242). Nothing was deleted.")
    );
    assert_eq!(ran.report.exit, 2);
    assert!(a.exists());
    assert!(ex.audit.records().is_empty());
}

#[test]
fn safety_apply_should_leave_the_tree_unchanged_When_the_audit_log_cannot_be_written() {
    for fail in [AuditCall::RunStart, AuditCall::ExplorerConfirm] {
        let mut ex = Ex::new();
        let (a, b) = two_marks(&ex);
        let confirmed = ex.confirmed(&[a.clone(), b.clone()]);
        ex.audit = FakeAuditSink::new().fail_nth(fail, 1);
        let before = manifest(&ex.code);
        let ran = ex.run(confirmed, ex.confined_ops());
        assert_eq!(ran.report.exit, 2, "{fail:?}");
        assert_eq!(ran.minted, 0, "{fail:?}");
        assert!(
            ran.report.refused.unwrap().contains("audit log"),
            "{fail:?}"
        );
        assert_eq!(manifest(&ex.code), before, "{fail:?}");
        assert!(!ex.audit.calls().contains(&AuditCall::Intent), "{fail:?}");
    }
}

#[test]
fn safety_apply_should_stop_after_the_first_item_and_keep_the_second_When_an_intent_cannot_be_written(
) {
    let mut ex = Ex::new();
    let (a, b) = two_marks(&ex);
    let confirmed = ex.confirmed(&[a.clone(), b.clone()]);
    ex.audit = FakeAuditSink::new().fail_nth(AuditCall::Intent, 1);
    let ran = ex.run(confirmed, ex.confined_ops());
    assert_eq!(ran.report.exit, 2);
    assert!(
        a.exists() && b.exists(),
        "no delete without a written intent"
    );
}

#[test]
fn apply_should_refuse_with_the_changed_text_and_log_a_refused_run_When_the_tree_grew_by_two_gib() {
    let ex = Ex::new();
    let (a, _) = two_marks(&ex);
    let gib = 1u64 << 30;
    let then = FixedSizer(HashMap::from([(a.clone(), 10 * gib)]));
    let reviewed = ex.review_with(&[a.clone()], &ex.roots(), &then);
    let now = FixedSizer(HashMap::from([(a.clone(), 12 * gib)]));
    let before = manifest(&ex.code);
    let ran = ex.run_with(
        Ex::confirm(reviewed),
        ex.confined_ops(),
        &Knobs {
            sizer: Some(now),
            ..Knobs::default()
        },
    );
    let text = "Changed since you confirmed: 10.0 GiB then, 12.0 GiB now. Nothing was deleted. Press d to review again.";
    assert_eq!(ran.report.refused.as_deref(), Some(text));
    assert_eq!(changed_text(10 * gib, 12 * gib), text);
    assert_eq!(ran.minted, 0);
    assert_eq!(
        ran.steps,
        [
            "check_plan_id",
            "run_lock",
            "audit_open",
            "explorer_confirm",
            "remeasure"
        ]
    );
    assert_eq!(manifest(&ex.code), before);
    assert_eq!(
        kinds_of(&ex.audit),
        ["run_start", "explorer_confirm", "run_end"]
    );
    let end = ex.audit.records().into_iter().find_map(|r| match r {
        AuditRecord::RunEnd(e) => Some(e),
        _ => None,
    });
    assert_eq!(end.unwrap().refused.as_deref(), Some(text));
}

#[test]
fn grew_too_much_should_refuse_more_than_ten_percent_or_one_gib_and_nothing_less() {
    let gib = 1u64 << 30;
    // 10 %: exactly the limit passes, one byte more is refused.
    assert!(!grew_too_much(1000, 1100));
    assert!(grew_too_much(1000, 1101));
    // 1 GiB: with a base so large that 10 % is over a GiB, a GiB is the limit.
    let big = 100 * gib;
    assert!(!grew_too_much(big, big + gib));
    assert!(grew_too_much(big, big + gib + 1));
    // Shrinking and equal never refuse; a zero base refuses any growth.
    assert!(!grew_too_much(500, 100));
    assert!(!grew_too_much(500, 500));
    assert!(!grew_too_much(0, 0));
    assert!(grew_too_much(0, 1));
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "explorer delete fails closed without the kernel mount id (statx STATX_MNT_ID): Linux only"
)]
fn apply_should_delete_When_the_tree_grew_by_less_than_the_bound() {
    let ex = Ex::new();
    let (a, _) = two_marks(&ex);
    let then = FixedSizer(HashMap::from([(a.clone(), 100_000)]));
    let reviewed = ex.review_with(&[a.clone()], &ex.roots(), &then);
    let now = FixedSizer(HashMap::from([(a.clone(), 105_000)]));
    let ran = ex.run_with(
        Ex::confirm(reviewed),
        ex.confined_ops(),
        &Knobs {
            sizer: Some(now),
            ..Knobs::default()
        },
    );
    assert_eq!(ran.report.exit, 0, "{:?}", ran.report);
    assert!(!a.exists());
}

#[test]
fn skip_text_should_call_a_changed_identity_changed_since_the_plan() {
    assert_eq!(
        skip_text(SkipReason::IdentityChanged),
        "changed since the plan"
    );
    assert_eq!(
        skip_text(SkipReason::Symlink),
        SkipReason::Symlink.reason_text()
    );
}

#[test]
fn exit_for_signal_should_fold_hup_and_quit_into_143() {
    assert_eq!(exit_for_signal(2), 130);
    assert_eq!(exit_for_signal(15), 143);
    assert_eq!(exit_for_signal(1), 143);
    assert_eq!(exit_for_signal(3), 143);
    assert_eq!(exit_for_signal(10), 138);
}

fn symlink_swap_case(after_gate: bool) {
    let ex = Ex::new();
    let (a, b) = two_marks(&ex);
    let keep = ex.tree("keepdir", 2, 500);
    let keep_before = manifest(&keep);
    let confirmed = ex.confirmed(&[a.clone(), b.clone()]);
    let (victim, target) = (a.clone(), keep.clone());
    let swap = move || {
        std::fs::remove_dir_all(&victim).unwrap();
        std::os::unix::fs::symlink(&target, &victim).unwrap();
    };
    let ran = run_swapped(&ex, confirmed, swap, after_gate);
    assert_eq!(
        manifest(&keep),
        keep_before,
        "the link's target is untouched"
    );
    assert!(std::fs::symlink_metadata(&a)
        .unwrap()
        .file_type()
        .is_symlink());
    if after_gate {
        assert!(!b.exists(), "the other item is still removed");
        assert_eq!(ran.report.exit, 0);
        let statuses = result_statuses(&ex);
        assert!(
            statuses.contains(&Status::Skipped(SkipReason::IdentityChanged)),
            "{statuses:?}"
        );
    } else {
        assert!(
            b.exists(),
            "the whole set is refused, the other item stays too"
        );
        assert_gate_refused(&ex, &ran);
    }
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "explorer delete fails closed without the kernel mount id (statx STATX_MNT_ID): Linux only"
)]
fn safety_apply_should_skip_an_item_that_became_a_symlink_to_a_directory_that_must_stay() {
    symlink_swap_case(true);
}

#[test]
fn safety_apply_should_refuse_the_whole_set_When_an_item_became_a_symlink_before_the_apply() {
    symlink_swap_case(false);
}

fn recreated_case(after_gate: bool) {
    let ex = Ex::new();
    let (a, _) = two_marks(&ex);
    let confirmed = ex.confirmed(&[a.clone()]);
    let (victim, decoy) = (a.clone(), ex.p("decoy"));
    let swap = move || {
        use std::os::unix::fs::MetadataExt;
        let old_ino = std::fs::metadata(&victim).unwrap().ino();
        std::fs::remove_dir_all(&victim).unwrap();
        std::fs::create_dir_all(&decoy).unwrap();
        std::fs::write(decoy.join("f0"), [0u8; 10]).unwrap();
        // ext4 reuses a freed inode number at once: park directories that got the old number until the
        // new victim gets a different one (the engine's identity is (dev, ino)).
        for n in 0..10_000 {
            std::fs::create_dir(&victim).unwrap();
            if std::fs::metadata(&victim).unwrap().ino() != old_ino {
                break;
            }
            std::fs::rename(&victim, decoy.join(format!("held{n}"))).unwrap();
        }
        std::fs::create_dir_all(victim.join("precious")).unwrap();
        std::fs::write(victim.join("precious/data"), b"new life").unwrap();
    };
    let ran = run_swapped(&ex, confirmed, swap, after_gate);
    assert!(
        a.join("precious/data").exists(),
        "the new directory is intact"
    );
    assert_eq!(ran.report.summary.removed, 0);
    if after_gate {
        assert_eq!(
            result_statuses(&ex),
            [Status::Skipped(SkipReason::IdentityChanged)]
        );
    } else {
        assert_gate_refused(&ex, &ran);
    }
}

#[test]
fn safety_apply_should_skip_an_item_removed_and_recreated_with_a_new_inode() {
    recreated_case(true);
}

#[test]
fn safety_apply_should_refuse_the_whole_set_When_an_item_was_recreated_before_the_apply() {
    recreated_case(false);
}

fn parent_swap_case(after_gate: bool) {
    let ex = Ex::new();
    let sub = ex.tree("proj/dist", 2, 300);
    let confirmed = ex.confirmed(&[sub.clone()]);
    let elsewhere = ex.p("elsewhere");
    std::fs::create_dir_all(elsewhere.join("dist")).unwrap();
    std::fs::write(elsewhere.join("dist/precious"), b"keep me").unwrap();
    let before = manifest(&elsewhere);
    let proj = ex.code.join("proj");
    let (old, target) = (ex.code.join("proj.old"), elsewhere.clone());
    let swap = move || {
        std::fs::rename(&proj, &old).unwrap();
        std::os::unix::fs::symlink(&target, &proj).unwrap();
    };
    let ran = run_swapped(&ex, confirmed, swap, after_gate);
    assert_eq!(manifest(&elsewhere), before);
    assert!(ex.code.join("proj.old/dist").exists());
    assert_eq!(ran.report.summary.removed, 0);
    if after_gate {
        assert!(matches!(
            result_statuses(&ex)[..],
            [Status::Skipped(
                SkipReason::IdentityChanged | SkipReason::Symlink
            )]
        ));
    } else {
        assert_gate_refused(&ex, &ran);
    }
}

#[test]
fn safety_apply_should_skip_every_item_below_a_parent_that_was_swapped_for_a_symlink() {
    parent_swap_case(true);
}

#[test]
fn safety_apply_should_refuse_the_whole_set_When_a_parent_was_swapped_for_a_symlink_before_the_apply(
) {
    parent_swap_case(false);
}

fn root_replaced_case(after_gate: bool) {
    let ex = Ex::new();
    let a = ex.tree("app/dist", 2, 300);
    let confirmed = ex.confirmed(&[a.clone()]);
    // The root is renamed away and a new directory, holding a same-named tree, takes its name.
    let (code, old) = (ex.code.clone(), ex.home.join("code.old"));
    let swap = move || {
        std::fs::rename(&code, &old).unwrap();
        std::fs::create_dir_all(code.join("app/dist")).unwrap();
        std::fs::write(code.join("app/dist/new"), b"x").unwrap();
    };
    let ran = run_swapped(&ex, confirmed, swap, after_gate);
    assert!(ex.code.join("app/dist/new").exists());
    assert!(ex.home.join("code.old/app/dist").exists());
    assert_eq!(ran.report.summary.removed, 0);
    if after_gate {
        assert_eq!(
            result_statuses(&ex),
            [Status::Skipped(SkipReason::IdentityChanged)]
        );
    } else {
        assert_gate_refused(&ex, &ran);
    }
}

#[test]
fn safety_apply_should_skip_every_item_When_the_scan_root_was_replaced_by_a_new_directory() {
    root_replaced_case(true);
}

#[test]
fn safety_apply_should_refuse_the_whole_set_When_the_scan_root_was_replaced_before_the_apply() {
    root_replaced_case(false);
}

#[test]
fn safety_apply_should_skip_every_item_When_the_scan_root_became_a_symlink() {
    let ex = Ex::new();
    let a = ex.tree("app/dist", 2, 300);
    let confirmed = ex.confirmed(&[a.clone()]);
    let moved = ex.home.join("code.old");
    std::fs::rename(&ex.code, &moved).unwrap();
    std::os::unix::fs::symlink(&moved, &ex.code).unwrap();
    let before = manifest(&moved);
    // Resolving the roots again follows the link and finds the old directory under another spelling.
    let roots = ex.roots_of(&[ex.code.clone()]);
    let ran = ex.run_with(
        confirmed,
        ex.confined_ops(),
        &Knobs {
            roots: Some(Ok(roots)),
            ..Knobs::default()
        },
    );
    assert_eq!(
        manifest(&moved),
        before,
        "nothing is deleted through the swapped root"
    );
    assert_eq!(ran.report.summary.removed, 0);
}

fn roots_no_longer_hold_case(after_gate: bool) {
    let ex = Ex::new();
    let a = ex.tree("app/dist", 2, 300);
    let confirmed = ex.confirmed(&[a.clone()]);
    let other = ex.home.join("other");
    std::fs::create_dir_all(&other).unwrap();
    let roots = ex.roots_of(&[other]);
    let knobs = if after_gate {
        Knobs {
            roots_after_gate: Some(Ok(roots)),
            ..Knobs::default()
        }
    } else {
        Knobs {
            roots: Some(Ok(roots)),
            ..Knobs::default()
        }
    };
    let ran = ex.run_with(confirmed, ex.confined_ops(), &knobs);
    assert!(a.exists());
    assert_eq!(ran.report.summary.removed, 0);
    if after_gate {
        // The planned root no longer resolves: a root change since the plan, not a path outside every root.
        assert_eq!(
            result_statuses(&ex),
            [Status::Skipped(SkipReason::IdentityChanged)]
        );
    } else {
        assert_gate_refused(&ex, &ran);
    }
}

#[test]
fn safety_apply_should_skip_an_item_When_the_roots_resolved_now_no_longer_hold_it() {
    roots_no_longer_hold_case(true);
}

#[test]
fn safety_apply_should_refuse_the_whole_set_When_the_roots_resolved_at_the_gate_differ_from_the_reviewed_ones(
) {
    roots_no_longer_hold_case(false);
}

#[test]
fn safety_apply_should_end_refused_and_delete_nothing_When_the_roots_cannot_be_resolved_now() {
    let ex = Ex::new();
    let a = ex.tree("app/dist", 2, 300);
    let confirmed = ex.confirmed(&[a.clone()]);
    let ran = ex.run_with(
        confirmed,
        ex.confined_ops(),
        &Knobs {
            roots: Some(Err("error: config.toml is unreadable.".into())),
            ..Knobs::default()
        },
    );
    assert!(a.exists());
    assert_eq!(ran.minted, 0);
    assert!(ran
        .report
        .refused
        .unwrap()
        .ends_with("Nothing was deleted."));
    assert_eq!(
        kinds_of(&ex.audit),
        ["run_start", "explorer_confirm", "run_end"]
    );
}

#[test]
fn safety_apply_should_skip_an_item_that_is_now_inside_the_tools_own_state_M5_against_fresh_data() {
    let ex = Ex::new();
    let a = ex.tree("app/dist", 2, 300);
    let confirmed = ex.confirmed(&[a.clone()]);
    // The state directory is relocated onto the item between the plan and the apply.
    let moved = dunnage_core::Dirs {
        state: a.clone(),
        ..ex.base_dirs.clone()
    };
    let ran = ex.run_with(
        confirmed,
        ex.confined_ops(),
        &Knobs {
            own_state: Some(dunnage_core::explore::mark_check::OwnState::new(
                &moved,
                ex.p("bin/x"),
            )),
            ..Knobs::default()
        },
    );
    assert!(a.exists());
    assert_eq!(
        result_statuses(&ex),
        [Status::Skipped(SkipReason::ProtectedPath)]
    );
    assert_eq!(ran.report.summary.removed, 0);
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "explorer delete fails closed without the kernel mount id (statx STATX_MNT_ID): Linux only"
)]
fn safety_apply_should_leave_an_intent_with_a_result_and_a_run_end_When_the_abort_is_raised_inside_the_second_item(
) {
    let ex = Ex::new();
    let a = ex.tree("app/a1", 3, 300);
    let b = ex.tree("app/b2", 4, 300);
    let c = ex.tree("app/c3", 2, 300);
    let confirmed = ex.confirmed(&[a.clone(), b.clone(), c.clone()]);
    let abort = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = std::sync::Arc::clone(&abort);
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let ops = |_: &dunnage_core::ApplyToken| {
        let flag = std::sync::Arc::clone(&flag);
        let hits = std::sync::Arc::clone(&hits);
        FaultDirOps::wrap(RealDirOps::confined_to(&ex.base))
            .hook(Event::BeforeUnlinkat, move |ctx| {
                let in_b2 = ctx.base_abs.to_string_lossy().contains("b2")
                    || ctx.rel_path.to_string_lossy().contains("b2");
                if in_b2 && hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(300));
                }
            })
            .build()
    };
    let ran = ex.run_with(
        confirmed,
        ops,
        &Knobs {
            abort: Some(abort),
            ..Knobs::default()
        },
    );
    let kinds = kinds_of(&ex.audit);
    assert_eq!(kinds.last(), Some(&"run_end"));
    let intents = kinds.iter().filter(|k| **k == "intent").count();
    let results = kinds.iter().filter(|k| **k == "result").count();
    assert_eq!(
        intents.min(results),
        intents,
        "every intent has its result: {kinds:?}"
    );
    assert!(ran.report.summary.interrupted);
    assert_eq!(ran.report.exit, 130);
    let rows = result_statuses(&ex);
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(rows[0], Status::Removed);
    assert!(
        matches!(&rows[1], Status::Error(m) if m == "stopped by a signal; the directory is partly removed"),
        "{rows:?}"
    );
    assert_eq!(rows[2], Status::Skipped(SkipReason::Aborted));
    // The first tree was removed whole, the last one was never started.
    assert!(!a.exists());
    assert!(c.exists());
    let end = ex.audit.records().into_iter().find_map(|r| match r {
        AuditRecord::RunEnd(e) => Some(e),
        _ => None,
    });
    assert!(end.unwrap().interrupted);
}

/// The step names of the cleaner's apply as `wiring/compose.rs` documents them in its first lines.
fn cleaner_steps() -> Vec<String> {
    let text = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/wiring/compose.rs"),
    )
    .unwrap();
    let doc: Vec<&str> = text
        .lines()
        .skip_while(|l| !l.starts_with("//! `parse_args,"))
        .take_while(|l| l.starts_with("//!"))
        .collect();
    assert!(!doc.is_empty(), "the step list in the module doc");
    let joined = doc
        .iter()
        .map(|l| l.trim_start_matches("//!").trim())
        .collect::<Vec<_>>()
        .join(" ");
    let inner = joined.trim_start_matches('`');
    inner[..inner.find('`').unwrap()]
        .split(", ")
        .map(str::to_string)
        .collect()
}

#[test]
fn the_explorer_apply_should_share_named_steps_with_the_cleaner_and_keep_their_relative_order() {
    // Step 1 of the explorer is the cleaner's `plan_id_gate` (the same `destructive_gate`). The rest are the
    // cleaner's own.
    let shared = ["run_lock", "audit_open", "execute", "run_end"];
    let cleaner = cleaner_steps();
    assert_eq!(cleaner.len(), 14, "{cleaner:?}");
    let pos = |list: &[String], s: &str| list.iter().position(|x| x == s).unwrap();
    let explorer: Vec<String> = ORDER.iter().map(ToString::to_string).collect();
    let mut last = (0, 0);
    for s in shared {
        let (c, e) = (pos(&cleaner, s), pos(&explorer, s));
        assert!(
            c >= last.0 && e >= last.1,
            "{s} moved relative to the other shared steps"
        );
        last = (c, e);
    }
    // The steps only the explorer has are not in the cleaner's list, and the cleaner's `plan_id_gate` is
    // answered by `check_plan_id`.
    for own in [
        "check_plan_id",
        "explorer_confirm",
        "remeasure",
        "resolve_roots",
        "mint",
        "settle",
    ] {
        assert!(!cleaner.iter().any(|c| c == own), "{own}");
        assert!(ORDER.contains(&own));
    }
    assert!(cleaner.iter().any(|c| c == "plan_id_gate"));
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "explorer delete fails closed without the kernel mount id (statx STATX_MNT_ID): Linux only"
)]
fn apply_should_send_progress_item_and_done_messages_in_order_to_the_loop() {
    use dunnage_core::explore::msg::{DeleteItemOutcome, DeleteMsg, DeleteSummary};
    let ex = Ex::new();
    let (a, b) = two_marks(&ex);
    let (tx, rx) = std::sync::mpsc::sync_channel(64);
    let ran = ex.run_with(
        ex.confirmed(&[a.clone(), b.clone()]),
        ex.confined_ops(),
        &Knobs {
            events: Some(tx),
            ..Knobs::default()
        },
    );
    let msgs: Vec<DeleteMsg> = rx.try_iter().collect();
    assert_eq!(msgs.len(), 5, "{msgs:?}");
    assert_eq!(msgs[0], DeleteMsg::Progress { index: 1, total: 2 });
    assert!(
        matches!(&msgs[1], DeleteMsg::Item(r) if r.path == a && matches!(r.outcome, DeleteItemOutcome::Removed { bytes } if bytes > 0))
    );
    assert_eq!(msgs[2], DeleteMsg::Progress { index: 2, total: 2 });
    assert!(matches!(&msgs[3], DeleteMsg::Item(r) if r.path == b));
    let DeleteMsg::Done(summary) = msgs[4] else {
        panic!("{msgs:?}");
    };
    assert_eq!(summary.removed, 2);
    assert!(summary.bytes > 0);
    assert!(!summary.interrupted);
    assert_eq!(summary, ran.report.summary);
    let _ = DeleteSummary::default();
}

#[test]
fn apply_should_send_exactly_one_refused_message_and_nothing_else_When_a_gate_stops_it() {
    use dunnage_core::explore::msg::DeleteMsg;
    let mut ex = Ex::new();
    ex.guard = FakeRunGuard::lock_held_by(4242);
    let (a, _) = two_marks(&ex);
    let (tx, rx) = std::sync::mpsc::sync_channel(64);
    let _ = ex.run_with(
        ex.confirmed(&[a.clone()]),
        ex.confined_ops(),
        &Knobs {
            events: Some(tx),
            ..Knobs::default()
        },
    );
    let msgs: Vec<DeleteMsg> = rx.try_iter().collect();
    assert_eq!(
        msgs,
        [DeleteMsg::Refused(
            "another dunnage apply is running (pid 4242). Nothing was deleted.".to_string()
        )]
    );
}

#[test]
fn explorer_run_refuse_should_send_the_text_to_the_loop() {
    use dunnage_cli::explore::apply::ExplorerRun;
    use dunnage_core::explore::msg::DeleteMsg;
    let (tx, rx) = std::sync::mpsc::sync_channel(4);
    let run = ExplorerRun {
        abort: std::sync::Arc::default(),
        events: Some(tx),
    };
    run.refuse("not in this build");
    assert_eq!(
        rx.try_recv().unwrap(),
        DeleteMsg::Refused("not in this build".into())
    );
    // A headless run has nowhere to send it and must not panic.
    ExplorerRun::headless().refuse("x");
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "explorer delete fails closed without the kernel mount id (statx STATX_MNT_ID): Linux only"
)]
fn apply_should_leave_no_measured_bytes_on_a_skipped_item_and_some_on_a_removed_one() {
    let ex = Ex::new();
    let (a, b) = two_marks(&ex);
    let confirmed = ex.confirmed(&[a.clone(), b.clone()]);
    let victim = b.clone();
    let swap = move || {
        std::fs::remove_dir_all(&victim).unwrap();
        std::fs::write(&victim, b"a file now").unwrap();
    };
    let _ = run_swapped(&ex, confirmed, swap, true);
    let rows: Vec<_> = ex
        .audit
        .records()
        .into_iter()
        .filter_map(|r| match r {
            AuditRecord::Result(o) => Some(o.0),
            _ => None,
        })
        .collect();
    assert_eq!(rows.len(), 2);
    let removed = rows.iter().find(|o| o.status == Status::Removed).unwrap();
    assert!(removed.measured_bytes.is_some_and(|b| b > 0));
    let skipped = rows.iter().find(|o| o.status != Status::Removed).unwrap();
    assert_eq!(skipped.measured_bytes, None);
}

#[test]
fn apply_should_record_the_reviewed_scan_roots_in_run_start() {
    let ex = Ex::new();
    let (a, _) = two_marks(&ex);
    let _ = ex.run(ex.confirmed(&[a]), ex.confined_ops());
    let start = ex
        .audit
        .records()
        .into_iter()
        .find_map(|r| match r {
            AuditRecord::RunStart(s) => Some(s),
            _ => None,
        })
        .unwrap();
    assert_eq!(start.kinds, "explorer-selection");
    assert_eq!(start.roots.scan_roots, [ex.code.clone()]);
    assert!(start.plan_id.is_some());
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "explorer delete fails closed without the kernel mount id (statx STATX_MNT_ID): Linux only"
)]
fn apply_should_report_interrupted_When_the_abort_stops_the_last_item_part_way() {
    let ex = Ex::new();
    let only = ex.tree("app/only", 6, 300);
    let confirmed = ex.confirmed(&[only.clone()]);
    let abort = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = std::sync::Arc::clone(&abort);
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let ops = |_: &dunnage_core::ApplyToken| {
        let flag = std::sync::Arc::clone(&flag);
        let hits = std::sync::Arc::clone(&hits);
        FaultDirOps::wrap(RealDirOps::confined_to(&ex.base))
            .hook(Event::BeforeUnlinkat, move |_| {
                if hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 2 {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(300));
                }
            })
            .build()
    };
    let ran = ex.run_with(
        confirmed,
        ops,
        &Knobs {
            abort: Some(abort),
            ..Knobs::default()
        },
    );
    assert!(only.exists(), "the tree stopped part way");
    assert!(ran.report.summary.interrupted);
    assert_eq!(ran.report.exit, 130);
    let end = ex.audit.records().into_iter().find_map(|r| match r {
        AuditRecord::RunEnd(e) => Some(e),
        _ => None,
    });
    assert!(end.unwrap().interrupted);
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "explorer delete fails closed without the kernel mount id (statx STATX_MNT_ID): Linux only"
)]
fn apply_should_not_report_interrupted_When_the_abort_arrives_after_the_last_item() {
    let ex = Ex::new();
    let only = ex.tree("app/only", 2, 300);
    let abort = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let confirmed = ex.confirmed(&[only.clone()]);
    let flag = std::sync::Arc::clone(&abort);
    let ran = ex.run_with(
        confirmed,
        ex.confined_ops(),
        &Knobs {
            abort: Some(abort),
            ..Knobs::default()
        },
    );
    flag.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(!only.exists());
    assert!(!ran.report.summary.interrupted);
    assert_eq!(ran.report.exit, 0);
}

fn fake_id() -> dunnage_core::plan::ItemIdentity {
    dunnage_core::plan::ItemIdentity {
        dev: 1,
        ino: 1,
        kind: dunnage_core::FileKind::Dir,
    }
}

#[test]
fn safety_apply_should_refuse_a_forged_set_over_the_mark_limits_or_with_one_mark_inside_another() {
    let ex = Ex::new();
    let real = ex.tree("app/dist", 1, 100);
    let before = manifest(&ex.code);
    let long = ex.code.join("x".repeat(600));
    let sets: Vec<(Vec<PathBuf>, &str)> = vec![
        (
            (0..21).map(|i| ex.code.join(format!("m{i}"))).collect(),
            "21 marks",
        ),
        (vec![real.clone(), real.join("sub")], "a mark inside a mark"),
        (vec![long], "a path over 512 bytes"),
    ];
    for (paths, what) in sets {
        let items: Vec<_> = paths.into_iter().map(|p| (p, Some(fake_id()))).collect();
        let ran = ex.run(Ex::confirm(ex.forge(&items)), ex.confined_ops());
        assert_eq!(ran.steps, ["check_plan_id"], "{what}");
        assert_eq!(ran.minted, 0, "{what}");
        assert_eq!(ran.report.exit, 64, "{what}");
        assert!(
            ran.report
                .refused
                .unwrap()
                .ends_with("Nothing was deleted."),
            "{what}"
        );
        assert_eq!(manifest(&ex.code), before, "{what}");
    }
    assert!(ex.audit.records().is_empty());
}
