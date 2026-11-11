//! Composition root pieces. Every submodule is declared here and owned by one story, so none of them
//! shares this file: `dirs` Story 1.1.6, `token` Story 2.1.6, `confine` Story 2.1.6, `run_guard` Story 2.2.2,
//! `audit` Story 2.2.1, `env` and `cache_target` Story 1.2.4 (plan mode only: no token, lock or confinement
//! exists yet).

pub mod audit;
pub mod cache_target;
pub mod confine;
pub mod dirs;
pub mod env;
pub mod run_guard;
pub mod token;
