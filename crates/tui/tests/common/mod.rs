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
            plan_note: None,
            delete: None,
            audit_log: None,
            scroll_top: 0,
            kind_line: None,
            shared: None,
            owner: None,
            now_unix: 1_700_000_000,
            unreadable: &[],
            root_note: None,
            scan_root: Path::new("/home/u/code/app"),
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

// ----- the loop harness -----

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::mpsc::{channel, Sender};
use std::sync::Mutex;
use std::time::Duration;

use disk_cleanup_core::explore::confirm::ConfirmedPlan;
use disk_cleanup_core::explore::keymap::Key;
use disk_cleanup_core::explore::msg::{PlanPreview, ScanMsg};
use disk_cleanup_core::explore::ports::{
    DeleteHandle, DirLister, ExplorerActions, Handoff, PreviewHandle, ScanHandle, ScanOpts,
    ScanSource, TermSignals,
};
use disk_cleanup_core::explore::testing::FakeDirLister;
use disk_cleanup_core::ids::Identity;
use disk_cleanup_core::roots::RootSet;
use disk_cleanup_tui::app::{ExplorerConfig, Ports};
use disk_cleanup_tui::event_loop::{
    run_explorer, Clock, EventSource, ExplorerExit, Input, Restore, Runtime, SizeSource,
    SystemClock, Tuning,
};
use disk_cleanup_tui::terminal::{fixed_terminal, DiagQueue};

pub fn root_set() -> RootSet {
    let dirs = disk_cleanup_core::dirs::Dirs {
        home: "/h".into(),
        state: "/h/.local/state".into(),
        cache: "/h/.cache".into(),
        config: "/h/.config".into(),
    };
    let canon = |p: &Path| -> io::Result<(PathBuf, Identity)> {
        let ino = p
            .to_string_lossy()
            .bytes()
            .fold(7u64, |a, b| a.wrapping_mul(31).wrapping_add(u64::from(b)));
        Ok((p.to_path_buf(), Identity { dev: 1, ino }))
    };
    RootSet::resolve(
        &dirs,
        &[],
        Some(&[PathBuf::from("/h/code")]),
        Path::new("/h"),
        &canon,
    )
    .unwrap()
}

#[derive(Debug, Default)]
pub struct FakeClock(AtomicU64);

impl FakeClock {
    pub fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Clone)]
pub enum Time {
    Virtual(Arc<FakeClock>),
    Real(Arc<SystemClock>),
}

impl Time {
    pub fn now(&self) -> u64 {
        match self {
            Time::Virtual(c) => c.now_ms(),
            Time::Real(c) => c.now_ms(),
        }
    }

    fn wait(&self, ms: u64) {
        match self {
            Time::Virtual(c) => c.advance(ms),
            Time::Real(_) => std::thread::sleep(Duration::from_millis(ms)),
        }
    }

    pub fn clock(&self) -> Arc<dyn Clock + Send + Sync> {
        match self {
            Time::Virtual(c) => c.clone(),
            Time::Real(c) => c.clone(),
        }
    }
}

/// Scripted input at clock times; records every poll timeout and the frames drawn at that moment.
pub struct FakeEvents {
    pub time: Time,
    queue: VecDeque<(u64, Input)>,
    pub polls: Arc<Mutex<Vec<(Duration, u64, usize)>>>,
    flushes: Arc<std::sync::atomic::AtomicUsize>,
    safety: u64,
}

impl FakeEvents {
    pub fn new(
        time: Time,
        script: Vec<(u64, Input)>,
        flushes: Arc<std::sync::atomic::AtomicUsize>,
    ) -> FakeEvents {
        FakeEvents {
            time,
            queue: script.into(),
            polls: Arc::new(Mutex::new(Vec::new())),
            flushes,
            safety: 0,
        }
    }
}

impl EventSource for FakeEvents {
    fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
        self.safety += 1;
        assert!(self.safety < 20_000_000, "the loop spins");
        let now = self.time.now();
        self.polls
            .lock()
            .unwrap()
            .push((timeout, now, self.flushes.load(Ordering::SeqCst)));
        let ready =
            |q: &VecDeque<(u64, Input)>, now: u64| q.front().is_some_and(|(at, _)| *at <= now);
        if ready(&self.queue, now) {
            return Ok(true);
        }
        let to_next = self.queue.front().map(|(at, _)| at - now);
        let ms = u64::try_from(timeout.as_millis()).unwrap();
        let wait = to_next.map_or(ms, |n| n.min(ms));
        if wait == 0 {
            return Ok(false);
        }
        self.time.wait(wait);
        Ok(ready(&self.queue, self.time.now()))
    }

    fn read(&mut self) -> io::Result<Input> {
        Ok(self.queue.pop_front().unwrap().1)
    }
}

pub fn key(k: Key) -> Input {
    Input::Key(k)
}

pub fn ch(c: char) -> Input {
    Input::Key(Key::Char(c))
}

pub struct FakeSize(
    pub Arc<Mutex<Option<(u16, u16)>>>,
    pub Arc<std::sync::atomic::AtomicUsize>,
);

impl SizeSource for FakeSize {
    fn size(&mut self) -> Option<(u16, u16)> {
        self.1.fetch_add(1, Ordering::SeqCst);
        *self.0.lock().unwrap()
    }
}

#[derive(Clone, Default)]
pub struct Log(pub Arc<Mutex<Vec<&'static str>>>);

impl Log {
    pub fn push(&self, s: &'static str) {
        self.0.lock().unwrap().push(s);
    }

    pub fn get(&self) -> Vec<&'static str> {
        self.0.lock().unwrap().clone()
    }
}

pub struct FakeRestore {
    pub log: Log,
    pub fail: bool,
}

impl Restore for FakeRestore {
    fn restore(&mut self) -> io::Result<()> {
        self.log.push("restore");
        if self.fail {
            Err(io::Error::from_raw_os_error(5))
        } else {
            Ok(())
        }
    }

    fn reenter(&mut self) -> io::Result<()> {
        self.log.push("reenter");
        Ok(())
    }
}

/// Reports `signal` from the `at_poll`-th call of `interrupted()`; logs a suspend.
pub struct LogSignals {
    pub log: Log,
    pub signal: Mutex<Option<(usize, i32)>>,
    pub polls: std::sync::atomic::AtomicUsize,
    pub suspend: AtomicBool,
}

impl LogSignals {
    pub fn never(log: &Log) -> Arc<LogSignals> {
        Arc::new(LogSignals {
            log: log.clone(),
            signal: Mutex::new(None),
            polls: std::sync::atomic::AtomicUsize::new(0),
            suspend: AtomicBool::new(false),
        })
    }

    pub fn at(log: &Log, tick: usize, sig: i32) -> Arc<LogSignals> {
        let s = LogSignals::never(log);
        *s.signal.lock().unwrap() = Some((tick, sig));
        s
    }
}

impl TermSignals for LogSignals {
    fn interrupted(&self) -> Option<i32> {
        let n = self.polls.fetch_add(1, Ordering::SeqCst) + 1;
        self.signal
            .lock()
            .unwrap()
            .filter(|(t, _)| n >= *t)
            .map(|(_, s)| s)
    }

    fn suspend_requested(&self) -> bool {
        self.suspend.swap(false, Ordering::SeqCst)
    }

    fn suspend(&self) {
        self.log.push("suspend");
    }
}

/// One `start()` of a [`ControlSource`].
pub struct StartRec {
    pub roots: Vec<PathBuf>,
    pub opts: ScanOpts,
    pub tx: Sender<ScanMsg>,
    pub cancel: Arc<AtomicBool>,
    pub save: Arc<AtomicBool>,
}

/// A scan source the test drives by hand.
#[derive(Default)]
pub struct ControlSource {
    pub starts: Mutex<Vec<StartRec>>,
    pub prefill: Mutex<Vec<ScanMsg>>,
}

impl ControlSource {
    pub fn send(&self, nth: usize, m: ScanMsg) -> bool {
        self.starts.lock().unwrap()[nth].tx.send(m).is_ok()
    }

    pub fn count(&self) -> usize {
        self.starts.lock().unwrap().len()
    }
}

impl ScanSource for ControlSource {
    fn start(&self, roots: &[PathBuf], opts: &ScanOpts) -> ScanHandle {
        let (tx, rx) = channel();
        for m in std::mem::take(&mut *self.prefill.lock().unwrap()) {
            tx.send(m).unwrap();
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let save = Arc::new(AtomicBool::new(true));
        self.starts.lock().unwrap().push(StartRec {
            roots: roots.to_vec(),
            opts: opts.clone(),
            tx,
            cancel: Arc::clone(&cancel),
            save: Arc::clone(&save),
        });
        ScanHandle::new(rx, cancel, save)
    }
}

/// Blocks `delay` inside `preview` and `finish`, on the worker thread the app starts.
pub struct SlowActions {
    pub delay: Duration,
    pub plan: PlanPreview,
    pub calls: Log,
}

impl ExplorerActions for SlowActions {
    fn preview(&self, _marks: &[PathBuf], cancel: Arc<AtomicBool>) -> PreviewHandle {
        self.calls.push("preview");
        std::thread::sleep(self.delay);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let _ = tx.send(ScanMsg::Preview(self.plan.clone()));
        PreviewHandle::new(rx, cancel)
    }

    fn delete(&self, _confirmed: ConfirmedPlan) -> DeleteHandle {
        self.calls.push("delete");
        let (_tx, rx) = std::sync::mpsc::sync_channel(1);
        DeleteHandle::new(rx, Arc::new(AtomicBool::new(false)))
    }

    fn finish(&self, _marks: &[PathBuf]) -> Handoff {
        self.calls.push("finish");
        std::thread::sleep(self.delay);
        Handoff {
            lines: vec!["DRY RUN - nothing was deleted.".into()],
            exit_code: 0,
        }
    }
}

pub fn plan_fixture() -> PlanPreview {
    PlanPreview {
        plan_id: "5d2e77a1".into(),
        saved: true,
        rows: vec![],
        est_bytes: 0,
        scope_line: "1 marked item".into(),
        guards_line: "Guards run now.".into(),
    }
}

pub struct Run {
    pub source: Arc<ControlSource>,
    pub signals: Arc<LogSignals>,
    pub actions: Arc<dyn ExplorerActions>,
    pub lister: Arc<dyn DirLister>,
    pub time: Time,
    pub script: Vec<(u64, Input)>,
    pub size: (u16, u16),
    pub fail_restore: bool,
    pub tuning: Tuning,
    pub log: Log,
    pub tweak: Box<dyn FnOnce(&mut ExplorerConfig)>,
    pub resize_to: Option<(u16, u16)>,
    /// Whether stdin is a terminal for the typed count.
    pub terminal: bool,
}

impl Run {
    pub fn virtual_time(script: Vec<(u64, Input)>) -> Run {
        let log = Log::default();
        Run {
            source: Arc::new(ControlSource::default()),
            signals: LogSignals::never(&log),
            actions: Arc::new(SlowActions {
                delay: Duration::ZERO,
                plan: plan_fixture(),
                calls: Log::default(),
            }),
            lister: Arc::new(FakeDirLister::default()),
            time: Time::Virtual(Arc::new(FakeClock::default())),
            script,
            size: (100, 24),
            fail_restore: false,
            tuning: Tuning::default(),
            log,
            tweak: Box::new(|_| {}),
            resize_to: None,
            terminal: true,
        }
    }

    pub fn real_time(script: Vec<(u64, Input)>) -> Run {
        let mut r = Run::virtual_time(script);
        r.time = Time::Real(Arc::new(SystemClock::default()));
        r
    }
}

pub struct Outcome {
    pub exit: ExplorerExit,
    pub screen: String,
    pub polls: Vec<(Duration, u64, usize)>,
    pub log: Vec<&'static str>,
    pub frames: usize,
    pub elapsed_ms: u64,
    pub size_queries: usize,
    pub diagnostics: Arc<DiagQueue>,
}

pub fn run(r: Run) -> Outcome {
    let spy = Spy::new(r.size.0, r.size.1);
    let flushes = Arc::clone(&spy.flushes);
    let mut term = fixed_terminal(spy, r.size.0, r.size.1).unwrap();
    let mut events = FakeEvents::new(r.time.clone(), r.script, Arc::clone(&flushes));
    let polls = Arc::clone(&events.polls);
    let queries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut sizer = FakeSize(Arc::new(Mutex::new(r.resize_to)), Arc::clone(&queries));
    let mut guard = FakeRestore {
        log: r.log.clone(),
        fail: r.fail_restore,
    };
    let diagnostics = Arc::new(DiagQueue::default());
    let clock = r.time.clock();
    let mut cfg = ExplorerConfig::new(vec![PathBuf::from("/h/code/app")], root_set());
    cfg.now_unix = 1_700_000_000;
    cfg.home = Some(PathBuf::from("/h"));
    (r.tweak)(&mut cfg);
    let ports = Ports {
        scan: r.source.clone(),
        lister: r.lister,
        signals: r.signals,
        actions: r.actions,
        typed: Arc::new(disk_cleanup_core::explore::testing::FakeTypedCount::new(
            r.terminal,
        )),
    };
    let started = r.time.now();
    let exit = {
        let mut rt = Runtime {
            term: &mut term,
            guard: &mut guard,
            events: &mut events,
            sizer: &mut sizer,
            clock: &*clock,
            tuning: r.tuning,
            diagnostics: Arc::clone(&diagnostics),
        };
        run_explorer(&mut rt, ports, cfg, r.size)
    };
    let elapsed_ms = r.time.now() - started;
    let screen = buffer_text(term.backend().inner().inner.buffer());
    let polls = polls.lock().unwrap().clone();
    Outcome {
        exit,
        screen,
        polls,
        log: r.log.get(),
        frames: flushes.load(Ordering::SeqCst),
        elapsed_ms,
        size_queries: queries.load(Ordering::SeqCst),
        diagnostics,
    }
}

/// A root with `kids` children and the batch that carries them; `open` keeps the root unfinished.
pub fn root_batch(kids: &[(&str, u64)], open: bool) -> ScanMsg {
    let mut ev = vec![dir_ev(
        0,
        NO_PARENT,
        "/h/code/app",
        stats(1, kids.len() as u64, 4096, 400),
        if open { 1_000_000 } else { kids.len() as u32 },
        1_700_000_000,
    )];
    for (i, (n, a)) in kids.iter().enumerate() {
        ev.push(dir_ev(
            1 + i as u64,
            0,
            n,
            stats(2, 0, *a, *a),
            0,
            1_700_000_000,
        ));
    }
    ScanMsg::Batch {
        generation: 0,
        mode: ScanMode::Fresh,
        events: ev,
    }
}

pub fn finished(
    generation: u32,
    truncated: bool,
    stop: disk_cleanup_core::explore::msg::StopCause,
) -> ScanMsg {
    ScanMsg::Finished(ScanSummary {
        generation,
        truncated,
        stop,
        ..ScanSummary::default()
    })
}

/// The buffer as plain text, one line per row, trailing blanks trimmed.
pub fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
    let a = buf.area;
    (0..a.height)
        .map(|y| {
            let mut line = String::new();
            let mut x = 0;
            while x < a.width {
                let sym = buf[(x, y)].symbol();
                line.push_str(sym);
                // the cells after a wide character are blank placeholders
                x += u16::try_from(unicode_width::UnicodeWidthStr::width(sym).max(1)).unwrap();
            }
            line.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

use disk_cleanup_tui::view::render;
use ratatui::Terminal;

/// The screen drawn into a `TestBackend` of `w` x `h`; returns the terminal for style checks.
pub fn draw(s: &Screen<'_>, w: u16, h: u16) -> Terminal<TestBackend> {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| render(f, s)).unwrap();
    term
}

pub fn draw_text(s: &Screen<'_>, w: u16, h: u16) -> String {
    buffer_text(draw(s, w, h).backend().buffer())
}

use disk_cleanup_core::explore::layout::cells;
use disk_cleanup_core::explore::marks::Mark;
use disk_cleanup_core::explore::rows::SortKey;

impl Fixture {
    /// Re-sorts with a badge function of (tree, node) so rows carry cleaner badges.
    pub fn with_badge(mut self, badge: impl Fn(&ExploreTree, NodeId) -> Badge) -> Fixture {
        {
            let Fixture { tree, state, .. } = &mut self;
            let tree: &ExploreTree = tree;
            let b = |id: NodeId| badge(tree, id);
            state.view.press_sort(SortKey::Usage, tree, 20_000, &b);
            state.view.press_sort(SortKey::Usage, tree, 20_001, &b);
        }
        self
    }

    pub fn mark(&mut self, path: &str, bytes: u64) {
        self.state
            .marks
            .add(Mark {
                path: PathBuf::from(path),
                est_bytes: bytes,
                cleaner_item: true,
            })
            .unwrap();
    }
}

pub fn assert_fits(text: &str, w: usize) {
    for (i, l) in text.lines().enumerate() {
        assert!(
            cells(l) <= w,
            "line {i} is {} cells, terminal is {w}: {l:?}",
            cells(l)
        );
    }
}

// ----- a scripted delete (Story 6.6.4) -----

use disk_cleanup_core::explore::confirm::{ConfirmRecord, ReviewedPlan};
use disk_cleanup_core::explore::msg::{
    DeleteItemOutcome, DeleteItemResult, DeleteMsg, DeleteSummary,
};
use std::sync::mpsc::SyncSender;

/// What a scripted delete does on its worker: the item paths, the sender and the abort flag.
pub type OnDelete = Box<dyn Fn(Vec<PathBuf>, SyncSender<DeleteMsg>, Arc<AtomicBool>) + Send + Sync>;
type ReviewFn = Box<dyn Fn(usize, &[PathBuf]) -> Result<ReviewedPlan, String> + Send + Sync>;

/// Actions over fakes with every call recorded: `preview` answers at once, `review` is scripted per call
/// and `delete` runs `on_delete` on a thread of its own.
pub struct ControlActions {
    pub plan: PlanPreview,
    pub calls: Log,
    pub review_fn: ReviewFn,
    pub on_delete: OnDelete,
    pub reviews: Mutex<Vec<Vec<PathBuf>>>,
    pub confirmed: Mutex<Vec<ConfirmRecord>>,
    pub aborts: Mutex<Vec<Arc<AtomicBool>>>,
}

pub fn removed_item(path: &Path, bytes: u64) -> DeleteMsg {
    DeleteMsg::Item(DeleteItemResult {
        path: path.to_path_buf(),
        outcome: DeleteItemOutcome::Removed { bytes },
    })
}

/// Every item reported removed with 1 GiB, then `Done`.
pub fn remove_all() -> OnDelete {
    Box::new(|paths, tx, _abort| {
        std::thread::spawn(move || {
            let n = paths.len();
            for (i, p) in paths.iter().enumerate() {
                let _ = tx.send(DeleteMsg::Progress {
                    index: i + 1,
                    total: n,
                });
                let _ = tx.send(removed_item(p, GIB));
            }
            let _ = tx.send(DeleteMsg::Done(DeleteSummary {
                removed: n,
                bytes: GIB * n as u64,
                interrupted: false,
            }));
        });
    })
}

impl ControlActions {
    pub fn new() -> ControlActions {
        ControlActions {
            plan: plan_fixture(),
            calls: Log::default(),
            review_fn: Box::new(|_, marks| {
                Ok(disk_cleanup_core::explore::testing::reviewed_of(marks, GIB))
            }),
            on_delete: remove_all(),
            reviews: Mutex::new(Vec::new()),
            confirmed: Mutex::new(Vec::new()),
            aborts: Mutex::new(Vec::new()),
        }
    }

    pub fn count(&self, call: &str) -> usize {
        self.calls.get().iter().filter(|c| **c == call).count()
    }
}

impl ExplorerActions for ControlActions {
    fn preview(&self, _marks: &[PathBuf], cancel: Arc<AtomicBool>) -> PreviewHandle {
        self.calls.push("preview");
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let _ = tx.send(ScanMsg::Preview(self.plan.clone()));
        PreviewHandle::new(rx, cancel)
    }

    fn finish(&self, _marks: &[PathBuf]) -> Handoff {
        self.calls.push("finish");
        Handoff {
            lines: vec!["DRY RUN - nothing was deleted.".into(), CLOSED.into()],
            exit_code: 0,
        }
    }

    fn delete(
        &self,
        confirmed: disk_cleanup_core::explore::confirm::ConfirmedPlan,
    ) -> DeleteHandle {
        self.calls.push("delete");
        self.confirmed.lock().unwrap().push(confirmed.record());
        let paths: Vec<PathBuf> = confirmed
            .reviewed()
            .items()
            .iter()
            .map(|i| i.path.clone())
            .collect();
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let abort = Arc::new(AtomicBool::new(false));
        self.aborts.lock().unwrap().push(Arc::clone(&abort));
        (self.on_delete)(paths, tx, Arc::clone(&abort));
        DeleteHandle::new(rx, abort)
    }

    fn review(&self, marks: &[PathBuf]) -> Result<ReviewedPlan, String> {
        self.calls.push("review");
        let n = {
            let mut r = self.reviews.lock().unwrap();
            r.push(marks.to_vec());
            r.len()
        };
        (self.review_fn)(n, marks)
    }
}

pub const CLOSED: &str = "EXPLORE CLOSED - nothing was deleted.";
