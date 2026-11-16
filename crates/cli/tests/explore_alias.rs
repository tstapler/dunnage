#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cloned_ref_to_slice_refs,
    non_snake_case
)]
//! 0.3.1 review finding 1 on real directories: a second path to a secret directory (a symlinked `~/.ssh`;
//! a bind mount or a btrfs top-level mount is the same case for identity) and to the tool's own state is
//! refused at plan time and again at apply time, whatever the path says. Written before the fix; same
//! author as the implementation, NOT an independent one.

mod explore_rig;

use std::ffi::{OsStr, OsString};
use std::path::Path;

use dunnage_adapters::dirops::DirOps;
use dunnage_adapters::targets::explorer_selection::{candidate_of, ExplorerSelectionTarget};
use dunnage_adapters::RealDirOps;
use dunnage_core::delete_types::StatInfo;
use dunnage_core::explorer_plan::{judge_fresh, mount_facts, FreshCheck};
use dunnage_core::ports::Fs;
use dunnage_core::target::{Guarded, Target};
use dunnage_core::{ApplyToken, DeleteError, Identity, SkipReason};
use explore_rig::{manifest, Ex};

/// An opened directory with this name reports this identity (the same directory under another spelling).
struct Lie {
    inner: RealDirOps,
    name: OsString,
    ident: Identity,
}
struct LDir {
    d: <RealDirOps as DirOps>::Dir,
    name: OsString,
}

impl DirOps for Lie {
    type Dir = LDir;
    type ReadDir = <RealDirOps as DirOps>::ReadDir;
    fn open_root(&self, p: &Path) -> std::io::Result<LDir> {
        Ok(LDir {
            d: self.inner.open_root(p)?,
            name: OsString::new(),
        })
    }
    fn open_dir(&self, parent: &LDir, name: &OsStr) -> std::io::Result<LDir> {
        Ok(LDir {
            d: self.inner.open_dir(&parent.d, name)?,
            name: name.to_os_string(),
        })
    }
    fn fstat(&self, d: &LDir) -> std::io::Result<StatInfo> {
        let mut st = self.inner.fstat(&d.d)?;
        if d.name == self.name {
            st.ident = self.ident;
        }
        Ok(st)
    }
    fn stat_at(&self, d: &LDir, name: &OsStr) -> std::io::Result<StatInfo> {
        self.inner.stat_at(&d.d, name)
    }
    fn read_dir(&self, d: &LDir) -> std::io::Result<Self::ReadDir> {
        self.inner.read_dir(&d.d)
    }
    fn unlinkat(&self, d: &LDir, name: &OsStr, dir: bool) -> std::io::Result<()> {
        self.inner.unlinkat(&d.d, name, dir)
    }
    fn fchmod(&self, d: &LDir, mode: u32) -> std::io::Result<()> {
        self.inner.fchmod(&d.d, mode)
    }
}

fn verdict(ex: &Ex, path: &Path) -> Result<(), SkipReason> {
    let roots = ex.roots();
    let (mounts, points) = mount_facts(&ex.probe);
    let own = ex.own_state();
    let st = ex.fs.lstat(path).unwrap();
    let fc = FreshCheck {
        roots: &roots,
        own_state: &own,
        fs: &ex.fs,
    };
    judge_fresh(path, &st, &mounts, &points, &fc)
        .allowed
        .map(|_| ())
}

#[test]
#[cfg_attr(not(target_os = "linux"), ignore = "mount table is read from /proc")]
fn safety_plan_should_refuse_a_folder_below_a_symlinked_secret_directory() {
    let ex = Ex::new();
    let real = ex.p("vault/ssh");
    std::fs::create_dir_all(real.join("keys/deep")).unwrap();
    std::os::unix::fs::symlink(&real, ex.home.join(".ssh")).unwrap();
    assert_eq!(
        verdict(&ex, &real.join("keys")),
        Err(SkipReason::ProtectedPath)
    );
    assert_eq!(
        verdict(&ex, &real.join("keys/deep")),
        Err(SkipReason::ProtectedPath)
    );
    // a neighbour of the vault stays deletable
    std::fs::create_dir_all(ex.p("vault/other/x")).unwrap();
    assert_eq!(verdict(&ex, &ex.p("vault/other/x")), Ok(()));
}

#[test]
#[cfg_attr(not(target_os = "linux"), ignore = "mount table is read from /proc")]
fn safety_plan_should_refuse_the_tools_own_state_below_a_symlinked_state_directory() {
    let ex = Ex::new();
    let real = ex.p("elsewhere/dunnage-state");
    std::fs::create_dir_all(real.join("runs/old")).unwrap();
    std::fs::create_dir_all(ex.base.join("state")).unwrap();
    std::os::unix::fs::symlink(&real, ex.base.join("state/dunnage")).unwrap();
    assert_eq!(
        verdict(&ex, &real.join("runs")),
        Err(SkipReason::ProtectedPath)
    );
}

fn ident_of(p: &Path) -> Identity {
    Identity::of(&std::fs::symlink_metadata(p).unwrap())
}

/// `app` is the parent of the marked `dist`: the engine opens it on the way down, and here it reports the
/// identity `secret` gives (another spelling of a protected directory).
fn apply_refused(secret: impl Fn(&Ex) -> Identity) {
    let ex = Ex::new();
    let lie = secret(&ex);
    let a = ex.tree("app/dist", 2, 100);
    let before = manifest(&a);
    let reviewed = ex.review(&[a.clone()]);
    let ops = Lie {
        inner: RealDirOps::confined_to(&ex.base),
        name: "app".into(),
        ident: lie,
    };
    let t = ExplorerSelectionTarget::new(
        ex.roots(),
        ex.own_state(),
        ops,
        reviewed.items().to_vec(),
        reviewed.roots().to_vec(),
    );
    let c = candidate_of(&a, &ex.fs.lstat(&a).unwrap());
    let env = ex.env();
    assert_eq!(t.guard(&c, &env), Guarded::Allow);
    let err = t.execute(&c, &env, &ApplyToken::for_tests()).unwrap_err();
    assert_eq!(err.error, DeleteError::ProtectedPath);
    assert_eq!(manifest(&a), before);
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "explorer delete needs the kernel mount id"
)]
fn safety_apply_should_refuse_when_an_ancestor_opened_on_the_way_is_a_secret_directory() {
    apply_refused(|ex| {
        let ssh = ex.home.join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        ident_of(&ssh)
    });
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "explorer delete needs the kernel mount id"
)]
fn safety_apply_should_refuse_when_an_ancestor_opened_on_the_way_is_the_tools_own_state() {
    apply_refused(|ex| {
        let own = ex.base.join("state/dunnage");
        std::fs::create_dir_all(&own).unwrap();
        ident_of(&own)
    });
}
