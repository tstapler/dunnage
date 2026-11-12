#![allow(non_snake_case, clippy::unwrap_used, clippy::expect_used)]
//! Mutation-killing tests for rule F (`DeletePolicy::validate`, `matches_pattern`), Task 2.1.5b.
//! Written by agent T against the surviving mutants of `core/src/floor_rule.rs`. Pure: no filesystem access.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use dunnage_core::{AllowedTarget, DeleteError, DeletePolicy, Identity, NamePattern, RootDev};

fn within(root: &str, pattern: NamePattern, target: &str) -> DeletePolicy {
    DeletePolicy {
        root_dev: RootDev(1),
        root_mnt: None,
        home: PathBuf::from("/h"),
        home_id: Identity { dev: 1, ino: 1 },
        scan_roots: vec![PathBuf::from("/h/scan")],
        allowed: AllowedTarget::Within {
            root: PathBuf::from(root),
            name: pattern,
        },
        target: PathBuf::from(target),
        require_sibling_any: vec![OsString::from("marker")],
        max_depth: 64,
        restore_owner_write: false,
    }
}

fn verdict(root: &str, pattern: NamePattern, target: &str) -> Result<(), DeleteError> {
    let p = within(root, pattern, target);
    let name = p.target.file_name().unwrap().to_os_string();
    p.validate(OsStr::new(&name))
}

#[test]
fn agent_worktree_should_need_the_agent_dash_prefix_and_a_non_empty_suffix_When_validated() {
    let root = "/h/code/proj/.claude/worktrees";
    let ok = |n: &str| verdict(root, NamePattern::AgentWorktree, &format!("{root}/{n}"));
    assert_eq!(ok("agent-a"), Ok(()));
    assert_eq!(ok("agent-abc123"), Ok(()));
    assert_eq!(ok("agent-"), Err(DeleteError::OutsideScope), "empty suffix");
    assert_eq!(ok("agent"), Err(DeleteError::OutsideScope));
    assert_eq!(ok("agentx"), Err(DeleteError::OutsideScope));
    assert_eq!(ok("worktree-abc123"), Err(DeleteError::OutsideScope));
    assert_eq!(ok("xxxxxxxxxxxx"), Err(DeleteError::OutsideScope));
}

#[test]
fn steam_app_id_should_accept_only_non_empty_all_digit_names_When_validated() {
    let root = "/h/.steam/steamapps/compatdata";
    let ok = |n: &str| verdict(root, NamePattern::SteamAppId, &format!("{root}/{n}"));
    assert_eq!(ok("1"), Ok(()));
    assert_eq!(ok("440"), Ok(()));
    assert_eq!(ok("1234567890"), Ok(()));
    for bad in ["abc", "12a", "a12", "1 2", "-1", "1.5", "pfx"] {
        assert_eq!(ok(bad), Err(DeleteError::OutsideScope), "{bad}");
    }
}

#[test]
fn steam_install_dir_should_refuse_empty_dot_prefixed_names_and_accept_plain_ones_When_validated() {
    let root = "/h/.steam/steamapps/common";
    let ok = |n: &str| verdict(root, NamePattern::SteamInstallDir, &format!("{root}/{n}"));
    assert_eq!(ok("Half-Life"), Ok(()));
    assert_eq!(ok("x"), Ok(()));
    assert_eq!(ok("a.b"), Ok(()));
    assert_eq!(ok(".hidden"), Err(DeleteError::OutsideScope));
    assert_eq!(ok(".x"), Err(DeleteError::OutsideScope));
}

#[test]
fn steam_install_dir_should_need_exactly_one_component_below_the_library_When_validated() {
    let root = "/h/.steam/steamapps/common";
    let v = |t: &str| verdict(root, NamePattern::SteamInstallDir, t);
    assert_eq!(v(&format!("{root}/Game")), Ok(()));
    assert_eq!(
        v(&format!("{root}/Game/bin")),
        Err(DeleteError::OutsideScope),
        "two components below the library"
    );
    assert_eq!(
        v(&format!("{root}/Game/bin/data")),
        Err(DeleteError::OutsideScope)
    );
    // zero components: the library itself
    assert_eq!(
        verdict(
            "/h/.steam/steamapps/common/Game",
            NamePattern::SteamInstallDir,
            "/h/.steam/steamapps/common/Game"
        ),
        Err(DeleteError::OutsideScope)
    );
}

#[test]
fn name_patterns_other_than_steam_install_dir_should_accept_any_depth_of_one_or_more_When_validated(
) {
    let root = "/h/code";
    let v = |t: &str| verdict(root, NamePattern::NodeModules, t);
    assert_eq!(v("/h/code/node_modules"), Ok(()), "depth 1");
    assert_eq!(v("/h/code/a/node_modules"), Ok(()), "depth 2");
    assert_eq!(v("/h/code/a/b/c/node_modules"), Ok(()), "depth 4");
    // depth 0: the allowed root itself, even though its name matches the pattern
    assert_eq!(
        verdict(
            "/h/code/node_modules",
            NamePattern::NodeModules,
            "/h/code/node_modules"
        ),
        Err(DeleteError::OutsideScope)
    );
    // a target that is not below the root at all
    assert_eq!(v("/h/other/node_modules"), Err(DeleteError::OutsideScope));
}
