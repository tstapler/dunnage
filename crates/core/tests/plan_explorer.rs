#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 6.6.2 (Task 6.6.2a): the `explorer-selection` kind and `explorer_plan`, over fake ports.

#[path = "common/xfs.rs"]
mod xfs;

use std::path::{Path, PathBuf};

use disk_cleanup_core::explorer_plan::{explorer_plan, CleanerView, ExplorerPlanCtx};
use disk_cleanup_core::plan::{OnlyMode, Planned};
use disk_cleanup_core::ports::{ExtentAttribution, ItemExtents, SharedExtent};
use disk_cleanup_core::selector::{KindSelector, Selection, SelectorError};
use disk_cleanup_core::target::Guarded;
use disk_cleanup_core::testing::FakeExtentProbe;
use disk_cleanup_core::{Kind, Risk, SelectorId, SkipReason};
use xfs::{none, with_ctx, World};

fn p(s: &str) -> PathBuf {
    PathBuf::from(s)
}

fn will_not(r: SkipReason) -> Planned {
    Planned::WillNot(r)
}

// ---------------------------------------------------------------- the kind exists, and only the explorer names it

#[test]
fn safety_explorer_selection_is_destructive_hidden_and_unnameable_When_the_registry_is_read() {
    assert_eq!(SelectorId::ExplorerSelection.tier(), Risk::Destructive);
    assert_eq!(Kind::ExplorerSelection.risk(), Risk::Destructive);
    assert!(SelectorId::ExplorerSelection.is_destructive());
    // `kinds` prints SelectorId::ALL: the output cannot change with the explorer.
    assert!(!SelectorId::ALL.contains(&SelectorId::ExplorerSelection));
    assert!(!Kind::ALL.contains(&Kind::ExplorerSelection));
    // Not in the default --apply set.
    assert!(!KindSelector::default_apply().contains(SelectorId::ExplorerSelection));
    // Not nameable, whatever the registry says is built.
    let everything: Vec<SelectorId> = SelectorId::ALL
        .iter()
        .copied()
        .chain([SelectorId::ExplorerSelection])
        .collect();
    for list in ["explorer-selection", "caches,explorer-selection"] {
        let err = KindSelector::parse(list, &everything).unwrap_err();
        assert_eq!(
            err,
            SelectorError::Unnameable {
                id: "explorer-selection"
            }
        );
        assert_eq!(
            err.to_string(),
            "error: explorer-selection is not a kind you can name; it exists only inside 'disk-cleanup explore'. Nothing was deleted."
        );
    }
    // The explorer's own selection needs the plan-id gate and echoes the flags the store keeps.
    let sel = Selection::explorer(vec![p("/home/u/code/a")]);
    assert!(sel.needs_plan_id());
    assert_eq!(sel.kinds_resolved(), "explorer-selection");
    assert_eq!(sel.flags(), "--only /home/u/code/a");
}

// ---------------------------------------------------------------- the items are the marks, every guard runs

fn five_marks() -> World {
    let w = World::with_roots(&["/home/u/code"]).mounts(&[("/home/u/code/holder/mnt", 2)]);
    w.fs.dir("/home/u/code/app/node_modules", 3 << 20)
        .dir("/home/u/code/plain", 1 << 20)
        .file("/home/u/code/notes.txt", 4096)
        .dir("/home/u/code/holder", 2 << 20)
        .dir("/home/u/code/holder/mnt", 0);
    w.fs.on_dev("/home/u/code/holder/mnt", 2);
    w
}

fn marks5() -> Vec<PathBuf> {
    [
        "/home/u/code/app/node_modules",
        "/home/u/code/plain",
        "/home/u/code/notes.txt",
        "/home/u/code/gone",
        "/home/u/code/holder",
    ]
    .map(p)
    .to_vec()
}

#[test]
fn explorer_plan_should_make_one_item_per_mark_and_run_the_guards_on_fresh_data_When_five_kinds_of_mark_are_planned(
) {
    let w = five_marks();
    let plan = with_ctx(&w, &none(), |ctx| {
        explorer_plan(&marks5(), OnlyMode::Explorer, ctx).unwrap()
    });
    let row = |name: &str| {
        plan.rows
            .iter()
            .find(|r| r.path == Path::new(name))
            .unwrap_or_else(|| panic!("no row for {name}"))
    };
    for ok in [
        "/home/u/code/app/node_modules",
        "/home/u/code/plain",
        "/home/u/code/notes.txt",
    ] {
        assert_eq!(row(ok).planned, Planned::Will, "{ok}");
        assert!(row(ok).identity.is_some());
    }
    assert_eq!(row("/home/u/code/gone").planned, will_not(SkipReason::Gone));
    assert!(row("/home/u/code/gone").identity.is_none());
    // The directory that holds another filesystem stays FREE, with the M3 note.
    let holder = row("/home/u/code/holder");
    assert_eq!(holder.planned, Planned::Will);
    assert_eq!(
        holder.note.as_deref(),
        Some("contains another filesystem (device 2); it is not entered and the directory stays")
    );
    // sizes come from the tree walk, and the total counts the rows that would be removed.
    assert_eq!(row("/home/u/code/plain").attributed_bytes, 1 << 20);
    assert_eq!(plan.total_bytes(), (3 << 20) + (1 << 20) + 4096 + (2 << 20));
    assert_eq!(plan.will_count(), 4);
    // The engine's plan holds one item per mark that exists, none for the vanished one, all of the new kind.
    assert_eq!(plan.plan.items.len(), 4);
    assert!(plan
        .plan
        .items
        .iter()
        .all(|i| i.candidate.kind() == Kind::ExplorerSelection
            && i.candidate.selector() == SelectorId::ExplorerSelection));
    // The flags are the quoted marks (what the store keeps), gone ones included.
    assert_eq!(plan.selection.flags().matches("--only ").count(), 5);
    // Nothing was discovered: only the marks, their ancestors and `.git` probes were stat-ed.
    let seen = w.fs.lstats.lock().unwrap().clone();
    assert!(
        seen.iter()
            .all(|s| marks5().iter().any(|m| s.starts_with(m))
                || marks5().iter().any(|m| m.starts_with(s))),
        "{seen:?}"
    );
}

#[test]
fn explorer_plan_should_refuse_each_rule_m1_to_m5_as_a_skip_row_When_the_mark_breaks_it() {
    let w = World::with_roots(&["/home/u/code"])
        .mounts(&[("/home/u/code/mnt", 2), ("/home/u/code/bound/inner", 1)]);
    w.fs.dir("/home/u/code/mnt", 0)
        .dir("/home/u/Documents/x", 10)
        .dir("/home/u/code/app/.git/objects", 10)
        .dir("/home/u/code/real", 10)
        .symlink("/home/u/code/link")
        .dir("/home/u/code/bound", 10)
        .dir("/home/u/code/bound/inner", 0)
        .dir("/home/u/.local/state/disk-cleanup", 10)
        .dir("/home/u/code/ok", 10);
    w.fs.on_dev("/home/u/code/mnt", 2);
    // The parent becomes a symlink after the child exists (lstat of the child still answers, as it would
    // through the link).
    w.fs.file("/home/u/code/linked-parent/child", 1)
        .symlink("/home/u/code/linked-parent");
    let marks: Vec<PathBuf> = [
        "/home/u/code",
        "/home/u/Documents/x",
        "/home/u/code/app/.git/objects",
        "/home/u/code/link",
        "/home/u/code/linked-parent/child",
        "/home/u/code/mnt",
        "/home/u/code/bound",
        "/home/u/.local/state/disk-cleanup",
        "/home/u/code/ok",
    ]
    .map(p)
    .to_vec();
    let plan = with_ctx(&w, &none(), |ctx| {
        explorer_plan(&marks, OnlyMode::Explorer, ctx).unwrap()
    });
    let status = |name: &str| {
        plan.rows
            .iter()
            .find(|r| r.path == Path::new(name))
            .unwrap()
            .planned
    };
    assert_eq!(status("/home/u/code"), will_not(SkipReason::ProtectedPath));
    assert_eq!(
        status("/home/u/Documents/x"),
        will_not(SkipReason::OutsideRoot)
    );
    assert_eq!(
        status("/home/u/code/app/.git/objects"),
        will_not(SkipReason::ProtectedPath)
    );
    assert_eq!(status("/home/u/code/link"), will_not(SkipReason::Symlink));
    // The engine's own order: a mount of another device is `CrossDevice` before `MountPoint`.
    assert_eq!(
        status("/home/u/code/mnt"),
        will_not(SkipReason::CrossDevice)
    );
    assert_eq!(
        status("/home/u/code/bound"),
        will_not(SkipReason::MountPoint)
    );
    assert_eq!(
        status("/home/u/.local/state/disk-cleanup"),
        will_not(SkipReason::OutsideRoot)
    );
    assert_eq!(status("/home/u/code/ok"), Planned::Will);
    // A symlinked parent: refused (the file under it is not a symlink itself, M4 sees the parent).
    assert_eq!(
        status("/home/u/code/linked-parent/child"),
        will_not(SkipReason::Symlink)
    );
    assert_eq!(plan.will_count(), 1);
}

struct SkipsEverything(&'static str);

impl CleanerView for SkipsEverything {
    fn guard_of(&self, path: &Path) -> Option<Guarded> {
        path.ends_with(self.0)
            .then_some(Guarded::Skip(SkipReason::RunningBuild))
    }
}

#[test]
fn explorer_plan_should_add_advice_without_changing_the_status_When_the_cleaner_would_skip_the_mark(
) {
    let w = five_marks();
    let view = SkipsEverything("node_modules");
    let plan = with_ctx(&w, &view, |ctx| {
        explorer_plan(&marks5(), OnlyMode::Explorer, ctx).unwrap()
    });
    let nm = plan
        .rows
        .iter()
        .find(|r| r.path.ends_with("node_modules"))
        .unwrap();
    assert_eq!(nm.planned, Planned::Will);
    assert_eq!(
        nm.advice.as_deref(),
        Some(
            format!(
                "the cleaner would skip this: {}",
                SkipReason::RunningBuild.reason_text()
            )
            .as_str()
        )
    );
    assert!(plan
        .rows
        .iter()
        .filter(|r| !r.path.ends_with("node_modules"))
        .all(|r| r.advice.is_none()));
}

#[test]
fn explorer_plan_should_add_an_informational_risk_row_without_blocking_When_a_mark_holds_a_git_repository(
) {
    let w = World::new();
    w.fs.dir("/home/u/code/repo", 10)
        .dir("/home/u/code/repo/.git", 5)
        .dir("/home/u/code/loose", 10);
    let marks = vec![p("/home/u/code/repo"), p("/home/u/code/loose")];
    let plan = with_ctx(&w, &none(), |ctx| {
        explorer_plan(&marks, OnlyMode::Explorer, ctx).unwrap()
    });
    let repo = &plan.rows[1];
    assert_eq!(repo.path, Path::new("/home/u/code/repo"));
    assert_eq!(
        repo.risk.as_deref(),
        Some("git repository: unpushed work is lost")
    );
    assert_eq!(repo.planned, Planned::Will);
    assert!(plan.rows[0].risk.is_none());
}

#[test]
fn explorer_plan_should_subtract_shared_extents_When_the_probe_measured_them() {
    let w = World::new();
    w.fs.dir("/home/u/code/big", 10_000);
    let probe = FakeExtentProbe::new(ExtentAttribution::Measured(vec![ItemExtents {
        path: p("/home/u/code/big"),
        shared: vec![SharedExtent {
            physical: 0,
            length: 4000,
        }],
    }]));
    let sizer = xfs::XSizer(&w.fs);
    let ctx = ExplorerPlanCtx {
        roots: &w.roots,
        own_state: &w.own,
        fs: &w.fs,
        probe: &w.probe,
        clock: &w.clock,
        sizer: &sizer,
        extents: Some(&probe),
        cleaner: &disk_cleanup_core::explorer_plan::NoCleanerView,
    };
    let plan = explorer_plan(&[p("/home/u/code/big")], OnlyMode::Explorer, &ctx).unwrap();
    let row = &plan.rows[0];
    assert_eq!((row.shared_bytes, row.attributed_bytes), (Some(4000), 6000));
    assert_eq!(plan.total_bytes(), 6000);
}

// ---------------------------------------------------------------- modes and limits

#[test]
fn explorer_plan_should_refuse_in_strict_mode_and_drop_in_the_survivor_mode_When_a_mark_vanished() {
    let w = five_marks();
    let err = with_ctx(&w, &none(), |ctx| {
        explorer_plan(&marks5(), OnlyMode::Strict, ctx).unwrap_err()
    });
    assert!(err.text().contains("/home/u/code/gone"), "{}", err.text());
    // Drop the rows of vanished marks (the survivors' plan) in the third mode.
    let survivors = with_ctx(&w, &none(), |ctx| {
        explorer_plan(&marks5(), OnlyMode::Tolerant, ctx).unwrap()
    });
    assert_eq!(survivors.rows.len(), 4);
}

#[test]
fn explorer_plan_should_store_twenty_512_byte_marks_and_refuse_a_quoted_text_over_the_cap_as_path_too_long(
) {
    let w = World::new();
    let long = |i: usize, ch: char| {
        let head = format!("/home/u/code/d{i:02}/");
        format!("{head}{}", ch.to_string().repeat(512 - head.len()))
    };
    let fits: Vec<PathBuf> = (0..20).map(|i| p(&long(i, 'x'))).collect();
    for m in &fits {
        w.fs.dir(m.to_str().unwrap(), 10);
    }
    let plan = with_ctx(&w, &none(), |ctx| {
        explorer_plan(&fits, OnlyMode::Explorer, ctx).unwrap()
    });
    assert_eq!(plan.will_count(), 20);
    let saved = plan.saved_selection(1000, 5);
    assert!(saved.flags.len() <= 24 * 1024, "{}", saved.flags.len());
    assert_eq!(saved.kinds, "explorer-selection");
    assert!(saved.flags.starts_with("--only "));

    // Each quote becomes four bytes once shell-quoted: 20 such marks are over 24 KiB.
    let quoted: Vec<PathBuf> = (0..20).map(|i| p(&long(i, '\''))).collect();
    for m in &quoted {
        w.fs.dir(m.to_str().unwrap(), 10);
    }
    let err = with_ctx(&w, &none(), |ctx| {
        explorer_plan(&quoted, OnlyMode::Explorer, ctx).unwrap_err()
    });
    assert!(matches!(
        err,
        disk_cleanup_core::explorer_plan::ExplorerPlanError::PathTooLong(_)
    ));
    // Nothing was stat-ed for a refused request.
    let before = w.fs.lstats.lock().unwrap().len();
    let _ = with_ctx(&w, &none(), |ctx| {
        explorer_plan(&quoted, OnlyMode::Explorer, ctx)
    });
    assert_eq!(w.fs.lstats.lock().unwrap().len(), before);
}

#[test]
fn explorer_plan_should_hold_no_delete_capability_When_its_source_is_read() {
    let src =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/explorer_plan.rs"))
            .unwrap();
    for banned in [
        "ApplyToken",
        "delete_tree",
        "remove_file",
        "remove_dir",
        "unlink",
    ] {
        assert!(!src.contains(banned), "explorer_plan.rs names {banned}");
    }
}

#[test]
fn explorer_plan_should_call_a_replaced_parent_gone_and_keep_an_unreadable_mark_as_a_skip_row() {
    let w = World::new();
    w.fs.dir("/home/u/code/a", 10)
        .dir("/home/u/code/b", 10)
        .dir("/home/u/code/c", 10);
    w.fs.fail("/home/u/code/a", 13).fail("/home/u/code/b", 20);
    let marks = vec![
        p("/home/u/code/a"),
        p("/home/u/code/b"),
        p("/home/u/code/c"),
    ];
    // ENOTDIR (a parent became a file) is gone; EACCES is not, and fails closed.
    for mode in [OnlyMode::Explorer, OnlyMode::Tolerant] {
        let plan = with_ctx(&w, &none(), |ctx| explorer_plan(&marks, mode, ctx).unwrap());
        let a = plan.rows.iter().find(|r| r.path.ends_with("a")).unwrap();
        assert_eq!(a.planned, will_not(SkipReason::ProtectedPath), "{mode:?}");
        let b = plan.rows.iter().find(|r| r.path.ends_with("b"));
        assert_eq!(b.is_some(), mode == OnlyMode::Explorer, "{mode:?}");
        if let Some(b) = b {
            assert_eq!(b.planned, will_not(SkipReason::Gone));
        }
    }
    let err = with_ctx(&w, &none(), |ctx| {
        explorer_plan(&marks, OnlyMode::Strict, ctx).unwrap_err()
    });
    assert!(err.text().contains("/home/u/code/b"), "{}", err.text());
    // The saved entry keeps the keys of the rows that would be removed only.
    let plan = with_ctx(&w, &none(), |ctx| {
        explorer_plan(&marks, OnlyMode::Explorer, ctx).unwrap()
    });
    let saved = plan.saved_selection(1, 2);
    let keys: Vec<&str> = saved
        .items
        .iter()
        .map(disk_cleanup_core::ItemKey::as_str)
        .collect();
    assert_eq!(keys, ["explorer-selection:/home/u/code/c"]);
}
