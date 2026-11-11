//! Composition root pieces. Every submodule is declared here and owned by one story, so none of them
//! shares this file: `dirs` Story 1.1.6, `token` Story 2.1.6, `confine` Story 2.1.6, `run_guard` Story 2.2.2,
//! `audit` Story 2.2.1, `env` Story 1.2.4, `kinds` and `compose` Story 2.3.4 (the per-gate registry and the apply path).

pub mod audit;
pub mod compose;
pub mod confine;
pub mod dirs;
pub mod env;
pub mod kinds;
pub mod run_guard;
pub mod token;
