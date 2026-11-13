//! Task 6.6.3f (review 2, N4): `DeletePolicy::for_allowed_roots` reads `home`, `home_id` and the scan roots
//! from the `RootSet` and exists only for the identity the plan recorded.
#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]

use std::io;
use std::path::{Path, PathBuf};

use dunnage_core::floor_rule::ForAllowedError;
use dunnage_core::guard::{decide_path_common, Facts, GuardDecision};
use dunnage_core::plan::ItemIdentity;
use dunnage_core::roots::RootSet;
use dunnage_core::{
    Allowed, AllowedTarget, DeleteError, DeletePolicy, Dirs, FileKind, Identity, Kind, RootDev,
};

fn id(path: &str) -> Identity {
    let mut ino: u64 = 0xcbf2_9ce4_8422_2325;
    for b in path.bytes() {
        ino ^= u64::from(b);
        ino = ino.wrapping_mul(0x0100_0000_01b3);
    }
    Identity {
        dev: 1,
        ino: ino | 1,
    }
}

#[allow(clippy::unnecessary_wraps)]
fn canon(p: &Path) -> io::Result<(PathBuf, Identity)> {
    Ok((p.to_path_buf(), id(p.to_str().unwrap())))
}

fn dirs(home: &str) -> Dirs {
    Dirs {
        home: home.into(),
        state: format!("{home}/.local/state").into(),
        cache: format!("{home}/.cache").into(),
        config: format!("{home}/.config").into(),
    }
}

fn roots(home: &str, scan: &[&str]) -> RootSet {
    let scan: Vec<PathBuf> = scan.iter().map(PathBuf::from).collect();
    RootSet::resolve(&dirs(home), &scan, None, Path::new("/"), &canon).unwrap()
}

/// The witness the guard mints under a permissive grant (decoy home, one wide root above the target).
fn witness_of(kind: Kind, target: &str) -> Allowed {
    let wide = roots("/wide-home", &["/wide"]);
    let f = Facts {
        kind,
        path: target.into(),
        ident: id(target),
        is_symlink: false,
        root_dev: RootDev(1),
        is_mount_point: false,
    };
    match decide_path_common(&f, &wide) {
        GuardDecision::Allow(a) => a,
        GuardDecision::Skip(r) => panic!("{target}: {r:?}"),
    }
}

fn planned(target: &str) -> ItemIdentity {
    let i = id(target);
    ItemIdentity {
        dev: i.dev,
        ino: i.ino,
        kind: FileKind::Dir,
        btime: None,
    }
}

const TARGET: &str = "/wide/app/dist";

fn real_roots() -> RootSet {
    roots("/wide-home", &["/wide"])
}

fn build(roots: &RootSet, planned: ItemIdentity) -> Result<DeletePolicy, ForAllowedError> {
    DeletePolicy::for_allowed_roots(
        &witness_of(Kind::ExplorerSelection, TARGET),
        RootDev(1),
        7,
        roots,
        planned,
    )
}

#[test]
fn for_allowed_roots_should_read_home_home_id_and_scan_roots_from_the_root_set() {
    let r = roots("/home/u", &["/home/u/code", "/home/u/dotfiles"]);
    let p = DeletePolicy::for_allowed_roots(
        &witness_of(Kind::ExplorerSelection, TARGET),
        RootDev(1),
        7,
        &r,
        planned(TARGET),
    )
    .unwrap();
    assert_eq!(p.home, PathBuf::from("/home/u"));
    assert_eq!(p.home_id, id("/home/u"));
    assert_eq!(p.home_id, r.home_id());
    assert_eq!(p.scan_roots, r.scan_roots().to_vec());
    assert_eq!(p.root_dev, RootDev(1));
    assert_eq!(p.root_mnt, Some(7));
    assert_eq!(p.target, PathBuf::from(TARGET));
    assert!(matches!(p.allowed, AllowedTarget::BelowScanRoot(_)));
    assert!(p.require_sibling_any.is_empty());
}

#[test]
fn safety_for_allowed_roots_should_refuse_a_target_outside_the_real_roots_When_the_witness_came_from_a_wider_grant(
) {
    // The N4 trap: with loose `scan_roots = ["/"]` the policy would admit this; with the RootSet it cannot.
    let r = roots("/home/u", &["/home/u/code"]);
    let p = build(&r, planned(TARGET)).unwrap();
    assert_eq!(p.validate("dist".as_ref()), Err(DeleteError::OutsideScope));
}

#[test]
fn for_allowed_roots_should_accept_a_target_below_a_real_root() {
    let p = build(&real_roots(), planned(TARGET)).unwrap();
    assert_eq!(p.validate("dist".as_ref()), Ok(()));
}

#[test]
fn safety_for_allowed_roots_should_refuse_When_the_planned_inode_differs_from_the_witness() {
    let mut other = planned(TARGET);
    other.ino ^= 1;
    assert_eq!(
        build(&real_roots(), other).unwrap_err(),
        ForAllowedError::ChangedSincePlan
    );
}

#[test]
fn safety_for_allowed_roots_should_refuse_When_the_planned_device_differs_from_the_witness() {
    let mut other = planned(TARGET);
    other.dev += 1;
    assert_eq!(
        build(&real_roots(), other).unwrap_err(),
        ForAllowedError::ChangedSincePlan
    );
}

#[test]
fn for_allowed_roots_should_refuse_When_the_plan_recorded_something_other_than_a_directory() {
    for kind in [FileKind::File, FileKind::Symlink, FileKind::Other] {
        let mut other = planned(TARGET);
        other.kind = kind;
        assert_eq!(
            build(&real_roots(), other).unwrap_err(),
            ForAllowedError::NotADirectory,
            "{kind:?}"
        );
    }
}

#[test]
fn for_allowed_roots_should_refuse_a_witness_of_another_kind() {
    let w = witness_of(Kind::NodeModules, "/wide/app/node_modules");
    let r = DeletePolicy::for_allowed_roots(
        &w,
        RootDev(1),
        7,
        &real_roots(),
        planned("/wide/app/node_modules"),
    );
    assert_eq!(r.unwrap_err(), ForAllowedError::WrongKind);
}

#[test]
fn safety_for_allowed_roots_should_refuse_a_witness_whose_identity_is_home_or_an_ancestor() {
    // The witness was minted before the chain was known to include its identity (a second `RootSet`).
    let chain = {
        let canon_alias = |p: &Path| -> io::Result<(PathBuf, Identity)> {
            let ident = if p == Path::new("/home") {
                id(TARGET)
            } else {
                id(p.to_str().unwrap())
            };
            Ok((p.to_path_buf(), ident))
        };
        RootSet::resolve(
            &dirs("/home/u"),
            &["/home/u/code".into()],
            None,
            Path::new("/"),
            &canon_alias,
        )
        .unwrap()
    };
    assert!(chain.in_home_chain(id(TARGET)));
    assert_eq!(
        build(&chain, planned(TARGET)).unwrap_err(),
        ForAllowedError::ProtectedPath
    );
}

#[test]
fn for_allowed_roots_should_check_the_kind_before_the_identity() {
    // A wrong-kind witness with a wrong identity is a kind error, not a stale-plan error.
    let w = witness_of(Kind::NodeModules, "/wide/app/node_modules");
    let mut p = planned("/wide/app/node_modules");
    p.ino ^= 1;
    let r = DeletePolicy::for_allowed_roots(&w, RootDev(1), 7, &real_roots(), p);
    assert_eq!(r.unwrap_err(), ForAllowedError::WrongKind);
}

#[test]
fn witness_matches_should_be_true_only_for_the_identity_the_witness_saw() {
    let p = build(&real_roots(), planned(TARGET)).unwrap();
    assert!(p.witness_matches(id(TARGET)));
    assert!(!p.witness_matches(Identity {
        dev: id(TARGET).dev,
        ino: id(TARGET).ino ^ 1
    }));
    assert!(!p.witness_matches(Identity {
        dev: id(TARGET).dev + 1,
        ino: id(TARGET).ino
    }));
}

#[test]
fn witness_matches_should_not_bind_a_policy_that_has_no_witness() {
    let mut p = build(&real_roots(), planned(TARGET)).unwrap();
    p.allowed = AllowedTarget::Exact(TARGET.into());
    assert!(p.witness_matches(Identity { dev: 9, ino: 9 }));
}
