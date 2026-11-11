//! Signal handling for one run (Story 2.2.2): SIGINT and SIGTERM become an [`AbortFlag`] and a recorded signal
//! number, SIGPIPE is ignored and a closed stdout is swallowed. The handler itself only writes one byte to a
//! pipe (async-signal-safe); a watcher thread does everything else, so the executor is never interrupted
//! mid-entry and decides itself where to stop.

use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use disk_cleanup_core::AbortFlag;

pub const SIGINT: i32 = libc::SIGINT;
pub const SIGTERM: i32 = libc::SIGTERM;

pub const SCAN_INTERRUPTED: &str =
    "INTERRUPTED during scan. Nothing was deleted; no report produced.";

/// What a signal means for the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Apply: the first signal asks the executor to stop; a second SIGINT exits unless a command child runs.
    Apply,
    /// Plan: nothing is mutated that a kill can break, so the first signal exits at once.
    Plan,
}

/// The flag and the signal number the watcher records; shared with `RealRunGuard`.
#[derive(Debug, Default)]
pub struct SignalState {
    pub abort: AbortFlag,
    signal: AtomicI32,
}

impl SignalState {
    pub fn new() -> SignalState {
        SignalState::default()
    }

    /// The first signal received (2 or 15), if any.
    pub fn interrupted(&self) -> Option<i32> {
        match self.signal.load(Ordering::SeqCst) {
            0 => None,
            s => Some(s),
        }
    }

    fn record(&self, sig: i32) {
        let _ = self
            .signal
            .compare_exchange(0, sig, Ordering::SeqCst, Ordering::SeqCst);
        self.abort.set();
    }
}

/// What the watcher does with one arrived signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reaction {
    /// Set the abort flag and remember the signal.
    Abort,
    /// Already stopping; nothing to do.
    Ignore,
    /// Print the line on stderr, then keep running.
    Acknowledge(String),
    /// Print the line (if any) and exit with the code.
    Exit { code: i32, line: Option<String> },
}

/// The signal rules as a pure state machine; the watcher feeds it and acts on the answer.
#[derive(Debug)]
pub struct Machine {
    mode: Mode,
    stopping: bool,
}

impl Machine {
    pub fn new(mode: Mode) -> Machine {
        Machine {
            mode,
            stopping: false,
        }
    }

    /// `child` is the description of the command child in flight, if any.
    pub fn on_signal(&mut self, sig: i32, child: Option<&str>) -> Reaction {
        if self.mode == Mode::Plan {
            return Reaction::Exit {
                code: 128 + sig,
                line: Some(SCAN_INTERRUPTED.to_string()),
            };
        }
        if !self.stopping {
            self.stopping = true;
            return Reaction::Abort;
        }
        if sig != SIGINT {
            return Reaction::Ignore;
        }
        match child {
            Some(desc) => Reaction::Acknowledge(format!(
                "Ctrl-C again is ignored while '{desc}' runs; the run stops when it ends."
            )),
            None => Reaction::Exit {
                code: 128 + SIGINT,
                line: None,
            },
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

static CHILD: Mutex<Option<String>> = Mutex::new(None);

/// Marks a command child (one the tool must wait for rather than orphan) as in flight until dropped.
#[derive(Debug)]
#[must_use]
pub struct ChildInFlight(());

impl ChildInFlight {
    /// `desc` is the shell-quoted command line shown in the acknowledgement.
    pub fn begin(desc: String) -> ChildInFlight {
        *lock(&CHILD) = Some(desc);
        ChildInFlight(())
    }
}

impl Drop for ChildInFlight {
    fn drop(&mut self) {
        *lock(&CHILD) = None;
    }
}

static PIPE_W: AtomicI32 = AtomicI32::new(-1);

#[cfg(target_os = "linux")]
unsafe fn errno_slot() -> *mut libc::c_int {
    unsafe { libc::__errno_location() }
}

#[cfg(not(target_os = "linux"))]
unsafe fn errno_slot() -> *mut libc::c_int {
    unsafe { libc::__error() }
}

extern "C" fn on_signal(sig: libc::c_int) {
    let fd = PIPE_W.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = sig as u8;
        let saved = unsafe { *errno_slot() };
        unsafe { libc::write(fd, std::ptr::addr_of!(byte).cast(), 1) };
        unsafe { *errno_slot() = saved };
    }
}

/// `signal(2)` on Linux and macOS restarts interrupted system calls, which is what the run wants.
pub(crate) fn set_handler(sig: i32, handler: libc::sighandler_t) -> io::Result<()> {
    if unsafe { libc::signal(sig, handler) } == libc::SIG_ERR {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A write to a closed pipe then returns `EPIPE` instead of killing the process.
pub fn ignore_sigpipe() -> io::Result<()> {
    set_handler(libc::SIGPIPE, libc::SIG_IGN)
}

/// Puts SIGPIPE back to its default action; tests use it to prove [`ignore_sigpipe`] is what protects a run.
pub fn default_sigpipe() -> io::Result<()> {
    set_handler(libc::SIGPIPE, libc::SIG_DFL)
}

static INSTALLED: OnceLock<()> = OnceLock::new();

/// Installs the SIGINT and SIGTERM handlers and the watcher thread, once per process (a second call is an
/// error: the handlers feed one state). `err` receives the acknowledgement and interrupt lines.
pub fn install(mode: Mode, state: Arc<SignalState>, err: Box<dyn Write + Send>) -> io::Result<()> {
    if TERM_INSTALLED.load(Ordering::SeqCst) {
        return Err(io::Error::other(
            "the explorer's terminal signal handlers are installed in this process",
        ));
    }
    if INSTALLED.set(()).is_err() {
        return Err(io::Error::other("signal handlers are already installed"));
    }
    ignore_sigpipe()?;
    let mut fds: [RawFd; 2] = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    for fd in fds {
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let rd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    PIPE_W.store(fds[1], Ordering::SeqCst);
    set_handler(
        SIGINT,
        on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
    )?;
    set_handler(
        SIGTERM,
        on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
    )?;
    std::thread::Builder::new()
        .name("dc-signals".into())
        .spawn(move || watch(mode, &state, &rd, err))?;
    Ok(())
}

fn watch(mode: Mode, state: &SignalState, rd: &OwnedFd, mut err: Box<dyn Write + Send>) {
    let mut machine = Machine::new(mode);
    let mut byte = 0u8;
    loop {
        let n = unsafe { libc::read(rd.as_raw_fd(), std::ptr::addr_of_mut!(byte).cast(), 1) };
        if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if n <= 0 {
            return;
        }
        let child = lock(&CHILD).clone();
        match machine.on_signal(i32::from(byte), child.as_deref()) {
            Reaction::Abort => state.record(i32::from(byte)),
            Reaction::Ignore => {}
            Reaction::Acknowledge(line) => {
                let _ = writeln!(err, "{line}");
            }
            Reaction::Exit { code, line } => {
                if let Some(line) = line {
                    let _ = writeln!(err, "{line}");
                }
                std::process::exit(code);
            }
        }
    }
}

// ---------------------------------------------------------------- the explorer's signals (Story 6.3.3)
//
// The explorer reads signals as data from its own loop: the handlers below only store into atomics
// (async-signal-safe), nothing is written and nothing exits. They are installed by `RealTermSignals` and
// never by `install`, so an apply keeps SIGHUP at its default action.

static TERM_INSTALLED: AtomicBool = AtomicBool::new(false);
static TERM_SIGNAL: AtomicI32 = AtomicI32::new(0);
static TSTP_PENDING: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term_signal(sig: libc::c_int) {
    if sig == libc::SIGTSTP {
        TSTP_PENDING.store(true, Ordering::SeqCst);
    } else {
        let _ = TERM_SIGNAL.compare_exchange(0, sig, Ordering::SeqCst, Ordering::SeqCst);
    }
}

fn set_term_handler(sig: i32) -> io::Result<()> {
    set_handler(
        sig,
        on_term_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
    )
}

/// Registers SIGINT, SIGTERM, SIGHUP, SIGQUIT and SIGTSTP for the explorer. Refused when `install` has
/// already claimed SIGINT and SIGTERM in this process (the two modes never share one).
pub fn install_term_handlers() -> io::Result<()> {
    if INSTALLED.get().is_some() {
        return Err(io::Error::other(
            "the run's signal handlers are installed in this process",
        ));
    }
    TERM_INSTALLED.store(true, Ordering::SeqCst);
    for sig in [
        libc::SIGINT,
        libc::SIGTERM,
        libc::SIGHUP,
        libc::SIGQUIT,
        libc::SIGTSTP,
    ] {
        set_term_handler(sig)?;
    }
    Ok(())
}

/// The first of SIGINT, SIGTERM, SIGHUP and SIGQUIT received since the handlers were installed.
pub fn term_signal() -> Option<i32> {
    match TERM_SIGNAL.load(Ordering::SeqCst) {
        0 => None,
        s => Some(s),
    }
}

/// Whether a SIGTSTP arrived since the last call (reads and clears).
pub fn take_tstp() -> bool {
    TSTP_PENDING.swap(false, Ordering::SeqCst)
}

/// Stops the whole process with SIGTSTP's default action and returns once SIGCONT resumes it, then puts the
/// explorer's SIGTSTP handler back.
pub fn suspend_process() -> io::Result<()> {
    set_handler(libc::SIGTSTP, libc::SIG_DFL)?;
    let raised = unsafe { libc::raise(libc::SIGTSTP) };
    let outcome = if raised == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    };
    TSTP_PENDING.store(false, Ordering::SeqCst);
    set_term_handler(libc::SIGTSTP)?;
    outcome
}

/// Wraps a writer so a closed pipe never fails the run: after the first `EPIPE` every write is dropped.
#[derive(Debug)]
pub struct SwallowBrokenPipe<W: Write> {
    inner: W,
    closed: bool,
}

impl<W: Write> SwallowBrokenPipe<W> {
    pub fn new(inner: W) -> SwallowBrokenPipe<W> {
        SwallowBrokenPipe {
            inner,
            closed: false,
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    fn absorb<T>(&mut self, r: io::Result<T>, ok: T) -> io::Result<T> {
        match r {
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                self.closed = true;
                Ok(ok)
            }
            other => other,
        }
    }
}

impl<W: Write> Write for SwallowBrokenPipe<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.closed {
            return Ok(buf.len());
        }
        let r = self.inner.write(buf);
        self.absorb(r, buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.closed {
            return Ok(());
        }
        let r = self.inner.flush();
        self.absorb(r, ())
    }
}
