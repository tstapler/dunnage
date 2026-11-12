#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cloned_ref_to_slice_refs,
    clippy::redundant_closure_for_method_calls,
    non_snake_case
)]
//! Task 6.6.3b, the mount legs of the TOCTOU list (UX-T37 (c) and (g)): a tmpfs mounted on a reviewed
//! directory, and a real same-device bind mount made below it, both after the plan and before the apply. The
//! parent test re-runs a child test inside `unshare -rm` (no root needed); where that is not permitted the leg
//! is a named skip, and `DC_REQUIRE_BIND_MOUNT=1` turns the skip into a failure.

mod explore_rig;

use std::path::Path;
use std::process::Command;

use dunnage_core::{SkipReason, Status};
use explore_rig::{manifest, Ex, Knobs};

const VAR: &str = "DC_EX_MOUNT_CHILD";

fn in_namespace(child: &str) {
    let out = Command::new("unshare")
        .args(["-rm"])
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", child, "--nocapture"])
        .env(VAR, "1")
        .output();
    let ok = match &out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).contains("CHILD_OK"),
        Err(_) => false,
    };
    if !ok {
        let why = out.map_or_else(
            |e| e.to_string(),
            |o| {
                String::from_utf8_lossy(&o.stderr).into_owned()
                    + &String::from_utf8_lossy(&o.stdout)
            },
        );
        assert!(
            std::env::var_os("DC_REQUIRE_BIND_MOUNT").is_none(),
            "DC_REQUIRE_BIND_MOUNT is set and the child failed or unshare -rm does not work here: {why}"
        );
        eprintln!("SKIPPED {child}: unshare -rm unavailable or the child failed: {why}");
    }
}

fn sh(cmd: &str, path: &Path, src: &Path) {
    let st = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .arg("sh")
        .arg(path)
        .arg(src)
        .status()
        .unwrap();
    assert!(st.success(), "{cmd}");
}

#[test]
fn safety_apply_should_skip_a_reviewed_directory_that_a_tmpfs_now_covers() {
    in_namespace("child_tmpfs_on_the_item");
}

#[test]
fn safety_apply_should_refuse_the_whole_set_When_a_tmpfs_covers_a_reviewed_directory_before_the_apply(
) {
    in_namespace("child_tmpfs_on_the_item_before_the_apply");
}

/// Mounts a tmpfs on the reviewed directory `a` after the plan: before the apply (`after_gate` false, the
/// plan-id gate refuses the whole set) or in the window after the gate (the per-item compare skips it).
fn tmpfs_on_the_item(after_gate: bool) {
    let ex = Ex::new();
    let a = ex.tree("app/dist", 3, 3000);
    let confirmed = ex.confirmed(&[a.clone()]);
    let under = manifest(&a);
    let cover = {
        let a = a.clone();
        move || {
            sh("mount -t tmpfs tmpfs \"$1\"", &a, &a);
            std::fs::write(a.join("on-the-tmpfs"), b"x").unwrap();
        }
    };
    let ran = if after_gate {
        ex.run_with(confirmed, ex.confined_ops(), &Knobs::after_gate(cover))
    } else {
        cover();
        ex.run(confirmed, ex.confined_ops())
    };
    let covered = manifest(&a);
    assert!(
        covered.contains_key(&a.join("on-the-tmpfs")),
        "the tmpfs is still there"
    );
    sh("umount \"$1\"", &a, &a);
    assert_eq!(
        manifest(&a),
        under,
        "the directory under the mount is intact"
    );
    let statuses: Vec<Status> = ex
        .audit
        .records()
        .into_iter()
        .filter_map(|r| match r {
            dunnage_core::AuditRecord::Result(o) => Some(o.0.status),
            _ => None,
        })
        .collect();
    if after_gate {
        assert_eq!(statuses, [Status::Skipped(SkipReason::IdentityChanged)]);
    } else {
        assert!(statuses.is_empty(), "{statuses:?}");
        assert_eq!(ran.report.exit, 64, "{:?}", ran.report);
        assert!(ran.report.refused.is_some());
    }
    assert_eq!(ran.report.summary.removed, 0);
}

#[test]
fn child_tmpfs_on_the_item() {
    if std::env::var_os(VAR).is_none() {
        return;
    }
    tmpfs_on_the_item(true);
    println!("CHILD_OK");
}

#[test]
fn child_tmpfs_on_the_item_before_the_apply() {
    if std::env::var_os(VAR).is_none() {
        return;
    }
    tmpfs_on_the_item(false);
    println!("CHILD_OK");
}

#[test]
fn safety_apply_should_never_delete_the_source_of_a_bind_mount_made_below_a_reviewed_directory() {
    in_namespace("child_bind_mount_below_the_item");
}

#[test]
fn child_bind_mount_below_the_item() {
    if std::env::var_os(VAR).is_none() {
        return;
    }
    let ex = Ex::new();
    let a = ex.tree("app/dist", 3, 3000);
    let src = ex.p("src");
    std::fs::create_dir_all(src.join("data")).unwrap();
    std::fs::write(src.join("data/keep"), b"precious").unwrap();
    let confirmed = ex.confirmed(&[a.clone()]);
    std::fs::create_dir_all(a.join("bind")).unwrap();
    sh("mount --bind \"$2\" \"$1/bind\"", &a, &src);
    let src_before = manifest(&src);
    let ran = ex.run(confirmed, ex.confined_ops());
    assert_eq!(
        manifest(&src),
        src_before,
        "the bind mount's source is intact"
    );
    assert!(a.exists());
    assert_eq!(ran.report.summary.removed, 0, "{:?}", ran.report);
    sh("umount \"$1/bind\"", &a, &a);
    println!("CHILD_OK");
}
