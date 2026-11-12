#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Task 6.6.5k (review N7): cases that kill the survivors of the first recorded mutation run over the BelowScanRoot code
//! (ci/mutants.d/6.6.5k.files). A new, unpinned file: the pinned oracles stay untouched.

use std::path::{Path, PathBuf};

use dunnage_core::floor_rule::floor_protected;
use dunnage_core::WrongKind;

#[test]
fn floor_protected_should_protect_slash_even_when_home_is_not_an_absolute_path() {
    let home = Path::new("relative/home");
    assert!(floor_protected(Path::new("/"), home, &[]));
}

#[test]
fn floor_protected_should_protect_home_itself_and_a_scan_root_but_not_a_deep_sibling() {
    let home = Path::new("/home/u");
    let roots = vec![PathBuf::from("/home/u/code")];
    assert!(floor_protected(home, home, &roots));
    assert!(floor_protected(Path::new("/home/u/code"), home, &roots));
    assert!(!floor_protected(
        Path::new("/home/u/code/app"),
        home,
        &roots
    ));
}

#[test]
fn wrong_kind_should_display_the_reason_it_was_refused() {
    assert_eq!(
        WrongKind.to_string(),
        "a worktree removal needs an Allowed of kind Worktree"
    );
}
