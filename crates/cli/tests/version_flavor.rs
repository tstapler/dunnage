#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::process::Command;

#[test]
fn version_output_should_contain_build_flavor_when_spawned_with_version_or_short_flag() {
    for flag in ["--version", "-V"] {
        let out = Command::new(env!("CARGO_BIN_EXE_disk-cleanup"))
            .arg(flag)
            .output()
            .unwrap();
        assert!(out.status.success());
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(text.contains(disk_cleanup_core::build_flavor()), "{text}");
    }
}
