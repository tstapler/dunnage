#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]

use std::io;
use std::path::{Path, PathBuf};

use disk_cleanup_core::guard::{decide_path_common, Facts, GuardDecision};
use disk_cleanup_core::ports::SystemProbe;
use disk_cleanup_core::roots::RootSet;
use disk_cleanup_core::testing::FakeProbe;
use disk_cleanup_core::{
    Dirs, Identity, Kind, MountEntry, RootDev, SkipReason, WorktreeRemoval, WrongKind,
};

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

fn facts(kind: Kind, path: &str) -> Facts {
    Facts {
        kind,
        path: path.into(),
        ident: Identity { dev: 28, ino: 7 },
        is_symlink: false,
        root_dev: RootDev(28),
        is_mount_point: false,
    }
}

fn skip(f: &Facts) -> Option<SkipReason> {
    match decide_path_common(f, &roots()) {
        GuardDecision::Skip(r) => Some(r),
        GuardDecision::Allow(_) => None,
    }
}

#[test]
fn decide_path_common_should_skip_Symlink_CrossDevice_ProtectedPath_OutsideRoot_and_unclean_paths()
{
    let nm = "/h/code/app/node_modules";
    assert_eq!(skip(&facts(Kind::NodeModules, nm)), None);

    let mut f = facts(Kind::NodeModules, nm);
    f.is_symlink = true;
    assert_eq!(skip(&f), Some(SkipReason::Symlink));

    let mut f = facts(Kind::NodeModules, nm);
    f.ident.dev = 99;
    assert_eq!(skip(&f), Some(SkipReason::CrossDevice));

    let mut f = facts(Kind::NodeModules, nm);
    f.is_mount_point = true;
    assert_eq!(skip(&f), Some(SkipReason::MountPoint));

    assert_eq!(
        skip(&facts(Kind::NodeModules, "/h/code")),
        Some(SkipReason::ProtectedPath)
    );
    assert_eq!(
        skip(&facts(Kind::NodeModules, "/h/code/app/.git/node_modules")),
        Some(SkipReason::ProtectedPath)
    );
    for p in [
        "/h/other/node_modules",
        "/h/Documents/x/node_modules",
        "/tmp/elsewhere/node_modules",
        "/h/code/../.ssh/node_modules",
        "/h/code/./app/node_modules",
        "/h//code/app/node_modules",
        "/h/code/app/node_modules/",
        "h/code/app/node_modules",
    ] {
        assert_eq!(
            skip(&facts(Kind::NodeModules, p)),
            Some(SkipReason::OutsideRoot),
            "{p}"
        );
    }
}

#[test]
fn worktree_removal_new_should_be_Ok_for_worktree_and_WrongKind_for_node_modules_When_built_from_Allowed(
) {
    let allow = |kind, p: &str| match decide_path_common(&facts(kind, p), &roots()) {
        GuardDecision::Allow(a) => a,
        GuardDecision::Skip(r) => panic!("{p}: {r:?}"),
    };
    let wt = allow(Kind::Worktree, "/h/code/r/.claude/worktrees/agent-1");
    assert!(WorktreeRemoval::new(wt, "/h/code/r".into()).is_ok());
    let nm = allow(Kind::NodeModules, "/h/code/app/node_modules");
    assert_eq!(
        WorktreeRemoval::new(nm, "/h/code/r".into()).unwrap_err(),
        WrongKind
    );
}

#[test]
fn decide_path_common_should_use_the_entry_st_dev_and_never_the_mountinfo_device_When_four_subvolumes_share_0_26(
) {
    let mount = |p: &str| MountEntry {
        mount_point: p.into(),
        dev: 26,
        mnt_id: 1,
    };
    let probe = FakeProbe::new().with_mounts(vec![
        mount("/"),
        mount("/var"),
        mount("/var/log"),
        mount("/var/cache"),
    ]);
    assert!(probe.mounts().unwrap().iter().all(|m| m.dev == 26));

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
    let roots =
        RootSet::resolve(&dirs, &["/var/data".into()], None, Path::new("/"), &canon).unwrap();
    let f = |dev| Facts {
        kind: Kind::NodeModules,
        path: "/var/data/log/node_modules".into(),
        ident: Identity { dev, ino: 3 },
        is_symlink: false,
        root_dev: RootDev(28),
        is_mount_point: false,
    };
    assert!(matches!(
        decide_path_common(&f(34), &roots),
        GuardDecision::Skip(SkipReason::CrossDevice)
    ));
    assert!(matches!(
        decide_path_common(&f(28), &roots),
        GuardDecision::Allow(_)
    ));
}
