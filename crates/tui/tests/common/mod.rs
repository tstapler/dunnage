#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]
//! Shared fixtures: sample trees folded through the core reducer, a spy backend and a `Screen` builder.

use std::convert::Infallible;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use disk_cleanup_core::explore::input::ExploreState;
use disk_cleanup_core::explore::msg::{NodeEvent, ScanMode, ScanSummary, NO_PARENT};
use disk_cleanup_core::explore::rows::{Badge, Sort};
use disk_cleanup_core::explore::testing::{DirInfo, Stats};
use disk_cleanup_core::explore::tree::{ExploreTree, NodeId};
use disk_cleanup_core::explore::view::ViewState;
use disk_cleanup_core::snapshot_types::{AllocSnapshot, FsSnapshot, SpaceUse};
use disk_cleanup_tui::theme::Theme;
use disk_cleanup_tui::view::{ScanFacts, Screen};
use ratatui::backend::{Backend, ClearType, TestBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};

pub const GIB: u64 = 1 << 30;
pub const MIB: u64 = 1 << 20;

pub fn stats(files: u64, subdirs: u64, alloc: u64, size: u64) -> Stats {
    Stats {
        inodes: files + subdirs,
        files,
        dirs: subdirs,
        dir_entries: files + subdirs,
        name_bytes: 8 * (files + subdirs),
        size_bytes: size,
        allocated_bytes: alloc,
        ..Stats::default()
    }
}

pub fn dir_ev(id: u64, parent: u64, name: &str, st: Stats, subdirs: u32, mtime: i64) -> NodeEvent {
    NodeEvent::Dir {
        id,
        parent,
        name: OsString::from(name),
        dev: 1,
        info: DirInfo {
            stats: st,
            mtime,
            ..DirInfo::default()
        },
        subdirs,
        from_cache: false,
        painted: false,
    }
}

pub fn dir_named(id: u64, parent: u64, name: OsString, st: Stats) -> NodeEvent {
    NodeEvent::Dir {
        id,
        parent,
        name,
        dev: 1,
        info: DirInfo {
            stats: st,
            ..DirInfo::default()
        },
        subdirs: 0,
        from_cache: false,
        painted: false,
    }
}

/// `/home/u/code/app` with the children of the wireframe E2. `open` leaves the root and `.git` incomplete.
pub fn sample_tree(finished: bool) -> ExploreTree {
    let mut t = ExploreTree::new();
    let kids: [(&str, u64, u64); 7] = [
        ("node_modules", 217 * GIB / 10, 249 * GIB / 10),
        ("target", 9 * GIB, 94 * GIB / 10),
        (".claude", 19 * GIB / 10, 2 * GIB),
        (".git", 9216 * MIB / 10, GIB),
        ("dist", 80 * MIB, 82 * MIB),
        ("src", 12 * MIB, 12 * MIB),
        ("docs", 4 * MIB, 4 * MIB),
    ];
    let mut ev = vec![dir_ev(
        0,
        NO_PARENT,
        "/home/u/code/app",
        stats(1, 7, 4096, 400),
        7,
        1_700_000_000,
    )];
    for (i, (n, a, s)) in kids.iter().enumerate() {
        let incomplete = !finished && (*n == "target" || *n == ".git");
        ev.push(dir_ev(
            1 + i as u64,
            0,
            n,
            stats(10 + i as u64, 0, *a, *s),
            u32::from(incomplete),
            1_700_000_000 - 86_400 * 63,
        ));
    }
    t.apply_batch(0, ScanMode::Fresh, ev).unwrap();
    if finished {
        t.finish(false);
    }
    t
}

pub fn child(t: &ExploreTree, parent: NodeId, name: &str) -> NodeId {
    t.children(parent).find(|c| t.name(*c) == name).unwrap()
}

pub fn open_root(t: &ExploreTree, st: &mut ExploreState) {
    let root = t.roots().next().unwrap();
    st.view.open_dir(t, root, 0, &|_| Badge::None);
}

pub fn state_for(t: &ExploreTree) -> ExploreState {
    let mut st = ExploreState::new(ViewState::new(Sort::default()));
    open_root(t, &mut st);
    st
}

pub fn btrfs_snapshot() -> FsSnapshot {
    FsSnapshot {
        path: PathBuf::from("/"),
        taken_at: 0,
        total_bytes: 1800 * GIB,
        used_bytes: 1700 * GIB,
        avail_bytes: 70 * GIB + 2 * GIB / 10,
        fs_type: "btrfs".into(),
        btrfs: Some(AllocSnapshot {
            data: SpaceUse {
                bytes_used: 1000 * GIB,
                total_bytes: 1051 * GIB + 2 * GIB / 10,
            },
            metadata: SpaceUse {
                bytes_used: 4 * GIB,
                total_bytes: 5 * GIB + 9 * GIB / 10,
            },
            system: SpaceUse {
                bytes_used: 0,
                total_bytes: 0,
            },
            unallocated: 9 * GIB + 8 * GIB / 10,
        }),
    }
}

/// The owned pieces a [`Screen`] borrows.
pub struct Fixture {
    pub tree: ExploreTree,
    pub state: ExploreState,
    pub snapshot: Result<FsSnapshot, String>,
    pub summary: Option<ScanSummary>,
    pub color: bool,
    pub elapsed_tenths: u64,
    pub cache_age: Option<String>,
    pub home: Option<PathBuf>,
}

impl Fixture {
    pub fn new(tree: ExploreTree) -> Fixture {
        let state = state_for(&tree);
        Fixture {
            tree,
            state,
            snapshot: Ok(btrfs_snapshot()),
            summary: None,
            color: false,
            elapsed_tenths: 31,
            cache_age: None,
            home: None,
        }
    }

    pub fn screen(&self) -> Screen<'_> {
        Screen {
            tree: &self.tree,
            state: &self.state,
            theme: Theme::new(self.color),
            tty: true,
            home: self.home.as_deref(),
            headroom: self.snapshot.as_ref().map_err(String::as_str),
            headroom_path: Path::new("/"),
            scan: ScanFacts {
                elapsed_tenths: self.elapsed_tenths,
                stop: disk_cleanup_core::explore::msg::StopCause::None,
                max_seconds: None,
                summary: self.summary.as_ref(),
                cache_age: self.cache_age.as_deref(),
            },
            planning: false,
            banner: None,
            plan: None,
            scroll_top: 0,
            kind_line: None,
            shared: None,
            owner: None,
            now_unix: 1_700_000_000,
            unreadable: &[],
            root_note: None,
            notice: None,
        }
    }
}

/// A `TestBackend` that counts `size()` calls and flushes.
pub struct Spy {
    pub inner: TestBackend,
    pub size_calls: Arc<AtomicUsize>,
    pub flushes: Arc<AtomicUsize>,
}

impl Spy {
    pub fn new(w: u16, h: u16) -> Spy {
        Spy {
            inner: TestBackend::new(w, h),
            size_calls: Arc::new(AtomicUsize::new(0)),
            flushes: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl Backend for Spy {
    type Error = Infallible;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.inner.draw(content)
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> Result<Size, Self::Error> {
        self.size_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.size()
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.flushes.fetch_add(1, Ordering::SeqCst);
        self.inner.flush()
    }
}
