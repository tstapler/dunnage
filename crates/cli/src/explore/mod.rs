//! The `explore` subcommand's glue in the CLI crate (Phase 6). Each file is filled by the story that owns it;
//! Story 6.4.2 only declares them so the planner entry `OnlyMode::Tolerant` has its one allowed caller path.

pub mod actions;
pub mod handoff;
pub mod json;
pub mod mode;
pub mod run;
pub mod text;
