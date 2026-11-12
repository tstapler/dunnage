//! `RealTermSignals` (Task 6.3.3a): SIGINT, SIGTERM, SIGHUP and SIGQUIT arrive as a number the loop polls,
//! an external SIGTSTP as a flag, and `suspend()` stops the process through SIGTSTP's default action. The
//! handlers and the one `unsafe` call live in [`crate::signals`]; nothing here is unsafe.

use std::io;

use dunnage_core::explore::ports::TermSignals;

use crate::signals;

/// Installing it registers the five handlers for the whole process, so create one per run.
#[derive(Debug)]
pub struct RealTermSignals {
    _installed: (),
}

impl RealTermSignals {
    pub fn new() -> io::Result<RealTermSignals> {
        signals::install_term_handlers()?;
        Ok(RealTermSignals { _installed: () })
    }
}

impl TermSignals for RealTermSignals {
    fn interrupted(&self) -> Option<i32> {
        signals::term_signal()
    }

    fn suspend_requested(&self) -> bool {
        signals::take_tstp()
    }

    fn suspend(&self) {
        // A failed raise leaves the process running, which is the safe outcome for a suspend request.
        let _ = signals::suspend_process();
    }
}
