//! The text, JSON or interactive mode decision of `explore` (Story 6.5.4, UX-T26).
//!
//! `choose_mode` is pure: the caller reads the terminal and the environment and passes the answers in, so the
//! whole truth table is tested without a terminal.

/// The smallest window the interactive screen is started in. Smaller than that is a screen nobody can read,
/// so the text view is printed instead (INFERRED: the plan names 0x0, 1x1 and 20x5 as text-view sizes and
/// does not give the boundary; the in-screen "too small" message of `tui/ux-explorer.md` 6 covers a window
/// that shrinks after the start).
pub const MIN_START_COLUMNS: u16 = 20;
pub const MIN_START_ROWS: u16 = 6;

/// What the flags asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModeFlags {
    pub text: bool,
    pub json: bool,
}

/// The view `explore` runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Interactive,
    /// `note` is the stderr line for an automatic fallback; `None` when `--text` asked for it.
    Text {
        note: Option<String>,
    },
    Json,
}

/// The first reason the interactive screen cannot run, worded for the note.
fn blocker(
    stdin_tty: bool,
    stdout_tty: bool,
    term: Option<&str>,
    size: Option<(u16, u16)>,
    plain_env: bool,
) -> Option<String> {
    if !stdout_tty {
        return Some("stdout is not a terminal".to_string());
    }
    if !stdin_tty {
        return Some("stdin is not a terminal".to_string());
    }
    match term {
        None | Some("") => return Some("TERM is not set".to_string()),
        Some("dumb") => return Some("TERM is dumb".to_string()),
        Some(_) => {}
    }
    match size {
        None | Some((0, _) | (_, 0)) => {
            return Some("the terminal size cannot be read".to_string());
        }
        Some((c, r)) if c < MIN_START_COLUMNS || r < MIN_START_ROWS => {
            return Some(format!(
                "the terminal is {c}x{r}, too small for the explorer"
            ));
        }
        Some(_) => {}
    }
    plain_env.then(|| "DUNNAGE_PLAIN is set".to_string())
}

/// `size` is `(columns, rows)` from the ioctl, `None` when it failed. `plain_env` is "`DUNNAGE_PLAIN`
/// is set". `--json` wins over everything, `--text` over the terminal checks.
pub fn choose_mode(
    stdin_tty: bool,
    stdout_tty: bool,
    term: Option<&str>,
    size: Option<(u16, u16)>,
    flags: ModeFlags,
    plain_env: bool,
) -> Mode {
    if flags.json {
        return Mode::Json;
    }
    if flags.text {
        return Mode::Text { note: None };
    }
    match blocker(stdin_tty, stdout_tty, term, size, plain_env) {
        None => Mode::Interactive,
        Some(why) => Mode::Text {
            note: Some(format!(
                "note: {why}; printing the text view (same as --text)"
            )),
        },
    }
}
