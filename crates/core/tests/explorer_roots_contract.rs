//! Blind adversarial cases for the revised delete-engine contract of Task 6.6.3f (review 2, N3 and N4).
//! Written from ADR-003, ADR-006 (amended 2026-11-11), plan.md Task 6.6.3f and the frozen contract only:
//! `DeletePolicy::for_allowed_roots`, `DeletePolicy::witness_matches`, `RootSet::{home_id, home_chain,
//! in_home_chain}` and the home-chain refusal of `decide_path_common`.
//!
//! The only way to hold a witness is `decide_path_common`, so each case first mints an `Allowed` under a
//! permissive scope_dir (decoy home) and then builds the policy against the `RootSet` under test. Identities
//! are synthetic (a hash of the path, device 1), so a second spelling of a directory is simply a different
//! path that reports the same identity.

#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]

use std::io;
use std::path::{Path, PathBuf};

use dunnage_core::delete_types::FileKind;
use dunnage_core::floor_rule::ForAllowedError;
use dunnage_core::guard::{decide_path_common, Facts, GuardDecision};
use dunnage_core::plan::ItemIdentity;
use dunnage_core::roots::RootSet;
use dunnage_core::{Allowed, DeletePolicy, Dirs, Identity, Kind, RootDev, SkipReason};

const HOME: &str = "/h/a/b/c";
const CODE: &str = "/h/a/b/c/code";
const WORK: &str = "/srv/work";

fn fake_ino(p: &Path) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in p.as_os_str().as_encoded_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h | 1
}

fn ident_of(p: &str) -> Identity {
    Identity {
        dev: 1,
        ino: fake_ino(Path::new(p)),
    }
}

#[allow(clippy::unnecessary_wraps)] // the `Canon` callback type returns io::Result
fn canon(p: &Path) -> io::Result<(PathBuf, Identity)> {
    Ok((p.to_path_buf(), ident_of(p.to_str().unwrap())))
}

fn dirs(home: &str) -> Dirs {
    Dirs {
        home: home.into(),
        state: format!("{home}/.local/state").into(),
        cache: format!("{home}/.cache").into(),
        config: format!("{home}/.config").into(),
    }
}

fn roots_with(home: &str, scan: &[&str]) -> RootSet {
    let flags: Vec<PathBuf> = scan.iter().map(PathBuf::from).collect();
    RootSet::resolve(&dirs(home), &flags, None, Path::new("/"), &canon).expect("roots resolve")
}

/// The roots under test: `$HOME` is `/h/a/b/c`, scan roots `/h/a/b/c/code` and `/srv/work`.
fn roots() -> RootSet {
    roots_with(HOME, &[CODE, WORK])
}

fn facts(kind: Kind, path: &str, ident: Identity) -> Facts {
    Facts {
        kind,
        path: PathBuf::from(path),
        ident,
        is_symlink: false,
        root_dev: RootDev(1),
        is_mount_point: false,
    }
}

/// A witness of `kind` for `path` that reports `ident`, minted under a permissive scope_dir that knows nothing of
/// the home chain under test.
fn mint(kind: Kind, path: &str, ident: Identity, grant_root: &str) -> Result<Allowed, String> {
    let permissive = roots_with("/scope_dir-home", &[grant_root]);
    match decide_path_common(&facts(kind, path, ident), &permissive) {
        GuardDecision::Allow(a) => Ok(a),
        GuardDecision::Skip(r) => Err(format!("{r:?}")),
    }
}

fn explorer_witness(path: &str, ident: Identity, grant_root: &str) -> Allowed {
    mint(Kind::ExplorerSelection, path, ident, grant_root)
        .unwrap_or_else(|why| panic!("the permissive guard should allow {path}: {why}"))
}

fn planned(ident: Identity, kind: FileKind) -> ItemIdentity {
    ItemIdentity {
        dev: ident.dev,
        ino: ident.ino,
        kind,
    }
}

fn dir_planned(ident: Identity) -> ItemIdentity {
    planned(ident, FileKind::Dir)
}

fn build(
    w: &Allowed,
    roots: &RootSet,
    planned: ItemIdentity,
) -> Result<DeletePolicy, ForAllowedError> {
    DeletePolicy::for_allowed_roots(w, RootDev(1), 7, roots, planned)
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Why {
    WrongKind,
    NotADirectory,
    ChangedSincePlan,
    ProtectedPath,
}

fn why(r: &Result<DeletePolicy, ForAllowedError>) -> Option<Why> {
    match r {
        Ok(_) => None,
        Err(ForAllowedError::WrongKind) => Some(Why::WrongKind),
        Err(ForAllowedError::NotADirectory) => Some(Why::NotADirectory),
        Err(ForAllowedError::ChangedSincePlan) => Some(Why::ChangedSincePlan),
        Err(ForAllowedError::ProtectedPath) => Some(Why::ProtectedPath),
    }
}

fn assert_why(r: &Result<DeletePolicy, ForAllowedError>, want: Option<Why>, what: &str) {
    assert_eq!(why(r), want, "{what}");
}

fn name_of(p: &str) -> &std::ffi::OsStr {
    Path::new(p).file_name().expect("has a name")
}

fn decision(kind: Kind, path: &str, ident: Identity, roots: &RootSet) -> GuardDecision {
    decide_path_common(&facts(kind, path, ident), roots)
}

fn is_protected_skip(d: &GuardDecision) -> bool {
    matches!(d, GuardDecision::Skip(SkipReason::ProtectedPath))
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

/// `$HOME` first, then every ancestor up to `/`.
fn expected_chain() -> Vec<Identity> {
    let mut v = Vec::new();
    let mut p = Some(Path::new(HOME));
    while let Some(d) = p {
        v.push(ident_of(d.to_str().unwrap()));
        p = d.parent();
    }
    v
}

// ---------------------------------------------------------------- the constructor takes the RootSet

#[test]
fn for_allowed_roots_should_make_a_policy_from_the_root_set_When_the_plan_agrees_with_the_witness()
{
    let target = "/srv/work/app";
    let id = ident_of(target);
    let w = explorer_witness(target, id, "/srv/work");
    let rs = roots();
    let p = build(&w, &rs, dir_planned(id)).expect("a matching plan makes a policy");
    assert_eq!(p.home, rs.home(), "home comes from the RootSet");
    assert_eq!(p.home_id, rs.home_id(), "home_id comes from the RootSet");
    assert_eq!(
        p.scan_roots,
        rs.scan_roots().to_vec(),
        "scan_roots come from the RootSet"
    );
    assert_eq!(p.root_dev, RootDev(1));
    assert_eq!(p.validate(name_of(target)), Ok(()));
}

#[test]
fn safety_for_allowed_roots_should_judge_by_the_root_set_not_by_the_witness_grant_When_the_grant_was_broader(
) {
    // The witness was minted under a scope_dir that covers /h/a/b/c/code/.. (everything below /h); the RootSet
    // under test has no scan root above `/h/a/b/c/code` and `/srv/work`, so the policy must refuse.
    let target = "/h/other/x";
    let id = ident_of(target);
    let w = explorer_witness(target, id, "/h");
    let rs = roots();
    if let Ok(p) = build(&w, &rs, dir_planned(id)) {
        assert!(
            p.validate(name_of(target)).is_err(),
            "a target outside the RootSet's scan roots must not validate"
        );
    }
}

#[test]
fn safety_policy_should_not_inherit_home_or_roots_from_the_caller_When_the_root_set_differs() {
    // Two RootSets, same witness: the policy follows the one passed in.
    let target = "/srv/work/app";
    let id = ident_of(target);
    let w = explorer_witness(target, id, "/srv/work");
    let a = roots_with(HOME, &[WORK]);
    let b = roots_with(HOME, &["/srv/elsewhere"]);
    let pa = build(&w, &a, dir_planned(id)).expect("first set");
    assert_eq!(pa.validate(name_of(target)), Ok(()));
    if let Ok(pb) = build(&w, &b, dir_planned(id)) {
        assert!(
            pb.validate(name_of(target)).is_err(),
            "{target} is not below any scan root of the second RootSet"
        );
    }
}

// ---------------------------------------------------------------- ChangedSincePlan and witness_matches

#[test]
fn safety_for_allowed_roots_should_return_changed_since_plan_When_the_planned_device_differs() {
    let target = "/srv/work/app";
    let id = ident_of(target);
    let w = explorer_witness(target, id, "/srv/work");
    let mut pl = dir_planned(id);
    pl.dev += 1;
    assert_why(
        &build(&w, &roots(), pl),
        Some(Why::ChangedSincePlan),
        "device differs, inode equal",
    );
}

#[test]
fn safety_for_allowed_roots_should_return_changed_since_plan_When_the_planned_inode_differs() {
    let target = "/srv/work/app";
    let id = ident_of(target);
    let w = explorer_witness(target, id, "/srv/work");
    let mut pl = dir_planned(id);
    pl.ino += 1;
    assert_why(
        &build(&w, &roots(), pl),
        Some(Why::ChangedSincePlan),
        "inode differs, device equal",
    );
    pl.ino = id.ino ^ 0x8000_0000_0000_0000;
    assert_why(
        &build(&w, &roots(), pl),
        Some(Why::ChangedSincePlan),
        "high inode bit flipped",
    );
}

#[test]
fn safety_for_allowed_roots_should_return_changed_since_plan_When_both_device_and_inode_differ() {
    let target = "/srv/work/app";
    let id = ident_of(target);
    let w = explorer_witness(target, id, "/srv/work");
    let mut pl = dir_planned(id);
    pl.dev += 1;
    pl.ino += 1;
    assert_why(
        &build(&w, &roots(), pl),
        Some(Why::ChangedSincePlan),
        "both differ",
    );
}

#[test]
fn safety_for_allowed_roots_should_return_changed_since_plan_When_the_planned_identity_is_that_of_another_directory(
) {
    let target = "/srv/work/app";
    let id = ident_of(target);
    let w = explorer_witness(target, id, "/srv/work");
    for other in ["/srv/work/other", "/srv/work/app2", "/h/a/b/c/code/x"] {
        assert_why(
            &build(&w, &roots(), dir_planned(ident_of(other))),
            Some(Why::ChangedSincePlan),
            other,
        );
    }
}

#[test]
fn witness_matches_should_hold_only_for_the_witness_identity_When_dev_or_ino_changes() {
    let target = "/srv/work/app";
    let id = ident_of(target);
    let w = explorer_witness(target, id, "/srv/work");
    let p = build(&w, &roots(), dir_planned(id)).expect("policy");
    assert!(p.witness_matches(id), "the witness identity matches");
    assert!(!p.witness_matches(Identity {
        dev: id.dev + 1,
        ..id
    }));
    assert!(!p.witness_matches(Identity {
        ino: id.ino + 1,
        ..id
    }));
    assert!(!p.witness_matches(Identity { dev: 0, ino: 0 }));
    assert!(!p.witness_matches(ident_of("/srv/work/other")));
}

// ---------------------------------------------------------------- WrongKind and NotADirectory

#[test]
fn safety_for_allowed_roots_should_return_wrong_kind_When_the_witness_is_of_another_kind() {
    let mut tried = 0;
    for (kind, path) in [
        (Kind::NodeModules, "/h/a/b/c/code/proj/node_modules"),
        (Kind::CargoTarget, "/h/a/b/c/code/proj/target"),
    ] {
        let id = ident_of(path);
        let w = mint(kind, path, id, "/h/a/b/c/code")
            .unwrap_or_else(|why| panic!("{kind:?} witness for {path} should be minted: {why}"));
        tried += 1;
        assert_why(
            &build(&w, &roots_with(HOME, &[CODE]), dir_planned(id)),
            Some(Why::WrongKind),
            &format!("{kind:?} witness, matching directory plan"),
        );
    }
    assert_eq!(tried, 2, "both other-kind witnesses must be exercised");
}

#[test]
fn safety_for_allowed_roots_should_return_not_a_directory_When_the_plan_recorded_anything_but_a_directory(
) {
    let target = "/srv/work/app";
    let id = ident_of(target);
    let w = explorer_witness(target, id, "/srv/work");
    for kind in [
        FileKind::File,
        FileKind::Symlink,
        FileKind::Fifo,
        FileKind::Socket,
        FileKind::CharDev,
        FileKind::BlockDev,
        FileKind::Other,
    ] {
        assert_why(
            &build(&w, &roots(), planned(id, kind)),
            Some(Why::NotADirectory),
            &format!("{kind:?}"),
        );
    }
    assert_why(
        &build(&w, &roots(), planned(id, FileKind::Dir)),
        None,
        "a directory plan is accepted",
    );
}

// ---------------------------------------------------------------- ProtectedPath

#[test]
fn safety_for_allowed_roots_should_return_protected_path_When_the_witness_is_home() {
    let id = ident_of(HOME);
    let w = explorer_witness(HOME, id, "/h");
    assert_why(
        &build(&w, &roots(), dir_planned(id)),
        Some(Why::ProtectedPath),
        "$HOME",
    );
}

#[test]
fn safety_for_allowed_roots_should_return_protected_path_When_the_witness_is_an_ancestor_of_home() {
    for anc in ["/h/a/b", "/h/a"] {
        let id = ident_of(anc);
        let w = explorer_witness(anc, id, "/h");
        assert_why(
            &build(&w, &roots(), dir_planned(id)),
            Some(Why::ProtectedPath),
            anc,
        );
    }
}

#[test]
fn safety_for_allowed_roots_should_return_protected_path_When_the_witness_is_a_scan_root() {
    for root in [CODE, WORK] {
        let id = ident_of(root);
        let scope_dir = if root == CODE { "/h" } else { "/srv" };
        let w = explorer_witness(root, id, scope_dir);
        assert_why(
            &build(&w, &roots(), dir_planned(id)),
            Some(Why::ProtectedPath),
            root,
        );
    }
}

#[test]
fn safety_for_allowed_roots_should_return_protected_path_for_every_scan_root_When_the_list_order_changes(
) {
    let all = [CODE, WORK, "/opt/data"];
    for order in permutations(&all) {
        let rs = roots_with(HOME, &order);
        for root in all {
            let id = ident_of(root);
            let scope_dir = match root {
                CODE => "/h",
                WORK => "/srv",
                _ => "/opt",
            };
            let w = explorer_witness(root, id, scope_dir);
            // Order independence: refused at construction or by the policy it yields, whatever the list order.
            let r = build(&w, &rs, dir_planned(id));
            match r {
                Err(ForAllowedError::ProtectedPath) => {}
                Err(e) => panic!("{root} with roots {order:?}: unexpected {e:?}"),
                Ok(p) => assert!(
                    p.validate(name_of(root)).is_err(),
                    "{root} is a scan root and must not validate (roots {order:?})"
                ),
            }
        }
        let below = "/opt/data/x";
        let id = ident_of(below);
        let w = explorer_witness(below, id, "/opt/data");
        assert_why(
            &build(&w, &rs, dir_planned(id)),
            None,
            &format!("{below} with roots {order:?}"),
        );
    }
}

#[test]
fn safety_for_allowed_roots_should_return_protected_path_When_a_second_spelling_has_the_identity_of_home_or_an_ancestor(
) {
    // A path that is none of $HOME, an ancestor or a scan root by name, but reports the identity of one.
    for (alias, of) in [
        ("/srv/work/alias-home", HOME),
        ("/srv/work/alias-parent", "/h/a/b"),
        ("/srv/work/alias-grandparent", "/h/a"),
        ("/srv/work/alias-top", "/h"),
    ] {
        let id = ident_of(of);
        let w = explorer_witness(alias, id, "/srv/work");
        assert_why(
            &build(&w, &roots(), dir_planned(id)),
            Some(Why::ProtectedPath),
            &format!("{alias} has the identity of {of}"),
        );
    }
}

#[test]
fn for_allowed_roots_should_accept_a_sibling_and_a_descendant_of_home_When_they_are_below_a_scan_root(
) {
    for ok in [
        "/h/a/b/c/code/proj",
        "/h/a/b/c/code/proj/deeper",
        "/srv/work/a",
    ] {
        let id = ident_of(ok);
        let scope_dir = if ok.starts_with("/srv") {
            "/srv/work"
        } else {
            "/h/a/b/c/code"
        };
        let w = explorer_witness(ok, id, scope_dir);
        assert_why(&build(&w, &roots(), dir_planned(id)), None, ok);
    }
}

// ---------------------------------------------------------------- RootSet home chain

#[test]
fn home_chain_should_start_with_home_and_list_every_ancestor_in_order_When_roots_are_resolved() {
    let rs = roots();
    assert_eq!(rs.home_id(), ident_of(HOME));
    assert_eq!(rs.home_chain().first().copied(), Some(rs.home_id()));
    assert_eq!(rs.home_chain(), expected_chain().as_slice());
}

#[test]
fn home_chain_should_not_depend_on_the_scan_root_order_When_roots_are_permuted() {
    let base = expected_chain();
    for order in permutations(&[CODE, WORK, "/opt/data"]) {
        let rs = roots_with(HOME, &order);
        assert_eq!(rs.home_chain(), base.as_slice(), "order {order:?}");
        assert_eq!(rs.home_id(), ident_of(HOME), "order {order:?}");
    }
}

#[test]
fn in_home_chain_should_be_true_for_home_and_each_ancestor_When_asked_by_identity() {
    let rs = roots();
    for p in [HOME, "/h/a/b", "/h/a", "/h", "/"] {
        assert!(rs.in_home_chain(ident_of(p)), "{p} is in the chain");
    }
}

#[test]
fn in_home_chain_should_be_false_for_siblings_descendants_and_strangers_When_asked_by_identity() {
    let rs = roots();
    for p in [
        "/h/a/b/sibling",
        "/h/a/sibling",
        "/h/sibling",
        "/h/a/b/c/code",
        "/h/a/b/c/code/proj",
        "/h/a/b/c/.cache",
        "/h/a/b/c/x",
        "/srv",
        "/srv/work",
        "/etc",
        "/hh",
    ] {
        assert!(
            !rs.in_home_chain(ident_of(p)),
            "{p} must not be in the chain"
        );
    }
    let id = ident_of(HOME);
    assert!(!rs.in_home_chain(Identity {
        dev: id.dev + 1,
        ino: id.ino
    }));
    assert!(!rs.in_home_chain(Identity {
        dev: id.dev,
        ino: id.ino + 1
    }));
}

// ---------------------------------------------------------------- decide_path_common and the chain

#[test]
fn safety_decide_path_common_should_skip_protected_path_When_an_explorer_path_has_the_identity_of_home_or_an_ancestor(
) {
    let rs = roots();
    for (i, id) in expected_chain().into_iter().enumerate() {
        let path = format!("/h/a/b/c/code/alias{i}");
        let d = decision(Kind::ExplorerSelection, &path, id, &rs);
        assert!(
            is_protected_skip(&d),
            "chain element {i} under another spelling must be skipped as ProtectedPath, got {d:?}"
        );
    }
}

#[test]
fn safety_decide_path_common_should_skip_protected_path_for_every_kind_When_the_identity_is_in_the_chain(
) {
    let rs = roots();
    let shapes: [(Kind, &str); 3] = [
        (Kind::ExplorerSelection, "/h/a/b/c/code/alias"),
        (Kind::NodeModules, "/h/a/b/c/code/proj/node_modules"),
        (Kind::CargoTarget, "/h/a/b/c/code/proj/target"),
    ];
    for (kind, path) in shapes {
        // control: a fresh identity is allowed, so the shape itself is fine
        let fresh = decision(kind, path, ident_of(&format!("{path}#fresh")), &rs);
        assert!(
            matches!(fresh, GuardDecision::Allow(_)),
            "control: {kind:?} {path} with a fresh identity should be allowed, got {fresh:?}"
        );
        for id in expected_chain() {
            let d = decision(kind, path, id, &rs);
            assert!(
                is_protected_skip(&d),
                "{kind:?} {path} with chain identity {id:?}: expected Skip(ProtectedPath), got {d:?}"
            );
        }
    }
}

#[test]
fn safety_decide_path_common_should_skip_the_filesystem_root_When_it_is_offered_as_a_path() {
    let rs = roots();
    let d = decision(Kind::ExplorerSelection, "/", ident_of("/"), &rs);
    assert!(
        matches!(d, GuardDecision::Skip(_)),
        "/ must be skipped: {d:?}"
    );
}

#[test]
fn decide_path_common_should_allow_siblings_and_descendants_of_home_When_their_identity_is_not_in_the_chain(
) {
    let rs = roots();
    for path in [
        "/h/a/b/c/code/proj",
        "/h/a/b/c/code/proj/deeper",
        "/srv/work/a",
    ] {
        let d = decision(Kind::ExplorerSelection, path, ident_of(path), &rs);
        assert!(
            matches!(d, GuardDecision::Allow(_)),
            "{path} is not in the chain and should be allowed: {d:?}"
        );
    }
}

#[test]
fn safety_decide_path_common_should_skip_protected_path_for_every_chain_member_When_scan_root_order_changes(
) {
    for order in permutations(&[CODE, WORK, "/opt/data"]) {
        let rs = roots_with(HOME, &order);
        for id in expected_chain() {
            let d = decision(Kind::ExplorerSelection, "/srv/work/alias", id, &rs);
            assert!(is_protected_skip(&d), "order {order:?}: {d:?}");
        }
        let ok = decision(
            Kind::ExplorerSelection,
            "/srv/work/fine",
            ident_of("/srv/work/fine"),
            &rs,
        );
        assert!(matches!(ok, GuardDecision::Allow(_)), "order {order:?}");
    }
}
