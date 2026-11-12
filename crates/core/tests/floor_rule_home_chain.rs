//! Task 6.6.3f (review 2, N3): `RootSet` keeps the identity of `$HOME` and of each ancestor, and
//! `decide_path_common` refuses an entry with one of those identities for every kind, whatever its spelling.
#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]

use std::io;
use std::path::{Path, PathBuf};

use dunnage_core::guard::{decide_path_common, Facts, GuardDecision};
use dunnage_core::roots::RootSet;
use dunnage_core::{Dirs, Identity, Kind, RootDev, SkipReason};

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

fn roots() -> RootSet {
    RootSet::resolve(
        &dirs("/data/home/u"),
        &["/data/home/u/code".into()],
        None,
        Path::new("/"),
        &canon,
    )
    .unwrap()
}

fn facts(kind: Kind, path: &str, ident: Identity) -> Facts {
    Facts {
        kind,
        path: path.into(),
        ident,
        is_symlink: false,
        root_dev: RootDev(1),
        is_mount_point: false,
    }
}

#[test]
fn root_set_should_keep_home_and_every_ancestor_by_identity_with_home_first() {
    let r = roots();
    assert_eq!(
        r.home_chain(),
        [id("/data/home/u"), id("/data/home"), id("/data"), id("/")]
    );
    assert_eq!(r.home_id(), id("/data/home/u"));
    assert_eq!(r.home_chain()[0], r.home_id());
}

#[test]
fn in_home_chain_should_be_true_for_home_and_each_ancestor_and_false_for_anything_else() {
    let r = roots();
    for p in ["/data/home/u", "/data/home", "/data", "/"] {
        assert!(r.in_home_chain(id(p)), "{p}");
    }
    for p in ["/data/home/u/code", "/data/other", "/data/home/v"] {
        assert!(!r.in_home_chain(id(p)), "{p}");
    }
    // The device is part of the identity.
    let other_dev = Identity {
        dev: 2,
        ino: id("/data").ino,
    };
    assert!(!r.in_home_chain(other_dev));
}

#[test]
fn safety_decide_path_common_should_refuse_an_ancestor_of_home_by_identity_When_it_is_reached_under_another_spelling(
) {
    let r = roots();
    // `/data/home/u/code/alias` is a bind mount of `/data/home` (same device and inode, another path).
    for ancestor in ["/data/home/u", "/data/home", "/data", "/"] {
        for kind in Kind::ALL {
            let f = facts(kind, "/data/home/u/code/alias", id(ancestor));
            match decide_path_common(&f, &r) {
                GuardDecision::Skip(SkipReason::ProtectedPath) => {}
                other => panic!("{kind:?} {ancestor}: expected ProtectedPath, got {other:?}"),
            }
        }
    }
}

#[test]
fn decide_path_common_should_still_allow_an_ordinary_directory_below_a_root_When_its_identity_is_not_in_the_chain(
) {
    let r = roots();
    let p = "/data/home/u/code/app";
    let f = facts(Kind::ExplorerSelection, p, id(p));
    assert!(matches!(
        decide_path_common(&f, &r),
        GuardDecision::Allow(_)
    ));
}
