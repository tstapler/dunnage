#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! `decide_steam` table (Story 4.2.2): the pure order of checks, the newest-mtime age rule and the saves rule.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use disk_cleanup_core::guard::Facts;
use disk_cleanup_core::roots::{RootSet, SteamLibraryRoot};
use disk_cleanup_core::selector::Discard;
use disk_cleanup_core::targets::steam_guard::{
    decide_steam, SteamGuard, SteamGuardFacts, DETAIL_MTIME_UNKNOWN, DETAIL_NOW_INSTALLED,
};
use disk_cleanup_core::{Dirs, Identity, Kind, RootDev, SkipReason};

const NOW: i64 = 10_000_000;
const DAY: i64 = 86_400;
const MIN_AGE: Duration = Duration::from_secs(30 * 86_400);

fn roots() -> RootSet {
    let dirs = Dirs {
        home: "/h".into(),
        state: "/h/.local/state".into(),
        cache: "/h/.cache".into(),
        config: "/h/.config".into(),
    };
    let canon = |p: &Path| -> io::Result<(PathBuf, Identity)> {
        Ok((
            p.to_path_buf(),
            Identity {
                dev: 28,
                ino: p.as_os_str().len() as u64,
            },
        ))
    };
    RootSet::resolve(&dirs, &["/h/code".into()], None, Path::new("/h"), &canon)
        .unwrap()
        .with_steam_libraries(vec![
            SteamLibraryRoot {
                path: "/lib".into(),
                mounted: true,
            },
            SteamLibraryRoot {
                path: "/gone".into(),
                mounted: false,
            },
        ])
}

fn facts(kind: Kind, path: &str, newest_days_ago: i64) -> SteamGuardFacts {
    SteamGuardFacts {
        path: Facts {
            kind,
            path: path.into(),
            ident: Identity { dev: 28, ino: 7 },
            is_symlink: false,
            root_dev: RootDev(28),
            is_mount_point: false,
        },
        planned: Identity { dev: 28, ino: 7 },
        still_orphan: true,
        newest_mtime: Some(NOW - newest_days_ago * DAY),
        saves_dir: None,
    }
}

fn prefix(days: i64) -> SteamGuardFacts {
    facts(Kind::SteamPrefix, "/lib/steamapps/compatdata/99999", days)
}

fn skip(g: SteamGuard) -> (SkipReason, Option<String>) {
    match g {
        SteamGuard::Skip { reason, detail } => (reason, detail),
        SteamGuard::Allow(a) => panic!("expected a skip, got Allow({a:?})"),
    }
}

fn decide(f: &SteamGuardFacts, saves: Option<Discard>) -> SteamGuard {
    decide_steam(f, &roots(), NOW, MIN_AGE, saves)
}

#[test]
fn decide_should_allow_each_kind_When_old_orphan_with_no_saves() {
    for f in [
        prefix(31),
        facts(
            Kind::SteamShaderCache,
            "/lib/steamapps/shadercache/99999",
            31,
        ),
        facts(Kind::SteamRemnant, "/lib/steamapps/common/OldGame", 31),
    ] {
        assert!(matches!(decide(&f, None), SteamGuard::Allow(_)), "{f:?}");
    }
}

#[test]
fn decide_should_apply_the_age_rule_to_the_newest_file_inside_with_a_sharp_boundary() {
    assert!(matches!(decide(&prefix(30), None), SteamGuard::Allow(_)));
    assert_eq!(skip(decide(&prefix(29), None)).0, SkipReason::TooRecent);
    for f in [
        facts(
            Kind::SteamShaderCache,
            "/lib/steamapps/shadercache/99999",
            2,
        ),
        facts(Kind::SteamRemnant, "/lib/steamapps/common/OldGame", 2),
    ] {
        assert_eq!(skip(decide(&f, None)).0, SkipReason::TooRecent);
    }
}

#[test]
fn decide_should_fail_closed_as_too_recent_When_the_newest_mtime_is_unknown() {
    let mut f = prefix(90);
    f.newest_mtime = None;
    assert_eq!(
        skip(decide(&f, None)),
        (SkipReason::TooRecent, Some(DETAIL_MTIME_UNKNOWN.into()))
    );
}

#[test]
fn decide_should_skip_may_hold_saves_naming_the_dir_unless_discard_or_not_a_prefix() {
    let mut f = prefix(90);
    f.saves_dir = Some("Documents".into());
    assert_eq!(
        skip(decide(&f, None)),
        (SkipReason::MayHoldSaves, Some("Documents".into()))
    );
    assert!(matches!(
        decide(&f, Some(Discard::Discard)),
        SteamGuard::Allow(_)
    ));
    let mut shader = facts(
        Kind::SteamShaderCache,
        "/lib/steamapps/shadercache/99999",
        90,
    );
    shader.saves_dir = Some("Documents".into());
    assert!(matches!(decide(&shader, None), SteamGuard::Allow(_)));
}

#[test]
fn decide_should_check_age_before_saves() {
    let mut f = prefix(1);
    f.saves_dir = Some("AppData".into());
    assert_eq!(skip(decide(&f, None)).0, SkipReason::TooRecent);
}

#[test]
fn decide_should_skip_symlink_before_anything_else() {
    let mut f = prefix(1);
    f.path.is_symlink = true;
    f.newest_mtime = None;
    assert_eq!(skip(decide(&f, None)).0, SkipReason::Symlink);
}

#[test]
fn decide_should_skip_identity_changed_When_the_inode_differs_or_a_manifest_now_names_it() {
    let mut f = prefix(90);
    f.path.ident = Identity { dev: 28, ino: 8 };
    assert_eq!(skip(decide(&f, None)).0, SkipReason::IdentityChanged);
    let mut g = prefix(90);
    g.still_orphan = false;
    assert_eq!(
        skip(decide(&g, None)),
        (
            SkipReason::IdentityChanged,
            Some(DETAIL_NOW_INSTALLED.into())
        )
    );
}

#[test]
fn decide_should_skip_cross_device_and_mount_point() {
    let mut f = prefix(90);
    f.path.ident = Identity { dev: 99, ino: 7 };
    f.planned = f.path.ident;
    assert_eq!(skip(decide(&f, None)).0, SkipReason::CrossDevice);
    let mut m = prefix(90);
    m.path.is_mount_point = true;
    assert_eq!(skip(decide(&m, None)).0, SkipReason::MountPoint);
}

#[test]
fn decide_should_skip_outside_root_When_the_name_or_place_is_wrong() {
    for (kind, path) in [
        (Kind::SteamPrefix, "/lib/steamapps/compatdata/not-a-number"),
        (Kind::SteamPrefix, "/lib/steamapps/compatdata/99999/pfx"),
        (Kind::SteamPrefix, "/gone/steamapps/compatdata/99999"),
        (Kind::SteamPrefix, "/other/steamapps/compatdata/99999"),
        (Kind::SteamPrefix, "/lib/steamapps/shadercache/99999"),
        (Kind::SteamShaderCache, "/lib/steamapps/compatdata/99999"),
        (Kind::SteamRemnant, "/lib/steamapps/common/.hidden"),
        (Kind::SteamRemnant, "/lib/steamapps/common/a/b"),
        (Kind::SteamRemnant, "/lib/steamapps/common/../compatdata"),
        (Kind::SteamRemnant, "/lib/steamapps/common"),
    ] {
        assert_eq!(
            skip(decide(&facts(kind, path, 90), None)).0,
            SkipReason::OutsideRoot,
            "{path}"
        );
    }
}
