//! Colour and width rules (`design/ux.md` 1.3 and 3.6). `Terminal` is injected so every rule is tested with
//! plain values; [`detect`] reads the real stdout.

use std::io::IsTerminal;

/// Columns assumed when nothing says otherwise, and the narrowest width the layout honours.
pub const DEFAULT_WIDTH: usize = 80;
pub const MIN_WIDTH: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

/// What the renderer needs to know about stdout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Terminal {
    pub tty: bool,
    pub width: usize,
    pub color: bool,
}

/// `COLUMNS` as a width: an integer of at least 20.
fn columns_from_env(env: &dyn Fn(&str) -> Option<String>) -> Option<usize> {
    env("COLUMNS")
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|w| *w >= MIN_WIDTH)
}

/// First rule that applies, highest first: `--color`, a non-empty `NO_COLOR`, `CLICOLOR_FORCE` set and not
/// `0`, `TERM=dumb`, then whether stdout is a TTY.
pub fn color_on(choice: ColorChoice, tty: bool, env: &dyn Fn(&str) -> Option<String>) -> bool {
    match choice {
        ColorChoice::Always => return true,
        ColorChoice::Never => return false,
        ColorChoice::Auto => {}
    }
    if env("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return false;
    }
    if env("CLICOLOR_FORCE").is_some_and(|v| v != "0") {
        return true;
    }
    if env("TERM").is_some_and(|v| v == "dumb") {
        return false;
    }
    tty
}

impl Terminal {
    /// `ioctl_cols` is the `TIOCGWINSZ` value, consulted only when `tty`; otherwise `COLUMNS` (at least 20),
    /// otherwise 80.
    pub fn resolve(
        tty: bool,
        ioctl_cols: Option<u16>,
        choice: ColorChoice,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Terminal {
        let from_ioctl = ioctl_cols
            .filter(|_| tty)
            .map(|c| usize::from(c).max(MIN_WIDTH));
        let width = from_ioctl
            .or_else(|| columns_from_env(env))
            .unwrap_or(DEFAULT_WIDTH);
        Terminal {
            tty,
            width,
            color: color_on(choice, tty, env),
        }
    }

    /// A plain pipe at 80 columns: the shape of every snapshot.
    pub fn pipe() -> Terminal {
        Terminal {
            tty: false,
            width: DEFAULT_WIDTH,
            color: false,
        }
    }
}

/// The real stdout and the real environment.
pub fn detect(choice: ColorChoice) -> Terminal {
    let out = std::io::stdout();
    let tty = out.is_terminal();
    let cols = tty
        .then(|| rustix::termios::tcgetwinsize(&out).ok())
        .flatten()
        .map(|w| w.ws_col)
        .filter(|c| *c > 0);
    Terminal::resolve(tty, cols, choice, &|k| std::env::var(k).ok())
}

/// The 8 standard ANSI colours plus bold; colour only tints a word that already carries the meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tint {
    Green,
    Yellow,
    Red,
    Bold,
}

impl Tint {
    fn code(self) -> &'static str {
        match self {
            Tint::Green => "32",
            Tint::Yellow => "33",
            Tint::Red => "31",
            Tint::Bold => "1",
        }
    }
}

impl Terminal {
    /// `text` wrapped in the tint when colour is on, unchanged otherwise.
    pub fn paint(&self, text: &str, tint: Tint) -> String {
        if self.color {
            format!("\x1b[{}m{text}\x1b[0m", tint.code())
        } else {
            text.to_string()
        }
    }

    /// Tint the leading `word` of a line, leaving the rest (and the layout) alone.
    pub fn paint_lead(&self, line: &str, word: &str, tint: Tint) -> String {
        match line.strip_prefix(word) {
            Some(rest) if self.color => format!("{}{rest}", self.paint(word, tint)),
            _ => line.to_string(),
        }
    }
}
