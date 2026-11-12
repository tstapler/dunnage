#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 6.5.4, Task 6.5.4a: the clap surface of `explore`, the refused flags and their texts, the defaults
//! and the exit codes, through the spawned binary and through `args::parse`.

mod common;

use std::path::PathBuf;

#[cfg(feature = "explore")]
use common::tree_manifest;
use common::Fixture;
use dunnage_cli::args::{self, Command, ExploreSortArg, Parsed, EXPLORE_NO_APPLY};

fn explore_args(argv: &[&str]) -> args::ExploreArgs {
    match args::parse(argv.iter().map(|s| (*s).to_string())) {
        Parsed::Run(cli) => match cli.command {
            Some(Command::Explore(a)) => a,
            other => panic!("not explore: {other:?}"),
        },
        Parsed::Exit { code, stderr, .. } => panic!("exit {code}: {stderr}"),
    }
}

fn exit_of(argv: &[&str]) -> (i32, String, String) {
    match args::parse(argv.iter().map(|s| (*s).to_string())) {
        Parsed::Exit {
            code,
            stdout,
            stderr,
        } => (code, stdout, stderr),
        Parsed::Run(cli) => panic!("parsed as a run: {cli:?}"),
    }
}

const DELETING: [&str; 10] = [
    "--apply",
    "--plan-id=deadbeef",
    "--kinds=caches",
    "--except=caches",
    "--only=/x",
    "--force",
    "-y",
    "--yes",
    "-n",
    "--dry-run",
];

#[test]
fn every_deleting_flag_should_exit_64_with_the_apply_text_and_an_empty_stdout() {
    for flag in DELETING {
        let (code, stdout, stderr) = exit_of(&["explore", flag]);
        assert_eq!((code, stdout.as_str()), (64, ""), "{flag}");
        assert_eq!(stderr, format!("{EXPLORE_NO_APPLY}\n"), "{flag}");
        // Before the subcommand the cleaner's own flags get the same text; clap itself rejects the rest.
        let (code, stdout, stderr) = exit_of(&[flag, "explore"]);
        assert_eq!((code, stdout.as_str()), (64, ""), "{flag} explore");
        assert!(stderr.starts_with("error:"), "{flag} explore: {stderr}");
    }
}

#[test]
fn the_other_refused_flags_should_exit_64_naming_what_was_refused() {
    for (flag, word) in [
        ("-q", "-q"),
        ("-v", "-v"),
        ("--cross-device", "--cross-device"),
        ("--mouse", "--mouse"),
    ] {
        let (code, stdout, stderr) = exit_of(&["explore", flag]);
        assert_eq!((code, stdout.as_str()), (64, ""), "{flag}");
        assert!(stderr.starts_with("error: explore has no "), "{stderr}");
        assert!(stderr.contains(word), "{stderr}");
        assert!(
            stderr.trim_end().ends_with("Nothing was deleted."),
            "{stderr}"
        );
    }
}

#[test]
fn a_flag_explore_does_not_define_should_be_an_ordinary_usage_error() {
    let (code, stdout, stderr) = exit_of(&["explore", "--stale-days", "3"]);
    assert_eq!((code, stdout.as_str()), (64, ""));
    assert!(stderr.contains("--stale-days"), "{stderr}");
}

#[test]
fn the_refused_flags_should_not_appear_in_help() {
    let (code, stdout, _) = exit_of(&["explore", "--help"]);
    assert_eq!(code, 0);
    assert!(
        stdout.starts_with("Use --text with a screen reader."),
        "{stdout}"
    );
    for hidden in [
        "--apply",
        "--plan-id",
        "--kinds",
        "--mouse",
        "--cross-device",
        "--plain",
    ] {
        assert!(!stdout.contains(hidden), "{hidden} is listed:\n{stdout}");
    }
    for shown in [
        "--root",
        "--jobs",
        "--sort",
        "--max-seconds",
        "--no-cache",
        "--rescan",
        "--color",
        "--text",
        "--depth",
        "--top",
        "--bars",
        "--json",
    ] {
        assert!(stdout.contains(shown), "{shown} is missing:\n{stdout}");
    }
}

#[test]
fn the_defaults_should_be_the_flag_table_of_the_ux_document() {
    let a = explore_args(&["explore"]);
    assert_eq!(a.display_roots(), vec![PathBuf::from("/")]);
    assert_eq!(a.jobs, None);
    assert_eq!(a.sort, ExploreSortArg::Usage);
    assert_eq!((a.depth, a.top), (1, 10));
    assert!(!a.no_cache && !a.rescan && !a.wants_text() && !a.bars);
    assert_eq!(a.max_seconds, None);
}

#[test]
fn paths_and_root_should_be_equivalent_and_unioned() {
    let a = explore_args(&["explore", "/a", "--root", "/b", "/c"]);
    let got = a.display_roots();
    for p in ["/a", "/b", "/c"] {
        assert!(got.contains(&PathBuf::from(p)), "{got:?}");
    }
    assert!(!got.contains(&PathBuf::from("/")));
}

#[test]
fn the_flags_of_the_table_should_set_their_fields() {
    let a = explore_args(&[
        "explore",
        "--jobs",
        "3",
        "--sort",
        "name",
        "--max-seconds",
        "9",
        "--no-cache",
        "--depth",
        "2",
        "--top",
        "0",
        "--bars",
        "--plain",
        "-x",
    ]);
    assert_eq!(a.jobs, Some(3));
    assert_eq!(a.sort, ExploreSortArg::Name);
    assert_eq!(a.max_seconds, Some(9));
    assert!(a.no_cache && a.bars && a.wants_text() && a.one_file_system);
    assert_eq!((a.depth, a.top), (2, 0));
    let (code, _, _) = exit_of(&["explore", "--no-cache", "--rescan"]);
    assert_eq!(
        code, 64,
        "--no-cache reads and writes nothing, --rescan writes: they conflict"
    );
}

#[test]
fn json_and_color_should_reach_explore_through_the_global_flags() {
    let Parsed::Run(cli) = args::parse(["explore", "--json", "--color", "never"]) else {
        panic!("parse");
    };
    assert!(cli.json);
    assert_eq!(
        cli.color_choice(),
        dunnage_cli::terminal::ColorChoice::Never
    );
}

#[test]
fn the_default_jobs_should_be_the_cpu_count_capped_at_16() {
    let cpus = std::thread::available_parallelism().unwrap().get();
    assert_eq!(dunnage_cli::wiring::explore::default_jobs(), cpus.min(16));
}

#[cfg(feature = "explore")]
#[test]
fn the_binary_should_refuse_a_deleting_flag_and_leave_the_fixture_untouched() {
    let f = Fixture::new();
    let before = tree_manifest(f.root());
    for flag in DELETING {
        let out = f.fixture_cmd().args(["explore", flag]).output().unwrap();
        assert_eq!(out.status.code(), Some(64), "{flag}");
        assert!(out.stdout.is_empty(), "{flag}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.starts_with("error:"), "{flag}: {err}");
        assert!(err.contains("Nothing was deleted."), "{flag}: {err}");
    }
    assert_eq!(tree_manifest(f.root()), before, "the fixture changed");
    assert!(!f.xdg("state").join("dunnage").exists());
}

#[cfg(feature = "explore")]
#[test]
fn a_missing_path_should_exit_2_with_the_roots_unreadable_text() {
    let f = Fixture::new();
    let out = f
        .fixture_cmd()
        .args(["explore", "--text", "/definitely/not/here"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.starts_with("error: root /definitely/not/here does not exist or is unreadable"),
        "{err}"
    );
}

#[cfg(not(feature = "explore"))]
#[test]
fn a_build_without_the_explore_feature_should_exit_64() {
    let f = Fixture::new();
    let out = f
        .fixture_cmd()
        .args(["explore", "--text"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(64));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("'explore' is not available in this build")
    );
}
