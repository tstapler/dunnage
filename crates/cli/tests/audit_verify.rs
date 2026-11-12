#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 1.2.4, Task 1.2.4d: `audit verify` (requirement Safety). The AC scenarios run against the library
//! the subcommand calls; `audit_cmd_*` tests spawn the binary.

mod common;

use std::path::{Path, PathBuf};

use common::Fixture;
use dunnage_adapters::audit_verify::{verify_audit, AllowedTable, VerifyInputs};
use dunnage_core::cache_targets::{CacheSource, CACHE_ROWS};
use serde_json::json;

fn hex_of(b: &[u8]) -> String {
    use std::fmt::Write as _;
    b.iter().fold(String::new(), |mut s, x| {
        let _ = write!(s, "{x:02x}");
        s
    })
}

const HOME: &str = "/home/u";

fn start(run: &str, roots: &[&str], libs: &[&str]) -> String {
    json!({"type":"run_start","schema":1,"run_id":run,"ts":1,"roots":{
        "scan_roots":roots,"cache_paths":[],"worktree_roots":roots,"steam_libraries":libs}})
    .to_string()
}

fn removed(run: &str, kind: &str, path: &str) -> String {
    json!({"type":"result","schema":1,"run_id":run,"ts":2,"kind":kind,"status":"removed","path":path}).to_string()
}

fn end(run: &str) -> String {
    json!({"type":"run_end","run_id":run,"ts":3}).to_string()
}

fn verify(
    lines: &[String],
    config: Option<&[PathBuf]>,
) -> dunnage_adapters::audit_verify::VerifyReport {
    let table = AllowedTable::embedded();
    let text = lines.join("\n");
    verify_audit(
        text.as_bytes(),
        &VerifyInputs {
            table: &table,
            home: Path::new(HOME),
            config_roots: config,
        },
    )
    .unwrap()
}

fn clean_log() -> Vec<String> {
    vec![
        start("r1", &["/home/u/code"], &["/mnt/lib"]),
        removed("r1", "cache", "/home/u/.npm/_cacache"),
        removed("r1", "node-modules", "/home/u/code/app/node_modules"),
        end("r1"),
        start("r2", &["/home/u/code"], &["/mnt/lib"]),
        removed("r2", "steam-prefix", "/mnt/lib/steamapps/compatdata/12345"),
        end("r2"),
    ]
}

#[test]
fn audit_verify_should_print_3_deletions_in_2_runs_0_outside_When_the_log_is_clean() {
    let r = verify(&clean_log(), None);
    assert_eq!(
        r.summary_line(),
        "audit verify: 3 deletions in 2 runs, 0 outside the allowed roots"
    );
    assert_eq!(r.exit_code(), 0);
}

#[test]
fn audit_verify_should_list_each_violation_and_exit_2_When_a_path_is_outside_or_has_the_wrong_name()
{
    let mut log = clean_log();
    log.push(removed("r2", "cache", "/etc/ssh"));
    log.push(removed("r2", "node-modules", "/home/u/code/app/src"));
    let r = verify(&log, None);
    assert_eq!(
        r.summary_line(),
        "audit verify: 5 deletions in 2 runs, 2 outside the allowed roots"
    );
    assert_eq!(r.exit_code(), 2);
    assert_eq!(r.violations[0].run_id, "r2");
    assert_eq!(r.violations[0].kind, "cache");
    assert_eq!(r.violations[0].path, "/etc/ssh");
    assert_eq!(r.violations[1].path, "/home/u/code/app/src");
}

#[test]
fn audit_verify_should_list_a_run_without_run_end_as_unfinished_not_as_a_violation() {
    let log = vec![
        start("r9", &["/home/u/code"], &[]),
        removed("r9", "cache", "/home/u/.npm/_cacache"),
    ];
    let r = verify(&log, None);
    assert_eq!(r.unfinished, vec!["r9".to_string()]);
    assert!(r.violations.is_empty());
}

#[test]
fn audit_verify_should_flag_relative_dotdot_and_root_equal_paths() {
    for bad in [
        "code/app/node_modules",
        "/home/u/code/../etc/node_modules",
        "/home/u/code",
        "/home/u/code//app/node_modules",
    ] {
        let log = vec![
            start("r", &["/home/u/code"], &[]),
            removed("r", "node-modules", bad),
        ];
        assert_eq!(verify(&log, None).violations.len(), 1, "{bad}");
    }
}

#[test]
fn audit_verify_should_not_trust_the_roots_the_same_run_recorded() {
    let log = vec![
        start("r", &["/etc"], &[]),
        removed("r", "node-modules", "/etc/cron.d/node_modules"),
    ];
    let r = verify(&log, None);
    assert_eq!(r.exit_code(), 2);
    assert!(r.violations[0].reason.contains(
        "inside the roots recorded by that run but not in the reviewed table or your config"
    ));
    assert!(r.violations[0]
        .reason
        .ends_with("add the root to your config"));
    // The owner's config is the other legitimate source.
    let cfg = vec![PathBuf::from("/etc")];
    assert_eq!(verify(&log, Some(&cfg)).exit_code(), 0);
}

#[test]
fn audit_verify_should_decode_path_hex_compare_bytes_and_flag_disagreement_and_bad_hex() {
    let mut line: serde_json::Value = serde_json::from_str(&removed(
        "r",
        "node-modules",
        "/home/u/code/a\u{fffd}/node_modules",
    ))
    .unwrap();
    // `/home/u/code/a\xff/node_modules`
    let hex = hex_of(b"/home/u/code/a\xff/node_modules");
    line["path_hex"] = hex.into();
    let ok = verify(
        &[start("r", &["/home/u/code"], &[]), line.to_string()],
        None,
    );
    assert_eq!(ok.violations.len(), 0);
    line["path"] = "/home/u/code/zzz/node_modules".into();
    let differ = verify(
        &[start("r", &["/home/u/code"], &[]), line.to_string()],
        None,
    );
    assert_eq!(differ.violations[0].reason, "path and path_hex differ");
    assert!(differ.violations[0].path.contains("\\xff"));
    line["path_hex"] = "zz".into();
    let bad = verify(
        &[start("r", &["/home/u/code"], &[]), line.to_string()],
        None,
    );
    assert_eq!(bad.violations.len(), 1);
    // A non-UTF-8 name outside every root is printed escaped.
    let mut out: serde_json::Value =
        serde_json::from_str(&removed("r", "node-modules", "/tmp/a\u{fffd}")).unwrap();
    out["path_hex"] = hex_of(b"/tmp/a\xff").into();
    let r = verify(&[start("r", &[], &[]), out.to_string()], None);
    assert_eq!(r.violations[0].path, "/tmp/a\\xff");
}

#[test]
fn allowed_roots_should_list_every_cache_table_path_and_nothing_else() {
    let t = AllowedTable::embedded();
    let mut from_table: Vec<String> = Vec::new();
    let mut tools: Vec<String> = Vec::new();
    for row in CACHE_ROWS {
        match row.source {
            CacheSource::Home(p) => from_table.push(format!("~/{p}")),
            CacheSource::Absolute(p) => from_table.push(p.to_string()),
            CacheSource::Tool(tool) => tools.push(tool.basename().to_string()),
        }
    }
    let (mut have, mut have_tools) = (t.exact_paths("cache"), t.tool_names("cache"));
    for v in [&mut from_table, &mut have, &mut tools, &mut have_tools] {
        v.sort();
    }
    assert_eq!(
        have, from_table,
        "ci/allowed-roots.txt and the cache table must list the same paths"
    );
    assert_eq!(have_tools, tools);
}

#[test]
fn ci_gate_should_pin_the_sha256_of_allowed_roots() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let gate = std::fs::read_to_string(root.join("ci/gate.d/1.2.4.required")).unwrap();
    assert!(
        gate.contains("sha256sum -c ci/allowed-roots.sha256"),
        "{gate}"
    );
    let pin = std::fs::read_to_string(root.join("ci/allowed-roots.sha256")).unwrap();
    assert!(pin.trim_end().ends_with("ci/allowed-roots.txt"), "{pin}");
}

fn write_log(f: &Fixture, lines: &[String]) -> PathBuf {
    let dir = f.xdg("state").join("dunnage");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("audit.jsonl");
    std::fs::write(&p, lines.join("\n") + "\n").unwrap();
    p
}

#[test]
fn audit_cmd_should_exit_0_2_and_74_and_leave_the_state_directory_untouched() {
    let f = Fixture::new();
    let home = f.home().display().to_string();
    let log = vec![
        start("r1", &[&format!("{home}/code")], &[]),
        removed("r1", "cache", &format!("{home}/.npm/_cacache")),
        end("r1"),
    ];
    let p = write_log(&f, &log);
    let before = common::tree_manifest(&f.xdg("state"));
    let out = f.fixture_cmd().args(["audit", "verify"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        text.contains("audit verify: 1 deletions in 1 runs, 0 outside the allowed roots"),
        "{text}"
    );
    assert_eq!(common::tree_manifest(&f.xdg("state")), before, "read-only");

    let mut bad = log;
    bad.insert(2, removed("r1", "cache", "/etc/ssh"));
    std::fs::write(&p, bad.join("\n") + "\n").unwrap();
    let out = f
        .fixture_cmd()
        .args(["audit", "verify", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["mode"], "audit-verify");
    assert_eq!(doc["violations"][0]["path"], "/etc/ssh");
    assert_eq!(doc["exit_code"], 2);

    let missing = f.root().join("nope.jsonl");
    let out = f
        .fixture_cmd()
        .args(["audit", "verify", "--log"])
        .arg(&missing)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(74));
}

// ---- Task 6.6.5g (review C13): partial results, intents, unparsable lines and skipped schemas ---------------

fn partial(run: &str, kind: &str, path: &str) -> String {
    json!({"type":"result","schema":1,"run_id":run,"ts":2,"kind":kind,
        "status":{"error":"directory not empty"},"path":path,"measured_bytes":4096})
    .to_string()
}

fn intent(run: &str, kind: &str, path: &str) -> String {
    json!({"type":"intent","schema":1,"run_id":run,"ts":2,"kind":kind,"locator":{"path":path}})
        .to_string()
}

#[test]
fn safety_audit_verify_flags_a_partial_result_outside_the_table_and_names_the_path() {
    let log = vec![
        start("r1", &["/home/u/code"], &[]),
        partial("r1", "node-modules", "/home/u/Documents/x/node_modules"),
        end("r1"),
    ];
    let r = verify(&log, None);
    assert_eq!(r.exit_code(), 2);
    assert_eq!(r.violations.len(), 1);
    assert!(r.violations[0].path.ends_with("Documents/x/node_modules"));
    assert!(r.violations[0]
        .reason
        .starts_with("partial or failed delete"));
    assert_eq!(r.deletions, 0, "a partial result is not a counted deletion");
}

#[test]
fn safety_audit_verify_flags_an_intent_outside_the_table_even_without_a_result() {
    let log = vec![
        start("r1", &["/home/u/code"], &[]),
        intent("r1", "node-modules", "/home/u/Documents/x/node_modules"),
    ];
    let r = verify(&log, None);
    assert_eq!(r.exit_code(), 2);
    assert!(r.violations[0].reason.starts_with("intent"));
    let ok = vec![
        start("r1", &["/home/u/code"], &[]),
        intent("r1", "node-modules", "/home/u/code/app/node_modules"),
        end("r1"),
    ];
    assert_eq!(verify(&ok, None).exit_code(), 0);
}

#[test]
fn verify_should_warn_about_an_unparsable_line_and_a_schema_2_line_with_a_nonzero_exit() {
    let mut log = clean_log();
    log.insert(2, "{\"type\":\"result\",\"sch".to_string());
    log.insert(
        3,
        json!({"type":"result","schema":2,"run_id":"r1"}).to_string(),
    );
    let r = verify(&log, None);
    assert_eq!(r.exit_code(), 1);
    assert_eq!(r.warnings.len(), 2);
    assert!(
        r.warnings[0].starts_with("line 3: not valid JSON"),
        "{:?}",
        r.warnings
    );
    assert!(
        r.warnings[1].starts_with("line 4: schema 2"),
        "{:?}",
        r.warnings
    );
    assert!(r.violations.is_empty());
}

#[test]
fn a_skipped_result_should_not_be_judged() {
    let log = vec![
        start("r1", &["/home/u/code"], &[]),
        json!({"type":"result","schema":1,"run_id":"r1","ts":2,"kind":"node-modules",
            "status":{"skipped":"too-recent"},"path":"/home/u/Documents/x/node_modules"})
        .to_string(),
        end("r1"),
    ];
    assert_eq!(verify(&log, None).exit_code(), 0);
}
