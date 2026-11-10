#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]

use disk_cleanup_adapters::command::RealCommandRunner;
use disk_cleanup_core::ports::{Cmd, CommandRunner};
use disk_cleanup_core::{ApplyToken, DeleteError};

#[test]
fn run_should_refuse_a_mislabelled_or_non_allowlisted_cmd_without_spawning() {
    let r = RealCommandRunner::new();
    let sentinel = std::env::temp_dir().join(format!("dc-refused-{}", std::process::id()));
    let touch = Cmd::new("touch", [sentinel.as_os_str()], false);
    assert_eq!(r.run(&touch), Err(DeleteError::Refused));
    assert!(!sentinel.exists());
    let vol_rm = Cmd::new("docker", ["volume", "rm", "x"], false);
    assert_eq!(r.run(&vol_rm), Err(DeleteError::Refused));
    let info = Cmd::new("docker", ["info"], true);
    assert_eq!(
        r.run_mutating(&info, &ApplyToken::for_tests()),
        Err(DeleteError::Refused)
    );
    let touch = Cmd::new("touch", [sentinel.as_os_str()], true);
    assert_eq!(
        r.run_mutating(&touch, &ApplyToken::for_tests()),
        Err(DeleteError::Refused)
    );
    assert!(!sentinel.exists());
}

#[test]
fn run_should_return_git_output_When_a_read_only_row_runs_in_a_temp_repo() {
    let d = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(d.path())
            .output()
            .unwrap()
    };
    assert!(git(&["init", "-q"]).status.success());
    let repo = d.path().to_str().unwrap();
    let out = RealCommandRunner::new()
        .run(&Cmd::new(
            "git",
            ["-C", repo, "worktree", "list", "--porcelain"],
            false,
        ))
        .unwrap();
    assert!(out.stdout_lossy().starts_with("worktree "));
}
