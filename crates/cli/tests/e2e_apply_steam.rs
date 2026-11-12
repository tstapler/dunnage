#![cfg(feature = "gate-d")]
#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 4.2.2: Gate D, `--apply --kinds steam` end to end through the spawned binary under `fixture_cmd()`.
//! Run with `GATE=D cargo test --workspace --features dunnage/gate-d`; a build without the feature has no
//! `steam` kind, so this file is compiled only with it.
//!
//! Every library, prefix and game directory is a generated fixture below the fixture root, and the fixture's
//! own `proc/` stands in for `/proc` (`DC_TEST_ROOT`), so the developer's running Steam never matters.

mod common;

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, SystemTime};

use common::{extract_plan_id, next_commands, run_line, text_of, tree_manifest, Fixture};

struct World {
    f: Fixture,
    libs: Vec<PathBuf>,
}

fn plain(f: &Fixture, args: &[&str]) -> Output {
    f.fixture_cmd()
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn age(p: &Path, days: u64) {
    fs::File::open(p)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(days * 86_400))
        .unwrap();
}

fn age_tree(dir: &Path, days: u64) {
    for e in fs::read_dir(dir).unwrap().flatten() {
        if e.file_type().unwrap().is_dir() {
            age_tree(&e.path(), days);
        } else {
            age(&e.path(), days);
        }
    }
    age(dir, days);
}

fn set_proc(f: &Fixture, steam_running: bool) {
    let proc = f.root().join("proc");
    let _ = fs::remove_dir_all(&proc);
    fs::create_dir_all(proc.join("1")).unwrap();
    fs::write(proc.join("1/comm"), "init\n").unwrap();
    if steam_running {
        fs::create_dir_all(proc.join("2")).unwrap();
        fs::write(proc.join("2/comm"), "steam\n").unwrap();
    }
}

fn manifest(lib: &Path, appid: u32, installdir: &str) {
    let body = format!(
        "\"AppState\"\n{{\n\t\"appid\"\t\t\"{appid}\"\n\t\"name\"\t\t\"Game {appid}\"\n\t\"installdir\"\t\t\"{installdir}\"\n\t\"StateFlags\"\t\t\"4\"\n}}\n"
    );
    fs::write(lib.join(format!("steamapps/appmanifest_{appid}.acf")), body).unwrap();
}

fn entry(lib: &Path, sub: &str, name: &str, bytes: usize, days: u64) -> PathBuf {
    let dir = lib.join("steamapps").join(sub).join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("f"), vec![3u8; bytes]).unwrap();
    age_tree(&dir, days);
    dir
}

/// Steam root `~/.local/share/Steam` plus `lib1`; installed: 620 (lib0) and `MovedGame` (lib1). On disk: an
/// orphan prefix and shader cache `99999` (5 MiB), the kept `620` ones, remnant `OldGame`, and `Portal 2`.
fn world() -> World {
    let f = Fixture::new();
    set_proc(&f, false);
    let root = f.home().join(".local/share/Steam");
    let lib1 = f.root().join("lib1");
    let libs = vec![root.clone(), lib1];
    for l in &libs {
        fs::create_dir_all(l.join("steamapps")).unwrap();
    }
    let mut vdf = String::from("\"libraryfolders\"\n{\n");
    for (i, l) in libs.iter().enumerate() {
        write!(
            vdf,
            "\t\"{i}\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t\t\"label\"\t\t\"\"\n\t\t\"apps\"\n\t\t{{\n\t\t}}\n\t}}\n",
            l.display()
        )
        .unwrap();
    }
    vdf.push_str("}\n");
    fs::write(root.join("steamapps/libraryfolders.vdf"), vdf).unwrap();
    manifest(&libs[0], 620, "Portal 2");
    manifest(&libs[1], 1_091_500, "MovedGame");
    entry(&libs[0], "compatdata", "620", 1000, 90);
    entry(&libs[0], "compatdata", "99999", 5 << 20, 90);
    entry(&libs[0], "shadercache", "99999", 20_000, 90);
    entry(&libs[0], "common", "Portal 2", 1000, 90);
    entry(&libs[0], "common", "MovedGame", 1000, 90);
    entry(&libs[0], "common", "OldGame", 20_000, 90);
    World { f, libs }
}

/// Everything below the Steam libraries (the tool's own state files change with every run).
fn steam_state(w: &World) -> std::collections::BTreeMap<PathBuf, String> {
    w.libs.iter().flat_map(|l| tree_manifest(l)).collect()
}

fn orphan_prefix(w: &World) -> PathBuf {
    w.libs[0].join("steamapps/compatdata/99999")
}

fn dry(f: &Fixture, extra: &[&str]) -> Output {
    let mut args = vec!["--kinds", "steam"];
    args.extend_from_slice(extra);
    plain(f, &args)
}

fn refused(out: &Output, needle: &str) {
    let (stdout, stderr) = text_of(out);
    assert_eq!(out.status.code(), Some(64), "{stderr}");
    assert!(stdout.is_empty(), "stdout must be empty: {stdout}");
    assert!(
        stderr.to_lowercase().contains(&needle.to_lowercase()),
        "stderr lacks {needle:?}: {stderr}"
    );
}

fn with_id(out: &Output) -> String {
    next_commands(&text_of(out).0)
        .into_iter()
        .find(|c| c.contains("--plan-id"))
        .unwrap_or_else(|| panic!("no Next line carries --plan-id:\n{}", text_of(out).0))
}

// ------------------------------------------------------------------ registered at Gate D

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the steam kind is Linux only (Steam keeps its libraries elsewhere on macOS)"
)]
fn kinds_should_list_steam_as_a_destructive_kind_that_can_delete_When_the_gate_d_build_runs() {
    let w = world();
    let out = plain(&w.f, &["kinds", "--json"]);
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let row = doc["kinds"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["id"] == "steam")
        .expect("steam is listed");
    assert_eq!(row["available"], "yes");
    assert_eq!(row["tier"], "destructive");
}

// ------------------------------------------------------------------ the destructive-kind gate

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the steam kind is Linux only (Steam keeps its libraries elsewhere on macOS)"
)]
fn safety_steam_apply_needs_plan_id() {
    let w = world();
    let before = steam_state(&w);

    refused(
        &plain(&w.f, &["--apply", "--kinds", "steam"]),
        "need --plan-id",
    );
    assert_eq!(steam_state(&w), before);

    let out = dry(&w.f, &["--json"]);
    let id = extract_plan_id(&text_of(&out).0).expect("plan id");
    let mut digits: Vec<char> = id.chars().collect();
    digits[0] = if digits[0] == '0' { '1' } else { '0' };
    let altered: String = digits.into_iter().collect();
    refused(
        &plain(
            &w.f,
            &["--apply", "--kinds", "steam", "--plan-id", &altered],
        ),
        "no dry run with plan-id",
    );
    assert_eq!(steam_state(&w), before);
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the steam kind is Linux only (Steam keeps its libraries elsewhere on macOS)"
)]
fn apply_should_remove_the_orphans_and_log_steam_kinds_When_the_pasted_line_is_run() {
    let w = world();
    let out = dry(&w.f, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text_of(&out).1);
    let text = text_of(&out).0;
    assert!(text.contains("no appmanifest in 2 libraries"), "{text}");
    let line = with_id(&out);

    let applied = run_line(&w.f, &line);

    assert_eq!(
        applied.status.code(),
        Some(0),
        "{line}: {}",
        text_of(&applied).1
    );
    for gone in [
        orphan_prefix(&w),
        w.libs[0].join("steamapps/shadercache/99999"),
        w.libs[0].join("steamapps/common/OldGame"),
    ] {
        assert!(!gone.exists(), "{} survived", gone.display());
    }
    for kept in [
        w.libs[0].join("steamapps/compatdata/620/f"),
        w.libs[0].join("steamapps/common/Portal 2/f"),
        w.libs[0].join("steamapps/common/MovedGame/f"),
        w.libs[0].join("steamapps/appmanifest_620.acf"),
    ] {
        assert!(kept.exists(), "{} was removed", kept.display());
    }
    let audit = fs::read_to_string(w.f.xdg("state").join("dunnage/audit.jsonl")).unwrap();
    for kind in ["steam-prefix", "steam-shader-cache", "steam-remnant"] {
        assert!(
            audit.contains(&format!("\"kind\":\"{kind}\"")),
            "no {kind} in the audit log:\n{audit}"
        );
    }
    let run_start: serde_json::Value = audit
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["type"] == "run_start")
        .unwrap();
    let libs: Vec<&str> = run_start["roots"]["steam_libraries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(libs.len(), 2, "{run_start}");
    // every logged deletion lies under a library the run recorded
    let verify = plain(&w.f, &["audit", "verify"]);
    assert_eq!(verify.status.code(), Some(0), "{:?}", text_of(&verify));
}

// ------------------------------------------------------------------ the pasted line with the flag

fn prefix_with_save(w: &World) {
    let p = orphan_prefix(w);
    let d = p.join("pfx/drive_c/users/steamuser/Documents");
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("save.dat"), "x").unwrap();
    age_tree(&p, 90);
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the steam kind is Linux only (Steam keeps its libraries elsewhere on macOS)"
)]
fn a_pasted_next_line_should_carry_steam_saves_discard_and_refuse_the_line_without_it() {
    let w = world();
    prefix_with_save(&w);

    let plan = dry(&w.f, &["--steam-saves", "discard"]);
    assert_eq!(plan.status.code(), Some(0), "{}", text_of(&plan).1);
    let line = with_id(&plan);
    assert!(line.contains("--steam-saves discard"), "{line}");

    let without = line.replace(" --steam-saves discard", "");
    let before = steam_state(&w);
    refused(&run_line(&w.f, &without), "changed");
    assert_eq!(steam_state(&w), before);

    let out = run_line(&w.f, &line);
    assert_eq!(out.status.code(), Some(0), "{line}: {}", text_of(&out).1);
    assert!(!orphan_prefix(&w).exists());
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the steam kind is Linux only (Steam keeps its libraries elsewhere on macOS)"
)]
fn a_prefix_with_saves_should_stay_and_say_so_When_steam_saves_is_not_given() {
    let w = world();
    prefix_with_save(&w);

    let plan = dry(&w.f, &[]);
    let text = text_of(&plan).0;
    assert!(text.contains("prefix holds files in Documents"), "{text}");
    let out = run_line(&w.f, &with_id(&plan));
    assert_eq!(out.status.code(), Some(0), "{}", text_of(&out).1);
    assert!(orphan_prefix(&w)
        .join("pfx/drive_c/users/steamuser/Documents/save.dat")
        .exists());
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the steam kind is Linux only (Steam keeps its libraries elsewhere on macOS)"
)]
fn steam_saves_should_exit_64_When_steam_is_not_in_kinds() {
    let w = world();
    let out = plain(&w.f, &["--kinds", "caches", "--steam-saves", "discard"]);
    refused(&out, "--steam-saves only applies to steam");
}

// ------------------------------------------------------------------ whole-kind skips

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the steam kind is Linux only (Steam keeps its libraries elsewhere on macOS)"
)]
fn plan_should_say_steam_is_running_and_apply_should_end_without_deleting_When_steam_runs() {
    let w = world();
    let out = dry(&w.f, &["--json"]);
    let id = extract_plan_id(&text_of(&out).0).expect("plan id");
    set_proc(&w.f, true);
    let before = steam_state(&w);

    let plan = dry(&w.f, &[]);
    assert!(
        text_of(&plan)
            .0
            .contains("steam: SKIPPED (Steam is running or downloading)")
            || text_of(&plan)
                .1
                .contains("steam: SKIPPED (Steam is running or downloading)"),
        "{:?}",
        text_of(&plan)
    );
    let applied = plain(&w.f, &["--apply", "--kinds", "steam", "--plan-id", &id]);

    assert_ne!(applied.status.code(), Some(0), "{:?}", text_of(&applied));
    assert_eq!(steam_state(&w), before);
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the steam kind is Linux only (Steam keeps its libraries elsewhere on macOS)"
)]
fn plan_should_name_the_library_and_apply_should_delete_nothing_When_a_library_is_unmounted() {
    let w = world();
    fs::remove_dir_all(&w.libs[1]).unwrap();
    let before_lib0 = steam_state(&w);

    let plan = dry(&w.f, &[]);

    // the notes block wraps long lines
    let all = format!("{}{}", text_of(&plan).0, text_of(&plan).1)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        all.contains(&format!(
            "steam: SKIPPED (Steam library {} not mounted)",
            w.libs[1].display()
        )),
        "{all}"
    );
    let applied = plain(
        &w.f,
        &["--apply", "--kinds", "steam", "--plan-id", "deadbeef"],
    );
    assert_ne!(applied.status.code(), Some(0));
    assert_eq!(steam_state(&w), before_lib0);
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the steam kind is Linux only (Steam keeps its libraries elsewhere on macOS)"
)]
fn the_default_kinds_apply_should_never_touch_steam_data() {
    let w = world();
    let before = steam_state(&w);
    let out = plain(&w.f, &["--apply"]);
    assert_eq!(out.status.code(), Some(0), "{:?}", text_of(&out));
    assert_eq!(steam_state(&w), before);
}
