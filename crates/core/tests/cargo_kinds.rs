#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! The cargo-aware kinds (`cargo-artifact`, `cargo-home`): where the roots admit them, what the floor rule
//! says about their shape, and the pure decisions made before a delete.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use dunnage_core::guard::Facts;
use dunnage_core::roots::RootSet;
use dunnage_core::selector::{default_min_age_secs, KindSelector, Selection};
use dunnage_core::targets::build_dir::BuildGuard;
use dunnage_core::targets::cargo::{
    cargo_home_min_age, decide_cargo_home, decide_cargo_part, is_cargo_target, profile_selected,
    tag_has_signature, CargoHomeFacts, CargoPartFacts, DETAIL_NOT_A_CARGO_TARGET,
};
use dunnage_core::{
    AllowedTarget, CargoPart, DeleteError, DeletePolicy, Dirs, Identity, Kind, NamePattern,
    RootDev, SelectorId, SkipReason,
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

fn dirs() -> Dirs {
    Dirs {
        home: "/h/u".into(),
        state: "/h/u/.local/state".into(),
        cache: "/h/u/.cache".into(),
        config: "/h/u/.config".into(),
    }
}

fn roots() -> RootSet {
    RootSet::resolve(&dirs(), &["/h/u/code".into()], None, Path::new("/"), &canon).unwrap()
}

const NOW: i64 = 1_800_000_000;
const DAY: i64 = 86_400;

fn facts(kind: Kind, path: &str) -> Facts {
    Facts {
        kind,
        path: path.into(),
        ident: id(path),
        is_symlink: false,
        root_dev: RootDev(1),
        is_mount_point: false,
    }
}

#[test]
fn kinds_should_sit_in_the_documented_tiers_and_selectors() {
    assert_eq!(Kind::CargoArtifact.id(), "cargo-artifact");
    assert_eq!(Kind::CargoArtifact.family(), "build-dirs");
    assert_eq!(Kind::CargoArtifact.risk(), dunnage_core::Risk::Destructive);
    assert_eq!(Kind::CargoHome.id(), "cargo-home");
    assert_eq!(Kind::CargoHome.risk(), dunnage_core::Risk::Guarded);
    assert!(SelectorId::BuildDirs.kinds().contains(&Kind::CargoArtifact));
    assert_eq!(SelectorId::CargoHome.kinds(), &[Kind::CargoHome]);
    assert!(!SelectorId::CargoHome.is_destructive());
    assert!(SelectorId::BuildDirs.is_destructive());
    assert_eq!(
        default_min_age_secs(SelectorId::CargoHome),
        Some(30 * 86_400)
    );
}

#[test]
fn cargo_home_should_not_be_in_the_default_apply_set() {
    assert!(!KindSelector::default_apply().contains(SelectorId::CargoHome));
}

#[test]
fn the_cargo_home_age_floor_should_be_one_day_whatever_the_flag() {
    let sel = {
        let mut s = Selection::new(
            KindSelector::of(&[SelectorId::CargoHome]),
            KindSelector::default(),
        );
        s.min_age_secs = Some(0);
        s
    };
    assert_eq!(sel.effective_min_age(SelectorId::CargoHome), Some(86_400));
    assert_eq!(
        cargo_home_min_age(Duration::from_secs(60)),
        Duration::from_hours(24)
    );
}

#[test]
fn cargo_profile_flag_should_need_build_dirs_and_be_echoed_sorted() {
    let mut s = Selection::new(
        KindSelector::of(&[SelectorId::Caches]),
        KindSelector::default(),
    );
    s.cargo_profiles = vec!["debug".into()];
    let err = s.validate().unwrap_err();
    assert_eq!(err.flag, "--cargo-profile");
    let mut s = Selection::new(
        KindSelector::of(&[SelectorId::BuildDirs]),
        KindSelector::default(),
    );
    s.cargo_profiles = vec!["release".into(), "debug".into(), "debug".into()];
    s.validate().unwrap();
    assert_eq!(s.flags(), "--cargo-profile debug --cargo-profile release");
}

#[test]
fn profile_selected_should_match_the_whole_profile_or_its_last_component() {
    assert!(profile_selected("debug", &[]));
    assert!(profile_selected("debug", &["debug".into()]));
    assert!(!profile_selected("release", &["debug".into()]));
    assert!(profile_selected(
        "x86_64-unknown-linux-gnu/debug",
        &["debug".into()]
    ));
    assert!(profile_selected(
        "x86_64-unknown-linux-gnu/debug",
        &["x86_64-unknown-linux-gnu/debug".into()]
    ));
    assert!(!profile_selected("debugger", &["debug".into()]));
}

#[test]
fn a_cargo_target_should_be_told_by_its_tag_signature_or_its_rustc_info_cache() {
    assert!(is_cargo_target(true, false));
    assert!(is_cargo_target(false, true));
    assert!(!is_cargo_target(false, false));
    assert!(tag_has_signature(
        b"Signature: 8a477f597d28d172789f06886806bc55\n# This file is a cache directory tag"
    ));
    assert!(!tag_has_signature(b"Signature: 0000"));
    assert!(!tag_has_signature(b""));
}

#[test]
fn cargo_part_names_should_round_trip_and_only_incremental_should_judge_itself() {
    for p in CargoPart::ALL {
        assert_eq!(CargoPart::from_dir_name(p.dir_name()), Some(p));
        assert_eq!(p.follows_profile(), p != CargoPart::Incremental);
    }
    assert_eq!(CargoPart::from_dir_name("out"), None);
    assert_eq!(CargoPart::from_dir_name("debug"), None);
}

#[test]
fn roots_should_admit_a_part_below_a_scan_root_in_a_target_and_nothing_else() {
    let r = roots();
    let ok = [
        "/h/u/code/app/target/debug/incremental",
        "/h/u/code/app/target/release/deps",
        "/h/u/code/app/target/debug/.fingerprint",
        "/h/u/code/app/target/debug/build",
        "/h/u/code/app/target/debug/examples",
        "/h/u/code/app/target/x86_64-unknown-linux-gnu/debug/incremental",
    ];
    for p in ok {
        assert!(r.admits(Kind::CargoArtifact, Path::new(p)), "{p}");
    }
    let no = [
        "/h/u/code/app/target/debug",
        "/h/u/code/app/target",
        "/h/u/code/app/target/debug/deps/libfoo.rlib",
        "/h/u/code/app/target/debug/out",
        "/h/u/code/app/debug/incremental",
        "/h/u/code/app/incremental",
        "/h/u/code/app/target/x/y/z/deps",
        "/h/u/other/app/target/debug/incremental",
        "/h/u/code/app/target/debug/../../src/incremental",
    ];
    for p in no {
        assert!(!r.admits(Kind::CargoArtifact, Path::new(p)), "{p}");
    }
}

#[test]
fn roots_should_admit_a_cargo_home_entry_two_levels_below_registry_src_or_git_checkouts() {
    let r = roots();
    for p in [
        "/h/u/.cargo/registry/src/index.crates.io-6f17d22bba15001f/serde-1.0.200",
        "/h/u/.cargo/git/checkouts/foo-1a2b3c/abcdef1",
    ] {
        assert!(r.admits(Kind::CargoHome, Path::new(p)), "{p}");
    }
    for p in [
        "/h/u/.cargo/registry/src",
        "/h/u/.cargo/registry/src/index.crates.io-6f17d22bba15001f",
        "/h/u/.cargo/registry/src/x/y/z",
        "/h/u/.cargo/registry/cache/x/y",
        "/h/u/.cargo/registry/src/x/.hidden",
        "/h/u/.cargo/git/db/foo.git",
        "/h/u/.cargo/bin/cargo",
        "/h/u/.cargo",
    ] {
        assert!(!r.admits(Kind::CargoHome, Path::new(p)), "{p}");
    }
}

#[test]
fn with_cargo_home_should_follow_cargo_home_below_home_and_refuse_everything_else() {
    let moved = roots().with_cargo_home(Some(Path::new("/h/u/dev/cargo")));
    assert!(moved.admits(
        Kind::CargoHome,
        Path::new("/h/u/dev/cargo/registry/src/i/c-1.0.0")
    ));
    assert!(!moved.admits(
        Kind::CargoHome,
        Path::new("/h/u/.cargo/registry/src/i/c-1.0.0")
    ));
    for bad in [
        "/",
        "/h/u",
        "/h",
        "/opt/cargo",
        "/h/u/../x",
        "relative/cargo",
    ] {
        let r = roots().with_cargo_home(Some(Path::new(bad)));
        assert_eq!(r.cargo_home(), None, "{bad}");
        assert!(r.allowed(Kind::CargoHome).is_empty(), "{bad}");
    }
    assert_eq!(
        roots().with_cargo_home(None).cargo_home(),
        Some(Path::new("/h/u/.cargo"))
    );
}

fn policy(allowed: AllowedTarget, target: &str) -> DeletePolicy {
    DeletePolicy {
        root_dev: RootDev(1),
        root_mnt: None,
        home: "/h/u".into(),
        home_id: id("/h/u"),
        scan_roots: vec!["/h/u/code".into()],
        allowed,
        target: target.into(),
        require_sibling_any: Vec::new(),
        max_depth: 256,
        restore_owner_write: true,
    }
}

fn within(root: &str, name: NamePattern) -> AllowedTarget {
    AllowedTarget::Within {
        root: root.into(),
        name,
    }
}

#[test]
fn the_floor_rule_should_require_the_part_shape_under_a_target() {
    let a = || within("/h/u/code", NamePattern::CargoTargetPart);
    let ok = |t: &str| {
        let p = policy(a(), t);
        p.validate(Path::new(t).file_name().unwrap())
    };
    assert_eq!(ok("/h/u/code/app/target/debug/incremental"), Ok(()));
    assert_eq!(
        ok("/h/u/code/app/target/aarch64-apple-darwin/release/deps"),
        Ok(())
    );
    for bad in [
        "/h/u/code/app/target/debug",
        "/h/u/code/app/src/incremental",
        "/h/u/code/app/debug/deps",
        "/h/u/code/app/target/debug/deps/x/build",
        "/h/u/code/app/target/debug/other",
        "/h/u/code/target/incremental",
    ] {
        assert_eq!(ok(bad), Err(DeleteError::OutsideScope), "{bad}");
    }
}

#[test]
fn the_floor_rule_should_require_depth_two_for_a_cargo_home_entry() {
    let a = || within("/h/u/.cargo/registry/src", NamePattern::CargoHomeEntry);
    let ok = |t: &str| policy(a(), t).validate(Path::new(t).file_name().unwrap());
    assert_eq!(ok("/h/u/.cargo/registry/src/idx/serde-1.0.0"), Ok(()));
    assert_eq!(
        ok("/h/u/.cargo/registry/src/idx"),
        Err(DeleteError::OutsideScope)
    );
    assert_eq!(
        ok("/h/u/.cargo/registry/src/idx/a/b"),
        Err(DeleteError::OutsideScope)
    );
    assert_eq!(
        ok("/h/u/.cargo/registry/src/idx/.git"),
        Err(DeleteError::OutsideScope)
    );
}

fn part_facts(path: &str, activity_days: Option<i64>) -> CargoPartFacts {
    CargoPartFacts {
        path: facts(Kind::CargoArtifact, path),
        planned: id(path),
        structure_ok: true,
        activity_mtime: activity_days.map(|d| NOW - d * DAY),
    }
}

const PART: &str = "/h/u/code/app/target/debug/incremental";

#[test]
fn a_part_should_be_allowed_When_its_activity_is_older_than_min_age() {
    let g = decide_cargo_part(
        &part_facts(PART, Some(20)),
        &roots(),
        NOW,
        Duration::from_hours(14 * 24),
    );
    assert!(matches!(g, BuildGuard::Allow(_)), "{g:?}");
}

#[test]
fn a_part_should_be_too_recent_When_younger_than_min_age_or_the_60m_floor() {
    let r = roots();
    let g = decide_cargo_part(
        &part_facts(PART, Some(2)),
        &r,
        NOW,
        Duration::from_hours(14 * 24),
    );
    assert!(matches!(
        g,
        BuildGuard::Skip {
            reason: SkipReason::TooRecent,
            ..
        }
    ));
    // --min-age 0 cannot go below 60 minutes
    let mut f = part_facts(PART, None);
    f.activity_mtime = Some(NOW - 30 * 60);
    let g = decide_cargo_part(&f, &r, NOW, Duration::ZERO);
    match g {
        BuildGuard::Skip { reason, detail } => {
            assert_eq!(reason, SkipReason::TooRecent);
            assert!(detail.unwrap().contains("60m"));
        }
        BuildGuard::Allow(_) => panic!("30 minutes old passed the 60 minute floor"),
    }
}

#[test]
fn a_part_should_fail_closed_When_the_activity_is_unknown_the_identity_changed_or_the_structure_went(
) {
    let r = roots();
    let age = Duration::from_hours(14 * 24);
    let unknown = decide_cargo_part(&part_facts(PART, None), &r, NOW, age);
    assert!(matches!(
        unknown,
        BuildGuard::Skip {
            reason: SkipReason::TooRecent,
            ..
        }
    ));
    let mut moved = part_facts(PART, Some(30));
    moved.planned = Identity { dev: 9, ino: 9 };
    assert!(matches!(
        decide_cargo_part(&moved, &r, NOW, age),
        BuildGuard::Skip {
            reason: SkipReason::IdentityChanged,
            ..
        }
    ));
    let mut gone = part_facts(PART, Some(30));
    gone.structure_ok = false;
    match decide_cargo_part(&gone, &r, NOW, age) {
        BuildGuard::Skip { reason, detail } => {
            assert_eq!(reason, SkipReason::IdentityChanged);
            assert_eq!(detail.as_deref(), Some(DETAIL_NOT_A_CARGO_TARGET));
        }
        BuildGuard::Allow(_) => panic!("a part of a non-cargo target passed"),
    }
}

#[test]
fn a_part_should_be_refused_by_the_common_path_checks() {
    let r = roots();
    let age = Duration::from_hours(14 * 24);
    let mut link = part_facts(PART, Some(30));
    link.path.is_symlink = true;
    assert!(matches!(
        decide_cargo_part(&link, &r, NOW, age),
        BuildGuard::Skip {
            reason: SkipReason::Symlink,
            ..
        }
    ));
    let mut other_dev = part_facts(PART, Some(30));
    other_dev.path.root_dev = RootDev(2);
    assert!(matches!(
        decide_cargo_part(&other_dev, &r, NOW, age),
        BuildGuard::Skip {
            reason: SkipReason::CrossDevice,
            ..
        }
    ));
    let mut mount = part_facts(PART, Some(30));
    mount.path.is_mount_point = true;
    assert!(matches!(
        decide_cargo_part(&mount, &r, NOW, age),
        BuildGuard::Skip {
            reason: SkipReason::MountPoint,
            ..
        }
    ));
    let outside = part_facts("/h/u/elsewhere/app/target/debug/incremental", Some(30));
    assert!(matches!(
        decide_cargo_part(&outside, &r, NOW, age),
        BuildGuard::Skip {
            reason: SkipReason::OutsideRoot,
            ..
        }
    ));
}

fn home_facts(path: &str, newest_days: Option<i64>) -> CargoHomeFacts {
    CargoHomeFacts {
        path: facts(Kind::CargoHome, path),
        planned: id(path),
        newest_mtime: newest_days.map(|d| NOW - d * DAY),
    }
}

const CRATE: &str = "/h/u/.cargo/registry/src/idx/serde-1.0.200";

#[test]
fn a_cargo_home_entry_should_follow_the_age_with_a_one_day_floor() {
    let r = roots();
    let month = Duration::from_hours(30 * 24);
    assert!(matches!(
        decide_cargo_home(&home_facts(CRATE, Some(40)), &r, NOW, month),
        BuildGuard::Allow(_)
    ));
    assert!(matches!(
        decide_cargo_home(&home_facts(CRATE, Some(10)), &r, NOW, month),
        BuildGuard::Skip {
            reason: SkipReason::TooRecent,
            ..
        }
    ));
    let mut hours = home_facts(CRATE, None);
    hours.newest_mtime = Some(NOW - 3 * 3600);
    assert!(matches!(
        decide_cargo_home(&hours, &r, NOW, Duration::ZERO),
        BuildGuard::Skip {
            reason: SkipReason::TooRecent,
            ..
        }
    ));
    assert!(matches!(
        decide_cargo_home(&home_facts(CRATE, None), &r, NOW, month),
        BuildGuard::Skip {
            reason: SkipReason::TooRecent,
            ..
        }
    ));
}

#[test]
fn a_cargo_home_entry_outside_the_cargo_home_root_should_be_outside_root() {
    let r = roots();
    let month = Duration::from_hours(30 * 24);
    let f = home_facts("/h/u/.cargo/registry/cache/idx/serde-1.0.200", Some(90));
    assert!(matches!(
        decide_cargo_home(&f, &r, NOW, month),
        BuildGuard::Skip {
            reason: SkipReason::OutsideRoot,
            ..
        }
    ));
    let mut moved = home_facts(CRATE, Some(90));
    moved.planned = Identity { dev: 9, ino: 9 };
    assert!(matches!(
        decide_cargo_home(&moved, &r, NOW, month),
        BuildGuard::Skip {
            reason: SkipReason::IdentityChanged,
            ..
        }
    ));
}
