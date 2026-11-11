#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! `RealAuditSink` and the audit reader (Story 2.2.1, Task 2.2.1a, 2.2.1b; validation T-254 to T-262, T-324,
//! M-02). The executor-level legs ("nothing was deleted", exit 2) are covered with `FakeAuditSink` in
//! `core/tests/executor.rs` and end to end in Story 2.3.3.

use std::ffi::OsString;
use std::io::Write;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use disk_cleanup_adapters::audit::{unclean_run_warning, unfinished_run, RealAuditSink};
use disk_cleanup_adapters::state::{sudo_state_dir, Owner};
use disk_cleanup_core::ports::AuditSink;
use disk_cleanup_core::*;

#[path = "support/fullfs.rs"]
mod fullfs;

fn build() -> BuildInfo {
    BuildInfo {
        bin_version: "0.2.0".into(),
        git_commit: "c0ffee1c0ffee1c0ffee1c0ffee1c0ffee1c0ffe".into(),
        dirty: true,
        metascope_rev: "5ca1ab15ca1ab15ca1ab15ca1ab15ca1ab15ca1a".into(),
        patched: false,
        metascope_dirty: false,
    }
}

fn start(run: &str) -> RunStart {
    RunStart {
        schema: AUDIT_SCHEMA,
        run_id: run.into(),
        ts: 10,
        euid: 1000,
        kinds: "caches".into(),
        plan_id: Some("7f3a91c2".into()),
        build: build(),
        stamp: Some(StampSummary {
            gate: "B".into(),
            commit: "c0ffee".into(),
            binary_sha256: "ab".into(),
            verified: true,
        }),
        break_glass: Some(BreakGlass {
            reason: "rebuilt locally for Gate B".into(),
            tty_confirmed: true,
        }),
        roots: RunRoots::default(),
        verdict_start: Some(DiskVerdict::Warn),
    }
}

fn candidate(path: &Path) -> Candidate {
    Candidate::new(
        Locator::Path(path.to_path_buf()),
        KindFacts::BuildDir(BuildDirFacts {
            flavor: BuildDirFlavor::NodeModules,
            group: path.parent().unwrap().to_path_buf(),
            age_secs: 86_400,
            lockfile_hash: Some(7),
        }),
        AllocatedBytes(4096),
        Some(1_700_000_000),
        Identity { dev: 28, ino: 77 },
    )
}

fn result(run: &str, path: &Path) -> ItemResult {
    let c = candidate(path);
    ItemResult(Outcome {
        ts: 11,
        run_id: run.into(),
        kind: c.kind(),
        selector: c.selector(),
        id: c.id(),
        path: Some(path.to_path_buf()),
        status: Status::Removed,
        est_bytes: 4096,
        measured_bytes: Some(4096),
        freed: Freed::default(),
        elapsed_ms: 3,
        trimmed_bytes: None,
    })
}

fn end(run: &str) -> RunEnd {
    RunEnd {
        run_id: run.into(),
        ts: 12,
        verdict_after: DiskVerdict::Ok,
        attributed_bytes: 4096,
        measured_drop: Some(4096),
        attribution_pct: Some(100.0),
        causes: vec![],
        explained: true,
        interrupted: false,
    }
}

fn sink(dir: &Path) -> RealAuditSink {
    RealAuditSink::new(dir.join("audit.jsonl"), dir.join("audit.reserve"), None)
}

fn lines(dir: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(dir.join("audit.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn audit_should_have_run_start_intent_result_run_end_with_schema_1_and_same_run_id_When_one_item_applied(
) {
    let d = tempfile::tempdir().unwrap();
    let p = PathBuf::from("/home/u/.npm/_cacache");
    let s = sink(d.path());
    s.run_start(&start("r1")).unwrap();
    s.intent(&Intent::of(&candidate(&p), "r1", 11)).unwrap();
    s.result(&result("r1", &p)).unwrap();
    s.run_end(&end("r1")).unwrap();

    let v = lines(d.path());
    let types: Vec<_> = v.iter().map(|l| l["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["run_start", "intent", "result", "run_end"]);
    for l in &v {
        assert_eq!(l["schema"], 1, "{l}");
        assert_eq!(l["run_id"], "r1", "{l}");
    }
    // Build provenance, gate stamp, break-glass and the start verdict ride on run_start (T-443, first half).
    let rs = &v[0];
    assert_eq!(rs["build"]["dirty"], true);
    assert_eq!(rs["build"]["git_commit"], build().git_commit);
    assert_eq!(rs["build"]["metascope_rev"], build().metascope_rev);
    assert_eq!(rs["build"]["patched"], false);
    assert_eq!(rs["build"]["metascope_dirty"], false);
    assert_eq!(rs["stamp"]["verified"], true);
    assert_eq!(rs["break_glass"]["tty_confirmed"], true);
    assert_eq!(rs["verdict_start"], "warn");
    assert_eq!(v[2]["path"], "/home/u/.npm/_cacache");
    assert_eq!(v[2]["est_bytes"], 4096);
    assert_eq!(v[2]["measured_bytes"], 4096);
}

#[test]
fn readonly_audit_file_should_exit_2_and_delete_nothing_When_apply_starts() {
    let d = tempfile::tempdir().unwrap();
    let log = d.path().join("audit.jsonl");
    std::fs::write(&log, "").unwrap();
    std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o400)).unwrap();
    if std::fs::OpenOptions::new().append(true).open(&log).is_ok() {
        eprintln!("skip: running as a user that ignores file modes");
        return;
    }
    let err = sink(d.path()).run_start(&start("r1")).unwrap_err();
    assert_eq!(err.raw_os_error(), Some(13));
    // The executor turns this into FatalError::AuditUnwritable (exit 2) before the first delete.
    let fatal = FatalError::AuditUnwritable {
        path: log.clone(),
        errno: 13,
    };
    assert_eq!(fatal.exit_code(), 2);
    assert!(fatal.text().contains("Nothing was deleted"));
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "");
}

#[test]
fn sudo_state_dir_should_resolve_to_sudo_user_home_0600_and_chowned_When_euid_0_sudo_user_set() {
    let passwd = |u: &str| (u == "tstapler").then(|| PathBuf::from("/home/tstapler"));
    let root_state = Path::new("/root/.local/state");
    assert_eq!(
        sudo_state_dir(0, Some("tstapler"), root_state, &passwd),
        Path::new("/home/tstapler/.local/state/disk-cleanup")
    );
    assert_eq!(
        sudo_state_dir(0, None, root_state, &passwd),
        Path::new("/root/.local/state/disk-cleanup")
    );
    assert_eq!(
        sudo_state_dir(1000, Some("tstapler"), Path::new("/x"), &passwd),
        Path::new("/x/disk-cleanup")
    );
    assert_eq!(
        sudo_state_dir(0, Some("nobody-here"), root_state, &passwd),
        Path::new("/root/.local/state/disk-cleanup")
    );

    // Files are chowned to the owner of the user's state dir only when running as root.
    let d = tempfile::tempdir().unwrap();
    assert_eq!(Owner::for_euid(1000, d.path()), None);
    let me = Owner::for_euid(0, &d.path().join("not/yet/created")).expect("owner of the tempdir");
    let meta = std::fs::metadata(d.path()).unwrap();
    assert_eq!((me.uid, me.gid), (meta.uid(), meta.gid()));
    let s = RealAuditSink::new(
        d.path().join("audit.jsonl"),
        d.path().join("audit.reserve"),
        Some(me),
    );
    s.run_start(&start("r1")).unwrap();
    let m = std::fs::metadata(d.path().join("audit.jsonl")).unwrap();
    assert_eq!(m.mode() & 0o777, 0o600);
    assert_eq!(m.uid(), me.uid);
}

#[test]
fn first_intent_enospc_should_release_reserve_log_reserve_released_and_continue_When_reserve_present(
) {
    if let Some(dir) = fullfs::child_dir() {
        return enospc_child(&dir);
    }
    fullfs::run_child(
        "first_intent_enospc_should_release_reserve_log_reserve_released_and_continue_When_reserve_present",
        4096,
    );
}

fn enospc_child(dir: &Path) {
    use disk_cleanup_adapters::state::{StateDir, RESERVE_BYTES};
    let (sd, init) = StateDir::open(dir, 1000, &|_| Ok(1 << 30)).unwrap();
    assert!(init.reserves_created);
    assert_eq!(
        std::fs::metadata(sd.paths.audit_reserve()).unwrap().len(),
        RESERVE_BYTES
    );
    let s = RealAuditSink::new(sd.paths.audit(), sd.paths.audit_reserve(), None);
    s.run_start(&start("r1")).unwrap();
    fullfs::fill(dir);
    // A plain write now fails; the reserve is what lets the intent through.
    assert!(std::fs::write(dir.join("probe"), b"x").is_err());
    // Longer than the free tail of the last page, so the append really meets ENOSPC.
    let p = PathBuf::from(format!("/h/{}", "y".repeat(60_000)));
    s.intent(&Intent::of(&candidate(&p), "r1", 11)).unwrap();
    assert_eq!(
        std::fs::metadata(sd.paths.audit_reserve()).unwrap().len(),
        0
    );
    let types: Vec<String> = std::fs::read_to_string(sd.paths.audit())
        .unwrap()
        .lines()
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(types, ["run_start", "reserve_released", "intent"]);

    // With the reserve spent and the disk still full the append fails: the caller exits 2, nothing deleted.
    fullfs::fill(dir);
    let big = Intent::of(
        &candidate(&PathBuf::from(format!("/h/{}", "x".repeat(60_000)))),
        "r1",
        12,
    );
    let err = s.intent(&big).unwrap_err();
    assert!(matches!(err.raw_os_error(), Some(28 | 122)), "{err}");
    fullfs::ok("first_intent_enospc_should_release_reserve_log_reserve_released_and_continue_When_reserve_present");
}

#[test]
fn run_start_without_run_end_in_previous_run_should_print_the_warning_previous_run_did_not_finish_line_and_continue_When_any_run_starts(
) {
    let d = tempfile::tempdir().unwrap();
    let log = d.path().join("audit.jsonl");
    assert_eq!(unfinished_run(&log).unwrap(), None);
    let s = sink(d.path());
    s.run_start(&start("r1")).unwrap();
    s.run_end(&end("r1")).unwrap();
    assert_eq!(unfinished_run(&log).unwrap(), None);
    s.run_start(&start("r2")).unwrap();
    let w = unclean_run_warning(&log).unwrap().unwrap();
    assert!(
        w.text()
            .starts_with("warning: previous run r2 did not finish"),
        "{}",
        w.text()
    );
    assert!(
        w.text().contains("disk-cleanup audit verify")
            && w.text().contains("git -C <repo> worktree prune")
    );
    // A torn last line does not hide it, and a following run's end closes it.
    std::fs::OpenOptions::new().append(true).open(&log).unwrap();
    writeln!(
        std::fs::OpenOptions::new().append(true).open(&log).unwrap(),
        "{{\"type\":\"intent\",\"ru"
    )
    .unwrap();
    assert_eq!(unfinished_run(&log).unwrap().as_deref(), Some("r2"));
    s.run_end(&end("r2")).unwrap();
    assert_eq!(unfinished_run(&log).unwrap(), None);
}

#[test]
fn audit_should_preserve_existing_lines_byte_for_byte_and_open_append_only_When_second_run_appends()
{
    let d = tempfile::tempdir().unwrap();
    let a = sink(d.path());
    a.run_start(&start("r1")).unwrap();
    let before = std::fs::read(d.path().join("audit.jsonl")).unwrap();
    // A second writer with its own descriptor, then the first again: without O_APPEND the first would
    // overwrite from its own offset.
    let b = sink(d.path());
    b.run_start(&start("r2")).unwrap();
    a.run_end(&end("r1")).unwrap();
    b.run_end(&end("r2")).unwrap();
    let after = std::fs::read(d.path().join("audit.jsonl")).unwrap();
    assert!(after.starts_with(&before));
    assert_eq!(lines(d.path()).len(), 4);
}

#[test]
fn each_audit_line_should_be_a_single_write_all_and_valid_json_When_two_processes_append() {
    let d = tempfile::tempdir().unwrap();
    // Two writers with their own descriptors and no shared lock stand in for two processes.
    let handles: Vec<_> = ["a", "b"]
        .into_iter()
        .map(|run| {
            let dir = d.path().to_path_buf();
            std::thread::spawn(move || {
                let s = sink(&dir);
                let c = candidate(Path::new("/home/u/code/app/node_modules"));
                for _ in 0..500 {
                    s.intent(&Intent::of(&c, run, 1)).unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let v = lines(d.path());
    assert_eq!(v.len(), 1000);
    assert_eq!(v.iter().filter(|l| l["run_id"] == "a").count(), 500);
}

fn vm_hwm_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|r| r.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap_or(0)
}

#[test]
fn audit_and_plan_memory_should_be_bounded_When_100_000_items_planned() {
    let d = tempfile::tempdir().unwrap();
    let s = sink(d.path());
    s.run_start(&start("big")).unwrap();
    let before = vm_hwm_kib();
    let c = candidate(Path::new("/home/u/code/app/node_modules"));
    for i in 0..100_000 {
        s.intent(&Intent::of(&c, "big", i)).unwrap();
    }
    for _ in 0..200 {
        s.result(&result("big", Path::new("/home/u/code/app/node_modules")))
            .unwrap();
    }
    let grown = vm_hwm_kib().saturating_sub(before);
    assert!(grown < 128 * 1024, "VmHWM grew {grown} KiB");
    assert_eq!(lines(d.path()).len(), 100_201);
}

#[test]
fn audit_reader_should_ignore_unknown_fields_and_reject_unknown_schema_with_clear_error_When_schema_2_line_present(
) {
    let d = tempfile::tempdir().unwrap();
    let log = d.path().join("audit.jsonl");
    std::fs::write(
        &log,
        concat!(
            "{\"type\":\"run_start\",\"schema\":1,\"run_id\":\"r1\",\"future_field\":[1,2]}\n",
            "{\"type\":\"run_start\",\"schema\":2,\"run_id\":\"v2\"}\n",
        ),
    )
    .unwrap();
    assert_eq!(unfinished_run(&log).unwrap().as_deref(), Some("r1"));
    std::fs::write(
        &log,
        "{\"type\":\"run_start\",\"schema\":7,\"run_id\":\"x\"}\n",
    )
    .unwrap();
    let e = unfinished_run(&log).unwrap_err();
    assert_eq!(e.found, 7);
    assert_eq!(e.exit_code(), 2);
    let t = e.text();
    assert!(
        t.contains("unknown schema version (7)") && t.contains("Nothing was deleted"),
        "{t}"
    );
    assert!(t.contains("mv -- ") && t.contains("audit.jsonl.old"), "{t}");
    assert!(t.contains("audit verify --log"), "{t}");
}

#[test]
fn a_non_utf8_name_should_round_trip_through_json_with_path_hex_in_intent_and_result() {
    let d = tempfile::tempdir().unwrap();
    let name = PathBuf::from(OsString::from_vec(b"/home/u/a\xffb/node_modules".to_vec()));
    let s = sink(d.path());
    s.intent(&Intent::of(&candidate(&name), "r1", 1)).unwrap();
    s.result(&result("r1", &name)).unwrap();
    let ok = Path::new("/home/u/plain/node_modules");
    s.result(&result("r1", ok)).unwrap();
    let v = lines(d.path());
    assert_eq!(
        v[0]["path_hex"],
        "2f686f6d652f752f61ff622f6e6f64655f6d6f64756c6573"
    );
    assert_eq!(v[1]["path_hex"], v[0]["path_hex"]);
    assert!(v[2].get("path_hex").is_none());
}
