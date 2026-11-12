//! `--version` output. It names the build flavor so `scripts/check-release-features.sh` can prove
//! the release binary references `build_flavor()` (and is not blind to the marker). Keep the call.

use std::ffi::OsString;

use dunnage_cli::wiring::audit::build_info;

pub fn line() -> String {
    let b = build_info();
    format!(
        "dunnage {} {} commit {}{}{}",
        b.bin_version,
        dunnage_core::build_flavor(),
        b.git_commit,
        if b.dirty { " dirty" } else { "" },
        if b.metascope_dirty { " scan-dirty" } else { "" },
    )
}

/// True when the only argument is `--version` or `-V`.
pub fn requested(mut args: impl Iterator<Item = OsString>) -> bool {
    matches!((args.next(), args.next()), (Some(a), None) if a == "--version" || a == "-V")
}
