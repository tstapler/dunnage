//! `run_explorer` (Task 6.5.2b): the loop that owns the screen. Each iteration polls signals, applies scan
//! messages for at most the apply budget, redraws when something changed (scan-driven redraws at most four
//! times a second, a key at once) and waits for input for at most one tick. The wait is zero while a backlog
//! of messages remains, so a big cache paint drains at the speed of the reducer and not of the tick.
//!
//! The loop never joins a worker: a scan thread blocked in a syscall must not delay `q`.

use std::ffi::OsString;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use disk_cleanup_core::explore::keymap::Key;
use disk_cleanup_core::explore::ports::TermSignals;
use ratatui::backend::Backend;
use ratatui::layout::Rect;
use ratatui::Terminal;

use crate::app::{App, Ending, ExplorerConfig, Ports};
use crate::terminal::{
    query_size, DiagQueue, FixedSizeBackend, RawMode, TerminalGuard, EXIT_TERMINAL, RESTORE_FAILED,
};

pub const TICK: Duration = Duration::from_millis(50);
pub const APPLY_BUDGET: Duration = Duration::from_millis(25);
/// Scan-driven redraws are at most four a second (UX-T9).
pub const SCAN_REDRAW_GAP_MS: u64 = 250;
/// Events handled per iteration before the loop looks at the scan again.
const EVENTS_PER_ITERATION: usize = 64;

pub const EXIT_SIGINT: i32 = 130;
pub const EXIT_SIGTERM: i32 = 143;

/// Exit code of a signal the port reported: SIGINT is 130; SIGTERM, SIGHUP and SIGQUIT are 143.
pub fn signal_exit_code(signal: i32) -> i32 {
    if signal == 2 {
        EXIT_SIGINT
    } else {
        EXIT_SIGTERM
    }
}

/// Milliseconds on a monotonic clock. A fake advances by hand.
pub trait Clock {
    fn now_ms(&self) -> u64;
}

#[derive(Debug)]
pub struct SystemClock {
    start: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        SystemClock {
            start: Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    Key(Key),
    Resize,
    Other,
}

pub trait EventSource {
    fn poll(&mut self, timeout: Duration) -> io::Result<bool>;
    fn read(&mut self) -> io::Result<Input>;
}

/// Crossterm's event queue, with keys translated to the core's [`Key`].
#[derive(Debug, Default)]
pub struct CrosstermEvents;

fn translate(k: KeyEvent) -> Input {
    if k.kind == KeyEventKind::Release {
        return Input::Other;
    }
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let key = match k.code {
        KeyCode::Char(c) if ctrl => Key::Ctrl(c.to_ascii_lowercase()),
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Down => Key::Down,
        KeyCode::Up => Key::Up,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Enter => Key::Enter,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Delete => Key::Delete,
        KeyCode::Esc => Key::Esc,
        _ => return Input::Other,
    };
    Input::Key(key)
}

impl EventSource for CrosstermEvents {
    fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
        event::poll(timeout)
    }

    fn read(&mut self) -> io::Result<Input> {
        Ok(match event::read()? {
            Event::Key(k) => translate(k),
            Event::Resize(..) => Input::Resize,
            _ => Input::Other,
        })
    }
}

/// The size from the ioctl; `None` when it fails or answers 0.
pub trait SizeSource {
    fn size(&mut self) -> Option<(u16, u16)>;
}

#[derive(Debug, Default)]
pub struct IoctlSize;

impl SizeSource for IoctlSize {
    fn size(&mut self) -> Option<(u16, u16)> {
        query_size()
    }
}

/// What the loop asks of the terminal guard besides drawing.
pub trait Restore {
    fn restore(&mut self) -> io::Result<()>;
    fn reenter(&mut self) -> io::Result<()>;
}

impl<W: io::Write + Send + 'static, R: RawMode + 'static> Restore for TerminalGuard<W, R> {
    fn restore(&mut self) -> io::Result<()> {
        TerminalGuard::restore(self)
    }

    fn reenter(&mut self) -> io::Result<()> {
        TerminalGuard::reenter(self)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Tuning {
    pub tick: Duration,
    pub apply_budget: Duration,
    pub scan_redraw_gap_ms: u64,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning {
            tick: TICK,
            apply_budget: APPLY_BUDGET,
            scan_redraw_gap_ms: SCAN_REDRAW_GAP_MS,
        }
    }
}

/// Counters the tests read.
#[derive(Debug, Clone, Copy, Default)]
pub struct LoopStats {
    pub iterations: u64,
    pub frames: u64,
    pub messages: u64,
    pub node_events: u64,
    pub apply_time: Duration,
}

/// The loop's result: the exit code, and what to print once the terminal is restored. The loop prints
/// nothing itself.
#[derive(Debug, Clone, Default)]
pub struct ExplorerExit {
    pub code: i32,
    pub stdout_lines: Vec<OsString>,
    /// Diagnostics queued during the session, already escaped, and the restore error if any.
    pub stderr_lines: Vec<String>,
    pub stats: LoopStats,
}

/// The terminal side of a run.
pub struct Runtime<'a, B: Backend, E: EventSource, R: Restore, S: SizeSource> {
    pub term: &'a mut Terminal<FixedSizeBackend<B>>,
    pub guard: &'a mut R,
    pub events: &'a mut E,
    pub sizer: &'a mut S,
    pub clock: &'a dyn Clock,
    pub tuning: Tuning,
    pub diagnostics: Arc<DiagQueue>,
}

enum Why {
    Ending(Ending),
    Signal(i32),
    TerminalLost,
}

fn io_hangup(e: &io::Error) -> bool {
    e.raw_os_error() == Some(5) || e.kind() == io::ErrorKind::BrokenPipe
}

/// Runs the explorer until it ends and returns what to do after the restore. The terminal is restored on
/// every path out of this function.
pub fn run_explorer<B, E, R, S>(
    rt: &mut Runtime<'_, B, E, R, S>,
    ports: Ports,
    cfg: ExplorerConfig,
    size: (u16, u16),
) -> ExplorerExit
where
    B: Backend,
    B::Error: 'static,
    E: EventSource,
    R: Restore,
    S: SizeSource,
{
    let signals: Arc<dyn TermSignals> = Arc::clone(&ports.signals);
    let mut app = App::new(cfg, ports, size);
    let mut stats = LoopStats::default();
    let start = rt.clock.now_ms();
    app.start_initial_scan(start);

    let mut ui_dirty = true;
    let mut scan_dirty = false;
    let mut last_draw_ms: u64 = 0;
    let mut resize_pending = false;
    let mut last_resize_ms: u64 = 0;
    let mut banner_shown = false;

    let why = 'run: loop {
        let now = rt.clock.now_ms();
        stats.iterations += 1;

        if let Some(sig) = signals.interrupted() {
            app.detach_scan();
            break Why::Signal(signal_exit_code(sig));
        }
        if signals.suspend_requested() {
            if suspend(rt, &signals, app.size).is_err() {
                break Why::TerminalLost;
            }
            ui_dirty = true;
        }

        let planning_before = (app.busy_planning(), app.plan.is_some());
        let t0 = Instant::now();
        let (n, more) = app.drain(rt.tuning.apply_budget, now);
        stats.apply_time += t0.elapsed();
        stats.messages += n as u64;
        if n > 0 {
            scan_dirty = true;
        }
        if planning_before != (app.busy_planning(), app.plan.is_some()) {
            ui_dirty = true;
        }
        if let Some(end) = app.ending.take() {
            break Why::Ending(end);
        }
        if !banner_shown && !rt.diagnostics.is_empty() {
            app.banner =
                Some("a background thread panicked; the message prints when you quit".to_string());
            banner_shown = true;
            ui_dirty = true;
        }

        if resize_pending && now.saturating_sub(last_resize_ms) >= rt.tuning.tick.as_millis() as u64
        {
            resize_pending = false;
            last_resize_ms = now;
            let (c, r) = rt.sizer.size().unwrap_or((1, 1));
            if (c, r) != app.size {
                app.size = (c, r);
                rt.term.backend_mut().set_size(c, r);
                if rt.term.resize(Rect::new(0, 0, c, r)).is_err() {
                    break Why::TerminalLost;
                }
                ui_dirty = true;
            }
        }

        let since = now.saturating_sub(last_draw_ms);
        let scan_due = (scan_dirty || app.scan_running()) && since >= rt.tuning.scan_redraw_gap_ms;
        if ui_dirty || app.force_clear || scan_due {
            if app.force_clear {
                app.force_clear = false;
                if repaint(rt.term, app.size).is_err() {
                    break Why::TerminalLost;
                }
            }
            app.tick_view(now);
            let drawn = rt.term.draw(|f| app.render(f, now));
            if let Err(e) = drawn {
                break if backend_hangup(&e) {
                    Why::Signal(EXIT_SIGTERM)
                } else {
                    Why::TerminalLost
                };
            }
            stats.frames += 1;
            last_draw_ms = now;
            ui_dirty = false;
            scan_dirty = false;
        }

        let mut timeout = if more { Duration::ZERO } else { rt.tuning.tick };
        if scan_dirty || app.scan_running() {
            let wait = rt
                .tuning
                .scan_redraw_gap_ms
                .saturating_sub(now.saturating_sub(last_draw_ms));
            timeout = timeout.min(Duration::from_millis(wait));
        }
        if resize_pending {
            let wait = (rt.tuning.tick.as_millis() as u64)
                .saturating_sub(now.saturating_sub(last_resize_ms));
            timeout = timeout.min(Duration::from_millis(wait));
        }

        let mut handled = 0;
        let mut wait = timeout;
        loop {
            match rt.events.poll(wait) {
                Ok(false) => break,
                Ok(true) => {}
                Err(e) => {
                    break 'run if io_hangup(&e) {
                        Why::Signal(EXIT_SIGTERM)
                    } else {
                        Why::TerminalLost
                    };
                }
            }
            wait = Duration::ZERO;
            let input = match rt.events.read() {
                Ok(i) => i,
                Err(e) => {
                    break 'run if io_hangup(&e) {
                        Why::Signal(EXIT_SIGTERM)
                    } else {
                        Why::TerminalLost
                    };
                }
            };
            match input {
                Input::Resize => resize_pending = true,
                Input::Other => {}
                Input::Key(Key::Ctrl('z')) => {
                    if suspend(rt, &signals, app.size).is_err() {
                        break 'run Why::TerminalLost;
                    }
                    ui_dirty = true;
                    app.force_clear = true;
                }
                Input::Key(k) => {
                    let t = rt.clock.now_ms();
                    if app.on_key(k, t) {
                        ui_dirty = true;
                    }
                    if app.ending.is_some() {
                        break;
                    }
                }
            }
            handled += 1;
            if handled >= EVENTS_PER_ITERATION {
                break;
            }
        }
        if let Some(end) = app.ending.take() {
            break Why::Ending(end);
        }
    };

    stats.node_events = app.events_applied;
    finish(rt, &mut app, why, stats)
}

/// Ctrl-Z or an external SIGTSTP: restore, stop, re-enter.
fn suspend<B, E, R, S>(
    rt: &mut Runtime<'_, B, E, R, S>,
    signals: &Arc<dyn TermSignals>,
    size: (u16, u16),
) -> io::Result<()>
where
    B: Backend,
    B::Error: 'static,
    E: EventSource,
    R: Restore,
    S: SizeSource,
{
    rt.guard.restore()?;
    signals.suspend();
    rt.guard.reenter()?;
    repaint(rt.term, size).map_err(|e| io::Error::other(e.to_string()))
}

fn finish<B, E, R, S>(
    rt: &mut Runtime<'_, B, E, R, S>,
    app: &mut App,
    why: Why,
    stats: LoopStats,
) -> ExplorerExit
where
    B: Backend,
    B::Error: 'static,
    E: EventSource,
    R: Restore,
    S: SizeSource,
{
    let (code, lines, from_signal) = match why {
        Why::Ending(Ending::Leave { code, lines }) => {
            (code, lines, code == EXIT_SIGINT || code == EXIT_SIGTERM)
        }
        Why::Signal(code) => (code, Vec::new(), true),
        Why::TerminalLost => (EXIT_TERMINAL, Vec::new(), false),
    };
    app.detach_scan();
    let restored = rt.guard.restore();
    let mut stderr_lines = app.take_diagnostics();
    stderr_lines.extend(rt.diagnostics.take());
    let (code, lines) = match restored {
        Ok(()) => (code, lines),
        Err(_) if from_signal => (code, lines),
        Err(_) => {
            stderr_lines.push(RESTORE_FAILED.to_string());
            (EXIT_TERMINAL, lines)
        }
    };
    ExplorerExit {
        code,
        stdout_lines: lines,
        stderr_lines,
        stats,
    }
}

fn backend_hangup<E: std::error::Error + 'static>(e: &E) -> bool {
    (e as &dyn std::error::Error)
        .downcast_ref::<io::Error>()
        .is_some_and(io_hangup)
}

/// Forgets what is on the screen so the next frame is a full repaint. `Terminal::clear()` would ask the
/// terminal for the cursor position (`ESC[6n`) and wait for the answer, which a stopped-and-continued
/// session may never get; resizing the fixed viewport to its own size clears without asking.
fn repaint<B: Backend>(
    term: &mut Terminal<FixedSizeBackend<B>>,
    size: (u16, u16),
) -> Result<(), B::Error> {
    term.resize(Rect::new(0, 0, size.0, size.1))
}
