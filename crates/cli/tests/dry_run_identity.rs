#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 2.3.3 (moved from Story 2.1.3): a dry run leaves the fixture HOME exactly as it was. The whole home
//! is hashed (names, sizes, mode, mtime) before and after the full executor ran in `Mode::Plan` through the
//! spawned binary, with `XDG_STATE_HOME`, `XDG_CACHE_HOME` and `XDG_CONFIG_HOME` pointed at `<root>/xdg/*`,
//! outside the hashed home (Story 1.1.6, consistency C2). Runs with or without the `gate-b` feature: plan mode
//! does not depend on which kinds may delete.

mod common;

use common::{aged_dir, text_of, tree_hash, Fixture};

fn populate(f: &Fixture) {
    let home = f.home();
    for (rel, bytes) in [
        (".npm/_cacache/sub/blob", 20_000),
        (".cargo/registry/src/crate-1/lib.rs", 5_000),
        ("Documents/notes.txt", 100),
        (".ssh/config", 50),
    ] {
        let p = home.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, vec![6u8; bytes]).unwrap();
    }
    aged_dir(&home.join("code/proj/stale-a"), 9_000, 90);
    aged_dir(&home.join("code/proj/stale-b"), 100, 1);
}

#[test]
fn the_hash_of_the_fixture_home_should_be_unchanged_and_the_state_should_exist_under_xdg_When_the_full_executor_plans(
) {
    let f = Fixture::new();
    populate(&f);
    let h = tree_hash(&f.home());
    assert!(!h.is_empty());
    for args in [
        vec![],
        vec!["--json"],
        vec!["--kinds", "caches,fixture-destructive"],
        vec![
            "--kinds",
            "fixture-destructive",
            "--min-age",
            "60d",
            "--json",
        ],
        vec!["--rescan", "--no-progress"],
        vec!["--verbose"],
        vec!["status"],
        vec!["kinds"],
    ] {
        let out = f
            .fixture_cmd()
            .args(&args)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(
            matches!(out.status.code(), Some(0..=2)),
            "{args:?}: {}",
            text_of(&out).1
        );
        assert_eq!(tree_hash(&f.home()), h, "{args:?} changed the fixture home");
    }

    // The test cannot pass because nothing was written at all: the plan was saved under xdg.
    let state = f.xdg("state").join("dunnage");
    assert!(
        state.join("last_plan").is_file(),
        "no last_plan under xdg/state"
    );
    assert!(state.join("outcome.jsonl").is_file());
    assert!(
        !state.join("audit.jsonl").exists(),
        "a dry run opened the audit log"
    );

    // The walker cache belongs to `scan` here: no registered kind walks, so a plan run writes none (Story 1.3.3
    // finding). `scan` must write it under xdg/cache and still leave the home alone.
    let out = f
        .fixture_cmd()
        .args(["scan", "--backend", "walker", "--json"])
        .arg(f.home().join("code"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text_of(&out).1);
    assert!(
        f.xdg("cache").join("dunnage/scan.cache").is_file(),
        "no walker cache under xdg/cache"
    );
    assert_eq!(tree_hash(&f.home()), h, "scan changed the fixture home");
}
