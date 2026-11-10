#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! `KindGate`: whole-kind availability as a typed value (Story 1.1.4, UX-17).

mod common;

use common::{docker_cache, run, selection, Rig};
use disk_cleanup_core::candidate::Candidate;
use disk_cleanup_core::delete_types::{DeleteError, DeleteFailure, Freed};
use disk_cleanup_core::executor::ExitClass;
use disk_cleanup_core::plan::Planned;
use disk_cleanup_core::ports::{Cmd, Env};
use disk_cleanup_core::selector::Selection;
use disk_cleanup_core::target::{Guarded, KindGate, Target};
use disk_cleanup_core::{ApplyToken, Platform, SelectorId, SkipReason};

/// Asks the runner whether the daemon answers, the way a real Docker target will.
struct DockerLike {
    items: Vec<Candidate>,
    untested: bool,
}

impl Target for DockerLike {
    fn name(&self) -> &'static str {
        "docker"
    }

    fn selectors(&self) -> &'static [SelectorId] {
        &[SelectorId::Docker]
    }

    fn gate(&self, env: &Env) -> KindGate {
        match env.runner.run(&Cmd::new("docker", ["info"], false)) {
            Err(_) => KindGate::Unavailable(SkipReason::DaemonUnavailable),
            Ok(_) if self.untested => KindGate::ListOnly(SkipReason::DockerVersionUntested),
            Ok(_) => KindGate::Available,
        }
    }

    fn unavailable_detail(&self, env: &Env) -> Option<String> {
        match env.runner.run(&Cmd::new("docker", ["info"], false)) {
            Err(DeleteError::CommandFailed { message, .. }) => Some(message),
            _ => None,
        }
    }

    fn discover(&self, _env: &Env, _sel: &Selection) -> Vec<Candidate> {
        self.items.clone()
    }

    fn guard(&self, _c: &Candidate, _env: &Env) -> Guarded {
        Guarded::Allow
    }

    fn execute(&self, _c: &Candidate, _env: &Env, _p: &ApplyToken) -> Result<Freed, DeleteFailure> {
        panic!("a gated target must never execute")
    }
}

fn daemon_down(rig: &Rig) {
    rig.runner.on(
        &["docker", "info"],
        Err(DeleteError::CommandFailed {
            status: 1,
            message: "Cannot connect to the Docker daemon".into(),
        }),
    );
}

#[test]
fn kind_gate_should_carry_typed_reasons_and_the_kinds_available_column_should_derive_from_it_When_daemon_down_or_context_remote(
) {
    let rig = Rig::new();
    daemon_down(&rig);
    let t = DockerLike {
        items: vec![docker_cache(1, 1 << 20)],
        untested: false,
    };
    let sel = selection(&[SelectorId::Docker]);

    // plan mode: the s3.5 row text once, exit 0, nothing listed from the down kind
    let r = run(&rig, &[&t], &sel, false, 1, None).unwrap();
    assert_eq!(
        r.notes,
        ["docker: SKIPPED (daemon not reachable: Cannot connect to the Docker daemon)"]
    );
    assert_eq!(r.exit, ExitClass::Ok);
    assert!(r.plan.items.is_empty());

    // the AVAILABLE column derives from the typed gate
    let env = rig.env();
    let g = t.gate(&env);
    assert_eq!(g, KindGate::Unavailable(SkipReason::DaemonUnavailable));
    assert_eq!(g.available_label(Platform::Linux), "daemon down");
    assert_eq!(
        KindGate::Unavailable(SkipReason::NeedsRoot).available_label(Platform::Linux),
        "needs root"
    );
    assert_eq!(
        KindGate::Unavailable(SkipReason::UnsupportedOnThisOS).available_label(Platform::Macos),
        "not supported on macOS"
    );
    for reason in [
        SkipReason::NonLocalDockerContext,
        SkipReason::UnsupportedOnThisOS,
        SkipReason::UnrecognisedOutput,
        SkipReason::LibraryUnmounted,
    ] {
        assert!(KindGate::Unavailable(reason).is_unavailable());
    }

    // apply with only the down kind selected ends with exit 1 before any gate
    let e = run(&rig, &[&t], &sel, true, 1, None).unwrap_err();
    assert_eq!(e.exit, ExitClass::PartialFailure);
    assert_eq!(
        e.notes,
        ["docker: could not run (daemon not reachable: Cannot connect to the Docker daemon)"]
    );
}

#[test]
fn a_list_only_gate_should_show_the_items_without_making_them_removable() {
    let rig = Rig::new();
    rig.runner.on(
        &["docker", "info"],
        Ok(disk_cleanup_core::ports::CmdOutput::default()),
    );
    let t = DockerLike {
        items: vec![docker_cache(1, 1 << 20)],
        untested: true,
    };
    let sel = selection(&[SelectorId::Docker]);
    let r = run(&rig, &[&t], &sel, true, 1, None).unwrap();
    assert_eq!(r.plan.items.len(), 1);
    assert_eq!(
        r.plan.items[0].planned,
        Planned::WillNot(SkipReason::DockerVersionUntested)
    );
    assert_eq!(r.summary.attributed_bytes, 0);
    assert_eq!(r.exit, ExitClass::Ok);
}
