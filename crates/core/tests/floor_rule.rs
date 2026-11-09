#![allow(
    non_snake_case,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines
)]
//! Rule F (ADR-003 decision 2) as a pure table over `DeletePolicy::validate`, Story 2.1.1, Task 2.1.1b.
//!
//! Written by an independent blind author from the frozen contract (`DeletePolicy`, `AllowedTarget`,
//! `NamePattern`, `DeleteError` in `core::delete_types`, and the doc of `validate`) and ADR-003 only.
//! No path here touches the filesystem; the helpers build a `DeletePolicy` and call `validate`.
//!
//! CASE-TABLE-BEGIN
//! F-01 | allowed | validate_should_be_Ok_for_npm_cacache_gradle_caches_playwright_cargo_registry_src_When_Exact_targets
//! F-02 | refused | validate_should_refuse_home_ssh_documents_dot_npm_slash_scan_root_myrepo_myrepo_dot_git_myrepo_src_When_given
//! F-03 | C1 | validate_should_return_OutsideScope_for_dotdot_dot_double_slash_trailing_slash_relative_target_and_unclean_allowed_root_When_given
//! F-04 | refused | validate_should_return_ProtectedPath_When_name_is_empty_dot_dotdot_or_target_is_home_root_ancestor_scan_root
//! F-05 | Within | within_node_modules_should_accept_code_app_node_modules_and_refuse_dot_git_descendants_When_validated
//! F-06 | rule4 | validate_should_refuse_targets_with_fewer_than_2_components_below_home_When_under_home
//! CASE-TABLE-END

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use disk_cleanup_core::{AllowedTarget, DeleteError, DeletePolicy, Identity, NamePattern, RootDev};

fn policy(home: &str, scan_roots: &[&str], allowed: AllowedTarget, target: &str) -> DeletePolicy {
    DeletePolicy {
        root_dev: RootDev(1),
        root_mnt: None,
        home: PathBuf::from(home),
        home_id: Identity { dev: 1, ino: 1 },
        scan_roots: scan_roots.iter().map(PathBuf::from).collect(),
        allowed,
        target: PathBuf::from(target),
        require_sibling_any: vec![OsString::from("package.json")],
        max_depth: 64,
        restore_owner_write: false,
    }
}

fn exact(p: &str) -> AllowedTarget {
    AllowedTarget::Exact(PathBuf::from(p))
}

fn within(root: &str, name: NamePattern) -> AllowedTarget {
    AllowedTarget::Within {
        root: PathBuf::from(root),
        name,
    }
}

/// Validate a target by its own basename, as a caller holding a parent fd and a name would.
fn check(p: &DeletePolicy) -> Result<(), DeleteError> {
    let name = p.target.file_name().unwrap_or(OsStr::new(""));
    p.validate(name)
}

fn is_refusal(r: &Result<(), DeleteError>) -> bool {
    matches!(
        r,
        Err(DeleteError::ProtectedPath | DeleteError::OutsideScope)
    )
}

#[test]
fn validate_should_be_Ok_for_npm_cacache_gradle_caches_playwright_cargo_registry_src_When_Exact_targets(
) {
    for t in [
        "/h/.npm/_cacache",
        "/h/.gradle/caches",
        "/h/.cache/ms-playwright",
        "/h/.cargo/registry/src",
    ] {
        let p = policy("/h", &["/h/code"], exact(t), t);
        assert_eq!(check(&p), Ok(()), "{t} is a legitimate Exact cache target");
    }
}

#[test]
fn validate_should_refuse_home_ssh_documents_dot_npm_slash_scan_root_myrepo_myrepo_dot_git_myrepo_src_When_given(
) {
    // Exact targets that the kind's table would never contain, plus ones it might contain by mistake
    for t in ["/h", "/h/.ssh", "/h/Documents", "/h/.npm", "/"] {
        let p = policy("/h", &["/h/code"], exact(t), t);
        let r = check(&p);
        assert!(is_refusal(&r), "{t} must be refused: {r:?}");
    }
    // the scan root itself, a repo, its .git and a non-matching child, as Within targets
    for t in [
        "/h/code",
        "/h/code/myrepo",
        "/h/code/myrepo/.git",
        "/h/code/myrepo/src",
    ] {
        let p = policy(
            "/h",
            &["/h/code"],
            within("/h/code", NamePattern::NodeModules),
            t,
        );
        let r = check(&p);
        assert!(is_refusal(&r), "{t} must be refused: {r:?}");
    }
    // a target that is not the allowed Exact path
    let p = policy(
        "/h",
        &["/h/code"],
        exact("/h/.npm/_cacache"),
        "/h/.gradle/caches",
    );
    assert!(
        is_refusal(&check(&p)),
        "a path other than the Exact one must be refused"
    );
    // a Within target under a different root than the one allowed
    let p = policy(
        "/h",
        &["/h/code", "/h/work"],
        within("/h/code", NamePattern::NodeModules),
        "/h/work/app/node_modules",
    );
    assert!(is_refusal(&check(&p)), "outside the allowed root");
}

#[test]
fn validate_should_return_OutsideScope_for_dotdot_dot_double_slash_trailing_slash_relative_target_and_unclean_allowed_root_When_given(
) {
    let allowed = || within("/h/code", NamePattern::NodeModules);
    for t in [
        "/h/code/../.ssh/node_modules",
        "/h/code/app/../../.ssh/node_modules",
        "/h/code/./app/node_modules",
        "/h//code/app/node_modules",
        "/h/code/app/node_modules/",
        "code/app/node_modules",
    ] {
        let p = policy("/h", &["/h/code"], allowed(), t);
        assert_eq!(
            p.validate(OsStr::new("node_modules")),
            Err(DeleteError::OutsideScope),
            "unclean target {t:?}"
        );
    }
    // the same predicate on the allowed root, the scan roots and home
    for root in [
        "/h/code/../code",
        "/h/code/",
        "/h/./code",
        "/h//code",
        "code",
    ] {
        let p = policy(
            "/h",
            &["/h/code"],
            within(root, NamePattern::NodeModules),
            "/h/code/app/node_modules",
        );
        assert_eq!(
            p.validate(OsStr::new("node_modules")),
            Err(DeleteError::OutsideScope),
            "unclean allowed root {root:?}"
        );
    }
    let p = policy("/h", &["/h/code/."], allowed(), "/h/code/app/node_modules");
    assert_eq!(
        p.validate(OsStr::new("node_modules")),
        Err(DeleteError::OutsideScope),
        "unclean scan root"
    );
    let p = policy("/h/", &["/h/code"], allowed(), "/h/code/app/node_modules");
    assert_eq!(
        p.validate(OsStr::new("node_modules")),
        Err(DeleteError::OutsideScope),
        "unclean home"
    );
    let p = policy(
        "/h",
        &["/h/code"],
        exact("/h/.npm/../.npm/_cacache"),
        "/h/.npm/_cacache",
    );
    assert_eq!(
        p.validate(OsStr::new("_cacache")),
        Err(DeleteError::OutsideScope),
        "unclean Exact path"
    );
    // control: the clean spelling of the same policy is accepted
    let p = policy("/h", &["/h/code"], allowed(), "/h/code/app/node_modules");
    assert_eq!(p.validate(OsStr::new("node_modules")), Ok(()));
}

#[test]
fn validate_should_return_ProtectedPath_When_name_is_empty_dot_dotdot_or_target_is_home_root_ancestor_scan_root(
) {
    // bad names, regardless of an otherwise acceptable target
    let ok_target = "/home/u/.npm/_cacache";
    for name in ["", ".", ".."] {
        let p = policy("/home/u", &["/home/u/code"], exact(ok_target), ok_target);
        assert_eq!(
            p.validate(OsStr::new(name)),
            Err(DeleteError::ProtectedPath),
            "name {name:?}"
        );
    }
    let p = policy("/home/u", &["/home/u/code"], exact(ok_target), ok_target);
    assert!(
        is_refusal(&p.validate(OsStr::new("a/b"))),
        "a name with a slash"
    );
    assert!(
        is_refusal(&p.validate(OsStr::new("/"))),
        "a name that is a slash"
    );
    // target is `/`, an ancestor of home, home itself, or a scan root
    for (t, name) in [
        ("/", ""),
        ("/home", "home"),
        ("/home/u", "u"),
        ("/home/u/code", "code"),
        ("/mnt/data/code", "code"),
    ] {
        let p = policy("/home/u", &["/home/u/code", "/mnt/data/code"], exact(t), t);
        assert_eq!(
            p.validate(OsStr::new(name)),
            Err(DeleteError::ProtectedPath),
            "target {t}"
        );
    }
    // a scan root is protected even when the allowed rule would otherwise name it
    let p = policy(
        "/home/u",
        &["/home/u/code"],
        within("/home/u", NamePattern::NodeModules),
        "/home/u/code",
    );
    assert!(is_refusal(&p.validate(OsStr::new("code"))));
}

#[test]
fn within_node_modules_should_accept_code_app_node_modules_and_refuse_dot_git_descendants_When_validated(
) {
    let allowed = || within("/h/code", NamePattern::NodeModules);
    let p = policy("/h", &["/h/code"], allowed(), "/h/code/app/node_modules");
    assert_eq!(check(&p), Ok(()));
    for t in [
        "/h/code/app/node_modules/.git",
        "/h/code/app/.git",
        "/h/code/app",
        "/h/code/app/src",
        "/h/code/app/node_modules/pkg/.git",
        "/h/code/app/node_modules_old",
        "/h/code/app/Node_Modules",
    ] {
        let p = policy("/h", &["/h/code"], allowed(), t);
        let r = check(&p);
        assert!(is_refusal(&r), "{t} must be refused: {r:?}");
    }
    // other patterns accept only their own basename
    let p = policy(
        "/h",
        &["/h/code"],
        within("/h/code", NamePattern::CargoTarget),
        "/h/code/crate/target",
    );
    assert_eq!(check(&p), Ok(()));
    let p = policy(
        "/h",
        &["/h/code"],
        within("/h/code", NamePattern::CargoTarget),
        "/h/code/crate/node_modules",
    );
    assert!(is_refusal(&check(&p)));
}

#[test]
fn validate_should_refuse_targets_with_fewer_than_2_components_below_home_When_under_home() {
    for t in ["/h/x", "/h/.cache", "/h/.cargo", "/h/code"] {
        let p = policy("/h", &["/h/zzz"], exact(t), t);
        let r = check(&p);
        assert!(is_refusal(&r), "{t} has one component below home: {r:?}");
    }
    // two components below home are enough for an Exact cache path
    let t = "/h/.cache/pip";
    let p = policy("/h", &["/h/code"], exact(t), t);
    assert_eq!(check(&p), Ok(()));
    // rule 4 only applies below home: a Within target on another mount is not under $HOME
    let p = policy(
        "/h",
        &["/mnt/data/code"],
        within("/mnt/data/code", NamePattern::NodeModules),
        "/mnt/data/code/app/node_modules",
    );
    assert_eq!(check(&p), Ok(()));
}
