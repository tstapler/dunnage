#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 6.6.1 rule M3 and review C9: a REAL same-device bind mount, made under `unshare -rm` (no root
//! needed). The engine is not blind to it: `delete_tree` compares `mnt_id` when the policy's `root_mnt` is
//! set from the opened root, and `mark_check` refuses the directory above it as well. The parent test
//! re-runs `child_sees_a_real_bind_mount` inside the namespace. Where `unshare -rm` is not permitted the leg
//! is a named skip, and `DC_REQUIRE_BIND_MOUNT=1` turns the skip into a failure.

use std::path::{Path, PathBuf};
use std::process::Command;

use disk_cleanup_adapters::probe::basic::mount_sources;
use disk_cleanup_adapters::{
    delete_tree, mount_id_of, open_parent, open_verified_root, RealDirOps,
};
use disk_cleanup_core::explore::mark_check::{
    mark_check, MarkCtx, MarkEnv, MarkFacts, MarkMount, MountTable, OwnState, SymlinkFact,
};
use disk_cleanup_core::explore::marks::Marks;
use disk_cleanup_core::explore::msg::MarkRefusal;
use disk_cleanup_core::guard::{decide_path_common, Facts, GuardDecision};
use disk_cleanup_core::roots::RootSet;
use disk_cleanup_core::{
    AbortFlag, ApplyToken, DeleteError, DeletePolicy, Dirs, Identity, Kind, RootDev,
};

const VAR: &str = "DC_BIND_DIR";

struct Real;
impl MarkFacts for Real {
    fn symlink_in(&self, _: &Path) -> Option<SymlinkFact> {
        None
    }
    fn dev_of(&self, p: &Path) -> Option<u64> {
        use std::os::unix::fs::MetadataExt;
        std::fs::symlink_metadata(p).ok().map(|m| m.dev())
    }
    fn is_dir(&self, p: &Path) -> Option<bool> {
        std::fs::symlink_metadata(p).ok().map(|m| m.is_dir())
    }
    fn is_boundary(&self, _: &Path) -> bool {
        false
    }
    fn scanning(&self, _: &Path) -> bool {
        false
    }
}

fn ident(p: &Path) -> Identity {
    Identity::of(&std::fs::symlink_metadata(p).unwrap())
}

#[test]
fn a_real_same_device_bind_mount_is_refused_by_the_mark_check_and_stopped_by_the_engine() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(base.join("src/data")).unwrap();
    std::fs::write(base.join("src/data/keep"), b"keep").unwrap();
    std::fs::create_dir_all(base.join("code/app/bind")).unwrap();
    let out = Command::new("unshare")
        .args(["-rm", "sh", "-c"])
        .arg("mount --bind \"$1/src\" \"$1/code/app/bind\" && shift && exec \"$@\"")
        .arg("sh")
        .arg(&base)
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", "child_sees_a_real_bind_mount", "--nocapture"])
        .env(VAR, &base)
        .output();
    let ran = match &out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).contains("CHILD_OK"),
        Err(_) => false,
    };
    if !ran {
        let why = out.map_or_else(
            |e| e.to_string(),
            |o| {
                String::from_utf8_lossy(&o.stderr).into_owned()
                    + &String::from_utf8_lossy(&o.stdout)
            },
        );
        assert!(
            std::env::var_os("DC_REQUIRE_BIND_MOUNT").is_none(),
            "bind-mount leg required but did not run: {why}"
        );
        assert!(
            !why.contains("panicked"),
            "the child failed inside the namespace:\n{why}"
        );
        eprintln!("skip: unshare -rm bind mount not available: {why}");
    }
    assert!(base.join("src/data/keep").exists(), "the source is intact");
}

#[test]
fn child_sees_a_real_bind_mount() {
    let Some(base) = std::env::var_os(VAR).map(PathBuf::from) else {
        return;
    };
    let code = base.join("code");
    let app = code.join("app");
    let bind = app.join("bind");
    // The kernel reports a different mount id for the bind mount although st_dev is the same.
    let (app_mnt, bind_mnt) = (mount_id_of(&app), mount_id_of(&bind));
    assert!(
        app_mnt.is_some() && bind_mnt.is_some(),
        "statx reports mnt_id"
    );
    assert_ne!(app_mnt, bind_mnt);
    assert_eq!(
        ident(&app).dev,
        ident(&bind).dev,
        "same st_dev: a bind mount"
    );

    // The mark check refuses the directory above the bind mount (M3).
    let table = MountTable::new(
        mount_sources()
            .unwrap()
            .into_iter()
            .map(|(point, source)| MarkMount { point, source })
            .collect(),
    );
    let roots = vec![code.clone()];
    let home = base.join("home");
    let own = OwnState::new(
        &Dirs {
            home: home.clone(),
            state: home.join("s"),
            cache: home.join("c"),
            config: home.join("g"),
        },
        PathBuf::from("/usr/bin/x"),
    );
    let env = MarkEnv {
        roots: &roots,
        home: &home,
        mounts: &table,
        own_state: &own,
        facts: &Real,
        tty: false,
    };
    let marks = Marks::new();
    let got = mark_check(
        &app,
        &MarkCtx {
            env: &env,
            marks: &marks,
        },
    );
    assert_eq!(got, Err(MarkRefusal::BindMountBelow(bind.clone())));

    // The engine, with root_mnt set from the opened root, does not walk into it either.
    let ops = RealDirOps::confined_to(&base);
    let root = open_verified_root(&ops, &code, ident(&code)).unwrap();
    let parent = open_parent(&ops, &root, Path::new("")).unwrap();
    let root_mnt = mount_id_of(&code).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let canon = |p: &Path| -> std::io::Result<(PathBuf, Identity)> {
        let c = p.canonicalize()?;
        let id = Identity::of(&std::fs::metadata(&c)?);
        Ok((c, id))
    };
    let dirs = Dirs {
        home: home.clone(),
        state: home.join("s"),
        cache: home.join("c"),
        config: home.join("g"),
    };
    let rootset = RootSet::resolve(&dirs, &[code.clone()], None, &home, &canon).unwrap();
    let GuardDecision::Allow(allowed) = decide_path_common(
        &Facts {
            kind: Kind::ExplorerSelection,
            path: app.clone(),
            ident: ident(&app),
            is_symlink: false,
            root_dev: RootDev(ident(&code).dev),
            is_mount_point: false,
        },
        &rootset,
    ) else {
        panic!(
            "the guard allows the directory above the bind mount (it cannot see the bind mount)"
        );
    };
    let policy = DeletePolicy::for_allowed(
        &allowed,
        RootDev(ident(&code).dev),
        root_mnt,
        home,
        ident(&base.join("home")),
        roots,
    )
    .unwrap();
    let err = delete_tree(
        &ops,
        &parent,
        "app".as_ref(),
        ident(&app),
        &policy,
        &AbortFlag::new(),
        &ApplyToken::for_tests(),
    )
    .expect_err("the bind mount must stop the walk");
    assert_eq!(err.error, DeleteError::CrossDevice);
    assert!(base.join("src/data/keep").exists());
    println!("CHILD_OK");
}
