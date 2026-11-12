#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! The gate-stamp check (Story 2.2.1, Task 2.2.1d; validation T-442, T-444, T-516 text half).

use dunnage_core::{
    break_glass_reason, break_glass_warning, check_stamp, required_gate, BreakGlass, BuildInfo,
    GateStamp, Kind, StampRefusal, STAMP_SCHEMA,
};

const COMMIT: &str = "c0ffee1c0ffee1c0ffee1c0ffee1c0ffee1c0ffe";
const SHA: &str = "ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12";

fn build(dirty: bool) -> BuildInfo {
    BuildInfo {
        bin_version: "0.2.0".into(),
        git_commit: COMMIT.into(),
        dirty,
        metascope_rev: COMMIT.into(),
        patched: false,
        metascope_dirty: false,
    }
}

fn stamp(gate: &str) -> GateStamp {
    GateStamp {
        schema: STAMP_SCHEMA,
        gate: gate.into(),
        commit: COMMIT.into(),
        binary_sha256: SHA.into(),
    }
}

#[test]
fn check_stamp_should_refuse_absent_foreign_wrong_hash_low_gate_and_dirty_and_accept_a_match() {
    let b = build(false);
    assert_eq!(check_stamp(&b, None, SHA, 'B'), Err(StampRefusal::Absent));

    let mut s = stamp("B");
    s.schema = 2;
    assert_eq!(
        check_stamp(&b, Some(&s), SHA, 'B'),
        Err(StampRefusal::UnknownSchema { found: 2 })
    );

    let mut s = stamp("B");
    s.commit = "deadbeef".repeat(5);
    assert!(matches!(
        check_stamp(&b, Some(&s), SHA, 'B'),
        Err(StampRefusal::WrongCommit { .. })
    ));

    assert_eq!(
        check_stamp(&b, Some(&stamp("B")), &"00".repeat(32), 'B'),
        Err(StampRefusal::WrongBinary)
    );

    assert_eq!(
        check_stamp(&b, Some(&stamp("B")), SHA, 'C'),
        Err(StampRefusal::GateTooLow {
            have: "B".into(),
            need: 'C'
        })
    );
    assert!(check_stamp(&b, Some(&stamp("")), SHA, 'B').is_err());
    assert!(check_stamp(&b, Some(&stamp("BC")), SHA, 'B').is_err());

    assert_eq!(
        check_stamp(&build(true), Some(&stamp("D")), SHA, 'B'),
        Err(StampRefusal::DirtyBuild)
    );

    let ok = check_stamp(&b, Some(&stamp("D")), SHA, 'C').unwrap();
    assert!(ok.verified);
    assert_eq!((ok.gate.as_str(), ok.commit.as_str()), ("D", COMMIT));
    assert_eq!(ok.binary_sha256, SHA);
}

#[test]
fn required_gate_should_follow_the_kind_that_registered_it() {
    assert_eq!(required_gate(&[]), 'B');
    assert_eq!(required_gate(&[Kind::Cache]), 'B');
    assert_eq!(required_gate(&[Kind::Cache, Kind::NodeModules]), 'C');
    assert_eq!(required_gate(&[Kind::Worktree, Kind::CargoTarget]), 'C');
    assert_eq!(required_gate(&[Kind::Cache, Kind::DockerImage]), 'D');
    assert_eq!(required_gate(&[Kind::SteamPrefix]), 'D');
    assert_eq!(required_gate(&[Kind::BtrfsTrim, Kind::Worktree]), 'D');
}

#[test]
fn break_glass_should_proceed_with_the_header_warning_and_audit_field_and_refuse_an_empty_value() {
    assert_eq!(break_glass_reason(None), None);
    assert_eq!(break_glass_reason(Some("")), None);
    assert_eq!(break_glass_reason(Some("   ")), None);
    let reason = break_glass_reason(Some("rebuilt locally for Gate B")).unwrap();
    assert_eq!(reason, "rebuilt locally for Gate B");
    let bg = BreakGlass {
        reason,
        tty_confirmed: true,
    };
    assert_eq!(
        break_glass_warning(&bg),
        "WARNING: gate check bypassed (rebuilt locally for Gate B)"
    );
    let json = serde_json::to_string(&bg).unwrap();
    assert!(json.contains("\"tty_confirmed\":true"), "{json}");
}

#[test]
fn stamp_refusal_text_should_offer_both_the_rebuild_and_the_audited_break_glass_and_end_with_nothing_deleted(
) {
    for r in [
        StampRefusal::Absent,
        StampRefusal::UnknownSchema { found: 3 },
        StampRefusal::DirtyBuild,
        StampRefusal::WrongBinary,
        StampRefusal::WrongCommit {
            stamp: "a".into(),
            build: "b".into(),
        },
        StampRefusal::GateTooLow {
            have: "B".into(),
            need: 'C',
        },
    ] {
        let t = r.text();
        assert!(
            t.contains("If the disk has room for a build, run 'make gate"),
            "{t}"
        );
        assert!(
            t.contains("DUNNAGE_UNGATED=<reason>") && t.contains("type BYPASS"),
            "{t}"
        );
        assert!(
            t.contains("recorded in the audit log") && t.ends_with("Nothing was deleted."),
            "{t}"
        );
        assert_eq!(r.exit_code(), 64);
    }
}
