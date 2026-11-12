#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    non_snake_case,
    clippy::format_push_string
)]
//! Story 4.2.3: `steam list` end to end on fixture Steam libraries under the fixture HOME. Read-only: the
//! whole fixture tree must be byte-identical after every run. Never touches the owner's real libraries.

mod common;

use std::fs;
use std::path::Path;

use common::{tree_manifest, Fixture};

const GIB: u64 = 1_073_741_824;

fn manifest(lib: &Path, appid: u64, name: &str, bytes: u64, last_played: u64) {
    let body = format!(
        "\"AppState\"\n{{\n\t\"appid\"\t\t\"{appid}\"\n\t\"name\"\t\t\"{name}\"\n\t\"installdir\"\t\t\"{name}\"\n\t\"SizeOnDisk\"\t\t\"{bytes}\"\n\t\"LastPlayed\"\t\t\"{last_played}\"\n}}\n"
    );
    fs::write(lib.join(format!("steamapps/appmanifest_{appid}.acf")), body).unwrap();
}

fn steam_fixture(f: &Fixture, extra_lib: Option<&Path>) -> std::path::PathBuf {
    let root = f.home().join(".local/share/Steam");
    fs::create_dir_all(root.join("steamapps")).unwrap();
    let mut vdf = format!(
        "\"libraryfolders\"\n{{\n\"0\"\n{{\n\"path\"\t\"{}\"\n}}\n",
        root.display()
    );
    if let Some(l) = extra_lib {
        vdf.push_str(&format!("\"1\"\n{{\n\"path\"\t\"{}\"\n}}\n", l.display()));
    }
    vdf.push_str("}\n");
    fs::write(root.join("steamapps/libraryfolders.vdf"), vdf).unwrap();
    root
}

fn run(f: &Fixture, args: &[&str]) -> (Option<i32>, String, String) {
    let o = f.fixture_cmd().args(args).output().unwrap();
    (
        o.status.code(),
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the steam kind is Linux only (Steam keeps its libraries elsewhere on macOS)"
)]
fn steam_list_should_rank_ghostrunner_hide_proton_and_leave_the_home_unchanged() {
    let f = Fixture::new();
    let root = steam_fixture(&f, None);
    // LastPlayed far in the past keeps the score large without depending on today's date.
    manifest(&root, 1, "Ghostrunner", 12 * GIB, 100_000);
    manifest(&root, 2, "Proton 9.0", GIB, 0);
    let before = tree_manifest(&f.home());
    let (code, out, _) = run(&f, &["steam", "list", "--sort", "score"]);
    assert_eq!(code, Some(0), "{out}");
    assert_eq!(
        out.lines().next(),
        Some("read-only: nothing is deleted by this command")
    );
    assert!(
        out.lines()
            .any(|l| l.contains("12.0 GiB") && l.ends_with("Ghostrunner")),
        "{out}"
    );
    assert!(!out.contains("Proton"), "{out}");
    assert!(out.contains("'steam-clean tidy' has been retired"), "{out}");
    assert_eq!(tree_manifest(&f.home()), before, "steam list changed HOME");
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the steam kind is Linux only (Steam keeps its libraries elsewhere on macOS)"
)]
fn steam_list_should_note_an_unmounted_library_and_exit_0_and_exit_1_when_none_is_readable() {
    let f = Fixture::new();
    let gone = f.root().join("gone-lib");
    let root = steam_fixture(&f, Some(&gone));
    manifest(&root, 1, "Ghostrunner", 12 * GIB, 100_000);
    let (code, out, _) = run(&f, &["steam", "list"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(
        out.contains(&format!(
            "Steam library {} not mounted; its games are not listed",
            gone.display()
        )),
        "{out}"
    );
    assert!(out.contains("Ghostrunner"), "{out}");

    let g = Fixture::new();
    let root = steam_fixture(&g, None);
    fs::remove_dir_all(root.join("steamapps")).unwrap();
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("steamapps"), "").unwrap();
    let (code, out, err) = run(&g, &["steam", "list"]);
    assert_eq!(code, Some(1), "{out}{err}");
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the steam kind is Linux only (Steam keeps its libraries elsewhere on macOS)"
)]
fn steam_list_without_steam_should_exit_1_and_json_should_be_one_document() {
    let f = Fixture::new();
    let (code, out, err) = run(&f, &["steam", "list"]);
    assert_eq!(code, Some(1), "{out}");
    assert!(err.contains("Steam not found"), "{err}");
    let root = steam_fixture(&f, None);
    manifest(&root, 1, "Ghostrunner", 12 * GIB, 100_000);
    let (code, out, _) = run(&f, &["steam", "list", "--json", "--min-gib", "10"]);
    assert_eq!(code, Some(0));
    let d: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(d["mode"], "steam-list");
    assert_eq!(d["games"][0]["name"], "Ghostrunner");
    assert_eq!(d["read_only"], true);
}
