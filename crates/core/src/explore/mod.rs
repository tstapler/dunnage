//! The explorer's pure model (Epic 6.2): messages, reducer, layout and the deletion checks. Every submodule
//! is declared here so the stories of W5 to W9 each fill their own file and never edit this one or `lib.rs`.

pub mod msg;
pub mod ports;
pub mod fmt;
pub mod layout;
pub mod headroom;
pub mod tree;
pub mod reducer;
pub mod names;
pub mod revalidate;
pub mod view;
pub mod rows;
pub mod keymap;
pub mod input;
pub mod hint;
pub mod refusal;
pub mod marks;
pub mod help;
pub mod mark_check;
pub mod confirm;
#[cfg(feature = "test-support")]
pub mod testing;
