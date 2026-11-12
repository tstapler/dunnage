#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 1.2.3, Task 1.2.3b: the JSON documents (UX-26, UX-27; T-250 to T-253, T-263).

mod support;

use dunnage_cli::render_json::{
    apply_doc, fatal_doc, fatal_entry, gate_refusal_doc, kind_rows, kinds_doc, plan_doc, reason_id,
    status_doc, to_text,
};
use dunnage_cli::view::PlanView;
use dunnage_core::outcome::SkipReason;
use dunnage_core::plan::{destructive_gate, GateRefusal, GateRenderCtx, PlanId};
use dunnage_core::reasons::FatalError;
use dunnage_core::SelectorId;
use serde_json::Value;
use support::*;

fn parse(text: &str) -> Value {
    assert!(text.ends_with('\n'));
    serde_json::from_str(text).expect("exactly one JSON document")
}

fn plan_json() -> Value {
    let fx = Fixture::s1();
    let input = fx.input();
    parse(&to_text(&plan_doc(&input, &PlanView::build(&input), 0)))
}

#[test]
fn json_should_be_exactly_one_document_with_schema_version_1_mode_plan_id_items_totals_fs_When_plan(
) {
    let d = plan_json();
    assert_eq!(d["schema_version"], 1);
    assert_eq!(d["mode"], "plan");
    assert_eq!(d["plan_id"].as_str().unwrap().len(), 8);
    assert!(d["items"].is_array() && d["totals"].is_object() && d["fs"].is_object());
    for k in [
        "estimated_bytes",
        "attributed_bytes",
        "measured_drop_bytes",
        "unattributed_bytes",
        "attribution_pct",
    ] {
        assert!(d["totals"].get(k).is_some(), "{k}");
    }
    assert!(d["fs"].get("before").is_some() && d["fs"]["after"].is_null());
    assert_eq!(d["partial_scan"], false);
    assert_eq!(d["plan_saved"], true);
    assert!(d["next"].is_array());
}

#[test]
fn json_bytes_should_be_exact_integers_without_separators_and_paths_absolute_When_rendered() {
    let text = to_text(&plan_json());
    let d = parse(&text);
    let first = &d["items"][0];
    assert!(first["est_bytes"].is_u64());
    assert_eq!(first["est_bytes"].as_u64().unwrap(), gib(38.4));
    assert!(!text.contains("GiB") && !text.contains("\"~"));
    assert!(first["path"].as_str().unwrap().starts_with('/'));
    assert!(!text.contains(",000") || !text.contains("\"1,"));
    assert!(d["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|i| i["est_bytes"].is_u64()));
}

#[test]
fn json_item_should_carry_tier_family_reason_reason_text_and_top_level_exit_code_When_skipped_item()
{
    let d = plan_json();
    assert_eq!(d["exit_code"], 0);
    assert!(d["errors"].as_array().unwrap().is_empty());
    let skipped = d["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["status"] == "skipped")
        .unwrap();
    assert_eq!(skipped["tier"], "guarded");
    assert_eq!(skipped["family"], "worktrees");
    assert_eq!(skipped["kind"], "worktree");
    assert_eq!(skipped["selector"], "worktrees");
    assert_eq!(skipped["reason"], "dirty");
    assert_eq!(skipped["reason_text"], "dirty working tree");
    let free = d["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["status"] == "would_free")
        .unwrap();
    assert!(free["reason"].is_null() && free["reason_text"].is_null());
    let risk = d["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["status"] == "risk_not_selected")
        .unwrap();
    assert_eq!(risk["reason"], "kind-not-selected");
    assert_eq!(reason_id(SkipReason::KindNotSelected), "kind-not-selected");
}

#[test]
fn json_should_still_be_one_document_with_errors_and_exit_code_When_fatal_runtime_error() {
    let starved = FatalError::Starved {
        unallocated_mib: 600,
        metadata_headroom_mib: 32,
    };
    let d = parse(&to_text(&fatal_doc("apply", Some("91be04d7"), &starved)));
    assert_eq!(d["exit_code"], 3);
    assert_eq!(d["errors"][0]["code"], "starved");
    assert!(d["errors"][0]["message"]
        .as_str()
        .unwrap()
        .contains("unallocated 600 MiB and metadata headroom 32 MiB"));
    let lock = fatal_doc("apply", None, &FatalError::LockHeld { pid: 42 });
    assert_eq!(lock["exit_code"], 2);
    assert_eq!(lock["errors"][0]["code"], "lock_held");
    let state = fatal_entry(&FatalError::StateUnreadable {
        path: "/mnt/x y".into(),
        reason: "permission denied".into(),
    });
    assert_eq!(state.code, "state_unreadable");
    assert!(
        state.message.contains("df -- '/mnt/x y'")
            && state.message.contains(": permission denied."),
        "{}",
        state.message
    );
    let roots = FatalError::RootsUnreadable {
        path: "/nope".into(),
        errno: 2,
    }
    .text();
    assert!(roots.contains("(no such file or directory)"), "{roots}");
}

#[test]
fn item_record_should_carry_kind_selector_path_status_est_bytes_measured_bytes_and_elapsed_ms_When_apply(
) {
    let fx = RunFx::s3();
    let d = parse(&to_text(&apply_doc(&fx.input(1))));
    assert_eq!(d["mode"], "apply");
    assert_eq!(d["exit_code"], 1);
    assert_eq!(d["plan_id"], "91be04d7");
    let removed = &d["items"][0];
    for k in [
        "kind",
        "selector",
        "path",
        "status",
        "est_bytes",
        "measured_bytes",
        "elapsed_ms",
    ] {
        assert!(removed.get(k).is_some(), "{k}");
    }
    assert_eq!(removed["status"], "freed");
    assert!(removed["measured_bytes"].is_u64());
    let text = d.to_string();
    assert!(!text.contains("bytes_before") && !text.contains("bytes_after"));
    assert_eq!(d["errors"][0]["code"], "eacces");
    assert_eq!(
        d["totals"]["attributed_bytes"].as_u64().unwrap(),
        fx.summary.attributed_bytes
    );
    assert!(d["totals"]["attribution_pct"].as_f64().unwrap() > 95.0);
    assert!(d["fs"]["after"].is_object());
}

#[test]
fn json_should_emit_exactly_one_document_for_plan_apply_status_kinds_including_fatal() {
    let fx = RunFx::s3();
    let fs = fs_btrfs();
    let docs = vec![
        to_text(&plan_json()),
        to_text(&apply_doc(&fx.input(0))),
        to_text(&status_doc("/".as_ref(), Some(&fs), &[], 1)),
        to_text(&kinds_doc(&kind_rows(&|_| "yes".into()))),
        to_text(&fatal_doc("plan", None, &FatalError::LockHeld { pid: 1 })),
    ];
    for t in docs {
        let d = parse(&t);
        for k in ["schema_version", "mode", "exit_code", "errors"] {
            assert!(d.get(k).is_some(), "{k} in {t}");
        }
        assert!(!t.contains('\x1b'));
    }
    let s = status_doc("/".as_ref(), Some(&fs), &[], 1);
    assert_eq!(s["verdict"], "warn");
    assert_eq!(
        status_doc("/".as_ref(), None, &[], 74)["verdict"],
        "unknown"
    );
    let k = kinds_doc(&kind_rows(&|s| {
        if s == SelectorId::Steam {
            "not available in this build".into()
        } else {
            "yes".into()
        }
    }));
    assert_eq!(k["kinds"].as_array().unwrap().len(), 10);
    let steam = k["kinds"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "steam")
        .unwrap();
    assert_eq!(steam["available"], "not available in this build");
    assert_eq!(steam["tier"], "destructive");
    assert_eq!(steam["min_age"], "30d");
    let caches = &k["kinds"][0];
    assert_eq!(
        (
            caches["id"].as_str(),
            caches["default"].as_bool(),
            caches["min_age"].is_null()
        ),
        (Some("caches"), Some(true), true)
    );
}

#[test]
fn json_should_carry_schema_version_mode_plan_id_tier_status_integer_bytes_exit_code_errors_and_stable_reason_ids(
) {
    let d = plan_json();
    for item in d["items"].as_array().unwrap() {
        for k in [
            "kind",
            "selector",
            "family",
            "tier",
            "status",
            "est_bytes",
            "elapsed_ms",
            "reason",
            "reason_text",
        ] {
            assert!(item.get(k).is_some(), "{k}");
        }
        assert!(["safe", "guarded", "destructive"].contains(&item["tier"].as_str().unwrap()));
    }
    for r in SkipReason::ALL {
        let id = reason_id(r);
        assert!(
            !id.is_empty() && id == id.to_lowercase() && !id.contains(' '),
            "{id}"
        );
    }
}

#[test]
fn gate_refusal_should_be_one_document_with_the_code_and_detail_under_json() {
    let sel = selection_of(&[SelectorId::BuildDirs]);
    let ctx = GateRenderCtx {
        selection: &sel,
        saved: None,
        euid: 1000,
        state_dir_low: None,
        unavailable: vec![],
        home: None,
    };
    let r = destructive_gate(
        &sel,
        None,
        None,
        PlanId::from_hex("c4410aa2").unwrap(),
        &[],
        0,
    )
    .unwrap_err();
    assert!(matches!(r, GateRefusal::MissingPlanId { .. }));
    let d = parse(&to_text(&gate_refusal_doc(&r, &ctx)));
    assert_eq!(d["exit_code"], 64);
    assert_eq!(d["errors"][0]["code"], "plan_id_missing");
    assert_eq!(
        d["errors"][0]["detail"]["dry_run_command"],
        "dunnage --kinds build-dirs"
    );
    assert!(d["errors"][0]["message"]
        .as_str()
        .unwrap()
        .starts_with("error: destructive kinds"));
}
