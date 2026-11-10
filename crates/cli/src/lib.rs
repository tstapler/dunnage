//! Rendering of `disk-cleanup` output (Story 1.2.3): the human report of `design/ux.md` S1-S5, the JSON
//! documents of S7, the row layout of 3.6, path escaping (UX-57) and the colour and width rules of 1.3 and
//! 3.6. Pure: everything arrives as data, so the screens are tested without a terminal or a filesystem.

#![forbid(unsafe_code)]

pub mod escape;
pub mod layout;
pub mod terminal;
pub mod units;
