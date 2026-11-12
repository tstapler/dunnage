#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Story 6.5.5: the `--json` document, its golden file and the parity of its numbers with the text view and
//! with the rows the interactive view is built from.

mod explore_fixture;

use std::sync::Arc;

use dunnage_cli::explore::json::run_json;
use dunnage_cli::explore::text::{build_model, run_text, scan_to_completion, ScanEnd};
use dunnage_core::explore::fmt::{fmt_items_cell, fmt_size_cell, share_cell};
use dunnage_core::explore::rows::{build_rows, Filter, RowKind, RowSource};
use dunnage_core::explore::testing::FakeTermSignals;
use explore_fixture::{env, golden};
use serde_json::Value;

fn doc(truncated: bool) -> (i32, String, Value) {
    let e = env(80, truncated);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run_json(&e, &mut out, &mut err);
    let text = String::from_utf8(out).unwrap();
    let v: Value = serde_json::from_str(&text).expect("one JSON document");
    (code, text, v)
}

#[test]
fn the_document_should_match_the_golden_file_and_carry_the_required_keys() {
    let (code, text, v) = doc(false);
    assert_eq!(code, 0);
    golden("explore_json.json", &text);
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["mode"], "explore");
    for k in ["roots", "filesystem", "entries", "exit_code", "errors"] {
        assert!(v.get(k).is_some(), "{k}");
    }
    for k in [
        "verdict",
        "avail_bytes",
        "unallocated_bytes",
        "data_slack_bytes",
        "meta_slack_bytes",
    ] {
        assert!(v["filesystem"].get(k).is_some(), "filesystem.{k}");
    }
    // Key superset: every entry holds at least the documented keys.
    for e in v["entries"].as_array().unwrap() {
        for k in [
            "path",
            "usage_bytes",
            "apparent_bytes",
            "meta_est_bytes",
            "shared_est_bytes",
            "newest_dir_mtime",
            "items",
            "share_of_parent",
            "state",
            "kind",
            "errors",
        ] {
            assert!(e.get(k).is_some(), "entry.{k}: {e}");
        }
        assert!(e["shared_est_bytes"].is_null());
    }
}

#[test]
fn a_name_that_is_not_utf8_should_carry_path_hex_and_others_should_not() {
    let (_, _, v) = doc(false);
    let entries = v["entries"].as_array().unwrap();
    let bad: Vec<&Value> = entries
        .iter()
        .filter(|e| e.get("path_hex").is_some())
        .collect();
    assert_eq!(bad.len(), 1);
    assert!(bad[0]["path_hex"].as_str().unwrap().contains("ff"));
}

#[test]
fn integers_should_be_exact_and_states_words() {
    let (_, text, v) = doc(false);
    assert!(!text.contains("e+"), "{text}");
    let by = |n: &str| {
        v["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["path"].as_str().is_some_and(|p| p.ends_with(n)))
            .unwrap()
            .clone()
    };
    assert_eq!(by("/app")["usage_bytes"], 54u64 << 30);
    assert_eq!(by("root-owned")["state"], "unreadable");
    assert_eq!(by("root-owned")["errors"][0]["errno"], 13);
    assert_eq!(by("/mnt")["state"], "other_fs");
    assert_eq!(by("/app")["kind"], Value::Null);
}

#[test]
fn a_partial_scan_should_still_exit_zero() {
    let (code, _, v) = doc(true);
    assert_eq!(code, 0);
    assert_eq!(v["partial"], true);
}

#[test]
fn text_json_and_the_interactive_row_model_should_agree_on_every_number() {
    let mut e = env(200, false);
    e.top = 0;
    let ScanEnd::Done(scanned) = scan_to_completion(&e) else {
        panic!("interrupted")
    };
    let model = build_model(&e, &scanned);
    let (_, _, v) = doc(false);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    run_text(&e, &mut out, &mut err);
    let text = String::from_utf8(out).unwrap();
    // The rows the interactive view draws for the same directory.
    let root = scanned.tree.roots().next().unwrap();
    let list = build_rows(
        &RowSource {
            tree: &scanned.tree,
            dir: root,
            files: None,
            badge: &|_| dunnage_core::explore::rows::Badge::None,
        },
        e.sort,
        Filter::All,
        "",
    );
    let parent = scanned.tree.total(root).allocated_bytes;
    let jentries = v["entries"].as_array().unwrap();
    assert_eq!(list.rows.len(), model.roots[0].entries.len());
    assert_eq!(list.rows.len(), jentries.len());
    for ((row, ent), j) in list.rows.iter().zip(&model.roots[0].entries).zip(jentries) {
        assert_eq!(row.usage, ent.usage);
        assert_eq!(j["usage_bytes"], row.usage);
        assert_eq!(j["apparent_bytes"], row.apparent);
        assert_eq!(j["items"].as_u64(), row.items);
        assert_eq!(j["meta_est_bytes"].as_u64(), row.meta);
        let cells = [
            fmt_size_cell(row.usage),
            fmt_size_cell(row.apparent),
            row.meta.map_or("n/a".to_string(), fmt_size_cell),
            row.items.map_or("n/a".to_string(), fmt_items_cell),
            share_cell(row.usage, parent, 10).percent,
        ];
        let name = if row.kind == RowKind::Dir {
            Some(ent.name.clone())
        } else {
            None
        };
        let line = text
            .lines()
            .find(|l| {
                name.as_ref().is_some_and(|n| {
                    l.ends_with(&format!("  {n}")) || l.contains(&format!("  {n}  ["))
                })
            })
            .map(str::to_string);
        if let Some(line) = line {
            let mut rest = line.as_str();
            for c in cells.iter().take(4) {
                let at = rest
                    .find(c.as_str())
                    .unwrap_or_else(|| panic!("{c} not in {line}"));
                rest = &rest[at + c.len()..];
            }
        }
    }
}

#[test]
fn a_signal_during_the_scan_should_exit_130_with_no_document() {
    let mut e = env(80, false);
    e.signals = Arc::new(FakeTermSignals::at_tick(1, 2));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(run_json(&e, &mut out, &mut err), 130);
    assert!(out.is_empty());
}
