#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]

//! Parity: the folded-in scanner reproduces the scan of the old `metascope scan --backend walker --no-cache
//! --threads 1 --json` binary (built from metascope 3d36581) on the generated fixture (Story 0.1.5, ADR-007).
//!
//! `expected.json` was captured with the old binary before the import (scripts/capture-old-scan.sh). The only
//! differences allowed are the fields that depend on the clock or the host, named in `CLOCK_KEYS`, and the
//! absolute fixture root, which is substituted. Everything else must match byte for byte.

use disk_cleanup_scan::scan::{scan, BackendChoice, ScanOptions, ScanReport};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// JSON keys whose values change from run to run (wall-clock durations, age relative to now).
const CLOCK_KEYS: [&str; 3] = ["total_ms", "walk_ms", "age_secs"];

fn parity_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/parity")
}

/// A fixture tree under a fresh temp directory, removed (after restoring the mode-000 directory) on drop.
struct Fixture {
    base: PathBuf,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("dc-scan-parity-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let out = Command::new(parity_dir().join("make-fixture.sh"))
            .arg(base.join("fx"))
            .output()
            .unwrap();
        assert!(out.status.success(), "make-fixture.sh failed: {out:?}");
        let root = fs::canonicalize(base.join("fx")).unwrap();
        Fixture { base, root }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(
            self.root.join("repoB/private"),
            fs::Permissions::from_mode(0o755),
        );
        if self.base.starts_with(std::env::temp_dir()) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }
}

/// The scan the old CLI performed for the oracle command, serialised the way it printed it.
fn run_scan(root: &Path) -> ScanReport {
    let mut opts = ScanOptions::new(vec![root.to_path_buf()]);
    opts.backend = BackendChoice::Walker;
    opts.max_duration = Some(Duration::from_secs(120));
    opts.threads = 1;
    let mut report = scan(&opts).unwrap();
    // The old CLI sorted both tables by inodes, descending, with a stable sort.
    report.families.sort_by_key(|f| std::cmp::Reverse(f.inodes));
    report.groups.sort_by_key(|g| std::cmp::Reverse(g.inodes));
    report
}

fn mask_clock_fields(json: &str) -> String {
    json.lines()
        .map(|line| {
            let t = line.trim_start();
            match CLOCK_KEYS
                .iter()
                .find(|k| t.starts_with(&format!("\"{k}\":")))
            {
                Some(k) => format!("{}\"{k}\": <clock>", &line[..line.len() - t.len()]),
                None => line.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Compares a report with the oracle; `Err` carries the first differing line.
fn compare_with_oracle(report: &ScanReport, root: &Path) -> Result<(), String> {
    let dir = parity_dir();
    let old_root = fs::read_to_string(dir.join("expected.root")).unwrap();
    let expected = fs::read_to_string(dir.join("expected.json"))
        .unwrap()
        .replace(old_root.trim(), &root.display().to_string());
    let actual = serde_json::to_string_pretty(report).unwrap() + "\n";
    let (e, a) = (mask_clock_fields(&expected), mask_clock_fields(&actual));
    if e == a {
        return Ok(());
    }
    let line = e
        .lines()
        .zip(a.lines())
        .position(|(x, y)| x != y)
        .unwrap_or_else(|| e.lines().count().min(a.lines().count()));
    Err(format!(
        "line {}: expected {:?}, got {:?}",
        line + 1,
        e.lines().nth(line),
        a.lines().nth(line)
    ))
}

fn skip_as_root() -> bool {
    // The mode-000 directory is readable by root, which changes the `errors` count the oracle recorded.
    rustix::process::geteuid().is_root()
}

#[test]
fn scan_crate_should_build_as_a_workspace_member_and_reproduce_the_old_metascope_scan_output_on_the_parity_fixture_When_cargo_build_and_test(
) {
    if skip_as_root() {
        eprintln!("skipped: running as root");
        return;
    }
    let fx = Fixture::new();
    let report = run_scan(&fx.root);
    if let Err(diff) = compare_with_oracle(&report, &fx.root) {
        panic!("scan output differs from the old metascope oracle at {diff}");
    }
}

#[test]
fn fixture_should_match_its_recorded_checksum_list_When_regenerated() {
    if skip_as_root() {
        return;
    }
    let fx = Fixture::new();
    let out = Command::new(parity_dir().join("fixture-manifest.sh"))
        .arg(&fx.root)
        .output()
        .unwrap();
    let expected = fs::read_to_string(parity_dir().join("fixture.checksums")).unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), expected);
}

#[test]
fn comparison_should_fail_When_a_build_root_is_dropped() {
    if skip_as_root() {
        return;
    }
    let fx = Fixture::new();
    let mut report = run_scan(&fx.root);
    assert!(report.build_roots.pop().is_some());
    assert!(compare_with_oracle(&report, &fx.root).is_err());
}

#[test]
fn comparison_should_fail_When_a_lockfile_hash_changes() {
    if skip_as_root() {
        return;
    }
    let fx = Fixture::new();
    let mut report = run_scan(&fx.root);
    let b = report
        .build_roots
        .iter_mut()
        .find(|b| b.lockfile_hash.is_some())
        .expect("the fixture has build roots with lockfiles");
    b.lockfile_hash = b.lockfile_hash.map(|h| h ^ 1);
    assert!(compare_with_oracle(&report, &fx.root).is_err());
}
