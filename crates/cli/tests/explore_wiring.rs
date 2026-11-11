#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 6.5.4, Task 6.5.4c: the production `mark_check` closure, built by the wiring from the real
//! `lstat`, `/proc/self/mountinfo` and `Dirs` (the hand-off of Story 6.6.1).

mod common;

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use common::Fixture;
use disk_cleanup_cli::wiring::explore::mark_check_fn;
use disk_cleanup_core::explore::input::MarkQuery;
use disk_cleanup_core::explore::marks::Marks;
use disk_cleanup_core::explore::msg::MarkRefusal;
use disk_cleanup_core::explore::msg::{NodeEvent, ScanMode, NO_PARENT};
use disk_cleanup_core::explore::rows::{Badge, Row, RowId, RowKind};
use disk_cleanup_core::explore::testing::DirInfo;
use disk_cleanup_core::explore::tree::{ExploreTree, NodeId};
use disk_cleanup_core::roots::RootSet;
use disk_cleanup_core::Dirs;

/// A real `NodeId`: the row needs one and only a tree hands them out.
fn node_id() -> NodeId {
    let mut tree = ExploreTree::new();
    let event = NodeEvent::Dir {
        id: 0,
        parent: NO_PARENT,
        name: OsString::from("/r"),
        dev: 1,
        info: DirInfo::default(),
        subdirs: 0,
        from_cache: false,
        painted: false,
    };
    tree.apply_batch(tree.generation(), ScanMode::Fresh, vec![event])
        .unwrap();
    let id = tree.roots().next().unwrap();
    id
}

fn row(complete: bool, boundary: bool) -> Row {
    Row {
        id: RowId::Dir(node_id()),
        kind: RowKind::Dir,
        name: OsString::from("x"),
        usage: 4096,
        apparent: 10,
        meta: None,
        items: Some(1),
        count: 0,
        complete,
        unreadable: false,
        boundary,
        at_least: false,
        second_link: false,
        badge: Badge::None,
    }
}

type Check = std::sync::Arc<disk_cleanup_cli::wiring::explore::MarkCheckFn>;

fn check_for(dirs: &Dirs, roots: &[&Path]) -> Check {
    let configured: Vec<PathBuf> = roots.iter().map(|r| r.to_path_buf()).collect();
    let set = RootSet::resolve(
        dirs,
        &[],
        Some(&configured),
        &dirs.home,
        &disk_cleanup_cli::wiring::dirs::real_canon,
    )
    .unwrap();
    mark_check_fn(dirs, &set, PathBuf::from("/opt/disk-cleanup"), false)
}

fn ask(check: &Check, path: &Path, row: &Row) -> Result<(), MarkRefusal> {
    let marks = Marks::new();
    check(&MarkQuery {
        path,
        row,
        marks: &marks,
    })
    .map(|_| ())
}

fn fixture_dirs(f: &Fixture) -> Dirs {
    Dirs {
        home: f.home(),
        state: f.home().join("code/st"),
        cache: f.xdg("cache"),
        config: f.xdg("config"),
    }
}

#[test]
fn a_directory_below_a_root_should_pass_and_one_still_scanning_should_not() {
    let f = Fixture::new();
    let dirs = fixture_dirs(&f);
    let app = fs::canonicalize(f.home()).unwrap().join("code/app");
    fs::create_dir_all(&app).unwrap();
    let check = check_for(&dirs, &[&f.home().join("code")]);
    assert_eq!(ask(&check, &app, &row(true, false)), Ok(()));
    assert_eq!(
        ask(&check, &app, &row(false, false)),
        Err(MarkRefusal::StillScanning)
    );
}

#[test]
fn a_symlink_row_and_a_symlinked_parent_should_be_refused_from_the_real_lstat() {
    let f = Fixture::new();
    let dirs = fixture_dirs(&f);
    let code = fs::canonicalize(f.home()).unwrap().join("code");
    fs::create_dir_all(code.join("app/inner")).unwrap();
    std::os::unix::fs::symlink(code.join("app"), code.join("link")).unwrap();
    let check = check_for(&dirs, &[&f.home().join("code")]);
    assert_eq!(
        ask(&check, &code.join("link"), &row(true, false)),
        Err(MarkRefusal::SymlinkRow)
    );
    assert_eq!(
        ask(&check, &code.join("link/inner"), &row(true, false)),
        Err(MarkRefusal::SymlinkRow)
    );
}

#[test]
fn the_tools_own_state_should_be_refused_whatever_the_roots_admit() {
    let f = Fixture::new();
    let dirs = fixture_dirs(&f);
    let code = fs::canonicalize(f.home()).unwrap().join("code");
    let state = code.join("st/disk-cleanup");
    fs::create_dir_all(&state).unwrap();
    let check = check_for(&dirs, &[&f.home().join("code")]);
    assert_eq!(
        ask(&check, &state.join("audit.jsonl"), &row(true, false)),
        Err(MarkRefusal::OwnState)
    );
    assert_eq!(
        ask(&check, &code.join("st"), &row(true, false)),
        Err(MarkRefusal::OwnState)
    );
}

#[test]
fn a_real_mount_point_and_a_boundary_row_should_be_refused() {
    let f = Fixture::new();
    let dirs = fixture_dirs(&f);
    let check = check_for(&dirs, &[Path::new("/dev")]);
    if Path::new("/dev/shm").is_dir()
        && fs::read_to_string("/proc/self/mountinfo")
            .unwrap()
            .contains(" /dev/shm ")
    {
        assert_eq!(
            ask(&check, Path::new("/dev/shm"), &row(true, false)),
            Err(MarkRefusal::MountPoint)
        );
    }
    let code = fs::canonicalize(f.home()).unwrap().join("code");
    fs::create_dir_all(code.join("elsewhere")).unwrap();
    let check = check_for(&dirs, &[&f.home().join("code")]);
    assert_eq!(
        ask(&check, &code.join("elsewhere"), &row(true, true)),
        Err(MarkRefusal::MountPoint)
    );
}
