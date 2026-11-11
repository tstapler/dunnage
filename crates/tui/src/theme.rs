//! Colour policy and styles (UX-T25). Colour is decoration: state is always a character or a word, so
//! every style here degrades to reverse video, bold or dim, which `NO_COLOR` allows.

use ratatui::style::{Color, Modifier, Style};

/// The `--color` choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorMode {
    #[default]
    Auto,
    Always,
    Never,
}

/// First rule that applies, highest first (the cleaner's own order, `cli::terminal::color_on`): `--color`,
/// a non-empty `NO_COLOR`, `CLICOLOR_FORCE` set and not `0`, `TERM=dumb`, then whether stdout is a TTY.
/// `env` is the caller's environment lookup; this crate never reads the environment itself.
pub fn color_on(mode: ColorMode, tty: bool, env: &dyn Fn(&str) -> Option<String>) -> bool {
    match mode {
        ColorMode::Always => return true,
        ColorMode::Never => return false,
        ColorMode::Auto => {}
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    color: bool,
}

impl Theme {
    pub fn new(color: bool) -> Theme {
        Theme { color }
    }

    pub fn plain() -> Theme {
        Theme { color: false }
    }

    pub fn color(self) -> bool {
        self.color
    }

    /// The cursor row: reverse video, an attribute that `NO_COLOR` allows.
    pub fn cursor(self) -> Style {
        Style::default().add_modifier(Modifier::REVERSED)
    }

    /// The verdict word of the headroom line.
    pub fn verdict(self, word: &str) -> Style {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        if !self.color {
            return bold;
        }
        match word {
            "OK" => bold.fg(Color::Green),
            "WARN" => bold.fg(Color::Yellow),
            "CRIT" => bold.fg(Color::Red),
            _ => bold,
        }
    }

    /// A row whose directory is complete.
    pub fn done_row(self) -> Style {
        if self.color {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default()
        }
    }

    pub fn heading(self) -> Style {
        Style::default().add_modifier(Modifier::BOLD)
    }
}
