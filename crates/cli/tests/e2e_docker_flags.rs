#![cfg(feature = "gate-d")]
#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Gate D, the Docker selection flags through the spawned binary under `fixture_cmd()` (validation T-503,
//! second half). Run with `GATE=D cargo test --workspace --features disk-cleanup/gate-d`.

mod common;

use common::{text_of, Fixture};

#[test]
fn docker_volumes_flag_should_exit_64_and_delete_nothing_When_docker_volumes_is_not_in_kinds() {
    let f = Fixture::new();
    for flag in [
        ["--docker-volumes", "named"],
        ["--docker-volumes", "anonymous"],
    ] {
        let out = f
            .fixture_cmd()
            .args(["--kinds", "docker"])
            .args(flag)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        let (stdout, stderr) = text_of(&out);
        assert_eq!(out.status.code(), Some(64), "{stderr}");
        assert!(stdout.is_empty(), "{stdout}");
        assert!(
            stderr.contains(
                "--docker-volumes only applies to docker-volumes, which is not in --kinds (docker)"
            ),
            "{stderr}"
        );
    }
}
