#![allow(clippy::unwrap_used, non_snake_case)]
//! Task 6.6.5h (review C7): `OwnState` is compared canonically, and the binary's ancestors are covered.

use std::path::{Path, PathBuf};

use dunnage_core::explore::mark_check::OwnState;
use dunnage_core::Dirs;
use proptest::prelude::*;

fn dirs(state: &str) -> Dirs {
    Dirs {
        home: "/home/u".into(),
        state: state.into(),
        cache: "/home/u/.cache".into(),
        config: "/home/u/.config".into(),
    }
}

/// `~/.local/state` is a link to `/data/st`.
fn canon(p: &Path) -> PathBuf {
    match p.strip_prefix("/home/u/.local/state") {
        Ok(rest) => Path::new("/data/st").join(rest),
        Err(_) => p.to_path_buf(),
    }
}

#[test]
fn covers_should_include_the_canonical_target_of_a_symlinked_xdg_directory() {
    let lexical = OwnState::new(&dirs("/home/u/.local/state"), "/usr/bin/dunnage".into());
    assert!(!lexical.covers(Path::new("/data/st/dunnage/audit.jsonl")));
    let own = OwnState::canonical(
        &dirs("/home/u/.local/state"),
        "/usr/bin/dunnage".into(),
        &canon,
    );
    for p in [
        "/data/st/dunnage",
        "/data/st/dunnage/audit.jsonl",
        "/data/st",
        "/home/u/.local/state/dunnage/audit.jsonl",
    ] {
        assert!(own.covers(Path::new(p)), "{p}");
    }
    assert!(!own.covers(Path::new("/data/other")));
}

#[test]
fn covers_should_include_the_binary_and_every_ancestor_of_it() {
    let own = OwnState::new(
        &dirs("/home/u/.local/state"),
        "/home/u/code/dc/target/release/dunnage".into(),
    );
    for p in [
        "/home/u/code/dc/target/release/dunnage",
        "/home/u/code/dc/target/release",
        "/home/u/code/dc/target",
        "/home/u/code/dc",
    ] {
        assert!(own.covers(Path::new(p)), "{p}");
    }
    assert!(!own.covers(Path::new("/home/u/code/other")));
    assert!(!own.covers(Path::new("/home/u/code/dc/src")));
}

proptest! {
    /// Over random symlinked XDG layouts the check agrees with `canonicalize`: whatever `canon` maps a state
    /// path to is covered, and a sibling of the state directory is not.
    #[test]
    fn covers_should_agree_with_the_canonical_form_When_the_xdg_layout_is_a_random_link(
        target in "/[a-z]{1,4}/[a-z]{1,4}", leaf in "[a-z]{1,6}",
    ) {
        prop_assume!(!target.starts_with("/home"));
        let t = target.clone();
        let canon_fn = move |p: &Path| -> PathBuf {
            match p.strip_prefix("/home/u/.local/state") {
                Ok(rest) => Path::new(&t).join(rest),
                Err(_) => p.to_path_buf(),
            }
        };
        let own = OwnState::canonical(&dirs("/home/u/.local/state"), "/usr/bin/dc".into(), &canon_fn);
        let inside = Path::new(&target).join("dunnage").join(&leaf);
        prop_assert!(own.covers(&inside));
        let sibling = Path::new(&target).join(["not-", leaf.as_str()].concat());
        prop_assert!(!own.covers(&sibling));
    }
}
