#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 1.2.5, Task 1.2.5b: `dunnage scan` through the spawned binary. Parity is against the oracle the
//! old `metascope scan` produced (`crates/scan/tests/fixtures/parity`, Story 0.1.5); every run goes through
//! `fixture_cmd()` so HOME and the XDG directories are the fixture's.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use common::{tree_manifest, Fixture};

/// Wall-clock JSON keys; the same set the library parity test masks.
const CLOCK_KEYS: [&str; 3] = ["total_ms", "walk_ms", "age_secs"];

fn parity_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../scan/tests/fixtures/parity")
}

fn is_root() -> bool {
    let out = Command::new("id").arg("-u").output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim() == "0"
}

/// The generated parity tree under `<fixture>/scan/fx`; the mode-000 directory is restored on drop.
struct Tree {
    root: PathBuf,
}

impl Tree {
    fn new(f: &Fixture) -> Tree {
        let base = f.root().join("scan");
        fs::create_dir_all(&base).unwrap();
        let out = Command::new(parity_dir().join("make-fixture.sh"))
            .arg(base.join("fx"))
            .output()
            .unwrap();
        assert!(out.status.success(), "make-fixture.sh failed: {out:?}");
        Tree {
            root: fs::canonicalize(base.join("fx")).unwrap(),
        }
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::set_permissions(
            self.root.join("repoB/private"),
            fs::Permissions::from_mode(0o755),
        );
    }
}

fn scan(f: &Fixture, args: &[&str]) -> Output {
    f.fixture_cmd().arg("scan").args(args).output().unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn mask_json(json: &str) -> String {
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

/// The timings of the summary line are the only clock values in the table.
fn mask_table(text: &str) -> String {
    text.lines()
        .map(
            |line| match (line.find("; walk "), line.find(" unreadable")) {
                (Some(a), Some(b)) if a < b => {
                    let tail = &line[a..b];
                    let errors = tail.rsplit(", ").next().unwrap_or("");
                    format!(
                        "{}; walk <ms>, total <ms>, {errors}{}",
                        &line[..a],
                        &line[b..]
                    )
                }
                _ => line.to_string(),
            },
        )
        .collect::<Vec<_>>()
        .join("\n")
}

fn oracle(name: &str, root: &Path) -> String {
    let old_root = fs::read_to_string(parity_dir().join("expected.root")).unwrap();
    fs::read_to_string(parity_dir().join(name))
        .unwrap()
        .replace(old_root.trim(), &root.display().to_string())
}

const ORACLE_ARGS: [&str; 3] = ["--backend", "walker", "--no-cache"];

#[test]
fn scan_json_should_equal_the_oracle_When_run_on_the_parity_fixture() {
    if is_root() {
        return; // root reads the mode-000 directory, which changes the oracle's `errors`
    }
    let f = Fixture::new();
    let t = Tree::new(&f);
    let root = t.root.to_string_lossy().into_owned();
    // The oracle was captured with one thread, which fixes who owns the hardlinked inode.
    let out = scan(
        &f,
        &[
            &root,
            "--backend",
            "walker",
            "--no-cache",
            "--threads",
            "1",
            "--json",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        mask_json(&stdout(&out)),
        mask_json(&oracle("expected.json", &t.root))
    );
}

#[test]
fn scan_table_should_equal_the_oracle_When_run_on_the_parity_fixture() {
    if is_root() {
        return;
    }
    let f = Fixture::new();
    let t = Tree::new(&f);
    let root = t.root.to_string_lossy().into_owned();
    let out = scan(
        &f,
        &[&root, "--backend", "walker", "--no-cache", "--threads", "1"],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        mask_table(&stdout(&out)),
        mask_table(&oracle("expected.txt", &t.root))
    );
}

#[test]
fn scan_should_match_the_oracle_with_the_default_threads_too() {
    if is_root() {
        return;
    }
    let f = Fixture::new();
    let t = Tree::new(&f);
    let root = t.root.to_string_lossy().into_owned();
    let mut args = vec![root.as_str()];
    args.extend(ORACLE_ARGS);
    let out = scan(&f, &args);
    assert_eq!(
        mask_table(&stdout(&out)),
        mask_table(&oracle("expected.txt", &t.root))
    );
}

#[test]
fn comparison_should_fail_When_the_sort_key_is_size_instead_of_inodes() {
    if is_root() {
        return;
    }
    let f = Fixture::new();
    let t = Tree::new(&f);
    let root = t.root.to_string_lossy().into_owned();
    let mutant = scan(
        &f,
        &[
            &root,
            "--backend",
            "walker",
            "--no-cache",
            "--threads",
            "1",
            "--sort",
            "size",
            "--json",
        ],
    );
    assert_ne!(
        mask_json(&stdout(&mutant)),
        mask_json(&oracle("expected.json", &t.root)),
        "a size-sorted run must not equal the inode-sorted oracle"
    );
    // The family table of the fixture happens to order the same by size, the checkout table does not.
    let by = |sort: &str| {
        stdout(&scan(
            &f,
            &[
                &root,
                "--backend",
                "walker",
                "--no-cache",
                "--threads",
                "1",
                "--by",
                "checkout",
                "--sort",
                sort,
            ],
        ))
    };
    assert_ne!(mask_table(&by("size")), mask_table(&by("inodes")));
}

#[test]
fn scan_should_write_only_its_own_cache_file_and_nothing_in_the_state_directory_When_run() {
    if is_root() {
        return;
    }
    let f = Fixture::new();
    let t = Tree::new(&f);
    let root = t.root.to_string_lossy().into_owned();
    let home_before = tree_manifest(&f.home());
    let tree_before = tree_manifest(&t.root);
    let first = scan(&f, &[&root, "--backend", "walker", "--json"]);
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    let cache = f.xdg("cache").join("dunnage/scan.cache");
    assert!(cache.is_file(), "scan.cache missing at {}", cache.display());
    assert_eq!(
        fs::read_dir(f.xdg("cache").join("dunnage"))
            .unwrap()
            .count(),
        1
    );
    assert!(
        !f.xdg("state").join("dunnage").exists(),
        "state directory was created"
    );
    assert_eq!(fs::read_dir(f.xdg("state")).unwrap().count(), 0);
    assert_eq!(fs::read_dir(f.xdg("config")).unwrap().count(), 0);
    assert_eq!(tree_manifest(&f.home()), home_before);
    assert_eq!(tree_manifest(&t.root), tree_before);
    // The second run reads the cache it wrote.
    let second = scan(&f, &[&root, "--backend", "walker", "--json"]);
    let cached = stdout(&second)
        .lines()
        .find_map(|l| {
            l.trim()
                .strip_prefix("\"dirs_cached\": ")
                .map(|v| v.trim_end_matches(',').to_string())
        })
        .unwrap();
    assert_ne!(cached, "0", "the second run must hit the cache");
}

#[test]
fn scan_should_refuse_apply_and_the_cleaner_flags_When_given_them() {
    let f = Fixture::new();
    let out = scan(&f, &["--apply"]);
    assert_eq!(out.status.code(), Some(64), "{}", stderr(&out));
    let out = f
        .fixture_cmd()
        .args(["scan", "--kinds", "caches"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(64));
    assert!(!f.xdg("state").join("dunnage").exists());
}

#[test]
fn scan_should_exit_64_with_a_usage_message_When_a_root_does_not_exist() {
    let f = Fixture::new();
    let missing = f.root().join("nope");
    let out = scan(&f, &[missing.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(64));
    assert!(stdout(&out).is_empty());
    assert!(
        stderr(&out).contains("not a directory that exists"),
        "{}",
        stderr(&out)
    );
    assert!(!f.xdg("cache").join("dunnage").exists());
    // A file is not a root either.
    let file = f.root().join("file");
    fs::write(&file, "x").unwrap();
    assert_eq!(scan(&f, &[file.to_str().unwrap()]).status.code(), Some(64));
}

#[test]
fn scan_should_exit_2_When_the_scan_itself_fails() {
    if is_root() {
        return; // root may read the btrfs tree
    }
    let f = Fixture::new();
    let root = f.home();
    let out = scan(
        &f,
        &[root.to_str().unwrap(), "--backend", "btrfs", "--no-cache"],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).starts_with("error:"), "{}", stderr(&out));
    assert!(stdout(&out).is_empty());
}

#[test]
fn scan_should_say_the_two_things_in_help_and_list_no_status_subcommand() {
    let f = Fixture::new();
    let out = scan(&f, &["--help"]);
    assert_eq!(out.status.code(), Some(0));
    let help = stdout(&out);
    assert!(
        help.contains("scan.cache") && help.contains("cold"),
        "{help}"
    );
    assert!(
        help.contains("not the schema_version 1 agent contract"),
        "{help}"
    );
    assert!(help.contains("`dunnage status`"), "{help}");
    assert!(
        !help.contains("Commands:"),
        "scan lists a subcommand: {help}"
    );
    assert!(
        !help.contains("--include-ignored"),
        "the no-op flag is hidden"
    );
    let ok = scan(&f, &["--include-ignored", "--help"]);
    assert_eq!(ok.status.code(), Some(0));
}

#[test]
fn scan_should_accept_the_hidden_no_op_include_ignored_flag() {
    let f = Fixture::new();
    let root = f.home();
    let out = scan(
        &f,
        &[
            root.to_str().unwrap(),
            "--include-ignored",
            "--backend",
            "walker",
            "--no-cache",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
}

#[test]
fn scan_should_exit_128_plus_the_signal_When_interrupted_or_terminated() {
    // A cache file that is a FIFO with no writer blocks the scan before the report, so the signal is
    // the only way out; the test waits until the handlers are installed so the signal cannot arrive early.
    for (sig, name, code) in [(2, "INT", 130), (15, "TERM", 143)] {
        let f = Fixture::new();
        let fifo = f.root().join("blocker");
        assert!(Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success());
        let root = f.home();
        let child = f
            .fixture_cmd()
            .args(["scan", "--backend", "walker", "--cache-file"])
            .arg(&fifo)
            .arg(&root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let caught = |pid: u32| {
            let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
            status
                .lines()
                .find_map(|l| l.strip_prefix("SigCgt:"))
                .and_then(|v| u64::from_str_radix(v.trim(), 16).ok())
                .is_some_and(|m| m & (1 << (sig - 1)) != 0)
        };
        let end = Instant::now() + Duration::from_secs(10);
        while !caught(child.id()) {
            assert!(Instant::now() < end, "handlers never installed");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(Command::new("kill")
            .args([format!("-{name}"), child.id().to_string()])
            .status()
            .unwrap()
            .success());
        let out = child.wait_with_output().unwrap();
        assert_eq!(out.status.code(), Some(code), "{}", stderr(&out));
        assert!(
            stderr(&out).contains("INTERRUPTED during scan"),
            "{}",
            stderr(&out)
        );
        assert!(stdout(&out).is_empty());
    }
}
