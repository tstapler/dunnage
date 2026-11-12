//! The real [`TypedCountSource`]: whether the typed count can be collected at all.
//!
//! The entry itself is read by the explorer loop's single tty reader (crossterm, which reads stdin when it
//! is a terminal and the controlling terminal `/dev/tty`, opened `O_RDWR`, otherwise), never by a second
//! reader (review 2, N8). The interactive mode is only chosen when stdin is a terminal, so `false` here
//! guards a future caller, not a state the current entry point can reach.

use std::io::IsTerminal;

use dunnage_core::explore::ports::TypedCountSource;

#[derive(Debug, Clone, Copy, Default)]
pub struct StdinTerminal;

impl TypedCountSource for StdinTerminal {
    fn is_terminal(&self) -> bool {
        std::io::stdin().is_terminal()
    }
}
