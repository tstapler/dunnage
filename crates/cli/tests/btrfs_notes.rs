#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 4.3.1: the snapshot note shows in a plan, an apply summary and `status`, and is absent without
//! snapshots. (Qgroup detection reads the real sysfs here, so these cases assert only the snapshot text.)

mod rig;
mod support;

use disk_cleanup_core::kind::SelectorId;
use rig::{confined, run, FakeTarget, Rig};
use support::cache;

const NOTE: &str = "snapshots present (snapper: #12 2026-09-01, #13 2026-10-01): freed-bytes estimates may overcount shared extents";

/// Notes wrap at 80 columns in the text reports.
fn flat(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn rig_with_snapper() -> Rig {
    let mut rig = Rig::new();
    rig.fs.add_dir("/").add_dir("/.snapshots");
    for (n, date) in [(12, "2026-09-01 09:30:00"), (13, "2026-10-01 10:00:00")] {
        rig.fs.add_dir(format!("/.snapshots/{n}"));
        rig.fs.add_file(
            format!("/.snapshots/{n}/info.xml"),
            &format!("<snapshot><date>{date}</date></snapshot>"),
        );
        rig.fs.add_dir(format!("/.snapshots/{n}/snapshot"));
    }
    rig
}

fn caches() -> FakeTarget {
    FakeTarget::new(
        &[SelectorId::Caches],
        vec![cache("/home/u/.npm/_cacache", 1.0)],
    )
}

#[test]
fn a_plan_should_carry_the_snapshot_note_only_when_snapshots_are_detected() {
    let c = caches();
    let with = run(&rig_with_snapper(), &[&c], &[], false, &confined);
    assert_eq!(with.code, 0, "{with:?}");
    assert!(flat(&with.stdout).contains(NOTE), "{}", with.stdout);
    let json = run(&rig_with_snapper(), &[&c], &["--json"], false, &confined);
    assert!(json.stdout.contains(NOTE), "{}", json.stdout);
    let without = run(&Rig::new(), &[&c], &[], false, &confined);
    assert!(
        !without.stdout.contains("snapshots present"),
        "{}",
        without.stdout
    );
}

#[test]
fn an_apply_summary_should_carry_the_snapshot_note() {
    let c = caches();
    let r = run(
        &rig_with_snapper(),
        &[&c],
        &["--apply", "--kinds", "caches"],
        true,
        &confined,
    );
    assert_eq!(r.code, 0, "{r:?}");
    assert!(flat(&r.stdout).contains(NOTE), "{}", r.stdout);
    let plain = run(
        &Rig::new(),
        &[&c],
        &["--apply", "--kinds", "caches"],
        true,
        &confined,
    );
    assert!(!plain.stdout.contains("snapshots present"));
}

#[test]
fn status_should_print_the_snapshot_note_in_text_and_leave_the_json_document_unchanged() {
    let text = run(&rig_with_snapper(), &[], &["status"], false, &confined);
    assert!(
        flat(&text.stdout).contains(&format!("Note: {NOTE}")),
        "{}",
        text.stdout
    );
    let json = run(
        &rig_with_snapper(),
        &[],
        &["status", "--json"],
        false,
        &confined,
    );
    assert!(!json.stdout.contains("snapshots present"));
    let none = run(&Rig::new(), &[], &["status"], false, &confined);
    assert!(!none.stdout.contains("snapshots present"));
}
