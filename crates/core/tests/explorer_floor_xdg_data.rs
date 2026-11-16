//! `XDG_DATA_HOME` below `$HOME` moves the keyring and gnupg directories (0.3.1 review finding 3). Kept in its
//! own file because it needs `RootSet::with_xdg_data_home`. Same author as the implementation, NOT an
//! independent one.

#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]

use std::io;
use std::path::{Path, PathBuf};

use dunnage_core::guard::{decide_path_common, Facts, GuardDecision};
use dunnage_core::roots::RootSet;
use dunnage_core::{Dirs, Identity, Kind, RootDev};

fn canon(p: &Path) -> io::Result<(PathBuf, Identity)> {
    let ino = match p.to_str().unwrap() {
        "/" => 1,
        "/home" => 2,
        "/home/u" => 3,
        _ => return Err(io::Error::from(io::ErrorKind::NotFound)),
    };
    Ok((p.to_path_buf(), Identity { dev: 1, ino }))
}

fn roots(data_home: Option<&str>) -> RootSet {
    let dirs = Dirs {
        home: "/home/u".into(),
        state: "/home/u/.local/state".into(),
        cache: "/home/u/.cache".into(),
        config: "/home/u/.config".into(),
    };
    RootSet::resolve(&dirs, &[], Some(&[]), Path::new("/"), &canon)
        .unwrap()
        .with_xdg_data_home(data_home.map(Path::new))
}

fn skipped(r: &RootSet, p: &str) -> bool {
    let f = Facts {
        kind: Kind::ExplorerSelection,
        path: p.into(),
        ident: Identity { dev: 1, ino: 99 },
        is_symlink: false,
        root_dev: RootDev(1),
        is_mount_point: false,
    };
    matches!(decide_path_common(&f, r), GuardDecision::Skip(_))
}

#[test]
fn safety_guard_should_refuse_keyrings_and_gnupg_below_a_moved_xdg_data_home() {
    let r = roots(Some("/home/u/data"));
    for p in ["/home/u/data/keyrings/x", "/home/u/data/gnupg/x/y"] {
        assert!(skipped(&r, p), "{p}");
    }
    assert!(!skipped(&r, "/home/u/data/other/x"));
}

#[test]
fn guard_should_ignore_an_xdg_data_home_outside_home() {
    let r = roots(Some("/srv/data"));
    assert!(!skipped(&r, "/srv/data/keyrings/x"));
}
