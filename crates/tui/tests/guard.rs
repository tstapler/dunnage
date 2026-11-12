#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The terminal guard (Task 6.5.2a; UX-T23, C-13): exact undo, once, panics, and the cached size.

mod common;

use std::io::{self, Write};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use common::Spy;
use dunnage_tui::terminal::{
    fixed_terminal, install_panic_hook, DiagQueue, RawMode, TerminalGuard, EXIT_TERMINAL,
    RESTORE_FAILED,
};
use ratatui::widgets::Paragraph;

/// Panic hooks are process-global: one test at a time.
static HOOK: Mutex<()> = Mutex::new(());

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Sink {
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().clone()
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes()).into_owned()
    }
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Fails every write after the first `ok` bytes.
struct Failing {
    sink: Sink,
    fail: Arc<std::sync::atomic::AtomicBool>,
}

impl Write for Failing {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(io::Error::from_raw_os_error(5));
        }
        self.sink.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(io::Error::from_raw_os_error(5));
        }
        Ok(())
    }
}

#[derive(Clone, Default)]
struct FakeRaw(Arc<Mutex<Vec<&'static str>>>);

impl FakeRaw {
    fn log(&self) -> Vec<&'static str> {
        self.0.lock().unwrap().clone()
    }
}

impl RawMode for FakeRaw {
    fn enable(&mut self) -> io::Result<()> {
        self.0.lock().unwrap().push("raw-on");
        Ok(())
    }

    fn disable(&mut self) -> io::Result<()> {
        self.0.lock().unwrap().push("raw-off");
        Ok(())
    }
}

const ENTER: &str = "\x1b[?1049h\x1b[?25l";
const RESTORE: &str = "\x1b[?25h\x1b[?1049l\x1b[0m";

#[test]
fn enter_enables_raw_mode_the_alternate_screen_and_hides_the_cursor_and_nothing_else() {
    let (sink, raw) = (Sink::default(), FakeRaw::default());
    let guard = TerminalGuard::enter(sink.clone(), raw.clone()).unwrap();
    assert_eq!(
        sink.text(),
        ENTER,
        "no mouse, no focus events, no bracketed paste"
    );
    assert_eq!(raw.log(), ["raw-on"]);
    drop(guard);
}

#[test]
fn drop_undoes_exactly_what_enter_did_in_order_and_once() {
    let (sink, raw) = (Sink::default(), FakeRaw::default());
    let guard = TerminalGuard::enter(sink.clone(), raw.clone()).unwrap();
    guard.restore().unwrap();
    guard.restore().unwrap();
    drop(guard);
    assert_eq!(
        sink.text(),
        format!("{ENTER}{RESTORE}"),
        "restore twice, one set of sequences"
    );
    assert_eq!(
        raw.log(),
        ["raw-on", "raw-off"],
        "raw mode is switched off once, after the writes"
    );
}

#[test]
fn a_plain_drop_restores_when_restore_was_never_called() {
    let (sink, raw) = (Sink::default(), FakeRaw::default());
    drop(TerminalGuard::enter(sink.clone(), raw.clone()).unwrap());
    assert!(sink.text().ends_with(RESTORE));
    assert_eq!(raw.log(), ["raw-on", "raw-off"]);
}

#[test]
fn reenter_after_a_suspend_writes_the_enter_sequence_again() {
    let (sink, raw) = (Sink::default(), FakeRaw::default());
    let guard = TerminalGuard::enter(sink.clone(), raw.clone()).unwrap();
    guard.restore().unwrap();
    guard.reenter().unwrap();
    assert_eq!(sink.text(), format!("{ENTER}{RESTORE}{ENTER}"));
    assert!(guard.is_active());
}

#[test]
fn a_write_error_on_restore_is_returned_and_raw_mode_is_still_switched_off() {
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let raw = FakeRaw::default();
    let w = Failing {
        sink: Sink::default(),
        fail: Arc::clone(&fail),
    };
    let guard = TerminalGuard::enter(w, raw.clone()).unwrap();
    fail.store(true, Ordering::SeqCst);
    let err = guard.restore().unwrap_err();
    assert_eq!(err.raw_os_error(), Some(5));
    assert_eq!(
        raw.log(),
        ["raw-on", "raw-off"],
        "a failed write does not leave the terminal raw"
    );
    // the exit code and text a failed restore maps to (the loop test checks the mapping end to end)
    assert_eq!(EXIT_TERMINAL, 74);
    assert_eq!(
        RESTORE_FAILED,
        "error: could not restore the terminal; run 'reset'"
    );
}

#[test]
fn a_panic_on_the_ui_thread_restores_the_terminal_in_the_hook() {
    let _g = HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (sink, raw) = (Sink::default(), FakeRaw::default());
    let (s2, r2) = (sink.clone(), raw.clone());
    std::thread::spawn(move || {
        let guard = TerminalGuard::enter(s2, r2).unwrap();
        let q = Arc::new(DiagQueue::default());
        let hook = install_panic_hook(guard.restorer(), Arc::clone(&q));
        let r = std::panic::catch_unwind(|| panic!("boom on the ui thread"));
        assert!(r.is_err());
        assert!(
            q.is_empty(),
            "a UI-thread panic prints at once, it is not queued"
        );
        drop(hook);
    })
    .join()
    .unwrap();
    assert!(
        sink.text().ends_with(RESTORE),
        "the hook restored before the message: {:?}",
        sink.text()
    );
    assert_eq!(raw.log(), ["raw-on", "raw-off"]);
}

#[test]
fn a_panic_on_another_thread_is_queued_and_does_not_touch_the_screen() {
    let _g = HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (sink, raw) = (Sink::default(), FakeRaw::default());
    let guard = TerminalGuard::enter(sink.clone(), raw.clone()).unwrap();
    let q = Arc::new(DiagQueue::default());
    let hook = install_panic_hook(guard.restorer(), Arc::clone(&q));
    let r = std::thread::spawn(|| panic!("worker \x1b[31m failed")).join();
    assert!(r.is_err());
    drop(hook);
    assert_eq!(
        sink.text(),
        ENTER,
        "nothing is written until the loop restores"
    );
    assert!(guard.is_active());
    let lines = q.take();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("worker"), "{lines:?}");
    assert!(
        !lines[0].contains('\x1b'),
        "the queued text is escaped: {lines:?}"
    );
}

#[test]
fn a_dropped_hook_hands_later_panics_to_the_previous_hook() {
    let _g = HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let guard = TerminalGuard::enter(Sink::default(), FakeRaw::default()).unwrap();
    let q = Arc::new(DiagQueue::default());
    drop(install_panic_hook(guard.restorer(), Arc::clone(&q)));
    let _ = std::thread::spawn(|| {
        let _ = std::panic::catch_unwind(|| panic!("after the hook was dropped"));
    })
    .join();
    assert!(q.is_empty());
}

#[test]
fn the_fixed_viewport_never_asks_the_inner_backend_for_its_size() {
    let _g = HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // A recording `tput` on PATH must never run either.
    let dir = std::env::temp_dir().join(format!("tui-tput-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let ran = dir.join("ran");
    let stub = dir.join("tput");
    std::fs::write(&stub, format!("#!/bin/sh\necho x >> '{}'\n", ran.display())).unwrap();
    std::fs::set_permissions(&stub, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let old = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![dir.clone()];
    paths.extend(std::env::split_paths(&old));
    std::env::set_var("PATH", std::env::join_paths(paths).unwrap());

    let spy = Spy::new(80, 24);
    let calls = Arc::clone(&spy.size_calls);
    let mut term = fixed_terminal(spy, 80, 24).unwrap();
    for i in 0..1_000 {
        term.draw(|f| f.render_widget(Paragraph::new(format!("frame {i}")), f.area()))
            .unwrap();
        if i % 100 == 99 {
            let c = 80 + (i / 100) as u16;
            term.backend_mut().set_size(c, 24);
            term.backend_mut().inner_mut().inner.resize(c, 24);
            term.resize(ratatui::layout::Rect::new(0, 0, c, 24))
                .unwrap();
        }
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "no call reached the inner size()"
    );
    assert!(term.backend().size_reads() <= 10_000);
    assert!(!ran.exists(), "tput was spawned");

    std::env::set_var("PATH", old);
    let _ = std::fs::remove_dir_all(&dir);
}
