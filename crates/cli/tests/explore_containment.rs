#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cloned_ref_to_slice_refs,
    clippy::redundant_closure_for_method_calls,
    non_snake_case
)]
//! Task 6.6.3d, the token-site and containment assertions (UX-T37, ADR-006 section 6), and the default-build
//! half of Task 6.6.3e: explorer deletion stays off unless the build registers `explorer-selection`.

mod common;
mod explore_rig;

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use disk_cleanup_cli::explore::actions::{ExplorerPorts, RealExplorerActions};
use disk_cleanup_cli::explore::handle::DeleteWiring;
use disk_cleanup_cli::wiring::kinds::explorer_selection_registered;
use disk_cleanup_core::explore::msg::DeleteMsg;
use disk_cleanup_core::explore::ports::ExplorerActions;
use disk_cleanup_core::testing::{
    FakeAuditSink, FakeClock, FakeCommandRunner, FakeFs, FakePlanStore, FakeProbe, FakeRunGuard,
    FakeSpaceMeter,
};
use disk_cleanup_core::{BuildInfo, Dirs, RunRoots};
use explore_rig::Ex;

fn repo_root() -> PathBuf {
    common::repo_root()
}

fn script(args: &[&str]) -> (bool, String) {
    let out = Command::new(repo_root().join("scripts").join(args[0]))
        .args(&args[1..])
        .current_dir(repo_root())
        .output()
        .unwrap();
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

#[test]
fn safety_explorer_apply_should_add_no_second_mint_site_and_keep_tui_and_unlink_checks_green() {
    for args in [
        &["check-token-sites.sh", "--expect-sites", "1"][..],
        &["check-token-sites.sh", "--selftest"][..],
        &["check-tui-deps.sh"][..],
        &["check-no-stray-unlink.sh"][..],
    ] {
        let (ok, text) = script(args);
        assert!(ok, "{args:?} failed:\n{text}");
    }
}

#[test]
fn explorer_code_should_never_name_the_mint_the_token_constructors_or_the_unlink_engine() {
    let dir = repo_root().join("crates/cli/src/explore");
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let text = std::fs::read_to_string(&path).unwrap();
        // The apply may name `ApplyToken` (it receives one); nothing here may build one or delete by hand.
        for word in [
            "grant_apply",
            "ApplyToken::grant",
            "for_tests",
            "unconfined",
            "unlinkat",
            "remove_dir_all",
        ] {
            let code: String = text
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(!code.contains(word), "{} names {word}", path.display());
        }
    }
    // `crates/tui` names none of the engine's types (the explorer reaches the engine only through the port).
    let tui = repo_root().join("crates/tui/src");
    let mut stack = vec![tui];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                let t = std::fs::read_to_string(&p).unwrap();
                for w in ["ApplyToken", "delete_tree", "DirOps", "CommandRunner"] {
                    assert!(!t.contains(w), "{} names {w}", p.display());
                }
            }
        }
    }
}

fn actions_over_fakes(wired: bool) -> (RealExplorerActions, Ex) {
    let ex = Ex::new();
    let ports = ExplorerPorts {
        fs: Arc::new(FakeFs::new()),
        runner: Arc::new(FakeCommandRunner::new()),
        clock: Arc::new(FakeClock::at(explore_rig::NOW)),
        probe: Arc::new(FakeProbe::new()),
        space: Arc::new(FakeSpaceMeter::new(
            vec![FakeSpaceMeter::plain_snapshot("/x", 500, 1000)],
            FakeSpaceMeter::stable(),
        )),
        audit: Arc::new(FakeAuditSink::new()),
        guard: Arc::new(FakeRunGuard::new()),
        plans: Arc::new(FakePlanStore::new()),
        dirs: Dirs {
            home: ex.home.clone(),
            state: ex.dirs.state.clone(),
            cache: ex.dirs.cache.clone(),
            config: ex.dirs.config.clone(),
        },
        targets: Vec::new(),
        build: BuildInfo {
            bin_version: "0.1.0".into(),
            git_commit: "abc".into(),
            dirty: false,
            metascope_rev: "none".into(),
            patched: false,
            metascope_dirty: false,
        },
        roots: RunRoots::default(),
        run_id: "run-1".into(),
        jobs: 1,
    };
    let mut actions = RealExplorerActions::new(Arc::new(ports));
    if wired {
        actions = actions.with_delete_wiring(Arc::new(DeleteWiring {
            base: ex.base_dirs.clone(),
            roots: ex.roots(),
            exe: PathBuf::from("/usr/bin/disk-cleanup"),
        }));
    }
    (actions, ex)
}

#[cfg(not(feature = "gate-d"))]
#[test]
fn safety_default_build_should_refuse_d_and_register_no_explorer_apply() {
    assert!(!explorer_selection_registered());
    for gate in ['A', 'B', 'C'] {
        assert!(!disk_cleanup_cli::wiring::kinds::explorer_selection_registered_at(gate));
    }
    // Even fully wired, a build below gate D refuses the review and the delete, and deletes nothing.
    let (actions, ex) = actions_over_fakes(true);
    let a = ex.tree("app/dist", 1, 100);
    assert!(actions.review(&[a.clone()]).is_err());
    let confirmed = ex.confirmed(&[a.clone()]);
    let handle = actions.delete(confirmed);
    assert_eq!(
        handle.rx.recv().unwrap(),
        DeleteMsg::Refused("Deleting is not available in this build. Nothing was deleted.".into())
    );
    assert!(a.exists());
    // The default binary lists no kind that can delete from the explorer.
    let out = Command::new(env!("CARGO_BIN_EXE_disk-cleanup"))
        .arg("kinds")
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&out.stdout).contains("explorer-selection"));
}

#[cfg(feature = "gate-d")]
#[test]
fn gate_d_build_should_register_explorer_selection_and_review_the_marks() {
    assert!(explorer_selection_registered());
    assert!(disk_cleanup_cli::wiring::kinds::GATES.contains(&'D'));
    // Unwired actions still refuse: the wiring is what the explorer's composition root adds.
    let (unwired, ex) = actions_over_fakes(false);
    let a = ex.tree("app/dist", 1, 100);
    assert!(unwired.review(&[a.clone()]).is_err());
    let handle = unwired.delete(ex.confirmed(&[a.clone()]));
    assert!(matches!(handle.rx.recv().unwrap(), DeleteMsg::Refused(_)));
    assert!(a.exists());
}
