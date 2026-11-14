#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The `--text` explorer shows the cargo-aware kinds: a crate below `CARGO_HOME` is a possible `cargo-home`
//! item, a directory that holds some is `[cargo-home? inside]`, a part of a cargo target is a possible
//! `build-dirs` item, and the `Next:` line suggests the dry run of the kind.

mod explore_fixture;

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use dunnage_cli::explore::text::{run_text, ExploreEnv};
use dunnage_core::explore::msg::{NodeEvent, ScanMode, ScanMsg, ScanSummary, NO_PARENT};
use dunnage_core::explore::testing::{DirInfo, FakeScanSource, FakeTermSignals};
use explore_fixture::{btrfs, roots, stats, GIB};

fn dir(id: u64, parent: u64, name: &str, cargo: bool, subdirs: u32) -> NodeEvent {
    NodeEvent::Dir {
        id,
        parent,
        name: OsString::from(name),
        dev: 1,
        info: DirInfo {
            stats: stats(GIB, 1000),
            mtime: 1_700_000_000 + id as i64,
            has_cargo_toml: cargo,
            ..DirInfo::default()
        },
        subdirs,
        from_cache: false,
        painted: false,
    }
}

fn run(root: &str, events: Vec<NodeEvent>) -> String {
    let script = vec![
        ScanMsg::Batch {
            generation: 0,
            mode: ScanMode::Fresh,
            events,
        },
        ScanMsg::Finished(ScanSummary {
            generation: 0,
            ..ScanSummary::default()
        }),
    ];
    let mut e = ExploreEnv::new(
        Arc::new(FakeScanSource::new(script)),
        Arc::new(FakeTermSignals::never()),
        vec![PathBuf::from(root)],
        roots(),
    );
    e.headroom = Ok(btrfs());
    e.width = 120;
    e.depth = 4;
    e.top = 0;
    e.home = Some(PathBuf::from("/h"));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(run_text(&e, &mut out, &mut err), 0);
    String::from_utf8(out).unwrap()
}

#[test]
fn a_crate_below_cargo_home_should_be_marked_possible_and_its_parents_should_say_inside() {
    let out = run(
        "/h/.cargo",
        vec![
            dir(0, NO_PARENT, "/h/.cargo", false, 2),
            dir(1, 0, "registry", false, 1),
            dir(2, 1, "src", false, 1),
            dir(3, 2, "index.crates.io-6f17d22bba15001f", false, 1),
            dir(4, 3, "serde-1.0.200", false, 0),
            dir(5, 0, "bin", false, 0),
        ],
    );
    assert!(out.contains("serde-1.0.200/  [cargo-home?]"), "{out}");
    assert!(out.contains("registry/  [cargo-home? inside]"), "{out}");
    assert!(
        out.lines()
            .filter(|l| l.contains("bin/"))
            .all(|l| !l.contains("cargo-home")),
        "{out}"
    );
    assert!(
        out.contains("--kinds cargo-home"),
        "no Next line for the kind:\n{out}"
    );
}

#[test]
fn a_part_of_a_cargo_target_should_be_marked_possible_build_dirs() {
    let out = run(
        "/h/code",
        vec![
            dir(0, NO_PARENT, "/h/code", false, 1),
            dir(1, 0, "app", true, 1),
            dir(2, 1, "target", false, 1),
            dir(3, 2, "debug", false, 2),
            dir(4, 3, "incremental", false, 0),
            dir(5, 3, "deps", false, 0),
        ],
    );
    assert!(out.contains("incremental/  [build-dirs?]"), "{out}");
    assert!(out.contains("deps/  [build-dirs?]"), "{out}");
    assert!(out.contains("target/  [build-dirs]"), "{out}");
}
