//! `--version` output. It names the build flavor so `scripts/check-release-features.sh` can prove
//! the release binary references `build_flavor()` (and is not blind to the marker). Keep the call.

use std::ffi::OsString;

use crate::wiring::audit::build_info;

pub fn line() -> String {
    let b = build_info();
    format!(
        "disk-cleanup {} {} commit {}{}{}",
        b.bin_version,
        disk_cleanup_core::build_flavor(),
        b.git_commit,
        if b.dirty { " dirty" } else { "" },
        if b.metascope_dirty { " scan-dirty" } else { "" },
    )
}

/// True when the only argument is `--version` or `-V`.
pub fn requested(mut args: impl Iterator<Item = OsString>) -> bool {
    matches!((args.next(), args.next()), (Some(a), None) if a == "--version" || a == "-V")
}
