#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Task 1.2.3b: golden JSON shapes for agents (T-430, T-510). `docs/json-schema.md` holds the rule: an
//! additive field never bumps `schema_version`; removing, renaming or retyping one does.

mod support;

use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use dunnage_cli::render_json::{
    apply_doc, fatal_doc, kind_rows, kinds_doc, plan_doc, status_doc, to_text, JQ_TSV_ONE_LINER,
};
use dunnage_cli::view::PlanView;
use dunnage_core::reasons::FatalError;
use serde_json::Value;
use support::*;

const NAMES: [&str; 5] = ["plan", "apply", "status", "kinds", "fatal"];

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.json"))
}

fn current(name: &str) -> Value {
    match name {
        "plan" => {
            let fx = Fixture::s1();
            let input = fx.input();
            plan_doc(&input, &PlanView::build(&input), 0)
        }
        "apply" => apply_doc(&RunFx::s3().input(1)),
        "status" => status_doc("/".as_ref(), Some(&fs_btrfs()), &[], 1),
        "kinds" => kinds_doc(&kind_rows(&|_| "yes".into())),
        "fatal" => fatal_doc(
            "apply",
            Some("91be04d7"),
            &FatalError::LockHeld { pid: 4242 },
        ),
        other => panic!("{other}"),
    }
}

/// `jq -S '[paths(scalars)|map(if type=="number" then "*" else tostring end)|join(".")]|unique'`
fn shape(v: &Value) -> BTreeSet<String> {
    fn walk(v: &Value, at: &mut Vec<String>, out: &mut BTreeSet<String>) {
        match v {
            Value::Object(m) => m.iter().for_each(|(k, x)| {
                at.push(k.clone());
                walk(x, at, out);
                at.pop();
            }),
            Value::Array(a) => a.iter().for_each(|x| {
                at.push("*".into());
                walk(x, at, out);
                at.pop();
            }),
            _ => {
                out.insert(at.join("."));
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(v, &mut Vec::new(), &mut out);
    out
}

fn missing(golden: &Value, new: &Value) -> Vec<String> {
    shape(golden).difference(&shape(new)).cloned().collect()
}

fn golden(name: &str) -> Value {
    let text = std::fs::read_to_string(golden_path(name))
        .unwrap_or_else(|e| panic!("{name}: {e}; run `cargo test -- --ignored regen_golden`"));
    serde_json::from_str(&text).unwrap()
}

#[test]
fn json_should_keep_every_golden_key_When_schema_version_is_unchanged() {
    for name in NAMES {
        let (g, n) = (golden(name), current(name));
        if g["schema_version"] == n["schema_version"] {
            let gone = missing(&g, &n);
            assert!(
                gone.is_empty(),
                "{name}: keys removed without a schema_version bump: {gone:?}"
            );
        }
        for k in ["schema_version", "mode", "exit_code"] {
            assert_eq!(g[k], n[k], "{name}.{k}");
        }
        if name == "plan" || name == "apply" {
            let status = |v: &Value| {
                v["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|i| i["status"].clone())
                    .collect::<Vec<_>>()
            };
            assert_eq!(status(&g), status(&n), "{name} item status");
        }
    }
}

#[test]
fn a_removed_or_retyped_key_should_be_reported_and_a_new_key_should_not() {
    let g = current("plan");
    let mut removed = g.clone();
    removed["totals"]
        .as_object_mut()
        .unwrap()
        .remove("attributed_bytes");
    assert_eq!(
        missing(&g, &removed),
        vec!["totals.attributed_bytes".to_string()]
    );
    let mut added = g.clone();
    added["new_field"] = Value::Bool(true);
    assert!(missing(&g, &added).is_empty());
    let mut array_shape = g.clone();
    array_shape["items"] = Value::Array(vec![]);
    assert!(missing(&g, &array_shape)
        .iter()
        .any(|k| k.starts_with("items.*.")));
}

#[test]
fn jq_tsv_one_liner_of_help_should_give_four_fields_per_item() {
    let g = golden("plan");
    let n_items = g["items"].as_array().unwrap().len();
    let filter = JQ_TSV_ONE_LINER
        .split_once("jq -r ")
        .unwrap()
        .1
        .trim_matches('\'');
    let Ok(mut child) = Command::new("jq")
        .args(["-r", filter])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
    else {
        // no jq on this machine: the same projection in Rust
        assert!(g["items"].as_array().unwrap().iter().all(|i| [
            "status",
            "kind",
            "est_bytes",
            "path"
        ]
        .iter()
        .all(|k| i.get(k).is_some())));
        return;
    };
    child
        .stdin
        .take()
        .unwrap()
        .write_all(serde_json::to_string(&g).unwrap().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.lines().count(), n_items);
    assert!(text.lines().all(|l| l.split('\t').count() == 4), "{text}");
}

#[test]
#[ignore = "regenerates the golden files"]
fn regen_golden() {
    std::fs::create_dir_all(golden_path("plan").parent().unwrap()).unwrap();
    for name in NAMES {
        std::fs::write(golden_path(name), to_text(&current(name))).unwrap();
    }
}
