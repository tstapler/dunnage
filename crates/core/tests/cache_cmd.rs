#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! The command table of the command caches (Story 2.3.2): every row is an allowlist row, the root rules, and
//! the plan-id gate for a non-destructive `cache-apt` run.

use disk_cleanup_core::cache_targets::{cache_command, root_decision, CACHE_COMMANDS};
use disk_cleanup_core::plan::destructive_gate;
use disk_cleanup_core::ports::{classify, Class, Cmd};
use disk_cleanup_core::{
    AllocatedBytes, CacheFacts, Candidate, Identity, Intent, KindFacts, Locator, SelectorId,
    SkipReason,
};

fn cache(label: &str, path: &str) -> Candidate {
    Candidate::new(
        Locator::Path(path.into()),
        KindFacts::Cache(CacheFacts {
            label: label.into(),
            apt: false,
        }),
        AllocatedBytes(1),
        None,
        Identity { dev: 1, ino: 2 },
    )
}

#[test]
fn every_cache_command_should_be_a_mutating_allowlist_row_for_each_of_its_programs() {
    for spec in CACHE_COMMANDS {
        for p in spec.programs {
            let cmd = Cmd::new(*p, spec.args.iter().copied(), true);
            assert_eq!(classify(&cmd), Some(Class::Mutating), "{p} {:?}", spec.args);
        }
    }
    assert_eq!(CACHE_COMMANDS.len(), 5);
}

#[test]
fn brew_cleanup_prune_all_should_stay_refused_and_the_row_should_say_so() {
    let all = Cmd::new("brew", ["cleanup", "--prune=all"], true);
    assert_eq!(classify(&all), None);
    let note = cache_command("Homebrew cache").unwrap().row_note.unwrap();
    assert_eq!(
        note,
        "brew cleanup (Homebrew's default prune window; --prune=all is not run)"
    );
}

#[test]
fn root_decision_should_follow_the_euid_and_not_home() {
    assert_eq!(
        root_decision(true, 1000, false),
        Some(SkipReason::NeedsRoot)
    );
    assert_eq!(
        root_decision(false, 0, false),
        Some(SkipReason::RootRestricted)
    );
    assert_eq!(root_decision(false, 0, true), None);
    assert_eq!(root_decision(true, 0, false), None);
    assert_eq!(root_decision(false, 1000, false), None);
}

#[test]
fn intent_should_record_the_argv_of_a_command_cache_and_nothing_for_a_path_cache() {
    let go = cache("Go build cache", "/h/.cache/go-build");
    let npm = cache("npm _cacache", "/h/.npm/_cacache");
    let i = Intent::of(&go, "r", 1);
    assert_eq!(i.argv, ["go", "clean", "-cache"]);
    assert!(Intent::of(&npm, "r", 1).argv.is_empty());
    let line = serde_json::to_string(&Intent::of(&npm, "r", 1)).unwrap();
    assert!(!line.contains("argv"), "{line}");
}

#[test]
fn cache_apt_should_need_no_plan_id_even_when_another_uid_saved_the_dry_run() {
    let sel = common::selection(&[SelectorId::CacheApt]);
    let re = disk_cleanup_core::plan::PlanId::from_hex("c4410aa2").unwrap();
    assert!(destructive_gate(&sel, None, None, re, &[], 1_000).is_ok());
}
