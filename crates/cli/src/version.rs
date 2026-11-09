//! `--version` output. It names the build flavor so `scripts/check-release-features.sh` can prove
//! the release binary references `build_flavor()` (and is not blind to the marker). Keep the call.

use std::ffi::OsString;

pub fn line() -> String {
    format!(
        "disk-cleanup {} {}",
        env!("CARGO_PKG_VERSION"),
        disk_cleanup_core::build_flavor()
    )
}

/// True when the only argument is `--version` or `-V`.
pub fn requested(mut args: impl Iterator<Item = OsString>) -> bool {
    matches!((args.next(), args.next()), (Some(a), None) if a == "--version" || a == "-V")
}
