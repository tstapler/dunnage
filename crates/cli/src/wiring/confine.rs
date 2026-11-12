//! Binary-level confinement (Story 2.1.6): what the spawned binary may touch, decided from its environment
//! and its build, never from a flag. `confine_mode` is pure; this file is the only caller of
//! `RealDirOps::unconfined` (`scripts/check-token-sites.sh`).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use dunnage_adapters::command_confined::ConfinedCommandRunner;
use dunnage_adapters::{RealCommandRunner, RealDirOps};
use dunnage_core::ports::CommandRunner;
use dunnage_core::ApplyToken;

/// The sentinel the test harness writes into a fixture root; nothing else in the workspace creates it.
pub const SENTINEL: &str = ".dc-test-root";

pub const BAD_ROOT: &str = "error: DC_TEST_ROOT must be an existing directory that contains .dc-test-root (written by the test harness). Nothing was changed.";
pub const DEBUG_REFUSES: &str =
    "error: debug build refuses --apply outside DC_TEST_ROOT. Nothing was deleted.";
pub const TEST_ENV_REFUSES: &str =
    "error: refusing unconfined --apply in a test or cargo environment. Nothing was deleted.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confine {
    /// Confined to this canonical fixture root.
    Root(PathBuf),
    Unconfined,
    Refuse(&'static str),
}

pub type Vars = BTreeMap<OsString, OsString>;

pub fn current_vars() -> Vars {
    std::env::vars_os().collect()
}

fn cargo_env(vars: &Vars) -> bool {
    vars.keys().any(|k| {
        let k = k.to_string_lossy();
        k == "CARGO_MANIFEST_DIR" || k.starts_with("CARGO_BIN_EXE_")
    })
}

fn fixture_root(raw: &OsString) -> Option<PathBuf> {
    let p = Path::new(raw);
    if !p.is_absolute() {
        return None;
    }
    let canon = std::fs::canonicalize(p).ok()?;
    let ok = canon != Path::new("/")
        && canon.is_dir()
        && std::fs::symlink_metadata(canon.join(SENTINEL)).is_ok_and(|m| m.is_file());
    ok.then_some(canon)
}

/// `flavor` is `core::build_flavor()`; `debug_assertions` is `cfg!(debug_assertions)` of the caller, a
/// parameter so the table test covers both profiles. A valid `DC_TEST_ROOT` wins over everything else,
/// `DC_DEV_APPLY_REAL` included; the test and cargo signals beat `DC_DEV_APPLY_REAL`.
pub fn confine_mode(vars: &Vars, flavor: &str, debug_assertions: bool) -> Confine {
    if let Some(raw) = vars.get(&OsString::from("DC_TEST_ROOT")) {
        return fixture_root(raw).map_or(Confine::Refuse(BAD_ROOT), Confine::Root);
    }
    if flavor != "dc-flavor=release" || cargo_env(vars) {
        return Confine::Refuse(TEST_ENV_REFUSES);
    }
    let real_ok = vars
        .get(&OsString::from("DC_DEV_APPLY_REAL"))
        .is_some_and(|v| v == "1");
    if debug_assertions && !real_ok {
        return Confine::Refuse(DEBUG_REFUSES);
    }
    Confine::Unconfined
}

/// The `DirOps` of an apply for `mode`; `None` for `Refuse`.
pub fn apply_dir_ops(mode: &Confine, token: &ApplyToken) -> Option<RealDirOps> {
    match mode {
        Confine::Root(root) => Some(RealDirOps::confined_to(root)),
        Confine::Unconfined => Some(RealDirOps::unconfined(token)),
        Confine::Refuse(_) => None,
    }
}

/// The runner of an apply: confined to the fixture whenever there is one.
pub fn apply_runner(mode: &Confine) -> Option<Box<dyn CommandRunner>> {
    match mode {
        Confine::Root(root) => Some(Box::new(ConfinedCommandRunner::new(
            RealCommandRunner::new(),
            root.clone(),
        ))),
        Confine::Unconfined => Some(Box::new(RealCommandRunner::new())),
        Confine::Refuse(_) => None,
    }
}

/// `Err(message)` when the binary must exit 64 before doing anything. Called for `--apply`, and for any
/// mode when `DC_TEST_ROOT` is set.
pub fn startup_refusal(vars: &Vars, apply: bool) -> Option<&'static str> {
    if !apply && !vars.contains_key(&OsString::from("DC_TEST_ROOT")) {
        return None;
    }
    match confine_mode(vars, dunnage_core::build_flavor(), cfg!(debug_assertions)) {
        Confine::Refuse(msg) => Some(msg),
        _ => None,
    }
}
