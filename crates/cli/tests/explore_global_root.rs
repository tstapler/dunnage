#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Task 6.6.5l (review N2, ADR-006 O-8): the cleaner's global `--root` must not widen what the explorer may delete from;
//! adding a root is an edit of config.toml. `explore --root PATH` (the subcommand's own display alias) keeps working.

mod common;

#[cfg(feature = "explore")]
use common::{tree_manifest, Fixture};

#[cfg(feature = "explore")]
#[test]
fn a_global_root_before_explore_should_exit_64_naming_config_toml_with_an_empty_stdout() {
    let f = Fixture::new();
    let before = tree_manifest(f.root());
    let root = f.root().to_string_lossy().into_owned();
    for argv in [
        vec!["--root", root.as_str(), "explore", "--text"],
        vec!["--root", root.as_str(), "explore"],
        vec!["--root=/", "explore", "--json"],
    ] {
        let out = f.fixture_cmd().args(&argv).output().unwrap();
        assert_eq!(out.status.code(), Some(64), "{argv:?}");
        assert!(out.stdout.is_empty(), "{argv:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.starts_with("error:"), "{argv:?}: {err}");
        assert!(err.contains("config.toml"), "{argv:?}: {err}");
        assert!(err.contains("Nothing was deleted."), "{argv:?}: {err}");
    }
    assert_eq!(tree_manifest(f.root()), before, "the fixture changed");
    assert!(!f.xdg("state").join("dunnage").exists());
}

#[cfg(feature = "explore")]
#[test]
fn the_subcommand_s_own_root_should_still_scan_the_given_path() {
    let f = Fixture::new();
    let root = f.root().to_string_lossy().into_owned();
    let out = f
        .fixture_cmd()
        .args(["explore", "--text", "--root", root.as_str()])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!out.stdout.is_empty());
}

#[cfg(feature = "explore")]
#[test]
fn the_cleaner_s_global_root_should_still_work_without_explore() {
    let f = Fixture::new();
    let below_home = f.home().join("work");
    std::fs::create_dir_all(&below_home).unwrap();
    let out = f
        .fixture_cmd()
        .args(["--root", below_home.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
