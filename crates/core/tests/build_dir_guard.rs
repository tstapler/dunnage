#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! `decide_build_dir` table (Story 3.2.1): the pure order of checks and the 60-minute floor.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use disk_cleanup_core::guard::Facts;
use disk_cleanup_core::roots::RootSet;
use disk_cleanup_core::targets::build_dir::{
    decide_build_dir, effective_min_age, BuildFacts, BuildGuard, Reproducibility, BUILD_DIR_MIN_AGE,
};
use disk_cleanup_core::{Dirs, Identity, Kind, RootDev, SkipReason};

const NOW: i64 = 1_000_000;
const MIN: i64 = 60;

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
    RootSet::resolve(&dirs, &["/h/code".into()], None, Path::new("/h"), &canon).unwrap()
}

fn facts(kind: Kind, newest_minutes_ago: i64) -> BuildFacts {
    let path = match kind {
        Kind::NodeModules => "/h/code/app/node_modules",
        _ => "/h/code/app/target",
    };
    BuildFacts {
        path: Facts {
            kind,
            path: path.into(),
            ident: Identity { dev: 28, ino: 7 },
            is_symlink: false,
            root_dev: RootDev(28),
            is_mount_point: false,
        },
        planned: Identity { dev: 28, ino: 7 },
        marker_present: true,
        newest_mtime: Some(NOW - newest_minutes_ago * MIN),
        reproducibility: Reproducibility::Reproducible,
    }
}

fn skip(g: BuildGuard) -> (SkipReason, Option<String>) {
    match g {
        BuildGuard::Skip { reason, detail } => (reason, detail),
        BuildGuard::Allow(a) => panic!("expected a skip, got Allow({a:?})"),
    }
}

fn mins(m: u64) -> Duration {
    Duration::from_secs(m * 60)
}

#[test]
fn guard_should_allow_a_reproducible_old_build_dir() {
    let g = decide_build_dir(&facts(Kind::NodeModules, 61), &roots(), NOW, mins(5));
    assert!(matches!(g, BuildGuard::Allow(_)));
}

#[test]
fn guard_should_skip_identity_changed_When_the_sibling_marker_is_gone() {
    let mut f = facts(Kind::NodeModules, 600);
    f.marker_present = false;
    assert_eq!(
        skip(decide_build_dir(&f, &roots(), NOW, mins(60))).0,
        SkipReason::IdentityChanged
    );
}

#[test]
fn guard_should_skip_identity_changed_When_inode_differs_from_the_plan() {
    let mut f = facts(Kind::CargoTarget, 600);
    f.planned = Identity { dev: 28, ino: 8 };
    assert_eq!(
        skip(decide_build_dir(&f, &roots(), NOW, mins(60))).0,
        SkipReason::IdentityChanged
    );
}

#[test]
fn guard_should_run_the_common_checks_first() {
    let mut sym = facts(Kind::CargoTarget, 600);
    sym.path.is_symlink = true;
    sym.marker_present = false;
    assert_eq!(
        skip(decide_build_dir(&sym, &roots(), NOW, mins(60))).0,
        SkipReason::Symlink
    );

    let mut dev = facts(Kind::NodeModules, 600);
    dev.path.ident.dev = 34;
    assert_eq!(
        skip(decide_build_dir(&dev, &roots(), NOW, mins(60))).0,
        SkipReason::CrossDevice
    );

    let mut out = facts(Kind::NodeModules, 600);
    out.path.path = "/h/other/node_modules".into();
    assert_eq!(
        skip(decide_build_dir(&out, &roots(), NOW, mins(60))).0,
        SkipReason::OutsideRoot
    );
}

#[test]
fn guard_should_skip_too_recent_When_newest_is_20_minutes_old_whatever_min_age_says() {
    for flag in [mins(60), mins(5), Duration::ZERO] {
        let (r, d) = skip(decide_build_dir(
            &facts(Kind::CargoTarget, 20),
            &roots(),
            NOW,
            flag,
        ));
        assert_eq!(r, SkipReason::TooRecent, "{flag:?}");
        if flag < BUILD_DIR_MIN_AGE {
            assert_eq!(
                d.as_deref(),
                Some("younger than --min-age 60m (build-dir floor)")
            );
        }
    }
}

#[test]
fn guard_should_honour_a_higher_min_age_without_the_floor_wording() {
    let two_h = Duration::from_secs(2 * 3600);
    let (r, d) = skip(decide_build_dir(
        &facts(Kind::CargoTarget, 90),
        &roots(),
        NOW,
        two_h,
    ));
    assert_eq!(r, SkipReason::TooRecent);
    assert_eq!(d.as_deref(), Some("younger than --min-age 2h"));
}

#[test]
fn guard_should_allow_at_61_minutes_with_a_5m_flag_and_skip_at_exactly_59() {
    assert!(matches!(
        decide_build_dir(&facts(Kind::CargoTarget, 61), &roots(), NOW, mins(5)),
        BuildGuard::Allow(_)
    ));
    assert!(matches!(
        decide_build_dir(&facts(Kind::CargoTarget, 59), &roots(), NOW, mins(5)),
        BuildGuard::Skip { .. }
    ));
    assert!(matches!(
        decide_build_dir(&facts(Kind::CargoTarget, 60), &roots(), NOW, mins(5)),
        BuildGuard::Allow(_)
    ));
}

#[test]
fn guard_should_fail_closed_When_newest_mtime_is_unknown() {
    let mut f = facts(Kind::CargoTarget, 600);
    f.newest_mtime = None;
    assert_eq!(
        skip(decide_build_dir(&f, &roots(), NOW, mins(60))).0,
        SkipReason::TooRecent
    );
}

#[test]
fn guard_should_skip_not_reproducible_with_the_detail_for_node_modules_only() {
    for (r, want) in [
        (
            Reproducibility::LinkedOutside,
            "an npm-linked package points outside the project",
        ),
        (
            Reproducibility::NoLockfile,
            "no lockfile next to node_modules",
        ),
    ] {
        let mut f = facts(Kind::NodeModules, 600);
        f.reproducibility = r.clone();
        let (reason, detail) = skip(decide_build_dir(&f, &roots(), NOW, mins(60)));
        assert_eq!(reason, SkipReason::NotReproducible);
        assert_eq!(detail.as_deref(), Some(want));
        // a cargo target is rebuilt from source, so the same facts do not apply
        let mut c = facts(Kind::CargoTarget, 600);
        c.reproducibility = r;
        assert!(matches!(
            decide_build_dir(&c, &roots(), NOW, mins(60)),
            BuildGuard::Allow(_)
        ));
    }
    let mut f = facts(Kind::NodeModules, 600);
    f.reproducibility = Reproducibility::NotChecked;
    assert_eq!(
        skip(decide_build_dir(&f, &roots(), NOW, mins(60))).0,
        SkipReason::NotReproducible
    );
}

#[test]
fn effective_min_age_should_raise_below_the_floor_and_report_it() {
    assert_eq!(effective_min_age(Duration::ZERO), (BUILD_DIR_MIN_AGE, true));
    assert_eq!(effective_min_age(mins(5)), (BUILD_DIR_MIN_AGE, true));
    assert_eq!(effective_min_age(mins(60)), (BUILD_DIR_MIN_AGE, false));
    assert_eq!(effective_min_age(mins(90)), (mins(90), false));
}
