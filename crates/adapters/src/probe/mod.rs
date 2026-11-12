//! Real `SystemProbe`. This story fills `basic.rs` (`euid`, `platform`, `mounts`); each other fact has its own
//! file, owned by a later story, whose stub here fails closed (`Unknown`): `cwd.rs` Story 3.1.2,
//! `run_state.rs` Story 3.2.2, `process.rs` Story 4.2.1.

use std::io;
use std::path::Path;

use dunnage_core::ports::{RunState, SystemProbe};
use dunnage_core::{MountEntry, Platform, Tri};

pub mod basic;
pub mod cwd;
pub mod process;
pub mod run_state;

#[derive(Debug, Default, Clone, Copy)]
pub struct RealProbe;

impl SystemProbe for RealProbe {
    fn euid(&self) -> u32 {
        basic::euid()
    }

    fn platform(&self) -> Platform {
        basic::platform()
    }

    fn process_named(&self, name: &str) -> Tri {
        process::process_named(name)
    }

    fn run_state(&self, path: &Path) -> RunState {
        run_state::run_state(path)
    }

    fn cwd_in_use(&self, path: &Path) -> (Tri, Option<(String, u32)>) {
        cwd::cwd_in_use(path)
    }

    fn mounts(&self) -> io::Result<Vec<MountEntry>> {
        basic::mounts()
    }
}
