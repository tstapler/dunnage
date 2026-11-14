//! Adversarial cases for an explorer delete OUTSIDE every scan root (`feat/explore-mark-anywhere`), through
//! the real engine on real directories. Written from the owner's decision and the floor list in SECURITY.md
//! only, before the engine change; fixtures live in tempdirs, never in the real HOME.
//!
//! Layout: `<base>/home` is `$HOME` with one scan root `home/code`; `<base>/srv/data` is outside both. The
//! delete walks down from `<base>`, which stands for the mount point that holds the paths (the anchor).

#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, MetadataExt};
use std::path::{Path, PathBuf};

use dunnage_adapters::{delete_tree, open_parent, open_verified_root, DirOps, RealDirOps};
use dunnage_core::delete_types::FileKind;
use dunnage_core::guard::{decide_path_common, Facts, GuardDecision};
use dunnage_core::plan::ItemIdentity;
use dunnage_core::roots::RootSet;
use dunnage_core::{
    AbortFlag, Allowed, ApplyToken, DeleteError, DeleteFailure, DeletePolicy, Dirs, Freed,
    Identity, Kind, RootDev, SkipReason,
};

struct Fx {
    _td: tempfile::TempDir,
    _decoy: tempfile::TempDir,
    base: PathBuf,
    decoy_home: PathBuf,
}

impl Fx {
    fn new() -> Fx {
        let td = tempfile::tempdir().expect("tempdir");
        let decoy = tempfile::tempdir().expect("decoy tempdir");
        let base = fs::canonicalize(td.path()).expect("canonical");
        let decoy_home = fs::canonicalize(decoy.path()).expect("canonical");
        for d in [
            "home/code",
            "home/media/photos/old",
            "home/.ssh/keys",
            "home/Downloads/x",
            "srv/data/app/sub",
            "srv/data/repo/.git/hooks",
            "precious/inner",
        ] {
            fs::create_dir_all(base.join(d)).unwrap();
        }
        fs::write(base.join("srv/data/app/sub/f"), b"data").unwrap();
        fs::write(base.join("precious/inner/keep"), b"keep").unwrap();
        fs::write(base.join("home/.ssh/keys/id"), b"key").unwrap();
        Fx {
            _td: td,
            _decoy: decoy,
            base,
            decoy_home,
        }
    }
    fn p(&self, rel: &str) -> PathBuf {
        self.base.join(rel)
    }
    fn dirs(&self, home: &Path) -> Dirs {
        Dirs {
            home: home.to_path_buf(),
            state: home.join(".local/state"),
            cache: home.join(".cache"),
            config: home.join(".config"),
        }
    }
    fn roots(&self) -> RootSet {
        RootSet::resolve(
            &self.dirs(&self.p("home")),
            &[self.p("home/code")],
            None,
            &self.base,
            &real_canon,
        )
        .expect("roots resolve")
    }
    /// The guard's verdict for `target` under the roots under test.
    fn guard(&self, target: &Path) -> GuardDecision {
        decide_path_common(&facts_of(target), &self.roots())
    }
    /// A witness minted where `target` is not on the floor (another home, no scan roots).
    fn witness_elsewhere(&self, target: &Path) -> Allowed {
        let roots = RootSet::resolve(
            &self.dirs(&self.decoy_home),
            &[],
            None,
            &self.base,
            &real_canon,
        )
        .expect("decoy roots");
        match decide_path_common(&facts_of(target), &roots) {
            GuardDecision::Allow(a) => a,
            GuardDecision::Skip(r) => panic!("{} should be allowed there: {r:?}", target.display()),
        }
    }
}

fn real_canon(p: &Path) -> io::Result<(PathBuf, Identity)> {
    let c = fs::canonicalize(p)?;
    let m = fs::metadata(&c)?;
    Ok((c, Identity::of(&m)))
}

fn ident(p: &Path) -> Identity {
    Identity::of(&fs::symlink_metadata(p).unwrap())
}

fn dir_plan(p: &Path) -> ItemIdentity {
    let i = ident(p);
    ItemIdentity {
        dev: i.dev,
        ino: i.ino,
        kind: FileKind::Dir,
        btime: None,
    }
}

fn facts_of(path: &Path) -> Facts {
    let m = fs::symlink_metadata(path).unwrap();
    Facts {
        kind: Kind::ExplorerSelection,
        path: path.to_path_buf(),
        ident: Identity::of(&m),
        is_symlink: false,
        root_dev: RootDev(m.dev()),
        is_mount_point: false,
    }
}

/// Path -> description of everything under `root`: kind, inode, size, symlink target.
fn manifest(root: &Path) -> BTreeMap<PathBuf, String> {
    fn walk(p: &Path, out: &mut BTreeMap<PathBuf, String>) {
        let m = fs::symlink_metadata(p).unwrap();
        let body = if m.file_type().is_symlink() {
            format!("-> {}", fs::read_link(p).unwrap().display())
        } else {
            String::new()
        };
        out.insert(
            p.to_path_buf(),
            format!(
                "ino={} size={} {body}",
                m.ino(),
                if m.is_dir() { 0 } else { m.len() }
            ),
        );
        if m.is_dir() {
            for e in fs::read_dir(p).unwrap() {
                walk(&e.unwrap().path(), out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, &mut out);
    out
}

fn mnt_of(p: &Path) -> u64 {
    let ops = RealDirOps::confined_to(p);
    let root = open_verified_root(&ops, p, ident(p)).expect("open root");
    ops.fstat(root.dir())
        .expect("fstat")
        .mnt_id
        .expect("kernel reports mnt_id")
}

/// The delete as the explorer's target runs it: open the anchor against its planned identity, walk the
/// parent chain fd by fd, `delete_tree` with the planned identity.
fn delete_from_anchor(
    fx: &Fx,
    policy: &DeletePolicy,
    target: &Path,
    planned: Identity,
) -> Result<Freed, DeleteFailure> {
    let token = ApplyToken::for_tests();
    let ops = RealDirOps::confined_to(&fx.base);
    let root =
        open_verified_root(&ops, &fx.base, ident(&fx.base)).map_err(|error| DeleteFailure {
            error,
            freed: Freed::default(),
        })?;
    let rel_parent = target.parent().unwrap().strip_prefix(&fx.base).unwrap();
    let parent = open_parent(&ops, &root, rel_parent).map_err(|error| DeleteFailure {
        error,
        freed: Freed::default(),
    })?;
    delete_tree(
        &ops,
        &parent,
        target.file_name().unwrap(),
        planned,
        policy,
        &AbortFlag::new(),
        &token,
    )
}

fn policy_for(fx: &Fx, w: &Allowed, target: &Path) -> DeletePolicy {
    DeletePolicy::for_allowed_roots(
        w,
        RootDev(ident(&fx.base).dev),
        mnt_of(&fx.base),
        &fx.roots(),
        dir_plan(target),
    )
    .expect("policy")
}

fn allowed_witness(fx: &Fx, target: &Path) -> Allowed {
    match fx.guard(target) {
        GuardDecision::Allow(a) => a,
        GuardDecision::Skip(r) => panic!("{} should be allowed: {r:?}", target.display()),
    }
}

// ---------------------------------------------------------------- the delete works outside the roots

#[test]
fn safety_explorer_delete_should_remove_a_directory_outside_every_scan_root_and_nothing_beside_it()
{
    let fx = Fx::new();
    for rel in ["srv/data/app", "home/media/photos/old"] {
        let t = fx.p(rel);
        let before = manifest(&fx.base);
        let w = allowed_witness(&fx, &t);
        let policy = policy_for(&fx, &w, &t);
        let freed = delete_from_anchor(&fx, &policy, &t, ident(&t)).expect("deleted");
        assert!(freed.entries_removed >= 1);
        assert!(fs::symlink_metadata(&t).is_err(), "{rel} must be gone");
        let after = manifest(&fx.base);
        let gone: Vec<_> = before.keys().filter(|k| !after.contains_key(*k)).collect();
        assert!(
            gone.iter().all(|k| k.starts_with(&t)),
            "only {rel} may disappear: {gone:?}"
        );
    }
    assert!(fx.p("precious/inner/keep").is_file());
    assert!(fx.p("srv/data").is_dir() && fx.p("home/.ssh/keys/id").is_file());
}

// ---------------------------------------------------------------- the reviewed-delete protections hold there

#[test]
fn safety_explorer_delete_should_refuse_and_keep_everything_When_the_target_became_a_symlink_after_the_plan(
) {
    let fx = Fx::new();
    let t = fx.p("srv/data/app");
    let w = allowed_witness(&fx, &t);
    let policy = policy_for(&fx, &w, &t);
    let planned = ident(&t);
    fs::rename(&t, fx.p("srv/data/app.moved")).unwrap();
    symlink(fx.p("precious"), &t).unwrap();
    let before = manifest(&fx.base);
    let err =
        delete_from_anchor(&fx, &policy, &t, planned).expect_err("a symlink is never followed");
    assert!(
        matches!(
            err.error,
            DeleteError::Symlink | DeleteError::IdentityMismatch | DeleteError::OutsideScope
        ),
        "{:?}",
        err.error
    );
    assert_eq!(manifest(&fx.base), before, "nothing may change");
}

#[test]
fn safety_explorer_delete_should_refuse_and_keep_everything_When_the_target_was_replaced_by_another_directory(
) {
    let fx = Fx::new();
    let t = fx.p("srv/data/app");
    let w = allowed_witness(&fx, &t);
    let policy = policy_for(&fx, &w, &t);
    let planned = ident(&t);
    fs::rename(&t, fx.p("srv/data/app.moved")).unwrap();
    fs::create_dir(&t).unwrap();
    fs::write(t.join("new"), b"x").unwrap();
    let before = manifest(&fx.base);
    let err =
        delete_from_anchor(&fx, &policy, &t, planned).expect_err("a new inode is not the plan");
    assert_eq!(err.error, DeleteError::IdentityMismatch);
    assert_eq!(manifest(&fx.base), before);
}

#[test]
fn safety_explorer_delete_should_refuse_and_keep_everything_When_a_parent_became_a_symlink() {
    let fx = Fx::new();
    let t = fx.p("srv/data/app");
    let w = allowed_witness(&fx, &t);
    let policy = policy_for(&fx, &w, &t);
    let planned = ident(&t);
    fs::rename(fx.p("srv/data"), fx.p("srv/data.moved")).unwrap();
    symlink(fx.p("srv/data.moved"), fx.p("srv/data")).unwrap();
    let before = manifest(&fx.base);
    let err = delete_from_anchor(&fx, &policy, &t, planned).expect_err("the parent is a link now");
    assert!(
        matches!(err.error, DeleteError::Symlink | DeleteError::OutsideScope),
        "{:?}",
        err.error
    );
    assert_eq!(manifest(&fx.base), before);
}

#[test]
fn safety_explorer_delete_should_refuse_a_dotdot_or_absolute_parent_before_opening_anything() {
    let fx = Fx::new();
    let ops = RealDirOps::confined_to(&fx.base);
    let root = open_verified_root(&ops, &fx.base, ident(&fx.base)).unwrap();
    for bad in [
        "srv/../home",
        "../x",
        "/srv",
        "srv//data",
        "srv/./data",
        "srv/data/",
    ] {
        let r = open_parent(&ops, &root, Path::new(bad));
        assert!(
            matches!(r, Err(DeleteError::OutsideScope)),
            "{bad:?} must be refused"
        );
    }
}

// ---------------------------------------------------------------- the floor, on real directories

#[test]
fn safety_explorer_guard_should_skip_the_floor_on_real_directories() {
    let fx = Fx::new();
    let skip = |rel: &str| match fx.guard(&fx.p(rel)) {
        GuardDecision::Skip(r) => Some(r),
        GuardDecision::Allow(_) => None,
    };
    assert_eq!(
        skip("home"),
        Some(SkipReason::ProtectedPath),
        "$HOME itself"
    );
    assert_eq!(
        skip("home/Downloads"),
        Some(SkipReason::ProtectedPath),
        "directly inside $HOME"
    );
    assert_eq!(
        skip("home/.ssh/keys"),
        Some(SkipReason::ProtectedPath),
        "~/.ssh"
    );
    assert_eq!(
        skip("home/code"),
        Some(SkipReason::ProtectedPath),
        "a scan root"
    );
    assert_eq!(
        skip("srv/data/repo/.git"),
        Some(SkipReason::ProtectedPath),
        ".git"
    );
    assert_eq!(
        skip("srv/data/repo/.git/hooks"),
        Some(SkipReason::ProtectedPath),
        "inside .git"
    );
    assert_eq!(
        skip("srv/data/app"),
        None,
        "control: an ordinary directory outside the roots"
    );
}

#[test]
fn safety_explorer_delete_should_refuse_the_floor_at_the_engine_and_keep_everything_When_a_witness_was_minted_elsewhere(
) {
    let fx = Fx::new();
    for rel in [
        "home/.ssh/keys",
        "home/Downloads",
        "srv/data/repo/.git/hooks",
    ] {
        let t = fx.p(rel);
        let w = fx.witness_elsewhere(&t);
        let built = DeletePolicy::for_allowed_roots(
            &w,
            RootDev(ident(&fx.base).dev),
            mnt_of(&fx.base),
            &fx.roots(),
            dir_plan(&t),
        );
        let before = manifest(&fx.base);
        match built {
            Err(_) => {}
            Ok(policy) => {
                let err = delete_from_anchor(&fx, &policy, &t, ident(&t)).expect_err(rel);
                assert!(
                    matches!(
                        err.error,
                        DeleteError::ProtectedPath | DeleteError::OutsideScope
                    ),
                    "{rel}: {:?}",
                    err.error
                );
            }
        }
        assert_eq!(manifest(&fx.base), before, "{rel}: nothing may change");
    }
}

#[test]
fn safety_explorer_guard_should_skip_a_non_utf8_directory_name_and_keep_everything() {
    let fx = Fx::new();
    let odd = fx.base.join(OsStr::from_bytes(b"srv/data/\xff\xfe-dir"));
    if fs::create_dir(&odd).is_err() {
        return; // the filesystem rejects non-UTF-8 names (macOS): nothing to test here
    }
    let before = manifest(&fx.base);
    assert!(
        matches!(fx.guard(&odd), GuardDecision::Skip(SkipReason::OutsideRoot)),
        "a non-UTF-8 path is never admitted"
    );
    assert_eq!(manifest(&fx.base), before);
}

#[test]
fn safety_explorer_guard_should_skip_a_mount_point_and_another_filesystem_outside_the_roots() {
    let fx = Fx::new();
    let t = fx.p("srv/data/app");
    let mut mount = facts_of(&t);
    mount.is_mount_point = true;
    assert!(matches!(
        decide_path_common(&mount, &fx.roots()),
        GuardDecision::Skip(SkipReason::MountPoint)
    ));
    let mut other = facts_of(&t);
    other.root_dev = RootDev(other.ident.dev ^ 0xffff);
    assert!(matches!(
        decide_path_common(&other, &fx.roots()),
        GuardDecision::Skip(SkipReason::CrossDevice)
    ));
}
