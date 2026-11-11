//! The explorer's terminal front end (ADR-005, Epic 6.5). It draws what `disk-cleanup-core::explore` computes
//! and turns key events into messages; it has no filesystem, process or deletion code of its own, and
//! `scripts/check-tui-deps.sh` keeps it that way. Each module is declared here and filled by the story that
//! owns it, so no later story edits this file.

#![forbid(unsafe_code)]

pub mod app;
pub mod event_loop;
pub mod overlays;
pub mod terminal;
pub mod theme;
pub mod view;
