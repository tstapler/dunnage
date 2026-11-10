//! The explorer's pure model (Epic 6.2): messages, reducer, layout and the deletion checks. Every submodule
//! is declared here so the stories of W5 to W9 each fill their own file and never edit this one or `lib.rs`.

pub mod confirm;
pub mod fmt;
pub mod headroom;
pub mod help;
pub mod hint;
pub mod input;
pub mod keymap;
pub mod layout;
pub mod mark_check;
pub mod marks;
pub mod msg;
pub mod names;
pub mod ports;
pub mod reducer;
pub mod refusal;
pub mod revalidate;
pub mod rows;
#[cfg(feature = "test-support")]
pub mod testing;
pub mod tree;
pub mod view;
