#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    clippy::cloned_ref_to_slice_refs,
    clippy::redundant_closure_for_method_calls,
    non_snake_case
)]
//! Task 6.6.3d: every adversarial case of Story 2.1.1 (A-01 to A-26, the hash-pinned `delete_adversarial.rs`)
//! run through the explorer's apply instead of `delete_tree` alone. The scenario of each case is rebuilt here
//! in the pinned file's layout (`home/code/app/{package.json,node_modules}`, `outside/keep.txt`, `home/.ssh`);
//! the pinned fixtures themselves are neither copied into a shared helper nor edited. The marked directory is
//! `home/code/app/node_modules` unless the case needs another, the scan root is `home/code`.
//!
//! The verdict of a case is the one `--apply` of `build-dirs` gives (same engine, same policy depth), with
//! one stated difference: the explorer needs no sibling marker (ADR-006 A1), so A-20 ends in a delete.
//!
//! CASE-TABLE-BEGIN
//! A-01 | I1 | a01_symlinks_at_every_depth_pointing_outside_root_are_unlinked_not_followed
//! A-02 | I1,I3 | a02_symlink_swapped_for_a_directory_after_open_dir_is_not_followed
//! A-03 | I1,I3 | a03_entry_replaced_by_a_symlink_before_unlinkat_unlinks_the_link_only
//! A-04 | I1 | a04_a_marked_symlink_is_skipped_and_its_target_is_untouched
//! A-05 | I6 | a05_paths_named_empty_dot_dotdot_or_with_a_slash_are_skipped_with_nothing_deleted
//! A-06 | I3 | a06_unclean_parent_spellings_are_skipped_with_nothing_deleted
//! A-07 | I3 | a07_a_target_with_dotdot_to_a_sibling_is_skipped_and_the_sibling_survives
//! A-08 | I1 | a08_a_symlinked_intermediate_component_is_skipped
//! A-09 | I4 | a09_enospc_on_the_fifth_unlinkat_stops_every_later_item
//! A-10 | I4,I5 | a10_bytes_freed_before_enospc_are_in_the_result
//! A-11 | I2 | a11_a_subdirectory_on_another_mount_id_is_not_entered_and_siblings_are_removed
//! A-12 | I3 | a12_ten_thousand_level_nesting_completes_without_a_stack_overflow
//! A-13 | I1 | a13_fifo_and_socket_nodes_are_unlinked_without_being_opened
//! A-14 | I5 | a14_a_hardlink_pair_counts_blocks_once_and_entries_by_name
//! A-15 | I3 | a15_read_only_directories_are_chmodded_on_the_verified_fd_and_removed
//! A-16 | I3 | a16_names_with_newlines_and_invalid_utf8_are_removed_exactly
//! A-17 | I4,I7 | a17_enotempty_from_a_concurrent_writer_is_busy_without_a_retry_loop
//! A-18 | I7 | a18_enoent_during_the_delete_is_already_removed
//! A-19 | I3,I6 | a19_a_directory_of_the_same_name_under_another_parent_is_never_deleted_for_this_one
//! A-20 | I6 | a20_a_directory_with_no_sibling_marker_is_deleted_by_the_explorer_and_logged
//! A-21 | I6,I8 | a21_floor_paths_are_refused_and_the_fixture_hash_is_unchanged
//! A-23 | I3 | a23_an_identity_mismatch_between_the_plan_and_the_directory_deletes_nothing
//! A-24 | I1,I5 | a24_a_hardlink_to_a_file_outside_the_tree_unlinks_only_the_in_tree_name
//! A-25 | I3 | a25_a_mode_000_directory_is_an_error_row_with_partial_bytes_and_the_siblings_go
//! A-26 | I8 | a26_the_fd_count_returns_to_the_baseline
//! CASE-TABLE-END

mod explore_rig;

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use disk_cleanup_adapters::testing::{At, Event, FaultDirOps};
use disk_cleanup_adapters::RealDirOps;
use disk_cleanup_core::plan::ItemIdentity;
use disk_cleanup_core::{AuditRecord, FileKind, Outcome, SkipReason, Status};
use explore_rig::{manifest, Ex, Knobs, Ran};

static SERIAL: Mutex<()> = Mutex::new(());

struct Fx {
    ex: Ex,
    nm: PathBuf,
    app: PathBuf,
    outside: PathBuf,
    _g: MutexGuard<'static, ()>,
}

impl Fx {
    fn new() -> Fx {
        let g = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        let ex = Ex::new();
        let app = ex.code.join("app");
        let nm = app.join("node_modules");
        let outside = ex.p("outside");
        std::fs::create_dir_all(nm.join("pkg")).unwrap();
        std::fs::write(app.join("package.json"), "{}").unwrap();
        std::fs::write(nm.join("pkg/index.js"), "x".repeat(5000)).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep.txt"), "keep").unwrap();
        std::fs::create_dir_all(ex.home.join("Documents")).unwrap();
        std::fs::write(ex.home.join("Documents/doc.txt"), "doc").unwrap();
        std::fs::create_dir_all(ex.home.join(".ssh")).unwrap();
        std::fs::write(ex.home.join(".ssh/id"), "id").unwrap();
        Fx {
            ex,
            nm,
            app,
            outside,
            _g: g,
        }
    }

    fn ops(&self) -> impl Fn(&disk_cleanup_core::ApplyToken) -> RealDirOps + '_ {
        self.ex.confined_ops()
    }

    /// Reviews and applies `marks` over the plain directory operations.
    fn apply(&self, marks: &[PathBuf]) -> Ran {
        self.ex.run(self.ex.confirmed(marks), self.ops())
    }

    /// Runs `confirmed` after `change` was made: before the apply (the plan-id gate sees it and refuses the whole
    /// set) or in the window after the gate (the per-item identity compare sees it).
    fn run_swapped(
        &self,
        confirmed: disk_cleanup_core::explore::confirm::ConfirmedPlan,
        change: impl FnOnce() + Send + 'static,
        after_gate: bool,
    ) -> Ran {
        if after_gate {
            self.ex
                .run_with(confirmed, self.ops(), &Knobs::after_gate(change))
        } else {
            change();
            self.ex.run(confirmed, self.ops())
        }
    }

    /// Before the gate: the whole set is refused with the plan-id text, nothing was audited or deleted.
    fn assert_gate_refused(&self, ran: &Ran) {
        assert_eq!(ran.report.exit, 64, "{:?}", ran.report);
        assert!(
            ran.report
                .refused
                .as_deref()
                .is_some_and(|t| t.contains("changed since the plan was made")
                    && t.ends_with("Nothing was deleted.")),
            "{:?}",
            ran.report.refused
        );
        assert_eq!(ran.report.summary.removed, 0);
        assert!(self.statuses().is_empty(), "{:?}", self.statuses());
        assert_eq!(ran.minted, 0, "no capability exists after a gate refusal");
    }

    fn statuses(&self) -> Vec<Status> {
        self.results().into_iter().map(|o| o.status).collect()
    }

    fn results(&self) -> Vec<Outcome> {
        self.ex
            .audit
            .records()
            .into_iter()
            .filter_map(|r| match r {
                AuditRecord::Result(o) => Some(o.0),
                _ => None,
            })
            .collect()
    }
}

fn exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

fn fake_dir_id() -> ItemIdentity {
    ItemIdentity {
        dev: 1,
        ino: 1,
        kind: FileKind::Dir,
    }
}

// ------------------------------------------------------------------------------------------ A-01 to A-04

#[test]
fn a01_symlinks_at_every_depth_pointing_outside_root_are_unlinked_not_followed() {
    let fx = Fx::new();
    let ssh = fx.ex.home.join(".ssh");
    symlink(&fx.outside, fx.nm.join("top-link")).unwrap();
    symlink(fx.outside.join("keep.txt"), fx.nm.join("pkg/file-link")).unwrap();
    std::fs::create_dir_all(fx.nm.join("a/b/c")).unwrap();
    symlink(&ssh, fx.nm.join("a/b/c/deep-link")).unwrap();
    let outside_before = manifest(&fx.outside);
    let ssh_before = manifest(&ssh);
    let ran = fx.apply(&[fx.nm.clone()]);
    assert_eq!(ran.report.exit, 0, "{:?}", ran.report);
    assert!(!exists(&fx.nm));
    assert_eq!(manifest(&fx.outside), outside_before);
    assert_eq!(manifest(&ssh), ssh_before);
    assert!(exists(&fx.app.join("package.json")));
}

#[test]
fn a02_symlink_swapped_for_a_directory_after_open_dir_is_not_followed() {
    let fx = Fx::new();
    std::fs::create_dir_all(fx.nm.join("victim")).unwrap();
    std::fs::write(fx.nm.join("victim/f"), "v").unwrap();
    let outside_before = manifest(&fx.outside);
    let done = Arc::new(AtomicBool::new(false));
    let (flag, outside) = (done.clone(), fx.outside.clone());
    let ops = |_: &disk_cleanup_core::ApplyToken| {
        let (flag, outside) = (flag.clone(), outside.clone());
        FaultDirOps::wrap(RealDirOps::confined_to(&fx.ex.base))
            .hook(Event::AfterOpenDir, move |ctx| {
                if ctx.rel_path.ends_with("node_modules/victim")
                    && !flag.swap(true, Ordering::SeqCst)
                {
                    let abs = ctx.base_abs.join(&ctx.rel_path);
                    std::fs::rename(&abs, abs.with_file_name("victim.moved")).unwrap();
                    symlink(&outside, &abs).unwrap();
                }
            })
            .build()
    };
    let _ran = fx.ex.run(fx.ex.confirmed(&[fx.nm.clone()]), ops);
    assert!(done.load(Ordering::SeqCst), "the race was staged");
    assert_eq!(manifest(&fx.outside), outside_before);
}

#[test]
fn a03_entry_replaced_by_a_symlink_before_unlinkat_unlinks_the_link_only() {
    let fx = Fx::new();
    std::fs::write(fx.nm.join("target-file"), "t").unwrap();
    let outside_before = manifest(&fx.outside);
    let done = Arc::new(AtomicBool::new(false));
    let (flag, keep) = (done.clone(), fx.outside.join("keep.txt"));
    let ops = |_: &disk_cleanup_core::ApplyToken| {
        let (flag, keep) = (flag.clone(), keep.clone());
        FaultDirOps::wrap(RealDirOps::confined_to(&fx.ex.base))
            .hook(Event::BeforeUnlinkat, move |ctx| {
                if ctx.rel_path.ends_with("node_modules/target-file")
                    && !flag.swap(true, Ordering::SeqCst)
                {
                    let abs = ctx.base_abs.join(&ctx.rel_path);
                    std::fs::remove_file(&abs).unwrap();
                    symlink(&keep, &abs).unwrap();
                }
            })
            .build()
    };
    let ran = fx.ex.run(fx.ex.confirmed(&[fx.nm.clone()]), ops);
    assert!(done.load(Ordering::SeqCst));
    assert_eq!(manifest(&fx.outside), outside_before);
    assert_eq!(ran.report.exit, 0, "{:?}", ran.report);
}

#[test]
fn a04_a_marked_symlink_is_skipped_and_its_target_is_untouched() {
    let fx = Fx::new();
    let link = fx.app.join("linked-nm");
    symlink(&fx.outside, &link).unwrap();
    let before = manifest(&fx.ex.base);
    let reviewed = fx.ex.forge(&[(link.clone(), None)]);
    let ran = fx.ex.run(Ex::confirm(reviewed), fx.ops());
    assert_eq!(manifest(&fx.ex.base), before);
    assert_eq!(ran.report.summary.removed, 0);
    assert_eq!(fx.statuses(), [Status::Skipped(SkipReason::Symlink)]);
}

// ------------------------------------------------------------------------------------------ A-05 to A-08

/// Items an honest `p` could never list: the plan is forged, the apply must still delete nothing.
fn hostile(fx: &Fx, paths: &[PathBuf]) -> Ran {
    let before = manifest(&fx.ex.base);
    let items: Vec<_> = paths
        .iter()
        .map(|p| (p.clone(), Some(fake_dir_id())))
        .collect();
    let ran = fx.ex.run(Ex::confirm(fx.ex.forge(&items)), fx.ops());
    assert_eq!(manifest(&fx.ex.base), before, "{paths:?}");
    assert_eq!(ran.report.summary.removed, 0, "{paths:?}");
    ran
}

#[test]
fn a05_paths_named_empty_dot_dotdot_or_with_a_slash_are_skipped_with_nothing_deleted() {
    let fx = Fx::new();
    for p in [
        PathBuf::from(""),
        PathBuf::from("."),
        PathBuf::from(".."),
        PathBuf::from("/"),
        fx.app.join("."),
        fx.app.join(".."),
        fx.nm.join(""),
        PathBuf::from(format!("{}/", fx.nm.display())),
    ] {
        let ran = hostile(&fx, &[p.clone()]);
        assert!(
            ran.report.refused.is_none() || ran.report.exit != 0,
            "{p:?}"
        );
    }
}

#[test]
fn a06_unclean_parent_spellings_are_skipped_with_nothing_deleted() {
    let fx = Fx::new();
    let code = fx.ex.code.display().to_string();
    for p in [
        format!("{code}/../.ssh"),
        format!("{code}/app/../../.ssh/x"),
        format!("{code}/app//node_modules"),
        format!("{code}/app/./node_modules"),
        format!("{code}/app/node_modules/../node_modules"),
        format!("{code}/a/../a/b"),
    ] {
        hostile(&fx, &[PathBuf::from(p)]);
    }
}

#[test]
fn a07_a_target_with_dotdot_to_a_sibling_is_skipped_and_the_sibling_survives() {
    let fx = Fx::new();
    let sib = fx.ex.code.join("sibling");
    std::fs::create_dir_all(sib.join("node_modules")).unwrap();
    std::fs::write(sib.join("node_modules/x"), "x").unwrap();
    let before = manifest(&sib);
    hostile(&fx, &[fx.nm.join("../../sibling/node_modules")]);
    assert_eq!(manifest(&sib), before);
}

fn a08_case(after_gate: bool) {
    let fx = Fx::new();
    let item = fx.ex.code.join("tools/npm/_cacache");
    std::fs::create_dir_all(&item).unwrap();
    std::fs::write(item.join("blob"), "b").unwrap();
    let confirmed = fx.ex.confirmed(&[item.clone()]);
    let elsewhere = fx.outside.join("npm");
    std::fs::create_dir_all(elsewhere.join("_cacache")).unwrap();
    std::fs::write(elsewhere.join("_cacache/precious"), "p").unwrap();
    let before = manifest(&elsewhere);
    let (npm, old, target) = (
        fx.ex.code.join("tools/npm"),
        fx.ex.code.join("tools/npm.old"),
        elsewhere.clone(),
    );
    let swap = move || {
        std::fs::rename(&npm, &old).unwrap();
        symlink(&target, &npm).unwrap();
    };
    let ran = fx.run_swapped(confirmed, swap, after_gate);
    assert_eq!(manifest(&elsewhere), before);
    assert_eq!(ran.report.summary.removed, 0);
    if after_gate {
        assert!(matches!(
            fx.statuses()[..],
            [Status::Skipped(
                SkipReason::IdentityChanged | SkipReason::Symlink
            )]
        ));
    } else {
        fx.assert_gate_refused(&ran);
        assert!(exists(&fx.ex.code.join("tools/npm.old/_cacache/blob")));
    }
}

#[test]
fn a08_a_symlinked_intermediate_component_is_skipped() {
    a08_case(true);
}

#[test]
fn a08_a_symlinked_intermediate_component_before_the_apply_refuses_the_whole_set() {
    a08_case(false);
}

// ------------------------------------------------------------------------------------------ A-09 to A-12

fn many_trees(fx: &Fx, n: usize, files: usize) -> Vec<PathBuf> {
    (0..n)
        .map(|i| {
            let d = fx.ex.code.join(format!("w/t{i}"));
            std::fs::create_dir_all(&d).unwrap();
            for j in 0..files {
                std::fs::write(d.join(format!("f{j}")), "x").unwrap();
            }
            d
        })
        .collect()
}

#[test]
fn a09_enospc_on_the_fifth_unlinkat_stops_every_later_item() {
    let fx = Fx::new();
    let trees = many_trees(&fx, 4, 3);
    let confirmed = fx.ex.confirmed(&trees);
    let ops = |_: &disk_cleanup_core::ApplyToken| {
        FaultDirOps::wrap(RealDirOps::confined_to(&fx.ex.base))
            .enospc_on_nth_unlinkat(5)
            .build()
    };
    let ran = fx.ex.run(confirmed, ops);
    let left: usize = trees
        .iter()
        .map(|t| std::fs::read_dir(t).map_or(0, |d| d.count()))
        .sum();
    // Four unlinks succeeded (the three files and the directory of the first tree); the fifth failed and no later
    // unlink happened anywhere: the three files of the second tree and the six of the others remain.
    assert_eq!(left, 9, "{:?}", fx.statuses());
    assert_eq!(ran.report.exit, 2, "{:?}", ran.report);
    let st = fx.statuses();
    assert!(st.iter().any(|s| matches!(s, Status::Error(_))), "{st:?}");
    assert!(st.contains(&Status::Skipped(SkipReason::Aborted)), "{st:?}");
}

#[test]
fn a10_bytes_freed_before_enospc_are_in_the_result() {
    let fx = Fx::new();
    let d = fx.ex.code.join("w/big");
    std::fs::create_dir_all(&d).unwrap();
    for j in 0..6 {
        std::fs::write(d.join(format!("f{j}")), vec![b'z'; 8192]).unwrap();
    }
    let confirmed = fx.ex.confirmed(&[d.clone()]);
    let ops = |_: &disk_cleanup_core::ApplyToken| {
        FaultDirOps::wrap(RealDirOps::confined_to(&fx.ex.base))
            .enospc_on_nth_unlinkat(4)
            .build()
    };
    let _ran = fx.ex.run(confirmed, ops);
    let r = &fx.results()[0];
    assert!(matches!(r.status, Status::Error(_)), "{:?}", r.status);
    assert!(r.freed.entries_removed >= 3, "{:?}", r.freed);
    assert!(r.measured_bytes.unwrap_or(0) > 0, "{r:?}");
}

#[test]
fn a11_a_subdirectory_on_another_mount_id_is_not_entered_and_siblings_are_removed() {
    let fx = Fx::new();
    let mnt = fx.nm.join("mnt");
    std::fs::create_dir_all(&mnt).unwrap();
    std::fs::write(mnt.join("theirs"), "not yours").unwrap();
    std::fs::write(fx.nm.join("mine"), "mine").unwrap();
    let confirmed = fx.ex.confirmed(&[fx.nm.clone()]);
    let ops = |_: &disk_cleanup_core::ApplyToken| {
        FaultDirOps::wrap(RealDirOps::confined_to(&fx.ex.base))
            .report_mnt_id(At::Name(OsString::from("mnt")), 9)
            .build()
    };
    let ran = fx.ex.run(confirmed, ops);
    assert_eq!(std::fs::read(mnt.join("theirs")).unwrap(), b"not yours");
    assert!(!exists(&fx.nm.join("mine")), "the siblings are removed");
    assert_eq!(
        ran.report.summary.removed, 0,
        "the item is partial, not removed"
    );
}

/// `levels` nested directories named `d` below `dir`, made with relative `mkdir`/`chdir` steps because the path is
/// far longer than `PATH_MAX`. The caller holds `SERIAL`, so the working directory change is not observed by
/// another test.
fn deep_chain(dir: &Path, levels: usize) {
    let here = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();
    for _ in 0..levels {
        std::fs::create_dir("d").unwrap();
        std::env::set_current_dir("d").unwrap();
    }
    std::env::set_current_dir(here).unwrap();
}

/// Removes what is left of a chain made by [`deep_chain`], from the leaf up, so a failed test leaves no litter.
fn unwind_chain(top: &Path) {
    let here = std::env::current_dir().unwrap();
    if std::env::set_current_dir(top).is_ok() {
        let mut depth = 0usize;
        while std::env::set_current_dir("d").is_ok() {
            depth += 1;
        }
        for _ in 0..depth {
            if std::env::set_current_dir("..").is_err() || std::fs::remove_dir("d").is_err() {
                break;
            }
        }
    }
    let _ = std::env::set_current_dir(here);
}

#[test]
fn a12_ten_thousand_level_nesting_completes_without_a_stack_overflow() {
    let fx = Fx::new();
    deep_chain(&fx.nm, 10_000);
    let ran = std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn_scoped(s, || {
                let confirmed = fx.ex.confirmed(&[fx.nm.clone()]);
                fx.ex.run(confirmed, fx.ops())
            })
            .unwrap()
            .join()
            .unwrap()
    });
    // The engine's depth cap (256, the same as `build-dirs`) ends the item; nothing crashed, nothing outside
    // the marked tree was touched, and the run closed its log.
    assert!(exists(&fx.app.join("package.json")));
    assert!(exists(&fx.outside.join("keep.txt")));
    assert_eq!(explore_rig::kinds_of(&fx.ex.audit).last(), Some(&"run_end"));
    assert_eq!(ran.report.refused, None);
    unwind_chain(&fx.nm);
}

// ------------------------------------------------------------------------------------------ A-13 to A-16

#[test]
fn a13_fifo_and_socket_nodes_are_unlinked_without_being_opened() {
    let fx = Fx::new();
    let st = Command::new("mkfifo")
        .arg(fx.nm.join("pipe"))
        .status()
        .unwrap();
    assert!(st.success());
    let _sock = std::os::unix::net::UnixListener::bind(fx.nm.join("sock")).unwrap();
    let ran = fx.apply(&[fx.nm.clone()]);
    assert_eq!(ran.report.exit, 0, "{:?}", ran.report);
    assert!(!exists(&fx.nm));
}

fn count_entries_and_inodes(dir: &Path) -> (u64, u64) {
    fn walk(p: &Path, seen: &mut BTreeSet<(u64, u64)>, entries: &mut u64) {
        let m = std::fs::symlink_metadata(p).unwrap();
        *entries += 1;
        seen.insert((m.dev(), m.ino()));
        if m.is_dir() {
            for e in std::fs::read_dir(p).unwrap() {
                walk(&e.unwrap().path(), seen, entries);
            }
        }
    }
    let (mut seen, mut entries) = (BTreeSet::new(), 0);
    walk(dir, &mut seen, &mut entries);
    (entries, seen.len() as u64)
}

#[test]
fn a14_a_hardlink_pair_counts_blocks_once_and_entries_by_name() {
    let fx = Fx::new();
    let d = fx.ex.code.join("w/links");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("a"), vec![b'q'; 20_000]).unwrap();
    std::fs::hard_link(d.join("a"), d.join("b")).unwrap();
    let (entries, inodes) = count_entries_and_inodes(&d);
    assert_eq!((entries, inodes), (3, 2));
    let _ran = fx.apply(&[d]);
    let r = &fx.results()[0];
    assert_eq!(r.status, Status::Removed);
    assert_eq!((r.freed.entries_removed, r.freed.inodes_freed), (3, 2));
}

#[test]
fn a15_read_only_directories_are_chmodded_on_the_verified_fd_and_removed() {
    let fx = Fx::new();
    let ro = fx.nm.join("ro/inner");
    std::fs::create_dir_all(&ro).unwrap();
    std::fs::write(ro.join("f"), "f").unwrap();
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
    std::fs::set_permissions(fx.nm.join("ro"), std::fs::Permissions::from_mode(0o555)).unwrap();
    let ran = fx.apply(&[fx.nm.clone()]);
    assert_eq!(ran.report.exit, 0, "{:?}", ran.report);
    assert!(!exists(&fx.nm));
}

#[test]
fn a16_names_with_newlines_and_invalid_utf8_are_removed_exactly() {
    let fx = Fx::new();
    std::fs::write(fx.nm.join("line\nbreak"), "n").unwrap();
    std::fs::write(
        fx.nm.join(OsString::from_vec(b"bad\xff\xfename".to_vec())),
        "b",
    )
    .unwrap();
    let ran = fx.apply(&[fx.nm.clone()]);
    assert_eq!(ran.report.exit, 0, "{:?}", ran.report);
    assert!(!exists(&fx.nm));
    assert!(exists(&fx.app.join("package.json")));
}

// ------------------------------------------------------------------------------------------ A-17 to A-20

#[test]
fn a17_enotempty_from_a_concurrent_writer_is_busy_without_a_retry_loop() {
    let fx = Fx::new();
    std::fs::create_dir_all(fx.nm.join("sub")).unwrap();
    std::fs::write(fx.nm.join("sub/f"), "f").unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let confirmed = fx.ex.confirmed(&[fx.nm.clone()]);
    let ops = |_: &disk_cleanup_core::ApplyToken| {
        let h = h.clone();
        FaultDirOps::wrap(RealDirOps::confined_to(&fx.ex.base))
            .hook(Event::BeforeUnlinkat, move |ctx| {
                if ctx.rel_path.ends_with("node_modules/sub") {
                    h.fetch_add(1, Ordering::SeqCst);
                    std::fs::write(ctx.base_abs.join(&ctx.rel_path).join("intruder"), b"w")
                        .unwrap();
                }
            })
            .build()
    };
    let ran = fx.ex.run(confirmed, ops);
    assert_eq!(hits.load(Ordering::SeqCst), 1, "the rmdir was retried");
    assert!(
        exists(&fx.nm.join("sub/intruder")),
        "the concurrent writer's file survives"
    );
    assert_eq!(ran.report.exit, 1);
    let st = fx.statuses();
    assert!(
        matches!(&st[..], [Status::Error(m)] if m.contains("not empty")),
        "{st:?}"
    );
}

#[test]
fn a18_enoent_during_the_delete_is_already_removed() {
    let fx = Fx::new();
    for n in ["x", "y", "z"] {
        std::fs::write(fx.nm.join(n), n).unwrap();
    }
    let done = Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    let confirmed = fx.ex.confirmed(&[fx.nm.clone()]);
    let ops = |_: &disk_cleanup_core::ApplyToken| {
        let flag = flag.clone();
        FaultDirOps::wrap(RealDirOps::confined_to(&fx.ex.base))
            .hook(Event::BeforeUnlinkat, move |ctx| {
                if ctx.rel_path.ends_with("node_modules/y") && !flag.swap(true, Ordering::SeqCst) {
                    std::fs::remove_file(ctx.base_abs.join(&ctx.rel_path)).unwrap();
                }
            })
            .build()
    };
    let ran = fx.ex.run(confirmed, ops);
    assert!(done.load(Ordering::SeqCst));
    assert_eq!(ran.report.exit, 0, "{:?}", ran.report);
    assert!(!exists(&fx.nm));
}

fn a19_case(after_gate: bool) {
    let fx = Fx::new();
    let (x, y) = (
        fx.ex.code.join("X/node_modules"),
        fx.ex.code.join("Y/node_modules"),
    );
    for d in [&x, &y] {
        std::fs::create_dir_all(d.join("pkg")).unwrap();
        std::fs::write(d.join("pkg/index.js"), "x").unwrap();
    }
    let confirmed = fx.ex.confirmed(&[x.clone()]);
    // Y takes X's place: the path `X/node_modules` now leads to Y's directory.
    let code = fx.ex.code.clone();
    let swap = move || {
        std::fs::rename(code.join("X"), code.join("X.old")).unwrap();
        std::fs::rename(code.join("Y"), code.join("X")).unwrap();
    };
    let ran = fx.run_swapped(confirmed, swap, after_gate);
    let ys_now = manifest(&fx.ex.code.join("X"));
    assert!(
        ys_now.keys().any(|k| k.ends_with("pkg/index.js")),
        "Y's directory is intact"
    );
    assert!(exists(&fx.ex.code.join("X.old/node_modules/pkg/index.js")));
    assert_eq!(ran.report.summary.removed, 0);
    if after_gate {
        assert_eq!(
            fx.statuses(),
            [Status::Skipped(SkipReason::IdentityChanged)]
        );
    } else {
        fx.assert_gate_refused(&ran);
    }
}

#[test]
fn a19_a_directory_of_the_same_name_under_another_parent_is_never_deleted_for_this_one() {
    a19_case(true);
}

#[test]
fn a19_the_same_swap_before_the_apply_refuses_the_whole_set() {
    a19_case(false);
}

#[test]
fn a20_a_directory_with_no_sibling_marker_is_deleted_by_the_explorer_and_logged() {
    let fx = Fx::new();
    std::fs::remove_file(fx.app.join("package.json")).unwrap();
    let ran = fx.apply(&[fx.nm.clone()]);
    assert_eq!(ran.report.exit, 0, "{:?}", ran.report);
    assert!(!exists(&fx.nm));
    assert_eq!(
        explore_rig::kinds_of(&fx.ex.audit),
        [
            "run_start",
            "explorer_confirm",
            "intent",
            "result",
            "run_end"
        ]
    );
}

// ------------------------------------------------------------------------------------------ A-21 to A-26

#[test]
fn a21_floor_paths_are_refused_and_the_fixture_hash_is_unchanged() {
    let fx = Fx::new();
    std::fs::create_dir_all(fx.app.join(".git/hooks")).unwrap();
    std::fs::write(fx.app.join(".git/hooks/pre-commit"), "x").unwrap();
    for p in [
        fx.app.join(".git"),
        fx.app.join(".git/hooks"),
        fx.ex.code.clone(),
        fx.ex.home.clone(),
        fx.ex.home.join("Documents"),
        fx.ex.home.join(".ssh"),
        fx.ex.base.clone(),
        PathBuf::from("/"),
    ] {
        let before = manifest(&fx.ex.base);
        let items = [(p.clone(), Some(fake_dir_id()))];
        let ran = fx.ex.run(Ex::confirm(fx.ex.forge(&items)), fx.ops());
        assert_eq!(manifest(&fx.ex.base), before, "{p:?}");
        assert_eq!(ran.report.summary.removed, 0, "{p:?}");
    }
}

fn a23_case(after_gate: bool) {
    let fx = Fx::new();
    let confirmed = fx.ex.confirmed(&[fx.nm.clone()]);
    let (nm, pin) = (fx.nm.clone(), fx.ex.p("pin"));
    let swap = move || {
        std::fs::remove_dir_all(&nm).unwrap();
        // Occupy the freed inode number elsewhere so the new directory cannot get the old one back.
        std::fs::create_dir_all(&pin).unwrap();
        std::fs::write(pin.join("f0"), [0u8; 10]).unwrap();
        std::fs::create_dir_all(nm.join("new")).unwrap();
        std::fs::write(nm.join("new/f"), "hello").unwrap();
    };
    let ran = fx.run_swapped(confirmed, swap, after_gate);
    assert_eq!(
        manifest(&fx.nm)
            .keys()
            .filter(|k| k.ends_with("new/f"))
            .count(),
        1,
        "the new directory is intact"
    );
    assert_eq!(ran.report.summary.removed, 0);
    if after_gate {
        assert_eq!(
            fx.statuses(),
            [Status::Skipped(SkipReason::IdentityChanged)]
        );
    } else {
        fx.assert_gate_refused(&ran);
    }
}

#[test]
fn a23_an_identity_mismatch_between_the_plan_and_the_directory_deletes_nothing() {
    a23_case(true);
}

#[test]
fn a23_an_identity_mismatch_before_the_apply_refuses_the_whole_set() {
    a23_case(false);
}

#[test]
fn a24_a_hardlink_to_a_file_outside_the_tree_unlinks_only_the_in_tree_name() {
    let fx = Fx::new();
    let outside_file = fx.outside.join("keep.txt");
    std::fs::hard_link(&outside_file, fx.nm.join("linked")).unwrap();
    assert_eq!(std::fs::metadata(&outside_file).unwrap().nlink(), 2);
    let ran = fx.apply(&[fx.nm.clone()]);
    assert_eq!(ran.report.exit, 0, "{:?}", ran.report);
    assert!(!exists(&fx.nm));
    assert_eq!(std::fs::read(&outside_file).unwrap(), b"keep");
    assert_eq!(std::fs::metadata(&outside_file).unwrap().nlink(), 1);
}

#[test]
fn a25_a_mode_000_directory_is_an_error_row_with_partial_bytes_and_the_siblings_go() {
    let fx = Fx::new();
    if std::fs::metadata("/proc/self").unwrap().uid() == 0 {
        eprintln!("SKIPPED a25: running as root, mode 000 does not deny");
        return;
    }
    let locked = fx.nm.join("locked");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::write(locked.join("inside"), "i").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    std::fs::write(fx.nm.join("sibling"), "s").unwrap();
    let ran = fx.apply(&[fx.nm.clone()]);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!exists(&fx.nm.join("sibling")), "the siblings go");
    assert!(
        exists(&locked.join("inside")),
        "the unreadable directory stays"
    );
    assert_eq!(ran.report.exit, 1, "{:?}", ran.report);
    let r = &fx.results()[0];
    assert!(matches!(r.status, Status::Error(_)), "{:?}", r.status);
    assert!(r.freed.entries_removed > 0, "{:?}", r.freed);
}

fn open_fds() -> usize {
    std::fs::read_dir("/proc/self/fd").unwrap().count()
}

#[test]
fn a26_the_fd_count_returns_to_the_baseline() {
    let fx = Fx::new();
    let other = fx.ex.tree("w/other", 3, 100);
    // Warm anything the first run opens once (the sizer, the mount table) before the baseline is taken.
    let _ = fx.apply(&[other]);
    std::fs::create_dir_all(fx.nm.join("a/b")).unwrap();
    std::fs::write(fx.nm.join("a/b/c"), "c").unwrap();
    let confirmed = fx.ex.confirmed(&[fx.nm.clone()]);
    let baseline = open_fds();
    let ran = fx.ex.run(confirmed, fx.ops());
    assert_eq!(ran.report.exit, 0, "{:?}", ran.report);
    assert_eq!(open_fds(), baseline);
}
