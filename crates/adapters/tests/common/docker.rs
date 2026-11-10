#![allow(dead_code, clippy::unnecessary_wraps)]
//! Shared helpers for the Docker adapter tests: the recorded fixtures and a scripted healthy daemon.

use std::path::PathBuf;

use disk_cleanup_core::ports::CmdOutput;
use disk_cleanup_core::testing::FakeCommandRunner;
use disk_cleanup_core::DeleteError;

pub fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/docker")
}

pub fn fixture(name: &str) -> String {
    std::fs::read_to_string(fixture_dir().join(name)).unwrap()
}

pub fn ok(s: &str) -> Result<CmdOutput, DeleteError> {
    Ok(CmdOutput {
        stdout: s.as_bytes().to_vec(),
        stderr: Vec::new(),
    })
}

pub fn failed(message: &str) -> Result<CmdOutput, DeleteError> {
    Err(DeleteError::CommandFailed {
        status: 1,
        message: message.to_string(),
    })
}

pub const CTX: &[&str] = &["docker", "context", "show"];
pub const INFO: &[&str] = &["docker", "info"];
pub const VERSION: &[&str] = &["docker", "version", "--format", "{{.Server.Version}}"];
pub const DF: &[&str] = &["docker", "system", "df", "-v", "--format", "json"];
pub const PS: &[&str] = &["docker", "ps", "-a", "--format", "{{json .}}"];
pub const IMAGES: &[&str] = &["docker", "images", "--format", "{{json .}}"];
pub const VOLS: &[&str] = &["docker", "volume", "ls", "--format", "{{json .}}"];

pub const INSPECT_PG: &[&str] = &["docker", "volume", "inspect", "dc-spike-pg"];
pub const INSPECT_NAMED: &[&str] = &["docker", "volume", "inspect", "dc-spike-named"];

/// A daemon that answers every listing with the recorded fixtures.
pub fn healthy() -> FakeCommandRunner {
    healthy_but(&[])
}

/// The healthy daemon without the replies for the argvs in `skip` (the fake appends replies, it does not
/// replace them), so a test registers its own.
pub fn healthy_but(skip: &[&[&str]]) -> FakeCommandRunner {
    let r = FakeCommandRunner::new();
    let raw = fixture("volume_inspect.jsonl");
    let by_name = |n: &str| {
        raw.lines()
            .find(|l| l.contains(&format!("\"Name\":\"{n}\"")))
            .unwrap()
            .to_string()
    };
    let table: Vec<(&[&str], String)> = vec![
        (CTX, "default\n".into()),
        (INFO, "Server:\n".into()),
        (VERSION, "29.8.2\n".into()),
        (DF, fixture("system_df_v.jsonl")),
        (PS, fixture("ps_a.jsonl")),
        (IMAGES, fixture("docker_images.jsonl")),
        (VOLS, fixture("volume_ls.jsonl")),
        (INSPECT_NAMED, by_name("dc-spike-named")),
        (INSPECT_PG, by_name("dc-spike-pg")),
    ];
    for (argv, out) in table {
        if !skip.contains(&argv) {
            r.on(argv, ok(&out));
        }
    }
    r
}
