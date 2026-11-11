//! Task 6.6.3a: the typed count and the linear `ConfirmedPlan` (UX-T35; review C6, review 2 N5).
#![allow(clippy::unwrap_used, non_snake_case)]

use std::path::PathBuf;

use disk_cleanup_core::delete_types::FileKind;
use disk_cleanup_core::explore::confirm::{Confirm, ReviewedItem, ReviewedPlan};
use disk_cleanup_core::explore::msg::{DeleteMsg, DeleteSummary};
use disk_cleanup_core::explore::ports::{ExplorerActions, TypedCountSource};
use disk_cleanup_core::explore::testing::{FakeExplorerActions, FakeTypedCount};
use disk_cleanup_core::ids::Identity;
use disk_cleanup_core::plan::{ItemIdentity, PlanId};
use disk_cleanup_core::roots::RootIdentity;

fn item(path: &str, ino: u64) -> ReviewedItem {
    ReviewedItem {
        path: PathBuf::from(path),
        identity: ItemIdentity {
            dev: 1,
            ino,
            kind: FileKind::Dir,
        },
    }
}

fn roots() -> Vec<RootIdentity> {
    vec![RootIdentity {
        path: "/home/u/code".into(),
        ident: Identity { dev: 1, ino: 9 },
    }]
}

fn plan_of(n: usize, bytes: u64) -> ReviewedPlan {
    let items = (0..n)
        .map(|i| item(&format!("/home/u/code/app{i}/dist"), 100 + i as u64))
        .collect();
    ReviewedPlan::new(items, roots(), bytes)
}

#[test]
fn accepts_should_return_the_confirmed_plan_When_the_exact_digits_of_the_count_are_typed() {
    let reviewed = plan_of(3, 4_000_000_000);
    let id = reviewed.plan_id();
    let c = Confirm::new(reviewed).accepts("3").unwrap();
    assert_eq!(c.plan_id(), id);
    assert_eq!(c.count(), 3);
    assert_eq!(c.bytes(), 4_000_000_000);
    assert_eq!(c.typed(), "3");
}

#[test]
fn safety_confirm_should_refuse_every_near_miss_and_keep_everything_When_the_count_is_not_typed_exactly(
) {
    let bytes = 4_000_000_000_u64;
    let bytes_text = bytes.to_string();
    for bad in [
        "03",
        " 3",
        "3 ",
        "3\n",
        "\t3",
        "+3",
        "-3",
        "three",
        "y",
        "yes",
        "Y",
        "",
        "30",
        "33",
        "2",
        "4",
        "3.0",
        "0x3",
        "３",
        bytes_text.as_str(),
    ] {
        let r = Confirm::new(plan_of(3, bytes)).accepts(bad);
        assert!(r.is_err(), "{bad:?} must be refused");
        let refusal = r.unwrap_err();
        assert_eq!(
            refusal.text(),
            "That is not 3. Nothing was deleted.",
            "{bad:?}"
        );
        assert_eq!(refusal.to_string(), refusal.text());
    }
}

#[test]
fn safety_confirm_should_not_accept_a_larger_or_smaller_number_When_it_is_not_the_count() {
    // A mutant that compares with `>=` or `<=` accepts one of these.
    for typed in ["2", "4", "10", "300"] {
        assert!(
            Confirm::new(plan_of(3, 1)).accepts(typed).is_err(),
            "{typed}"
        );
    }
    for typed in ["1", "9", "11", "100"] {
        assert!(
            Confirm::new(plan_of(10, 1)).accepts(typed).is_err(),
            "{typed}"
        );
    }
    assert!(Confirm::new(plan_of(10, 1)).accepts("10").is_ok());
}

#[test]
fn digits_match_should_be_true_only_for_the_decimal_text_of_the_count() {
    assert!(Confirm::digits_match(3, "3"));
    assert!(Confirm::digits_match(20, "20"));
    assert!(Confirm::digits_match(0, "0"));
    for (count, typed) in [
        (3, "03"),
        (3, " 3"),
        (3, "3 "),
        (3, "+3"),
        (3, "30"),
        (20, "2"),
    ] {
        assert!(!Confirm::digits_match(count, typed), "{count} {typed:?}");
    }
}

#[test]
fn accepts_should_keep_the_prompt_When_the_entry_is_wrong() {
    let refusal = Confirm::new(plan_of(2, 5)).accepts("3").unwrap_err();
    let again = refusal.into_confirm();
    assert_eq!(again.count(), 2);
    assert_eq!(
        again.prompt_line(),
        "Type 2 and press Enter to delete, Esc to cancel:"
    );
    assert_eq!(again.accepts("2").unwrap().count(), 2);
}

#[test]
fn accepts_should_never_confirm_an_empty_plan() {
    let r = Confirm::new(plan_of(0, 0)).accepts("0").unwrap_err();
    assert_eq!(r.text(), "There is nothing to delete. Nothing was deleted.");
}

#[test]
fn safety_confirm_should_bind_the_typed_count_to_the_plan_it_consumed() {
    // Two three-item plans: the count is the same, the plan-id and the identities are not.
    let a = plan_of(3, 10);
    let b = ReviewedPlan::new(
        (0..3)
            .map(|i| item(&format!("/home/u/code/app{i}/dist"), 200 + i as u64))
            .collect(),
        roots(),
        10,
    );
    assert_eq!(a.count(), b.count());
    assert_ne!(a.plan_id(), b.plan_id());
    let (ida, idb) = (a.plan_id(), b.plan_id());
    let ca = Confirm::new(a).accepts("3").unwrap();
    let cb = Confirm::new(b).accepts("3").unwrap();
    assert_eq!(ca.plan_id(), ida);
    assert_eq!(cb.plan_id(), idb);
    assert_eq!(ca.reviewed().items()[0].identity.ino, 100);
    assert_eq!(cb.reviewed().items()[0].identity.ino, 200);
}

#[test]
fn the_record_should_hold_the_plan_id_count_bytes_and_typed_text_of_the_confirmation() {
    let reviewed = plan_of(2, 123);
    let id = reviewed.plan_id();
    let c = Confirm::new(reviewed).accepts("2").unwrap();
    let rec = c.record();
    assert_eq!((rec.plan_id, rec.count, rec.bytes), (id, 2, 123));
    assert_eq!(rec.typed, "2");
    let (reviewed, typed) = c.into_parts();
    assert_eq!(reviewed.plan_id(), id);
    assert_eq!(typed, "2");
}

#[test]
fn the_plan_id_should_come_from_the_items_and_roots_never_from_a_loose_field() {
    let reviewed = plan_of(2, 1);
    let expected = PlanId::for_explorer(
        reviewed
            .items()
            .iter()
            .map(|i| (i.path.as_path(), i.identity)),
        &roots(),
    );
    assert_eq!(reviewed.plan_id(), expected);
    assert_eq!(
        reviewed
            .item("/home/u/code/app1/dist".as_ref())
            .unwrap()
            .identity
            .ino,
        101
    );
    assert!(reviewed.item("/home/u/code/other".as_ref()).is_none());
}

#[test]
fn delete_should_take_only_a_confirmed_plan_and_the_fake_should_record_its_record() {
    let actions = FakeExplorerActions::new(disk_cleanup_core::explore::msg::PlanPreview {
        plan_id: String::new(),
        saved: true,
        rows: vec![],
        est_bytes: 0,
        scope_line: String::new(),
        guards_line: String::new(),
    })
    .with_delete_script(vec![
        DeleteMsg::Progress { index: 1, total: 2 },
        DeleteMsg::Done(DeleteSummary {
            removed: 2,
            bytes: 7,
            interrupted: false,
        }),
    ]);
    let confirmed = Confirm::new(plan_of(2, 7)).accepts("2").unwrap();
    let handle = actions.delete(confirmed);
    assert_eq!(
        handle.rx.recv().unwrap(),
        DeleteMsg::Progress { index: 1, total: 2 }
    );
    assert!(matches!(handle.rx.recv().unwrap(), DeleteMsg::Done(s) if s.removed == 2));
    assert!(!handle.is_aborted());
    handle.abort();
    assert!(handle.is_aborted());
    let calls = actions.deletes();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].typed, "2");
}

#[test]
fn the_typed_count_source_should_report_a_missing_terminal_and_cancel_when_it_runs_out() {
    let mut s = FakeTypedCount::new(true, &["3"]);
    assert!(s.is_terminal());
    assert_eq!(s.next_entry().as_deref(), Some("3"));
    assert_eq!(s.next_entry(), None);
    assert!(!FakeTypedCount::new(false, &[]).is_terminal());
}
