//! Adversarial cases for the explorer's hard floor (`feat/explore-mark-anywhere`). Written from the owner's
//! decision and the floor list in SECURITY.md only, before the engine change: an explorer mark may name any
//! directory the user may delete, except the floor, which is refused at four places that must agree: the
//! common guard (`decide_path_common`), `RootSet::admits`, the policy constructor and `DeletePolicy::validate`.
//!
//! A witness exists only if the guard allowed the path. To reach the policy's own refusal (the second line of
//! defence) a case mints the witness under another `$HOME`, where the path is not on the floor, and builds the
//! policy against the `RootSet` under test.

#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]

use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use dunnage_core::delete_types::FileKind;
use dunnage_core::guard::{decide_path_common, Facts, GuardDecision};
use dunnage_core::plan::ItemIdentity;
use dunnage_core::roots::RootSet;
use dunnage_core::{Allowed, DeleteError, DeletePolicy, Dirs, Identity, Kind, RootDev, SkipReason};

const HOME: &str = "/h/a/b/c";
const CODE: &str = "/h/a/b/c/code";

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

fn roots_with(home: &str, scan: &[&str]) -> RootSet {
    let dirs = Dirs {
        home: home.into(),
        state: format!("{home}/.local/state").into(),
        cache: format!("{home}/.cache").into(),
        config: format!("{home}/.config").into(),
    };
    let flags: Vec<PathBuf> = scan.iter().map(PathBuf::from).collect();
    RootSet::resolve(&dirs, &flags, None, Path::new("/"), &canon).expect("roots resolve")
}

/// `$HOME` is `/h/a/b/c`; the one scan root is `/h/a/b/c/code`.
fn roots() -> RootSet {
    roots_with(HOME, &[CODE])
}

fn facts(path: &Path) -> Facts {
    Facts {
        kind: Kind::ExplorerSelection,
        path: path.to_path_buf(),
        ident: ident_of(path),
        is_symlink: false,
        root_dev: RootDev(1),
        is_mount_point: false,
    }
}

fn guard(path: &str, roots: &RootSet) -> GuardDecision {
    decide_path_common(&facts(Path::new(path)), roots)
}

fn allowed(path: &str) -> bool {
    matches!(guard(path, &roots()), GuardDecision::Allow(_))
}

fn skip_of(path: &str) -> Option<SkipReason> {
    match guard(path, &roots()) {
        GuardDecision::Skip(r) => Some(r),
        GuardDecision::Allow(_) => None,
    }
}

/// Paths the owner may mark although they are outside every scan root.
const OUTSIDE_THE_ROOTS_BUT_FINE: [&str; 9] = [
    "/h/a/b/c/Downloads/old",
    "/h/a/b/c/.cache/thing/deeper",
    "/h/a/b/c/media/photos/2019",
    "/srv/data/old",
    "/opt/vendor/thing",
    "/mnt/disk/stuff",
    "/tmp/scratch",
    "/var/tmp/build",
    "/var/cache/thing",
];

/// Paths on the floor, each with the reason the owner is shown.
const FLOOR: [(&str, SkipReason); 31] = [
    ("/", SkipReason::ProtectedPath),
    ("/usr/local/share", SkipReason::ProtectedPath),
    ("/usr/lib/x", SkipReason::ProtectedPath),
    ("/etc/ssh", SkipReason::ProtectedPath),
    ("/boot/efi", SkipReason::ProtectedPath),
    ("/bin/x", SkipReason::ProtectedPath),
    ("/sbin/x", SkipReason::ProtectedPath),
    ("/lib/modules", SkipReason::ProtectedPath),
    ("/lib64/x", SkipReason::ProtectedPath),
    ("/dev/shm/x", SkipReason::ProtectedPath),
    ("/proc/1/fd", SkipReason::ProtectedPath),
    ("/sys/kernel/x", SkipReason::ProtectedPath),
    ("/run/user/1000", SkipReason::ProtectedPath),
    ("/var/lib/docker", SkipReason::ProtectedPath),
    ("/var/log/journal", SkipReason::ProtectedPath),
    ("/var/tmp", SkipReason::ProtectedPath),
    ("/var/cache", SkipReason::ProtectedPath),
    ("/home", SkipReason::ProtectedPath),
    ("/home/other", SkipReason::ProtectedPath),
    ("/h/a/b/c", SkipReason::ProtectedPath),
    ("/h/a/b", SkipReason::ProtectedPath),
    ("/h/a/b/c/Downloads", SkipReason::ProtectedPath),
    ("/h/a/b/c/.ssh/keys", SkipReason::ProtectedPath),
    (
        "/h/a/b/c/.gnupg/private-keys-v1.d",
        SkipReason::ProtectedPath,
    ),
    ("/h/a/b/c/.config/1Password/x", SkipReason::ProtectedPath),
    ("/h/a/b/c/.1password/agent", SkipReason::ProtectedPath),
    ("/h/a/b/c/.SSH/keys", SkipReason::ProtectedPath),
    ("/h/a/b/c/code/app/.git/hooks", SkipReason::ProtectedPath),
    ("/srv/data/repo/.git", SkipReason::ProtectedPath),
    ("/h/a/b/c/code", SkipReason::ProtectedPath),
    ("/tmp", SkipReason::ProtectedPath),
];

// ---------------------------------------------------------------- the guard

#[test]
fn safety_decide_path_common_should_allow_an_explorer_path_outside_every_scan_root_When_it_is_not_on_the_floor(
) {
    for p in OUTSIDE_THE_ROOTS_BUT_FINE {
        assert!(allowed(p), "{p} is outside the roots but not on the floor");
    }
}

#[test]
fn safety_decide_path_common_should_skip_every_floor_path_as_protected_When_the_kind_is_explorer_selection(
) {
    for (p, why) in FLOOR {
        assert_eq!(skip_of(p), Some(why), "{p}");
    }
}

#[test]
fn safety_decide_path_common_should_skip_a_relative_or_dotdot_or_unclean_path_When_it_is_offered() {
    for p in [
        "h/a/b/c/code/app",
        "/h/a/b/c/code/../.ssh",
        "/srv/data/../../etc",
        "/srv/./data",
        "/srv//data",
        "/srv/data/",
        "",
    ] {
        assert_eq!(skip_of(p), Some(SkipReason::OutsideRoot), "{p:?}");
    }
}

#[test]
fn safety_decide_path_common_should_skip_a_non_utf8_name_When_it_is_outside_the_roots_too() {
    let p = Path::new(OsStr::from_bytes(b"/srv/data/\xff\xfedir"));
    let d = decide_path_common(&facts(p), &roots());
    assert!(
        matches!(d, GuardDecision::Skip(SkipReason::OutsideRoot)),
        "{d:?}"
    );
}

#[test]
fn safety_decide_path_common_should_skip_another_filesystem_and_a_mount_point_When_the_path_is_fine(
) {
    let p = Path::new("/srv/data/old");
    let mut other_fs = facts(p);
    other_fs.ident.dev = 2;
    assert!(matches!(
        decide_path_common(&other_fs, &roots()),
        GuardDecision::Skip(SkipReason::CrossDevice)
    ));
    let mut mount = facts(p);
    mount.is_mount_point = true;
    assert!(matches!(
        decide_path_common(&mount, &roots()),
        GuardDecision::Skip(SkipReason::MountPoint)
    ));
    let mut link = facts(p);
    link.is_symlink = true;
    assert!(matches!(
        decide_path_common(&link, &roots()),
        GuardDecision::Skip(SkipReason::Symlink)
    ));
}

#[test]
fn safety_decide_path_common_should_skip_home_by_identity_When_it_is_reached_under_another_spelling_outside_the_roots(
) {
    let rs = roots();
    let mut f = facts(Path::new("/mnt/alias/of-home"));
    f.ident = ident_of(Path::new(HOME));
    assert!(matches!(
        decide_path_common(&f, &rs),
        GuardDecision::Skip(SkipReason::ProtectedPath)
    ));
}

#[test]
fn safety_decide_path_common_should_not_widen_the_other_kinds_When_the_explorer_floor_is_lifted() {
    let rs = roots();
    for (kind, path) in [
        (Kind::NodeModules, "/srv/data/proj/node_modules"),
        (Kind::CargoTarget, "/srv/data/proj/target"),
        (Kind::Worktree, "/srv/data/proj/.claude/worktrees/agent-1"),
        (Kind::Cache, "/srv/data/cache"),
    ] {
        let f = Facts {
            kind,
            ..facts(Path::new(path))
        };
        assert!(
            matches!(
                decide_path_common(&f, &rs),
                GuardDecision::Skip(SkipReason::OutsideRoot)
            ),
            "{kind:?} {path} must stay inside its narrow roots"
        );
    }
}

// ---------------------------------------------------------------- RootSet::admits

#[test]
fn safety_admits_should_agree_with_the_guard_When_the_kind_is_explorer_selection() {
    let rs = roots();
    for p in OUTSIDE_THE_ROOTS_BUT_FINE {
        assert!(rs.admits(Kind::ExplorerSelection, Path::new(p)), "{p}");
    }
    for (p, _) in FLOOR {
        assert!(!rs.admits(Kind::ExplorerSelection, Path::new(p)), "{p}");
    }
}

#[test]
fn safety_admits_should_keep_the_narrow_roots_When_the_kind_is_not_explorer_selection() {
    let rs = roots();
    assert!(!rs.admits(Kind::NodeModules, Path::new("/srv/data/proj/node_modules")));
    assert!(rs.admits(
        Kind::NodeModules,
        Path::new("/h/a/b/c/code/proj/node_modules")
    ));
}

// ---------------------------------------------------------------- the policy (second line of defence)

fn planned(p: &str) -> ItemIdentity {
    let i = ident_of(Path::new(p));
    ItemIdentity {
        dev: i.dev,
        ino: i.ino,
        kind: FileKind::Dir,
        btime: None,
    }
}

/// A witness minted where `p` is not on the floor (home elsewhere), for a policy built against `roots()`.
fn witness_elsewhere(p: &str) -> Allowed {
    let elsewhere = roots_with("/nowhere-home", &[]);
    match guard(p, &elsewhere) {
        GuardDecision::Allow(a) => a,
        GuardDecision::Skip(r) => panic!("the other home should allow {p}: {r:?}"),
    }
}

fn policy_for(p: &str) -> Result<DeletePolicy, dunnage_core::floor_rule::ForAllowedError> {
    DeletePolicy::for_allowed_roots(&witness_elsewhere(p), RootDev(1), 7, &roots(), planned(p))
}

fn name(p: &str) -> &OsStr {
    Path::new(p).file_name().unwrap()
}

#[test]
fn safety_validate_should_accept_a_directory_outside_every_scan_root_When_it_is_not_on_the_floor() {
    for p in OUTSIDE_THE_ROOTS_BUT_FINE {
        let policy = policy_for(p).unwrap_or_else(|e| panic!("{p}: {e:?}"));
        assert_eq!(policy.validate(name(p)), Ok(()), "{p}");
    }
}

#[test]
fn safety_validate_should_refuse_every_floor_path_When_the_witness_came_from_a_home_where_it_was_fine(
) {
    for (p, _) in FLOOR {
        let Some(file) = Path::new(p).file_name() else {
            continue; // "/" has no name; `validate` refuses an empty name anyway
        };
        let Ok(policy) = policy_for_floor(p) else {
            continue;
        };
        assert_eq!(
            policy.validate(file),
            Err(DeleteError::ProtectedPath),
            "{p} must be refused by the engine's own floor"
        );
    }
}

/// Floor paths whose witness exists under `/nowhere-home` (the others are refused by the guard everywhere).
fn policy_for_floor(p: &str) -> Result<DeletePolicy, dunnage_core::floor_rule::ForAllowedError> {
    let elsewhere = roots_with("/nowhere-home", &[]);
    match guard(p, &elsewhere) {
        GuardDecision::Allow(a) => {
            DeletePolicy::for_allowed_roots(&a, RootDev(1), 7, &roots(), planned(p))
        }
        GuardDecision::Skip(_) => Err(dunnage_core::floor_rule::ForAllowedError::ProtectedPath),
    }
}

#[test]
fn safety_validate_should_refuse_the_secret_and_home_paths_at_the_engine_When_a_witness_was_minted_elsewhere(
) {
    for p in [
        "/h/a/b/c/.ssh/keys",
        "/h/a/b/c/.gnupg/x",
        "/h/a/b/c/.config/1Password/x",
        "/h/a/b/c/.1password/x",
        "/h/a/b/c/Downloads",
    ] {
        let policy = policy_for(p).unwrap_or_else(|e| panic!("{p}: {e:?}"));
        assert_eq!(
            policy.validate(name(p)),
            Err(DeleteError::ProtectedPath),
            "{p}"
        );
    }
}

#[test]
fn safety_validate_should_still_refuse_the_scan_root_home_and_slash_When_they_are_the_target() {
    for p in [CODE, HOME, "/h/a/b"] {
        match policy_for(p) {
            Ok(policy) => assert_eq!(
                policy.validate(name(p)),
                Err(DeleteError::ProtectedPath),
                "{p}"
            ),
            Err(e) => assert_eq!(e, dunnage_core::floor_rule::ForAllowedError::ProtectedPath),
        }
    }
}

// ---------------------------------------------------------------- the one function

#[test]
fn safety_explorer_floor_should_refuse_exactly_what_the_guard_refuses_When_asked_directly() {
    for p in OUTSIDE_THE_ROOTS_BUT_FINE {
        assert_eq!(
            dunnage_core::explorer_floor::explorer_floor(Path::new(p), Path::new(HOME)),
            None,
            "{p}"
        );
    }
    for (p, _) in FLOOR {
        if p == CODE {
            continue; // a scan root is a separate rule (protected_position), not the lexical floor
        }
        assert!(
            dunnage_core::explorer_floor::explorer_floor(Path::new(p), Path::new(HOME)).is_some(),
            "{p}"
        );
    }
}
