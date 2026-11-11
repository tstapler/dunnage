#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cloned_ref_to_slice_refs,
    clippy::redundant_closure_for_method_calls,
    non_snake_case
)]
//! Task 6.6.3c: the explorer's checks of `audit verify` (ADR-006 A7, review 2 N6). The cases run against the
//! library the subcommand calls, over logs written by the real audit sink where the shape matters and by hand
//! where a line must be wrong. Read together they are the verifier's selftest for the new kind.

mod common;
mod explore_rig;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use disk_cleanup_adapters::audit::RealAuditSink;
use disk_cleanup_adapters::audit_verify::{
    verify_audit_with, verify_file_with, AllowedTable, VerifyExtras, VerifyInputs, VerifyReport,
};
use explore_rig::{Ex, Knobs};
use serde_json::{json, Value};

const HOME: &str = "/home/u";
const KIND: &str = "explorer-selection";

fn verify(lines: &[Value]) -> VerifyReport {
    verify_text(
        &lines
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn verify_text(text: &str) -> VerifyReport {
    let table = AllowedTable::embedded();
    verify_audit_with(
        text.as_bytes(),
        &VerifyInputs {
            table: &table,
            home: Path::new(HOME),
            config_roots: None,
        },
        &VerifyExtras {
            own_dirs: vec![PathBuf::from("/home/u/.local/state/disk-cleanup")],
        },
    )
    .unwrap()
}

fn start(run: &str) -> Value {
    json!({"type":"run_start","schema":1,"run_id":run,"ts":1,"kinds":KIND,
        "roots":{"scan_roots":["/home/u/code"],"cache_paths":[],"worktree_roots":[],"steam_libraries":[]}})
}

fn confirm(run: &str, count: u64, typed: &str, items: &[&str]) -> Value {
    json!({"type":"explorer_confirm","schema":1,"run_id":run,"ts":2,"plan_id":"abcd1234","count":count,
        "bytes":10,"typed":typed,"tty":true,
        "items":items.iter().map(|p| json!({"path":p,"dev":1,"ino":2})).collect::<Vec<_>>()})
}

fn intent(run: &str, path: &str) -> Value {
    json!({"type":"intent","schema":1,"run_id":run,"ts":3,"kind":KIND,"locator":{"path":path}})
}

fn removed(run: &str, path: &str) -> Value {
    json!({"type":"result","schema":1,"run_id":run,"ts":4,"kind":KIND,"status":"removed","path":path})
}

fn skipped(run: &str, path: &str) -> Value {
    json!({"type":"result","schema":1,"run_id":run,"ts":4,"kind":KIND,"status":{"skipped":"identity_changed"},"path":path})
}

fn partial(run: &str, path: &str) -> Value {
    json!({"type":"result","schema":1,"run_id":run,"ts":4,"kind":KIND,"status":{"error":"boom"},"path":path})
}

fn end(run: &str) -> Value {
    json!({"type":"run_end","run_id":run,"ts":5})
}

fn end_with(run: &str, key: &str, val: &Value) -> Value {
    json!({"type":"run_end","run_id":run,"ts":5, key: val})
}

const A: &str = "/home/u/code/app/dist";
const B: &str = "/home/u/code/app/out";

fn finished() -> Vec<Value> {
    vec![
        start("r"),
        confirm("r", 2, "2", &[A, B]),
        intent("r", A),
        removed("r", A),
        intent("r", B),
        removed("r", B),
        end("r"),
    ]
}

fn assert_clean(log: &[Value], what: &str) {
    let r = verify(log);
    assert_eq!(r.exit_code(), 0, "{what}: {r:?}");
    assert!(r.violations.is_empty(), "{what}: {r:?}");
}

fn assert_violation(log: &[Value], what: &str, needle: &str) {
    let r = verify(log);
    assert_eq!(r.exit_code(), 2, "{what}: {r:?}");
    let all = r
        .violations
        .iter()
        .map(|v| format!("{} {} {}", v.run_id, v.path, v.reason))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(all.contains(needle), "{what}: wanted {needle:?} in\n{all}");
}

#[test]
fn audit_verify_should_exit_0_for_a_finished_run_a_changed_item_a_refused_run_and_an_interrupted_run(
) {
    assert_clean(&finished(), "finished");

    let mut with_skip = finished();
    with_skip.splice(4..6, [skipped("r", B)]);
    assert_clean(&with_skip, "one item changed since the plan");

    assert_clean(
        &[
            start("r"),
            confirm("r", 2, "2", &[A, B]),
            end_with("r", "refused", &json!("Changed since you confirmed")),
        ],
        "refused re-measure: confirm, no intents, run_end",
    );

    assert_clean(
        &[
            start("r"),
            confirm("r", 3, "3", &[A, B, "/home/u/code/app/x"]),
            intent("r", A),
            removed("r", A),
            skipped("r", B),
            end_with("r", "interrupted", &json!(true)),
        ],
        "interrupted: fewer intents than items",
    );
}

#[test]
fn audit_verify_should_list_a_crash_between_intent_and_result_as_unfinished_and_exit_0() {
    let r = verify(&[start("r"), confirm("r", 2, "2", &[A, B]), intent("r", A)]);
    assert_eq!(r.exit_code(), 0, "{r:?}");
    assert_eq!(r.unfinished, ["r"]);
}

#[test]
fn audit_verify_should_name_a_torn_last_line_and_exit_0_but_fail_an_unparsable_line_elsewhere() {
    let mut text = finished()
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    text.push('\n');
    text.push_str("{\"type\":\"intent\",\"sche");
    let r = verify_text(&text);
    assert_eq!(r.exit_code(), 0, "{r:?}");
    assert_eq!(r.notes.len(), 1, "{r:?}");
    assert!(r.notes[0].contains("unfinished"), "{r:?}");
    assert!(r.warnings.is_empty());

    // The same garbage in the middle is a warning with a non-zero exit.
    let mut lines: Vec<String> = finished().iter().map(Value::to_string).collect();
    lines.insert(3, "{garbage".to_string());
    let r = verify_text(&format!("{}\n", lines.join("\n")));
    assert_eq!(r.exit_code(), 1, "{r:?}");
    assert_eq!(r.warnings.len(), 1);
    assert!(r.notes.is_empty());
}

#[test]
fn audit_verify_should_exit_2_and_name_the_run_When_the_explorer_confirm_line_is_missing_or_doubled(
) {
    let mut missing = finished();
    missing.remove(1);
    assert_violation(&missing, "confirm removed", "without an explorer_confirm");

    let mut doubled = finished();
    doubled.insert(2, confirm("r", 2, "2", &[A, B]));
    assert_violation(&doubled, "two confirms", "2 explorer_confirm lines");
}

#[test]
fn audit_verify_should_exit_2_When_a_result_or_an_intent_names_a_path_the_confirm_line_does_not_list(
) {
    let mut other_result = finished();
    other_result[5] = removed("r", "/home/u/code/app/other");
    assert_violation(&other_result, "result", "does not list");
    let mut other_intent = finished();
    other_intent[4] = intent("r", "/home/u/code/app/other");
    assert_violation(&other_intent, "intent", "does not list");
}

#[test]
fn audit_verify_should_exit_2_When_typed_differs_from_count_or_the_item_list_from_the_count() {
    let mut log = finished();
    log[1] = confirm("r", 2, "3", &[A, B]);
    assert_violation(&log, "typed", "typed \"3\" for a count of 2");
    let mut log = finished();
    log[1] = confirm("r", 2, "2", &[A]);
    assert_violation(&log, "items", "lists 1 items for a count of 2");
    // A padded or signed typed text is not the count either.
    for typed in ["02", " 2", "2 ", "+2", "two"] {
        let mut log = finished();
        log[1] = confirm("r", 2, typed, &[A, B]);
        assert_violation(&log, typed, "typed");
    }
}

#[test]
fn audit_verify_should_exit_2_When_a_finished_run_has_intents_plus_skips_other_than_the_count() {
    let mut short = finished();
    short.drain(4..6); // the second item has neither an intent nor a skip
    assert_violation(&short, "one item vanished", "is not the count 2");
    let mut extra = finished();
    extra.insert(6, skipped("r", A));
    assert_violation(&extra, "one item twice", "is not the count 2");
    let mut no_result = finished();
    no_result.remove(5);
    assert_violation(&no_result, "intent without result", "intents but");
}

#[test]
fn audit_verify_should_exit_2_When_a_refused_run_has_an_intent_or_an_unfinished_run_has_too_many() {
    assert_violation(
        &[
            start("r"),
            confirm("r", 2, "2", &[A, B]),
            intent("r", A),
            end_with("r", "refused", &json!("x")),
        ],
        "refused with an intent",
        "refused run with 1 intent",
    );
    assert_violation(
        &[
            start("r"),
            confirm("r", 1, "1", &[A]),
            intent("r", A),
            removed("r", A),
            skipped("r", A),
            end_with("r", "interrupted", &json!(true)),
        ],
        "interrupted with more than the count",
        "more than the count",
    );
}

#[test]
fn audit_verify_should_exit_2_for_a_result_an_intent_or_a_partial_outside_the_roots_or_inside_own_state(
) {
    let outside = "/data/else/dir";
    let state = "/home/u/.local/state/disk-cleanup/keep";
    let one_below = "/home/u/code";
    let git = "/home/u/code/app/.git/hooks";
    let ancestor = "/home";
    for (path, needle) in [
        (outside, "not strictly below a root"),
        (state, "not strictly below a root"),
        (one_below, "not strictly below a root"),
        (git, ".git"),
        (ancestor, "not strictly below a root"),
        ("/home/u/code/../.ssh", "lexically clean"),
    ] {
        for shape in 0..3 {
            let mut log = vec![start("r"), confirm("r", 1, "1", &[path])];
            match shape {
                0 => log.extend([intent("r", path), removed("r", path)]),
                1 => log.push(intent("r", path)), // an intent without a result counts
                _ => log.extend([intent("r", path), partial("r", path)]),
            }
            if shape != 1 {
                log.push(end("r"));
            }
            assert_violation(&log, &format!("{path} shape {shape}"), needle);
        }
    }
}

#[test]
fn audit_verify_should_apply_rule_f_and_the_state_rule_even_when_a_config_root_covers_the_path() {
    let table = AllowedTable::embedded();
    let roots = [
        PathBuf::from("/home/u"),
        PathBuf::from("/data"),
        PathBuf::from("/"),
    ];
    let extras = VerifyExtras {
        own_dirs: vec![PathBuf::from("/home/u/.local/state/disk-cleanup")],
    };
    for (path, needle) in [
        ("/home/u/one", "fewer than 2 components"),
        ("/home/u/.local/state/disk-cleanup/audit.jsonl", "own state"),
        ("/home/u/.local/state", "own state"),
        ("/home", "ancestor of $HOME"),
    ] {
        let text = [
            start("r"),
            confirm("r", 1, "1", &[path]),
            intent("r", path),
            removed("r", path),
            end("r"),
        ]
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
        let r = verify_audit_with(
            text.as_bytes(),
            &VerifyInputs {
                table: &table,
                home: Path::new(HOME),
                config_roots: Some(&roots),
            },
            &extras,
        )
        .unwrap();
        assert_eq!(r.exit_code(), 2, "{path}: {r:?}");
        assert!(
            r.violations.iter().any(|v| v.reason.contains(needle)),
            "{path}: {r:?}"
        );
    }
}

#[test]
fn audit_verify_should_judge_path_hex_of_intents_and_partials_as_it_does_for_results() {
    use std::os::unix::ffi::OsStrExt;
    let raw = std::ffi::OsStr::from_bytes(b"/data/else/\xff\xfe");
    let hex = raw.as_bytes().iter().fold(String::new(), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    });
    let lossy = raw.to_string_lossy().into_owned();
    for shape in 0..2 {
        let mut it = intent("r", &lossy);
        it["path_hex"] = json!(hex);
        let mut pr = partial("r", &lossy);
        pr["path_hex"] = json!(hex);
        let log = if shape == 0 {
            vec![start("r"), confirm("r", 1, "1", &[&lossy]), it]
        } else {
            vec![start("r"), confirm("r", 1, "1", &[&lossy]), pr, end("r")]
        };
        assert_violation(
            &log,
            "non-UTF-8 outside the roots",
            "not strictly below a root",
        );
    }
    // A hex string that does not match the text is itself a violation.
    let mut bad = intent("r", "/home/u/code/app/dist");
    bad["path_hex"] = json!("zz");
    assert_violation(
        &[start("r"), confirm("r", 1, "1", &[A]), bad],
        "bad hex",
        "not valid hex",
    );
}

#[test]
fn audit_verify_should_keep_the_cleaner_run_rules_When_a_log_mixes_both_kinds() {
    let mut log = vec![
        json!({"type":"run_start","schema":1,"run_id":"c","ts":1,"kinds":"build-dirs",
            "roots":{"scan_roots":["/home/u/code"],"cache_paths":[],"worktree_roots":[],"steam_libraries":[]}}),
        json!({"type":"result","schema":1,"run_id":"c","ts":2,"kind":"node-modules","status":"removed",
            "path":"/home/u/code/app/node_modules"}),
        end("c"),
    ];
    log.extend(finished());
    assert_clean(&log, "mixed");
}

#[test]
fn safety_audit_verify_should_read_back_a_real_explorer_run_written_by_the_real_audit_sink_as_clean(
) {
    let ex = Ex::new();
    let a = ex.tree("app/dist", 2, 2000);
    let b = ex.tree("app/out", 1, 2000);
    let log = ex.p("state/disk-cleanup/audit.jsonl");
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    let sink: Arc<dyn disk_cleanup_core::ports::AuditSink> = Arc::new(RealAuditSink::new(
        log.clone(),
        ex.p("state/disk-cleanup/audit.reserve"),
        None,
    ));
    let confirmed = ex.confirmed(&[a.clone(), b.clone()]);
    let ran = ex.run_with(
        confirmed,
        ex.confined_ops(),
        &Knobs {
            audit: Some(sink),
            ..Knobs::default()
        },
    );
    assert_eq!(ran.report.exit, 0, "{:?}", ran.report);
    let table = AllowedTable::embedded();
    let roots = [ex.code.clone()];
    let report = verify_file_with(
        &log,
        &VerifyInputs {
            table: &table,
            home: &ex.home,
            config_roots: Some(&roots),
        },
        &VerifyExtras {
            own_dirs: disk_cleanup_adapters::own_dirs(
                &ex.dirs.state,
                &ex.base_dirs.cache,
                &ex.base_dirs.config,
            ),
        },
    )
    .unwrap();
    assert_eq!(report.exit_code(), 0, "{report:?}");
    assert_eq!(report.deletions, 2);
    // The line is synced and carries the paths.
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains("\"type\":\"explorer_confirm\""));
    assert!(text.contains(&a.display().to_string()));
}

const SYNC_CHILD: &str = "DC_EX_SYNC_CHILD";

#[test]
fn safety_explorer_confirm_should_be_synced_to_disk_when_it_is_written() {
    if !common::strace_available() {
        assert!(
            std::env::var_os("DC_REQUIRE_STRACE").is_none(),
            "DC_REQUIRE_STRACE is set and strace is not installed"
        );
        eprintln!("SKIPPED: strace is not installed, so fdatasync cannot be observed");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let log = td.path().join("audit.jsonl");
    let trace = td.path().join("strace.txt");
    let st = std::process::Command::new("strace")
        .args(["-f", "-e", "trace=fdatasync,fsync", "-o"])
        .arg(&trace)
        .arg(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "child_writes_one_explorer_confirm",
            "--nocapture",
        ])
        .env(SYNC_CHILD, &log)
        .status()
        .unwrap();
    assert!(st.success());
    let calls = std::fs::read_to_string(&trace).unwrap();
    let syncs = calls
        .lines()
        .filter(|l| l.contains("fdatasync(") || l.contains("fsync("))
        .count();
    assert_eq!(syncs, 1, "one sync for the one line:\n{calls}");
    assert!(std::fs::read_to_string(&log)
        .unwrap()
        .contains("\"type\":\"explorer_confirm\""));
}

#[test]
fn child_writes_one_explorer_confirm() {
    use disk_cleanup_core::ports::AuditSink;
    let Some(log) = std::env::var_os(SYNC_CHILD) else {
        return;
    };
    let log = PathBuf::from(log);
    let sink = RealAuditSink::new(log.clone(), log.with_extension("reserve"), None);
    sink.explorer_confirm(&disk_cleanup_core::ExplorerConfirm {
        run_id: "r".into(),
        ts: 1,
        plan_id: "abcd1234".into(),
        count: 1,
        bytes: 10,
        typed: "1".into(),
        tty: true,
        items: vec![disk_cleanup_core::ConfirmedItem {
            path: PathBuf::from(A),
            path_hex: None,
            dev: 1,
            ino: 2,
        }],
    })
    .unwrap();
}
