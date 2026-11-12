//! Blind adversarial cases for `AllowedTarget::BelowScanRoot` at the rule-F level (Task 6.6.5b).
//! Written from ADR-003 (amended 2026-11-11), ADR-006 A1 and the frozen contract only.
//!
//! The only way to hold a witness is `decide_path_common`, so every case first obtains an `Allowed` under a
//! permissive grant (decoy home, one grant root above the target) and then builds the policy with the
//! home and scan roots under test: the policy parameters are what `validate` must judge.

#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]

use std::io;
use std::path::{Path, PathBuf};

use dunnage_core::guard::{decide_path_common, Facts, GuardDecision};
use dunnage_core::roots::RootSet;
use dunnage_core::{Allowed, DeleteError, DeletePolicy, Dirs, Identity, Kind, RootDev};

fn fake_ino(p: &Path) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in p.as_os_str().as_encoded_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h | 1
}

fn ident_of(p: &Path) -> Identity {
    Identity {
        dev: 1,
        ino: fake_ino(p),
    }
}

#[allow(clippy::unnecessary_wraps)] // the `Canon` callback type returns io::Result
fn canon(p: &Path) -> io::Result<(PathBuf, Identity)> {
    Ok((p.to_path_buf(), ident_of(p)))
}

fn grant_roots_for(target: &Path) -> PathBuf {
    let first = target
        .components()
        .nth(1)
        .expect("target needs two components");
    PathBuf::from("/").join(first.as_os_str())
}

fn facts(kind: Kind, path: &str) -> Facts {
    Facts {
        kind,
        path: PathBuf::from(path),
        ident: ident_of(Path::new(path)),
        is_symlink: false,
        root_dev: RootDev(1),
        is_mount_point: false,
    }
}

fn permissive_roots(root: &Path) -> RootSet {
    let dirs = Dirs {
        home: "/grant-home".into(),
        state: "/grant-home/.local/state".into(),
        cache: "/grant-home/.cache".into(),
        config: "/grant-home/.config".into(),
    };
    RootSet::resolve(&dirs, &[root.to_path_buf()], None, Path::new("/"), &canon)
        .expect("grant roots resolve")
}

fn witness_of_kind(kind: Kind, target: &str) -> Result<Allowed, String> {
    let roots = permissive_roots(&grant_roots_for(Path::new(target)));
    match decide_path_common(&facts(kind, target), &roots) {
        GuardDecision::Allow(a) => Ok(a),
        GuardDecision::Skip(r) => Err(format!("{r:?}")),
    }
}

fn witness(target: &str) -> Allowed {
    witness_of_kind(Kind::ExplorerSelection, target)
        .unwrap_or_else(|why| panic!("the guard should allow {target}: {why}"))
}

fn policy(target: &str, roots: &[&str], home: &str) -> DeletePolicy {
    DeletePolicy::for_allowed(
        &witness(target),
        RootDev(1),
        7,
        PathBuf::from(home),
        ident_of(Path::new(home)),
        roots.iter().map(PathBuf::from).collect(),
    )
    .expect("an explorer witness makes a policy")
}

fn name_of(target: &str) -> &std::ffi::OsStr {
    Path::new(target).file_name().expect("target has a name")
}

fn verdict(target: &str, roots: &[&str], home: &str) -> Result<(), DeleteError> {
    policy(target, roots, home).validate(name_of(target))
}

fn is_scope_refusal(e: &DeleteError) -> bool {
    matches!(e, DeleteError::OutsideScope | DeleteError::ProtectedPath)
}

fn assert_refused(target: &str, roots: &[&str], home: &str) {
    match verdict(target, roots, home) {
        Err(e) if is_scope_refusal(&e) => {}
        other => panic!("{target} roots={roots:?} home={home}: expected OutsideScope or ProtectedPath, got {other:?}"),
    }
}

fn assert_accepted(target: &str, roots: &[&str], home: &str) {
    assert_eq!(
        verdict(target, roots, home),
        Ok(()),
        "{target} roots={roots:?} home={home}"
    );
}

fn permutations<T: Clone>(items: &[T]) -> Vec<Vec<T>> {
    if items.len() <= 1 {
        return vec![items.to_vec()];
    }
    let mut out = Vec::new();
    for i in 0..items.len() {
        let mut rest = items.to_vec();
        let head = rest.remove(i);
        for mut tail in permutations(&rest) {
            tail.insert(0, head.clone());
            out.push(tail);
        }
    }
    out
}

#[test]
fn below_scan_root_should_accept_depth_one_two_and_many_When_the_directory_is_below_a_root() {
    assert_accepted("/h/code/a", &["/h/code"], "/h");
    assert_accepted("/h/code/a/b", &["/h/code"], "/h");
    assert_accepted("/h/code/a/b/c/d/e", &["/h/code"], "/h");
    assert_accepted("/h/.cache/x", &["/h/.cache"], "/h");
}

#[test]
fn below_scan_root_should_accept_depth_one_When_the_root_is_outside_home() {
    assert_accepted("/srv/work/a", &["/srv/work"], "/h");
    assert_accepted("/srv/work/a/b", &["/srv/work"], "/h");
}

#[test]
fn below_scan_root_should_accept_depth_one_When_home_is_only_a_string_prefix_of_the_path() {
    assert_accepted("/home/user2/a", &["/home/user2"], "/home/u");
}

#[test]
fn below_scan_root_should_accept_lookalike_git_names_When_no_component_is_exactly_dot_git() {
    for t in [
        "/h/code/.gitx",
        "/h/code/my.git",
        "/h/code/.git-old",
        "/h/code/git",
        "/h/code/.gitx/inner",
        "/h/code/x.git/y",
        "/h/code/a/.github",
        "/h/code/a/b.git/c/.gitignore.d",
    ] {
        assert_accepted(t, &["/h/code"], "/h");
    }
}

#[test]
fn safety_below_scan_root_should_refuse_the_scan_root_itself_When_it_is_the_witness_path() {
    assert_refused("/h/code/app", &["/h/code/app"], "/h");
    for roots in permutations(&["/h/code", "/h/code/app"]) {
        assert_refused("/h/code/app", &roots, "/h");
    }
    for roots in permutations(&["/h/code/app", "/h/code", "/h/zzz"]) {
        assert_refused("/h/code/app", &roots, "/h");
    }
}

#[test]
fn safety_below_scan_root_should_refuse_When_the_path_is_not_below_any_root() {
    assert_refused("/h/other/app", &["/h/code"], "/h");
    assert_refused("/h/code/app", &[], "/h");
    assert_refused("/srv/work/a", &["/srv/other"], "/h");
}

#[test]
fn safety_below_scan_root_should_refuse_When_a_root_is_only_a_string_prefix_of_the_path() {
    assert_refused("/h/code/app", &["/h/cod"], "/h");
    assert_refused("/h/code/app/x", &["/h/code/ap"], "/h");
    assert_refused("/srv/workshop/a", &["/srv/work"], "/h");
}

#[test]
fn safety_below_scan_root_should_refuse_When_the_path_is_an_ancestor_of_a_root() {
    assert_refused("/h/code/app", &["/h/code/app/x"], "/h");
    assert_refused("/srv/work", &["/srv/work/a/b"], "/h");
}

#[test]
fn safety_below_scan_root_should_refuse_When_the_path_is_home() {
    assert_refused("/h/code/app", &["/h/code"], "/h/code/app");
    assert_refused("/srv/work/a", &["/srv/work"], "/srv/work/a");
}

#[test]
fn safety_below_scan_root_should_refuse_When_the_path_is_home_by_identity_under_another_spelling() {
    let target = "/h/code/app";
    let w = witness(target);
    let p = DeletePolicy::for_allowed(
        &w,
        RootDev(1),
        7,
        PathBuf::from("/var/h-alias"),
        ident_of(Path::new(target)),
        vec![PathBuf::from("/h/code")],
    )
    .unwrap();
    match p.validate(name_of(target)) {
        Err(e) if is_scope_refusal(&e) => {}
        other => panic!("target with the identity of $HOME must be refused, got {other:?}"),
    }
}

#[test]
fn safety_below_scan_root_should_refuse_When_the_path_is_an_ancestor_of_home() {
    // home is three levels below the target, so "fewer than 2 components below home" cannot be what refuses.
    assert_refused("/h/code/app", &["/h/code"], "/h/code/app/x/y/z");
    assert_refused("/srv/work/a", &["/srv/work"], "/srv/work/a/b");
    assert_refused("/srv/work/a", &["/srv/work"], "/srv/work/a/b/c/d");
}

#[test]
fn safety_below_scan_root_should_refuse_with_protected_path_When_one_component_below_home() {
    for t in ["/h/code", "/h/.ssh", "/h/Documents", "/h/.npm"] {
        assert_eq!(
            verdict(t, &["/h"], "/h"),
            Err(DeleteError::ProtectedPath),
            "{t} with root = home"
        );
        assert_eq!(
            verdict(t, &["/"], "/h"),
            Err(DeleteError::ProtectedPath),
            "{t} with root = /"
        );
    }
}

#[test]
fn safety_below_scan_root_should_accept_exactly_two_components_below_home_When_the_boundary_is_tested(
) {
    assert_eq!(verdict("/h/.ssh/keys", &["/h/.ssh"], "/h"), Ok(()));
    assert_eq!(verdict("/h/.cache/x", &["/h/.cache"], "/h"), Ok(()));
    assert_eq!(
        verdict("/h/.ssh", &["/h"], "/h"),
        Err(DeleteError::ProtectedPath)
    );
}

#[test]
fn safety_below_scan_root_should_not_depend_on_root_list_order_When_roots_are_nested() {
    let cases: [(&str, bool); 6] = [
        ("/h/code/app", false),
        ("/h/code/app/x", true),
        ("/h/code/other", true),
        ("/h/code/app/x/y", true),
        ("/h/code/ap", true),
        ("/h/cod/app", false),
    ];
    let roots = ["/h/code", "/h/code/app"];
    for (target, ok) in cases {
        for order in permutations(&roots) {
            let got = verdict(target, &order, "/h");
            assert_eq!(got.is_ok(), ok, "{target} order={order:?}: {got:?}");
        }
    }
}

#[test]
fn safety_below_scan_root_should_not_depend_on_root_list_order_When_three_roots_nest() {
    let roots = ["/h/code", "/h/code/app", "/h/code/app/deep"];
    for order in permutations(&roots) {
        assert_refused("/h/code/app/deep", &order, "/h");
        assert_refused("/h/code/app", &order, "/h");
        assert_refused("/h/code", &order, "/h");
        assert_accepted("/h/code/app/deep/x", &order, "/h");
        assert_accepted("/h/code/app/other", &order, "/h");
        assert_accepted("/h/code/other", &order, "/h");
    }
}

#[test]
fn safety_below_scan_root_should_refuse_When_the_policy_target_is_changed_to_another_directory() {
    let mut p = policy("/h/code/a", &["/h/code"], "/h");
    p.target = PathBuf::from("/h/code/b");
    match p.validate(std::ffi::OsStr::new("b")) {
        Err(e) if is_scope_refusal(&e) => {}
        other => panic!("a policy made for /h/code/a must not allow /h/code/b: {other:?}"),
    }
}

#[test]
fn safety_below_scan_root_should_refuse_When_the_policy_target_is_changed_to_a_git_directory() {
    let mut p = policy("/h/code/a", &["/h/code"], "/h");
    p.target = PathBuf::from("/h/code/a/.git");
    assert!(p.validate(std::ffi::OsStr::new(".git")).is_err());
    let mut p = policy("/h/code/a", &["/h/code"], "/h");
    p.target = PathBuf::from("/h/code/.git");
    assert!(p.validate(std::ffi::OsStr::new(".git")).is_err());
}

#[test]
fn safety_below_scan_root_should_refuse_When_the_policy_target_is_changed_to_an_unclean_path() {
    for bad in [
        "/h/code/a/../b",
        "/h/code//b",
        "/h/code/b/",
        "h/code/b",
        "/h/code/./b",
        "/h/code/a/../../.ssh",
    ] {
        let mut p = policy("/h/code/b", &["/h/code"], "/h");
        p.target = PathBuf::from(bad);
        let name = Path::new(bad)
            .file_name()
            .unwrap_or(std::ffi::OsStr::new(".."));
        assert!(p.validate(name).is_err(), "{bad} must be refused");
    }
}

#[test]
fn safety_below_scan_root_should_refuse_When_the_policy_target_is_changed_to_home_or_an_ancestor() {
    for (bad, name) in [("/h", "h"), ("/", ""), ("/h/code", "code")] {
        let mut p = policy("/h/code/a", &["/h/code"], "/h");
        p.target = PathBuf::from(bad);
        assert!(
            p.validate(std::ffi::OsStr::new(name)).is_err(),
            "{bad} must be refused"
        );
    }
}

#[test]
fn safety_below_scan_root_should_refuse_When_the_name_does_not_belong_to_the_target() {
    let p = policy("/h/code/a", &["/h/code"], "/h");
    for name in ["b", "", ".", "..", "a/b", "/h/code/a"] {
        assert!(
            p.validate(std::ffi::OsStr::new(name)).is_err(),
            "name {name:?} must be refused"
        );
    }
}

#[test]
fn safety_guard_should_not_hand_out_a_witness_When_the_path_has_a_dot_git_component() {
    for t in [
        "/h/code/proj/.git",
        "/h/code/.git/objects",
        "/h/code/.git/a/b/c",
        "/h/code/a/.git/hooks",
    ] {
        assert!(
            witness_of_kind(Kind::ExplorerSelection, t).is_err(),
            "{t} must not be allowed"
        );
    }
}

#[test]
fn safety_guard_should_not_hand_out_a_witness_When_the_path_is_not_clean_and_absolute() {
    for t in [
        "/h/code/a/../b",
        "/h/code/../.ssh/x",
        "/h/code//b",
        "/h/code/b/",
        "/h/code/./b",
        "h/code/b",
        "code/b",
        "./h/code/b",
    ] {
        let roots = permissive_roots(Path::new("/h"));
        let d = decide_path_common(&facts(Kind::ExplorerSelection, t), &roots);
        assert!(matches!(d, GuardDecision::Skip(_)), "{t} must be skipped");
    }
}

#[test]
fn safety_guard_should_not_hand_out_a_witness_When_the_path_is_a_scan_root_or_not_below_one() {
    let roots = permissive_roots(Path::new("/h"));
    for t in ["/h", "/g/x", "/hh/x", "/"] {
        let d = decide_path_common(&facts(Kind::ExplorerSelection, t), &roots);
        assert!(matches!(d, GuardDecision::Skip(_)), "{t} must be skipped");
    }
}

#[test]
fn below_scan_root_should_not_make_a_policy_When_the_witness_is_of_another_kind() {
    // A witness of a non-explorer kind must not become an explorer policy. The guard for those kinds may
    // itself skip this path; then there is nothing to test and the case is vacuous by construction.
    let candidates = [
        (Kind::NodeModules, "/h/code/proj/node_modules"),
        (Kind::CargoTarget, "/h/code/proj/target"),
    ];
    let mut tried = 0;
    for (kind, path) in candidates {
        if let Ok(w) = witness_of_kind(kind, path) {
            tried += 1;
            let r = DeletePolicy::for_allowed(
                &w,
                RootDev(1),
                7,
                PathBuf::from("/h"),
                ident_of(Path::new("/h")),
                vec![PathBuf::from("/h/code")],
            );
            assert!(
                r.is_err(),
                "a {kind:?} witness must not make an explorer policy"
            );
        }
    }
    eprintln!("note: {tried} other-kind witnesses could be built");
}
