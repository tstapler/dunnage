//! The terminal guard (Story 6.5.2a; UX-T23). It undoes exactly what it did: raw mode, the alternate
//! screen and a hidden cursor, no mouse and no focus events. Restore runs once, in this order: show the
//! cursor, leave the alternate screen, reset attributes (`ESC[0m`), disable raw mode.
//!
//! The guard never calls `ratatui::init()`: that installs a hook that prints with `eprintln!` and a
//! `restore()` that leaves the cursor and mouse state alone. The guard owns its own writer, a second handle
//! to the same stream the ratatui backend draws on, so the panic hook can restore from any thread.

use std::io::{self, Write};
use std::panic;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use crossterm::cursor::{Hide, Show};
use crossterm::style::{Attribute, SetAttribute};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, window_size, EnterAlternateScreen, LeaveAlternateScreen,
};
use disk_cleanup_core::escape::escape;
use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Rect, Size};
use ratatui::{Terminal, TerminalOptions, Viewport};

/// A terminal that cannot be set up or restored (`design/ux.md` exit codes).
pub const EXIT_TERMINAL: i32 = 74;
pub const RESTORE_FAILED: &str = "error: could not restore the terminal; run 'reset'";

/// The raw-mode switch, so a test can run the guard over a `Vec<u8>` without a terminal.
pub trait RawMode: Send {
    fn enable(&mut self) -> io::Result<()>;
    fn disable(&mut self) -> io::Result<()>;
}

/// The real switch.
#[derive(Debug, Default, Clone, Copy)]
pub struct CrosstermRaw;

impl RawMode for CrosstermRaw {
    fn enable(&mut self) -> io::Result<()> {
        enable_raw_mode()
    }

    fn disable(&mut self) -> io::Result<()> {
        disable_raw_mode()
    }
}

struct Inner<W, R> {
    out: W,
    raw: R,
    active: bool,
}

impl<W: Write, R: RawMode> Inner<W, R> {
    fn enter(&mut self) -> io::Result<()> {
        self.raw.enable()?;
        let screen =
            crossterm::queue!(self.out, EnterAlternateScreen, Hide).and_then(|()| self.out.flush());
        if let Err(e) = screen {
            let _ = self.raw.disable();
            return Err(e);
        }
        self.active = true;
        Ok(())
    }

    /// Idempotent. A failed write does not skip the raw-mode switch: the first error is returned.
    fn restore(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        let written = crossterm::queue!(
            self.out,
            Show,
            LeaveAlternateScreen,
            SetAttribute(Attribute::Reset)
        )
        .and_then(|()| self.out.flush());
        let raw = self.raw.disable();
        written.and(raw)
    }
}

/// A closure that restores the terminal, for the panic hook.
pub type Restorer = Arc<dyn Fn() -> io::Result<()> + Send + Sync>;

pub struct TerminalGuard<W: Write + Send + 'static, R: RawMode + 'static> {
    inner: Arc<Mutex<Inner<W, R>>>,
}

impl<W: Write + Send + 'static, R: RawMode + 'static> TerminalGuard<W, R> {
    /// Raw mode, alternate screen, hidden cursor, and nothing else.
    pub fn enter(out: W, raw: R) -> io::Result<TerminalGuard<W, R>> {
        let mut inner = Inner {
            out,
            raw,
            active: false,
        };
        inner.enter()?;
        Ok(TerminalGuard {
            inner: Arc::new(Mutex::new(inner)),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner<W, R>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Writes the restore sequence once; a second call writes nothing.
    pub fn restore(&self) -> io::Result<()> {
        self.lock().restore()
    }

    /// After a suspend: the same state as `enter`.
    pub fn reenter(&self) -> io::Result<()> {
        let mut g = self.lock();
        if g.active {
            return Ok(());
        }
        g.enter()
    }

    pub fn is_active(&self) -> bool {
        self.lock().active
    }

    pub fn with_writer<T>(&self, f: impl FnOnce(&W) -> T) -> T {
        f(&self.lock().out)
    }

    pub fn restorer(&self) -> Restorer
    where
        W: Sync,
    {
        let inner = Arc::clone(&self.inner);
        Arc::new(move || {
            inner
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .restore()
        })
    }
}

impl<W: Write + Send + 'static, R: RawMode + 'static> Drop for TerminalGuard<W, R> {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

/// Messages from threads other than the UI thread, kept until the terminal is restored.
#[derive(Debug, Default)]
pub struct DiagQueue {
    lines: Mutex<Vec<String>>,
}

impl DiagQueue {
    pub fn push(&self, line: String) {
        self.lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(line);
    }

    pub fn len(&self) -> usize {
        self.lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.lines.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// Keeps the panic hook active; dropping it hands every later panic to the hook that was there before.
#[derive(Debug)]
pub struct PanicHookGuard {
    active: Arc<AtomicBool>,
}

impl Drop for PanicHookGuard {
    fn drop(&mut self) {
        self.active.store(false, Ordering::SeqCst);
    }
}

/// A panic on the calling (UI) thread restores the terminal first and then prints its message on the normal
/// screen. A panic on any other thread prints nothing: its escaped message goes to `queue`, to be reprinted
/// to stderr after the restore, and the run continues.
pub fn install_panic_hook(restorer: Restorer, queue: Arc<DiagQueue>) -> PanicHookGuard {
    let ui = thread::current().id();
    let active = Arc::new(AtomicBool::new(true));
    let flag = Arc::clone(&active);
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        if !flag.load(Ordering::SeqCst) {
            previous(info);
            return;
        }
        let text = info.to_string();
        if thread::current().id() == ui {
            let _ = restorer();
            let _ = writeln!(io::stderr(), "{text}");
        } else {
            queue.push(format!("warning: {}", escape(text.as_bytes(), true)));
        }
    }));
    PanicHookGuard { active }
}

/// Writes lines to stderr, for use after the terminal is restored.
pub fn write_stderr_lines<S: AsRef<str>>(lines: &[S]) {
    let mut err = io::stderr().lock();
    for l in lines {
        let _ = writeln!(err, "{}", l.as_ref());
    }
}

/// The size from the `TIOCGWINSZ` ioctl; `None` when it fails or answers 0 rows or 0 columns.
pub fn query_size() -> Option<(u16, u16)> {
    let s = window_size().ok()?;
    (s.columns > 0 && s.rows > 0).then_some((s.columns, s.rows))
}

/// A backend whose `size()` is the number the loop gave it, so ratatui's `autoresize` never reaches
/// `crossterm::terminal::size()` (which can spawn `tput` when the ioctl fails). Everything else is the
/// inner backend's.
pub struct FixedSizeBackend<B: Backend> {
    inner: B,
    size: Size,
    size_reads: Arc<AtomicUsize>,
}

impl<B: Backend> FixedSizeBackend<B> {
    pub fn new(inner: B, columns: u16, rows: u16) -> FixedSizeBackend<B> {
        FixedSizeBackend {
            inner,
            size: Size::new(columns, rows),
            size_reads: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn set_size(&mut self, columns: u16, rows: u16) {
        self.size = Size::new(columns, rows);
    }

    /// Calls answered from the cache.
    pub fn size_reads(&self) -> usize {
        self.size_reads.load(Ordering::SeqCst)
    }

    pub fn inner(&self) -> &B {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut B {
        &mut self.inner
    }
}

impl<B: Backend> Backend for FixedSizeBackend<B> {
    type Error = B::Error;

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
        self.size_reads.fetch_add(1, Ordering::SeqCst);
        Ok(self.size)
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush()
    }
}

/// A terminal over `backend` with a fixed viewport of `columns` x `rows`.
pub fn fixed_terminal<B: Backend>(
    backend: B,
    columns: u16,
    rows: u16,
) -> Result<Terminal<FixedSizeBackend<B>>, B::Error> {
    Terminal::with_options(
        FixedSizeBackend::new(backend, columns, rows),
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, columns, rows)),
        },
    )
}
