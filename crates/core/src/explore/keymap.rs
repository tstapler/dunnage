//! `KEYMAP` (Task 6.2.4a): the one table of keys, commands and help lines. The input state machine, the `?`
//! overlay and the key tests all read it, so a key cannot be added without a help line (UX-T29). `D`, `Delete`,
//! `y` and `b` are deliberately absent: they are refused with a message in `input.rs`, not commands.

use super::rows::SortKey;

/// A key press, independent of the terminal library. Space is `Char(' ')`; Ctrl-F is `Ctrl('f')`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Char(char),
    Ctrl(char),
    Down,
    Up,
    PageDown,
    PageUp,
    Home,
    End,
    Left,
    Right,
    Enter,
    Backspace,
    Delete,
    Esc,
}

impl Key {
    /// The name the help overlay prints: `j`, `Space`, `Ctrl-F`, `PgDn`.
    pub fn label(self) -> String {
        match self {
            Key::Char(' ') => "Space".to_string(),
            Key::Char(c) => c.to_string(),
            Key::Ctrl(c) => format!("Ctrl-{}", c.to_ascii_uppercase()),
            Key::Down => "Down".to_string(),
            Key::Up => "Up".to_string(),
            Key::PageDown => "PgDn".to_string(),
            Key::PageUp => "PgUp".to_string(),
            Key::Home => "Home".to_string(),
            Key::End => "End".to_string(),
            Key::Left => "Left".to_string(),
            Key::Right => "Right".to_string(),
            Key::Enter => "Enter".to_string(),
            Key::Backspace => "Backspace".to_string(),
            Key::Delete => "Delete".to_string(),
            Key::Esc => "Esc".to_string(),
        }
    }
}

/// What a key means in the main view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    CursorDown,
    CursorUp,
    PageDown,
    PageUp,
    First,
    Last,
    Open,
    Parent,
    Sort(SortKey),
    SharedColumn,
    ShareBar,
    FilterText,
    FilterCycle,
    Info,
    Mark,
    ClearMarks,
    Preview,
    Delete,
    Rescan,
    RescanAll,
    Escape,
    Errors,
    Help,
    Redraw,
    Quit,
    Interrupt,
}

/// The help overlay's section for an entry, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Move,
    OpenBack,
    Sort,
    Columns,
    Filter,
    Scan,
    Info,
    Plan,
    Quit,
    Screen,
}

impl Group {
    pub const ALL: [Group; 10] = [
        Group::Move,
        Group::OpenBack,
        Group::Sort,
        Group::Columns,
        Group::Filter,
        Group::Scan,
        Group::Info,
        Group::Plan,
        Group::Quit,
        Group::Screen,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Group::Move => "Move",
            Group::OpenBack => "Open/back",
            Group::Sort => "Sort",
            Group::Columns => "Columns",
            Group::Filter => "Filter",
            Group::Scan => "Scan",
            Group::Info => "Info",
            Group::Plan => "Plan",
            Group::Quit => "Quit",
            Group::Screen => "Screen",
        }
    }
}

/// One row of the table: the keys that run `cmd`, how the help writes them and what it says.
#[derive(Debug, Clone, Copy)]
pub struct KeyDef {
    pub keys: &'static [Key],
    pub label: &'static str,
    pub cmd: Cmd,
    pub group: Group,
    pub help: &'static str,
}

const fn c(ch: char) -> Key {
    Key::Char(ch)
}

const fn def(
    keys: &'static [Key],
    label: &'static str,
    cmd: Cmd,
    group: Group,
    help: &'static str,
) -> KeyDef {
    KeyDef {
        keys,
        label,
        cmd,
        group,
        help,
    }
}

/// Every key of `tui/ux-explorer.md` section 5 that is a command, once.
pub const KEYMAP: &[KeyDef] = &[
    def(
        &[c('j'), Key::Down],
        "j/Down",
        Cmd::CursorDown,
        Group::Move,
        "down",
    ),
    def(&[c('k'), Key::Up], "k/Up", Cmd::CursorUp, Group::Move, "up"),
    def(
        &[Key::PageDown, Key::Ctrl('f')],
        "PgDn/Ctrl-F",
        Cmd::PageDown,
        Group::Move,
        "page down",
    ),
    def(
        &[Key::PageUp, Key::Ctrl('b')],
        "PgUp/Ctrl-B",
        Cmd::PageUp,
        Group::Move,
        "page up",
    ),
    def(
        &[c('g'), Key::Home],
        "g/Home",
        Cmd::First,
        Group::Move,
        "top",
    ),
    def(
        &[c('G'), Key::End],
        "G/End",
        Cmd::Last,
        Group::Move,
        "bottom",
    ),
    def(
        &[c('l'), Key::Right, Key::Enter],
        "l/Right/Enter",
        Cmd::Open,
        Group::OpenBack,
        "open directory",
    ),
    def(
        &[c('h'), Key::Left, Key::Backspace],
        "h/Left/Backspace",
        Cmd::Parent,
        Group::OpenBack,
        "parent directory",
    ),
    def(
        &[c('s')],
        "s",
        Cmd::Sort(SortKey::Usage),
        Group::Sort,
        "usage",
    ),
    def(
        &[c('a')],
        "a",
        Cmd::Sort(SortKey::Apparent),
        Group::Sort,
        "apparent",
    ),
    def(
        &[c('m')],
        "m",
        Cmd::Sort(SortKey::Meta),
        Group::Sort,
        "metadata",
    ),
    def(
        &[c('c')],
        "c",
        Cmd::Sort(SortKey::Items),
        Group::Sort,
        "items",
    ),
    def(
        &[c('n')],
        "n",
        Cmd::Sort(SortKey::Name),
        Group::Sort,
        "name (again = reverse)",
    ),
    def(
        &[c('u')],
        "u",
        Cmd::SharedColumn,
        Group::Columns,
        "toggle shared-extent estimate column",
    ),
    def(
        &[c('t')],
        "t",
        Cmd::ShareBar,
        Group::Columns,
        "toggle share bar/percent",
    ),
    def(
        &[c('/')],
        "/",
        Cmd::FilterText,
        Group::Filter,
        "name contains",
    ),
    def(
        &[c('f')],
        "f",
        Cmd::FilterCycle,
        Group::Filter,
        "cycle: all, candidates only, errors only",
    ),
    def(
        &[Key::Esc],
        "Esc",
        Cmd::Escape,
        Group::Scan,
        "close, clear filter, stop the scan (keeps what was found)",
    ),
    def(
        &[c('r')],
        "r",
        Cmd::Rescan,
        Group::Scan,
        "rescan this directory",
    ),
    def(
        &[c('R')],
        "R",
        Cmd::RescanAll,
        Group::Scan,
        "rescan everything (like --rescan)",
    ),
    def(
        &[c('e')],
        "e",
        Cmd::Errors,
        Group::Scan,
        "list unreadable paths",
    ),
    def(
        &[c('i')],
        "i",
        Cmd::Info,
        Group::Info,
        "details of the selected item",
    ),
    def(
        &[c(' ')],
        "Space",
        Cmd::Mark,
        Group::Plan,
        "mark/unmark an item (max 20)",
    ),
    def(&[c('x')], "x", Cmd::ClearMarks, Group::Plan, "clear marks"),
    def(
        &[c('p')],
        "p",
        Cmd::Preview,
        Group::Plan,
        "preview the plan (deletes nothing)",
    ),
    def(
        &[c('d')],
        "d",
        Cmd::Delete,
        Group::Plan,
        "delete the marked items after a plan and a typed count",
    ),
    def(
        &[c('q')],
        "q",
        Cmd::Quit,
        Group::Quit,
        "quit (asks what to do with marks)",
    ),
    def(
        &[Key::Ctrl('c')],
        "Ctrl-C",
        Cmd::Interrupt,
        Group::Quit,
        "quit now, exit 130",
    ),
    def(
        &[Key::Ctrl('l')],
        "Ctrl-L",
        Cmd::Redraw,
        Group::Screen,
        "redraw",
    ),
    def(&[c('?')], "?", Cmd::Help, Group::Screen, "this help"),
];

/// The command a key runs in the main view.
pub fn lookup(key: Key) -> Option<&'static KeyDef> {
    KEYMAP.iter().find(|d| d.keys.contains(&key))
}

/// Every key the table binds, in table order.
pub fn all_keys() -> impl Iterator<Item = Key> {
    KEYMAP.iter().flat_map(|d| d.keys.iter().copied())
}
