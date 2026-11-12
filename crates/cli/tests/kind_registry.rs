#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 2.3.4, Task 2.3.4b: the per-gate kind registry is one explicit table (`ci/safety/kinds-by-gate.txt`)
//! and these tests compare it with what the code registers, then with the binaries that ship.
//!
//! Owner decision of 2026-11-10: the default `--release` and `--profile dist` builds register no apply-capable
//! kind and refuse `--apply`; the Gate B build (`--features gate-b`) registers `caches`.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::Fixture;
use dunnage_adapters::RealDirOps;
use dunnage_cli::wiring::dirs::real_canon;
use dunnage_cli::wiring::kinds::{build_gate, KindCtx, KindRegistry, GATES};
use dunnage_core::roots::RootSet;
use dunnage_core::{Dirs, SelectorId};

struct Row {
    id: String,
    listed: char,
    apply: char,
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The explorer's hidden kind has a row but is not a `--kinds` id, so the comparisons with `listed()` and with
/// `kinds --json` leave it out and `the_explorer_selection_kind_...` checks it.
const HIDDEN: &str = "explorer-selection";

fn table() -> (char, Vec<Row>) {
    let (active, rows) = full_table();
    (
        active,
        rows.into_iter().filter(|r| r.id != HIDDEN).collect(),
    )
}

fn full_table() -> (char, Vec<Row>) {
    let text = std::fs::read_to_string(repo().join("ci/safety/kinds-by-gate.txt")).unwrap();
    let mut lines = text
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'));
    let first = lines.next().unwrap();
    let active = first
        .strip_prefix("active-gate ")
        .unwrap_or_else(|| panic!("the first line must be 'active-gate <letter>': {first}"))
        .chars()
        .next()
        .unwrap();
    let rows = lines
        .map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            assert_eq!(f.len(), 3, "{l}");
            Row {
                id: f[0].to_string(),
                listed: f[1].chars().next().unwrap(),
                apply: f[2].chars().next().unwrap(),
            }
        })
        .collect();
    (active, rows)
}

/// `GATE` when set (`scripts/gate.sh` exports it), else the table's `active-gate`.
fn gate_in_force() -> (char, bool) {
    match std::env::var("GATE") {
        // Gates E and F register no kind of their own (the explorer's `explorer-selection` is hidden and unnamed);
        // `scripts/gate.sh` builds them with gate-d, so the registry in force is Gate D's.
        Ok(g) if !g.is_empty() => (
            match g.chars().next().unwrap() {
                'E' | 'F' => 'D',
                c => c,
            },
            true,
        ),
        _ => (table().0, false),
    }
}

fn ctx_in(home: &Path) -> (RootSet, PathBuf) {
    let dirs = Dirs {
        home: home.to_path_buf(),
        state: home.join(".state"),
        cache: home.join(".cache-dc"),
        config: home.join(".config-dc"),
    };
    let roots = RootSet::resolve(&dirs, &[], None, home, &real_canon).unwrap();
    (roots, home.to_path_buf())
}

fn ids(v: &[SelectorId]) -> Vec<&'static str> {
    let mut v: Vec<&str> = v.iter().map(|s| s.id()).collect();
    v.sort_unstable();
    v
}

fn expected(rows: &[Row], gate: char, pick: fn(&Row) -> char) -> Vec<String> {
    let mut v: Vec<String> = rows
        .iter()
        .filter(|r| pick(r) <= gate)
        .map(|r| r.id.clone())
        .collect();
    v.sort();
    v
}

#[test]
fn the_registry_should_register_every_table_row_up_to_the_active_gate_and_nothing_above_it() {
    let tmp = tempfile::tempdir().unwrap();
    let (roots, root) = ctx_in(tmp.path());
    let ops = || RealDirOps::confined_to(&root);
    let ctx = KindCtx {
        roots,
        fs_root: root.clone(),
        ops: &ops,
        fixture_mode: false,
    };
    let (_, rows) = table();
    let (active, explicit) = gate_in_force();
    assert!(GATES.contains(&active), "unknown gate {active}");
    for gate in GATES {
        let reg = KindRegistry::build_for(gate, &ctx);
        let (listed, capable) = (ids(&reg.listed()), ids(&reg.apply_capable()));
        let want_listed = expected(&rows, gate, |r| r.listed);
        let want_capable = expected(&rows, gate, |r| r.apply);
        if gate <= active {
            // An empty slot at or below the active gate is a failure (anti-vacuity).
            assert_eq!(
                listed, want_listed,
                "gate {gate}: listed ids differ from the table"
            );
            assert_eq!(
                capable, want_capable,
                "gate {gate}: apply-capable ids differ from the table"
            );
        } else if explicit {
            // Under an explicit GATE no kind ships before its gate.
            for r in rows.iter().filter(|r| r.listed > active) {
                assert!(
                    !ids(&KindRegistry::build_for(active, &ctx).listed()).contains(&r.id.as_str()),
                    "{} is registered at gate {active} but belongs to {}",
                    r.id,
                    r.listed
                );
            }
        }
    }
}

#[test]
fn every_selector_should_have_a_row_in_the_table() {
    let (_, rows) = table();
    let mut have: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
    have.sort_unstable();
    let mut all: Vec<&str> = SelectorId::ALL.iter().map(|s| s.id()).collect();
    all.sort_unstable();
    assert_eq!(have, all);
    assert!(rows.iter().all(|r| r.listed <= r.apply));
}

#[test]
fn the_binary_should_list_and_apply_exactly_what_its_build_gate_registers() {
    let (_, rows) = table();
    let built = build_gate();
    let (gate, explicit) = gate_in_force();
    if explicit {
        assert_eq!(
            built,
            gate,
            "GATE={gate} needs a build with --features dunnage/gate-{}; scripts/gate.sh passes it",
            gate.to_ascii_lowercase()
        );
    }
    let f = Fixture::new();
    let out = f.fixture_cmd().args(["kinds", "--json"]).output().unwrap();
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let cell = |id: &str| -> String {
        doc["kinds"]
            .as_array()
            .unwrap()
            .iter()
            .find(|k| k["id"] == id)
            .unwrap_or_else(|| panic!("{id} missing"))["available"]
            .as_str()
            .unwrap()
            .to_string()
    };
    for r in &rows {
        let got = cell(&r.id);
        if r.listed > built {
            assert_eq!(
                got, "not available in this build",
                "{} at build gate {built}",
                r.id
            );
        } else if r.id == "cache-apt" && !is_root() {
            assert_eq!(got, "needs root", "cache-apt at build gate {built}");
        } else if r.apply > built {
            assert_eq!(got, "list only", "{} at build gate {built}", r.id);
        } else if r.id.starts_with("btrfs") && !is_root() {
            // Both btrfs kinds need root (design/ux.md S5); a user sees the gate, not "yes".
            assert_eq!(got, "needs root", "{} at build gate {built}", r.id);
        } else if r.id.starts_with("docker") {
            // The test shim puts a daemon-down `docker` ahead of the real one (Stories 4.1.2 to 4.1.4).
            assert!(
                got == "yes" || got == "daemon down",
                "{} at build gate {built}: {got}",
                r.id
            );
        } else {
            assert_eq!(got, "yes", "{} at build gate {built}", r.id);
        }
        if r.listed > built {
            let named = f.fixture_cmd().args(["--kinds", &r.id]).output().unwrap();
            assert_eq!(named.status.code(), Some(64), "{}", r.id);
        }
    }
    // The test-only kind is listed here only because this test binary is a `test-support` build.
    assert_eq!(cell("fixture-destructive"), "yes");
}

#[test]
fn the_explorer_selection_kind_should_be_registered_from_its_gate_only_and_hidden_everywhere_else()
{
    let (_, rows) = full_table();
    let row = rows.iter().find(|r| r.id == HIDDEN).unwrap();
    assert_eq!((row.listed, row.apply), ('D', 'D'));
    let tmp = tempfile::tempdir().unwrap();
    let (roots, root) = ctx_in(tmp.path());
    let ops = || RealDirOps::confined_to(&root);
    let ctx = KindCtx {
        roots,
        fs_root: root.clone(),
        ops: &ops,
        fixture_mode: false,
    };
    for gate in GATES {
        let reg = KindRegistry::build_for(gate, &ctx);
        assert_eq!(
            reg.hidden().contains(&SelectorId::ExplorerSelection),
            gate >= row.listed,
            "gate {gate}"
        );
        // Never a listed or apply-capable selector: `kinds` and `--apply` cannot reach it.
        assert!(!reg.listed().contains(&SelectorId::ExplorerSelection));
        assert!(!reg.apply_capable().contains(&SelectorId::ExplorerSelection));
    }
    assert_eq!(
        dunnage_cli::wiring::kinds::explorer_selection_registered(),
        build_gate() >= row.listed,
        "the binary registers it exactly when its build gate reached D"
    );
}

#[test]
fn naming_the_explorer_selection_kind_should_exit_64_and_kinds_should_not_list_it() {
    let f = Fixture::new();
    for args in [
        vec!["--kinds", HIDDEN],
        vec!["--kinds", "caches,explorer-selection"],
        vec!["--apply", "--kinds", HIDDEN],
    ] {
        let out = f.fixture_cmd().args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(64), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stderr).trim_end(),
            "error: explorer-selection is not a kind you can name; it exists only inside 'dunnage explore'. Nothing was deleted.",
            "{args:?}"
        );
    }
    for json in [true, false] {
        let args: Vec<&str> = if json {
            vec!["kinds", "--json"]
        } else {
            vec!["kinds"]
        };
        let out = f.fixture_cmd().args(&args).output().unwrap();
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains(HIDDEN),
            "kinds ({args:?}) lists the hidden kind"
        );
    }
}

fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|o| o.stdout == b"0\n")
}

// ---------------------------------------------------------------- the shipped profiles

fn cargo_build(target: &Path, args: &[&str]) -> PathBuf {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let out = Command::new(cargo)
        .current_dir(repo())
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env("CARGO_TARGET_DIR", target)
        .args([
            "build",
            "--locked",
            "-p",
            "dunnage",
            "--message-format=json",
        ])
        .args(args)
        .output()
        .expect("cargo");
    assert!(
        out.status.success(),
        "cargo build {args:?} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["reason"] == "compiler-artifact" && v["target"]["name"] == "dunnage")
        .filter(|v| {
            v["target"]["kind"]
                .as_array()
                .is_some_and(|k| k.iter().any(|x| x == "bin"))
        })
        .find_map(|v| v["executable"].as_str().map(PathBuf::from))
        .expect("the dunnage executable in cargo's output")
}

fn bytes_contain(bin: &Path, needle: &str) -> bool {
    let data = std::fs::read(bin).unwrap();
    data.windows(needle.len()).any(|w| w == needle.as_bytes())
}

fn run_in(f: &Fixture, bin: &Path, args: &[&str]) -> std::process::Output {
    f.command_for(bin).args(args).output().unwrap()
}

fn refuses_apply(bin: &Path, label: &str) {
    let f = Fixture::new();
    let cache = f.home().join(".npm/_cacache");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("blob"), vec![1u8; 4096]).unwrap();

    let kinds = run_in(&f, bin, &["kinds", "--json"]);
    let doc: serde_json::Value = serde_json::from_slice(&kinds.stdout).unwrap();
    for k in doc["kinds"].as_array().unwrap() {
        assert_ne!(
            k["available"], "yes",
            "{label}: {} is apply-capable",
            k["id"]
        );
    }
    for args in [vec!["--apply"], vec!["--apply", "--kinds", "caches"]] {
        let out = run_in(&f, bin, &args);
        assert_eq!(out.status.code(), Some(64), "{label} {args:?}");
        assert!(out.stdout.is_empty(), "{label}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("apply is not enabled in this build"),
            "{label}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert!(
        cache.join("blob").is_file(),
        "{label}: a refused apply deleted something"
    );
    assert!(
        !f.xdg("state").join("dunnage").exists(),
        "{label}: a refused apply wrote state"
    );
    // No test-only string survives into a shipped binary.
    for s in ["fixture-destructive", "DC_FAKE_STARVED"] {
        assert!(!bytes_contain(bin, s), "{label}: {s} is in the binary");
    }
}

#[test]
fn the_nogate_release_and_dist_builds_should_register_no_apply_kind_and_the_default_and_gate_b_builds_should_register_caches(
) {
    if std::env::var_os("DC_SKIP_PROFILE_BUILDS").is_some() {
        eprintln!("skipped: DC_SKIP_PROFILE_BUILDS is set");
        return;
    }
    // A private target dir: the cargo that runs this test holds a lock on its own.
    let target = repo().join("target/kind-registry-builds");
    // `--no-default-features` is the build with no gate feature: the default features include gate-b to gate-d.
    let nogate = ["--no-default-features", "--features", "explore"];
    let release = cargo_build(&target, &[&["--release"][..], &nogate].concat());
    refuses_apply(&release, "release");
    let dist = cargo_build(&target, &[&["--profile", "dist"][..], &nogate].concat());
    refuses_apply(&dist, "dist");

    // The default release build (the shipped one) registers every kind of the gates; no test-only string in it.
    let shipped = cargo_build(&target, &["--release"]);
    let f = Fixture::new();
    let kinds = run_in(&f, &shipped, &["kinds", "--json"]);
    let doc: serde_json::Value = serde_json::from_slice(&kinds.stdout).unwrap();
    for id in [
        "caches",
        "worktrees",
        "build-dirs",
        "docker",
        "steam",
        "btrfs",
    ] {
        let k = doc["kinds"]
            .as_array()
            .unwrap()
            .iter()
            .find(|k| k["id"] == id)
            .unwrap_or_else(|| panic!("{id} is not registered in the default build"));
        assert!(k.get("available").is_some(), "{id}");
    }
    for s in ["fixture-destructive", "DC_FAKE_STARVED"] {
        assert!(!bytes_contain(&shipped, s), "{s} is in the default build");
    }

    // The Gate B build registers caches and deletes them (inside the fixture).
    let gate_b = cargo_build(
        &target,
        &[
            "--release",
            "--no-default-features",
            "--features",
            "explore,gate-b",
        ],
    );
    let f = Fixture::new();
    let cache = f.home().join(".npm/_cacache");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("blob"), vec![1u8; 4096]).unwrap();
    let kinds = run_in(&f, &gate_b, &["kinds", "--json"]);
    let doc: serde_json::Value = serde_json::from_slice(&kinds.stdout).unwrap();
    let caches = doc["kinds"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["id"] == "caches")
        .unwrap();
    assert_eq!(caches["available"], "yes");
    let out = run_in(&f, &gate_b, &["--apply", "--kinds", "caches"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!cache.exists(), "the Gate B build removed the cache");
    assert!(!bytes_contain(&gate_b, "fixture-destructive"));
}
