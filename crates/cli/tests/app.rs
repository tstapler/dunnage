#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 1.2.4, fake-driven acceptance criteria: the real parser and the real dispatch over fake ports
//! (apply-path ACs are executor tests at this wave; the spawned-binary counterparts are in `cli.rs`).

mod rig;
mod support;

use dunnage_cli::args;
use dunnage_core::candidate::BuildDirFlavor;
use dunnage_core::delete_types::{DeleteError, DeleteFailure, Freed};
use dunnage_core::kind::{Risk, SelectorId};
use dunnage_core::plan_state::{ItemKey, SavedSelection};
use dunnage_core::snapshot_types::{AllocSnapshot, SpaceUse};
use dunnage_core::testing::{AuditCall, FakePlanStore, FakeSpaceMeter};
use rig::{executed_total, plan_id_of, run, FakeTarget, Rig, NOW};
use support::{
    btrfs_balance, build_dir, cache, docker_dangling, docker_volumes, steam, worktree, GIB,
};

const CACHES: &[SelectorId] = &[SelectorId::Caches];
const WORKTREES: &[SelectorId] = &[SelectorId::Worktrees];
const BUILD_DIRS: &[SelectorId] = &[SelectorId::BuildDirs];
const DOCKER: &[SelectorId] = &[SelectorId::Docker];
const DOCKER_VOLUMES: &[SelectorId] = &[SelectorId::DockerVolumes];
const STEAM: &[SelectorId] = &[SelectorId::Steam];
const BTRFS: &[SelectorId] = &[SelectorId::Btrfs];

fn caches() -> FakeTarget {
    FakeTarget::new(CACHES, vec![cache("/home/u/.npm/_cacache", 1.0)])
}

fn build_dirs() -> FakeTarget {
    FakeTarget::new(
        BUILD_DIRS,
        vec![build_dir(
            "/home/u/code/a/node_modules",
            2.0,
            40,
            BuildDirFlavor::NodeModules,
        )],
    )
}

fn worktrees() -> FakeTarget {
    FakeTarget::new(
        WORKTREES,
        vec![worktree(
            "/home/u/code/app/.claude/worktrees/agent-1",
            1.0,
            30,
        )],
    )
}

fn no_scan_line(s: &str) -> String {
    s.lines()
        .filter(|l| !l.starts_with("Scanned"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn bare_run_should_be_a_dry_run_that_touches_nothing_When_a_cache_exists() {
    let rig = Rig::new();
    let c = caches();
    let r = run(&rig, &[&c], &[], true);
    assert_eq!(r.code, 0, "{r:?}");
    assert!(r.stdout.contains("DRY RUN"), "{}", r.stdout);
    assert!(c.executed().is_empty());
    assert!(
        rig.audit.calls().is_empty(),
        "plan mode writes no audit line"
    );
    assert_eq!(rig.guard.acquire_calls(), 0, "plan mode takes no run lock");
    assert_eq!(rig.plans.save_calls().len(), 1, "plan mode saves last_plan");
    assert_eq!(rig.runner.mutating_calls(), 0);
}

#[test]
fn dry_run_flags_alone_should_give_the_bare_runs_output() {
    let rig = Rig::new();
    let c = caches();
    let bare = run(&rig, &[&c], &[], true);
    for flag in ["-n", "--dry-run"] {
        let r = run(&rig, &[&c], &[flag], true);
        assert_eq!(r.code, 0, "{flag}");
        assert_eq!(
            no_scan_line(&r.stdout),
            no_scan_line(&bare.stdout),
            "{flag}"
        );
    }
}

#[test]
fn force_yes_and_y_should_exit_64_with_empty_stdout_and_the_tip() {
    let rig = Rig::new();
    for flag in ["--force", "-y", "--yes"] {
        let r = run(&rig, &[], &[flag], true);
        assert_eq!(r.code, 64, "{flag}");
        assert!(r.stdout.is_empty(), "{flag}: {}", r.stdout);
        assert!(
            r.stderr
                .contains("Tip: there is no force or yes flag; use --kinds."),
            "{flag}: {}",
            r.stderr
        );
    }
}

#[test]
fn kinds_all_and_plus_kind_should_exit_64_with_the_catalogue_text() {
    let rig = Rig::new();
    let r = run(&rig, &[], &["--kinds", "all"], true);
    assert_eq!(r.code, 64);
    assert!(r.stdout.is_empty());
    assert!(
        r.stderr
            .contains("'all' is not accepted: name the kinds you mean"),
        "{}",
        r.stderr
    );
    let r = run(&rig, &[], &["--kinds", "+build-dirs"], true);
    assert_eq!(r.code, 64);
    assert!(
        r.stderr
            .contains("'+build-dirs' is not accepted: --kinds replaces the default set"),
        "{}",
        r.stderr
    );
}

#[test]
fn a_kind_flag_without_its_kind_should_exit_64_with_the_only_applies_to_text_before_any_other_check(
) {
    let rig = Rig::new();
    let cases: [&[&str]; 4] = [
        &["--docker-volumes", "anonymous", "--kinds", "docker"],
        &["--steam-saves", "discard", "--kinds", "caches"],
        &["--worktree-ignored", "discard", "--kinds", "caches"],
        &["--keep-newest", "2", "--kinds", "caches"],
    ];
    for argv in cases {
        // No target registered at all: the kind-bound check still comes first.
        let r = run(&rig, &[], argv, true);
        assert_eq!(r.code, 64, "{argv:?}");
        assert!(r.stdout.is_empty());
        assert!(
            r.stderr.contains(" only applies to "),
            "{argv:?}: {}",
            r.stderr
        );
        assert!(r.stderr.contains("Nothing was deleted."));
    }
}

#[test]
fn dry_run_flags_with_apply_should_exit_64_with_the_contradiction_text_and_touch_nothing() {
    let rig = Rig::new();
    let c = caches();
    let combos: [&[&str]; 10] = [
        &["-n", "--apply"],
        &["--apply", "-n"],
        &["--dry-run", "--apply"],
        &["--apply", "--dry-run"],
        &["-n", "--apply", "--kinds", "caches"],
        &["--apply", "-n", "--kinds", "caches"],
        &["--dry-run", "--apply", "--kinds", "caches"],
        &["--apply", "--dry-run", "--kinds", "caches"],
        &["--kinds", "caches", "--apply", "-n"],
        &["--kinds", "caches", "--dry-run", "--apply"],
    ];
    for argv in combos {
        let r = run(&rig, &[&c], argv, true);
        assert_eq!(r.code, 64, "{argv:?}");
        assert!(r.stdout.is_empty(), "{argv:?}");
        assert_eq!(
            r.stderr,
            "error: -n/--dry-run and --apply contradict each other; drop one. Dry run is the default, so plain 'dunnage' is the dry run.\n",
            "{argv:?}"
        );
    }
    assert!(rig.audit.calls().is_empty());
    assert!(c.executed().is_empty());
}

#[test]
fn the_clap_definition_should_declare_the_apply_conflict() {
    let cmd = args::command();
    for (arg, other) in [("apply", "dry_run"), ("dry_run", "apply")] {
        let a = cmd.get_arguments().find(|a| a.get_id() == arg).unwrap();
        let conflicts: Vec<&str> = cmd
            .get_arg_conflicts_with(a)
            .iter()
            .map(|a| a.get_id().as_str())
            .collect();
        assert!(conflicts.contains(&other), "{arg}: {conflicts:?}");
    }
}

#[test]
fn help_should_say_in_its_first_line_that_nothing_is_deleted_without_apply_and_document_every_flag()
{
    let r = run(&Rig::new(), &[], &["--help"], true);
    assert_eq!(r.code, 0);
    let first = r.stdout.lines().next().unwrap();
    assert!(
        first.contains("nothing is deleted without --apply"),
        "{first}"
    );
    let h = &r.stdout;
    for flag in [
        "--apply",
        "--plan-id <ID>",
        "--kinds",
        "--except",
        "--root <PATH>",
        "--min-age",
        "--min-size",
        "--keep-newest",
        "--jobs <N>",
        "--docker-volumes",
        "--volume <NAME>",
        "--steam-saves",
        "--worktree-ignored",
        "--only <PATH>",
        "--top <N>",
        "--sort",
        "--max-seconds",
        "--rescan",
        "--no-progress",
        "--color",
        "--json",
        "-v, --verbose",
        "-q, --quiet",
        "-n, --dry-run",
        "status",
        "kinds",
        "audit",
        "steam",
    ] {
        assert!(h.contains(flag), "--help lacks {flag}:\n{h}");
    }
    for hidden in ["--stale-days", "--no-color"] {
        assert!(!h.contains(hidden), "{hidden} is a hidden alias");
    }
    assert!(h.contains(
        "0 ok, 1 partial failure (status: warn), 2 fatal (status: crit; audit verify: violations)"
    ));
    assert!(h.contains("3 btrfs starved, 64 usage, 74 status/audit verify could not read state"));
    assert!(h.contains("130 interrupted (SIGINT), 143 (SIGTERM)"));
    assert!(h.contains("Environment:"));
    assert!(h.contains("NO_COLOR") && !h.contains("UNGATED") && !h.contains("BYPASS"));
    assert!(h.contains("made in the last 24h"));
}

#[test]
fn hidden_aliases_and_plan_id_case_should_parse_and_quiet_with_verbose_should_exit_64() {
    let rig = Rig::new();
    let c = caches();
    for argv in [&["--stale-days", "3"][..], &["--no-color"][..]] {
        assert_eq!(run(&rig, &[&c], argv, true).code, 0, "{argv:?}");
    }
    match args::parse(["--apply", "--kinds", "build-dirs", "--plan-id", "91BE04D7"]) {
        args::Parsed::Run(cli) => {
            assert_eq!(cli.plan_id.unwrap().plan_id().to_string(), "91be04d7");
        }
        other @ args::Parsed::Exit { .. } => panic!("{other:?}"),
    }
    let r = run(&rig, &[&c], &["-q", "-v"], true);
    assert_eq!(r.code, 64);
    assert!(r.stdout.is_empty());
}

#[test]
fn a_malformed_plan_id_should_exit_64_before_every_other_check_with_empty_stdout() {
    let rig = Rig::new();
    let b = build_dirs();
    for id in ["91be04", "91be04d7a", "zzzzzzzz", "0x7f3a91c", ""] {
        let r = run(
            &rig,
            &[&b],
            &["--apply", "--kinds", "build-dirs", "--plan-id", id],
            true,
        );
        assert_eq!(r.code, 64, "{id:?}");
        assert!(r.stdout.is_empty(), "{id:?}");
        assert!(
            r.stderr.contains("a plan-id is 8 hex digits"),
            "{id:?}: {}",
            r.stderr
        );
    }
    assert!(rig.audit.calls().is_empty());
    assert_eq!(executed_total(&[&b]), 0);
}

#[test]
fn every_guarded_or_destructive_kind_not_named_should_be_a_risk_row_and_stay_untouched_When_apply()
{
    let registry: Vec<(SelectorId, FakeTarget)> = vec![
        (SelectorId::BuildDirs, build_dirs()),
        (
            SelectorId::Docker,
            FakeTarget::new(DOCKER, vec![docker_dangling(1.0)]),
        ),
        (
            SelectorId::DockerVolumes,
            FakeTarget::new(DOCKER_VOLUMES, vec![docker_volumes(1.0)]),
        ),
        (
            SelectorId::Steam,
            FakeTarget::new(
                STEAM,
                vec![steam("/mnt/lib/steamapps/compatdata/440", 1.0, 40)],
            ),
        ),
        (
            SelectorId::Btrfs,
            FakeTarget::new(BTRFS, vec![btrfs_balance()]),
        ),
    ];
    for (id, target) in &registry {
        assert_ne!(id.tier(), Risk::Safe, "{} is not a risky kind", id.id());
        let rig = Rig::new();
        let c = caches();
        let r = run(&rig, &[&c, target], &["--apply", "--json"], true);
        assert_eq!(r.code, 0, "{}: {r:?}", id.id());
        // The live apply table omits RISK rows (S2); the document lists every one as data.
        let doc: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
        let risky: Vec<&serde_json::Value> = doc["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|i| i["status"] == "risk_not_selected")
            .collect();
        assert_eq!(risky.len(), 1, "{}: {}", id.id(), r.stdout);
        assert_eq!(risky[0]["reason"], "kind-not-selected");
        assert_eq!(risky[0]["selector"], id.id());
        assert!(target.executed().is_empty(), "{} was touched", id.id());
        assert_eq!(c.executed().len(), 1, "the safe kind ran");
        let intents = rig
            .audit
            .calls()
            .iter()
            .filter(|c| **c == AuditCall::Intent)
            .count();
        assert_eq!(intents, 1, "{}: only the cache was logged", id.id());
    }
}

fn saved(id: &str, kinds: &str, age_secs: i64, items: &[&str]) -> SavedSelection {
    SavedSelection {
        plan_id: id.into(),
        saved_at: NOW - age_secs,
        euid: 1000,
        kinds: kinds.into(),
        flags: String::new(),
        items: items.iter().map(|k| ItemKey::new(k)).collect(),
        items_truncated: false,
    }
}

const GONE: &str = "node-modules:/home/u/code/gone/node_modules";
const NOW_KEY: &str = "node-modules:/home/u/code/a/node_modules";

fn rig_with(entry: SavedSelection) -> Rig {
    let mut r = Rig::new();
    r.plans = FakePlanStore::with_existing(vec![entry]);
    r
}

#[test]
fn a_destructive_apply_should_exit_64_without_stdout_or_effects_unless_the_reviewed_plan_id_is_given(
) {
    let b = build_dirs();
    let refusals: Vec<(Rig, Vec<&str>, &str)> = vec![
        (
            Rig::new(),
            vec![],
            "error: destructive kinds (build-dirs) need --plan-id from a dry run made in the last 24h.",
        ),
        (
            Rig::new(),
            vec!["--plan-id", "0badc0de"],
            "error: no dry run with plan-id 0badc0de in the last 24h",
        ),
        (
            rig_with(saved("91be04d7", "build-dirs", 25 * 3600, &[NOW_KEY])),
            vec!["--plan-id", "91be04d7"],
            "plan-id 91be04d7",
        ),
        (
            rig_with(saved("91be04d7", "build-dirs", 60, &[GONE])),
            vec!["--plan-id", "91be04d7"],
            "error: plan-id 91be04d7 does not match the plan just computed",
        ),
    ];
    for (rig, extra, text) in refusals {
        let mut argv = vec!["--apply", "--kinds", "build-dirs"];
        argv.extend(extra.iter());
        let r = run(&rig, &[&b], &argv, true);
        assert_eq!(r.code, 64, "{argv:?}: {r:?}");
        assert!(r.stdout.is_empty(), "{argv:?}: {}", r.stdout);
        assert!(r.stderr.contains(text), "{argv:?}: {}", r.stderr);
        assert!(r.stderr.contains("Nothing was deleted."));
        assert!(rig.audit.calls().is_empty(), "{argv:?}");
        assert_eq!(rig.guard.acquire_calls(), 0);
        assert_eq!(executed_total(&[&b]), 0);
    }
}

#[test]
fn a_destructive_apply_should_be_accepted_with_the_id_of_a_dry_run_of_the_same_selection() {
    let rig = Rig::new();
    let b = build_dirs();
    let id = plan_id_of(&rig, &[&b], &["--kinds", "build-dirs"]);
    let r = run(
        &rig,
        &[&b],
        &["--apply", "--kinds", "build-dirs", "--plan-id", &id],
        true,
    );
    assert_eq!(r.code, 0, "{r:?}");
    assert!(r.stdout.contains("APPLYING - deleting."), "{}", r.stdout);
    assert_eq!(b.executed().len(), 1);
}

#[test]
fn worktree_ignored_discard_and_the_default_sets_should_follow_the_same_rule() {
    let rig = Rig::new();
    let (w, c) = (worktrees(), caches());
    let r = run(
        &rig,
        &[&w, &c],
        &[
            "--apply",
            "--kinds",
            "worktrees",
            "--worktree-ignored",
            "discard",
        ],
        true,
    );
    assert_eq!(r.code, 64, "{r:?}");
    assert!(r.stdout.is_empty());
    assert!(r.stderr.contains("--plan-id"), "{}", r.stderr);
    assert_eq!(executed_total(&[&w, &c]), 0);
    for argv in [
        &["--apply"][..],
        &["--apply", "--kinds", "caches,worktrees"][..],
    ] {
        let r = run(&rig, &[&w, &c], argv, true);
        assert_eq!(r.code, 0, "{argv:?}: {r:?}");
    }
}

#[test]
fn a_gate_refusal_under_json_should_be_one_document_with_the_code_and_detail_and_empty_without_json(
) {
    let rig = rig_with(saved("91be04d7", "build-dirs", 60, &[GONE]));
    let b = build_dirs();
    let argv = [
        "--apply",
        "--json",
        "--kinds",
        "build-dirs",
        "--plan-id",
        "91be04d7",
    ];
    let r = run(&rig, &[&b], &argv, true);
    assert_eq!(r.code, 64);
    assert!(r.stderr.contains("does not match the plan just computed"));
    let d: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(d["mode"], "apply");
    assert_eq!(d["exit_code"], 64);
    assert_eq!(d["errors"][0]["code"], "plan_id_changed");
    let detail = &d["errors"][0]["detail"];
    for k in [
        "supplied",
        "recomputed",
        "vanished",
        "appeared",
        "incomplete",
        "recent",
        "dry_run_command",
    ] {
        assert!(detail.get(k).is_some(), "detail lacks {k}: {detail}");
    }
    assert_eq!(detail["vanished"][0], GONE);
    assert_eq!(detail["appeared"][0], NOW_KEY);
    let without: Vec<&str> = argv.iter().copied().filter(|a| *a != "--json").collect();
    let plain = run(&rig, &[&b], &without, true);
    assert_eq!(plain.code, 64);
    assert!(plain.stdout.is_empty());
    assert!(rig.audit.calls().is_empty());
}

#[test]
fn apply_should_be_refused_with_the_gate_a_text_When_the_build_has_no_apply_path() {
    let rig = Rig::new();
    let c = caches();
    let r = run(&rig, &[&c], &["--apply"], false);
    assert_eq!(r.code, 64);
    assert!(r.stdout.is_empty());
    assert!(r.stderr.contains("apply is not enabled in this build"));
    assert!(c.executed().is_empty() && rig.audit.calls().is_empty());
}

fn many_caches(n: u64) -> Vec<dunnage_core::Candidate> {
    (0..n)
        .map(|i| {
            let c = cache(&format!("/home/u/.cache/c{i}"), 1.0);
            dunnage_core::Candidate::new(
                c.locator().clone(),
                c.facts().clone(),
                c.est_bytes(),
                None,
                dunnage_core::Identity {
                    dev: 1,
                    ino: 100 + i,
                },
            )
        })
        .collect()
}

#[test]
fn exit_code_should_be_1_for_an_eacces_item_2_for_enospc_and_signal_plus_128_When_interrupted() {
    let mut eacces = FakeTarget::new(CACHES, many_caches(5));
    eacces.exec = Box::new(|c| {
        if c.locator() == &dunnage_core::Locator::Path("/home/u/.cache/c2".into()) {
            Err(DeleteFailure {
                error: DeleteError::Io(13),
                freed: Freed::default(),
            })
        } else {
            Ok(rig::freed(c.est_bytes().0))
        }
    });
    let r = run(&Rig::new(), &[&eacces], &["--apply"], true);
    assert_eq!(r.code, 1, "{r:?}");
    assert!(
        r.stdout.contains("APPLY FINISHED - exit 1."),
        "{}",
        r.stdout
    );

    let mut enospc = FakeTarget::new(CACHES, many_caches(5));
    enospc.exec = Box::new(|_| {
        Err(DeleteFailure {
            error: DeleteError::NoSpace,
            freed: Freed::default(),
        })
    });
    let r = run(&Rig::new(), &[&enospc], &["--apply", "--jobs", "1"], true);
    assert_eq!(r.code, 2, "{r:?}");

    for (sig, code) in [(15, 143), (2, 130)] {
        let rig = Rig::new();
        rig.guard.interrupt(sig);
        let c = caches();
        let r = run(&rig, &[&c], &["--apply"], true);
        assert_eq!(r.code, code, "signal {sig}: {r:?}");
        assert!(
            c.executed().is_empty(),
            "an interrupted run starts no delete"
        );
    }
    let rig = Rig::new();
    rig.guard.interrupt(15);
    let r = run(&rig, &[&caches()], &[], true);
    assert_eq!(r.code, 143);
    assert!(r
        .stderr
        .contains("INTERRUPTED during scan. Nothing was deleted; no report produced."));
    assert!(r.stdout.is_empty());
}

#[test]
fn clap_usage_errors_should_map_to_64_and_help_flags_to_0() {
    let rig = Rig::new();
    for argv in [
        &["--no-such-flag"][..],
        &["foo"],
        &["--top", "x"],
        &["status", "--nope"],
    ] {
        let r = run(&rig, &[], argv, true);
        assert_eq!(r.code, 64, "{argv:?}");
        assert!(r.stdout.is_empty());
    }
    assert_eq!(run(&rig, &[], &["--help"], true).code, 0);
    assert_eq!(run(&rig, &[], &["status", "--help"], true).code, 0);
}

fn btrfs_snapshot(unallocated_gib: f64) -> dunnage_core::FsSnapshot {
    let space = |used: u64, total: u64| SpaceUse {
        bytes_used: used,
        total_bytes: total,
    };
    let mut s = FakeSpaceMeter::plain_snapshot("/", 1000 * GIB, 2000 * GIB);
    s.fs_type = "btrfs".into();
    s.btrfs = Some(AllocSnapshot {
        data: space(900 * GIB, 950 * GIB),
        metadata: space(10 * GIB, 16 * GIB),
        system: space(1 << 20, 8 << 20),
        unallocated: (unallocated_gib * GIB as f64) as u64,
    });
    s
}

fn status_rig(snap: dunnage_core::FsSnapshot) -> Rig {
    let mut rig = Rig::new();
    rig.space = FakeSpaceMeter::new(vec![snap], FakeSpaceMeter::stable());
    rig
}

#[test]
fn status_should_exit_1_for_warn_2_for_crit_0_for_ok_and_write_nothing_but_the_outcome_line() {
    for (gib, code, word) in [(9.8, 1, "WARN"), (2.0, 2, "CRIT"), (50.0, 0, "OK")] {
        let rig = status_rig(btrfs_snapshot(gib));
        let r = run(&rig, &[], &["status"], true);
        assert_eq!(r.code, code, "{gib} GiB: {r:?}");
        assert!(
            r.stdout.contains(&format!("verdict: {word}")),
            "{}",
            r.stdout
        );
        assert!(r.stdout.starts_with("Filesystem /  (btrfs)"));
        assert!(rig.audit.calls().is_empty());
        assert_eq!(rig.guard.acquire_calls(), 0);
        assert!(rig.plans.save_calls().is_empty());
        assert_eq!(
            rig.outcomes.lock().unwrap().len(),
            1,
            "one outcome line per run"
        );
    }
}

#[test]
fn status_should_exit_74_with_the_catalogue_text_and_an_unknown_verdict_under_json_When_the_state_is_unreadable(
) {
    let mut rig = Rig::new();
    rig.space =
        FakeSpaceMeter::new(vec![btrfs_snapshot(50.0)], FakeSpaceMeter::stable()).failing(5);
    let r = run(&rig, &[], &["status"], true);
    assert_eq!(r.code, 74);
    assert!(r.stdout.is_empty());
    assert!(
        r.stderr
            .starts_with("error: cannot read filesystem state for /: I/O error (errno 5)."),
        "{}",
        r.stderr
    );
    let r = run(&rig, &[], &["status", "--json"], true);
    assert_eq!(r.code, 74);
    let d: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(d["verdict"], "unknown");
    assert_eq!(d["errors"][0]["code"], "state_unreadable");
    assert_eq!(d["exit_code"], 74);
    assert!(
        rig.outcomes.lock().unwrap().is_empty(),
        "nothing was read, nothing is recorded"
    );
}

#[test]
fn status_should_print_n_a_for_a_non_btrfs_path_and_for_fields_that_need_root_and_exit_64_for_a_missing_path(
) {
    let rig = status_rig(FakeSpaceMeter::plain_snapshot("/", 100 * GIB, 1000 * GIB));
    let r = run(&rig, &[], &["status"], true);
    assert_eq!(r.code, 0);
    assert!(
        r.stdout
            .lines()
            .any(|l| l.starts_with("  btrfs") && l.ends_with("n/a (filesystem is ext4)")),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("used "), "the df block is still printed");

    let mut needs_root = FakeSpaceMeter::plain_snapshot("/", 100 * GIB, 1000 * GIB);
    needs_root.fs_type = "btrfs".into();
    let r = run(&status_rig(needs_root), &[], &["status"], true);
    assert_eq!(r.code, 0, "{r:?}");
    assert!(r.stdout.contains("n/a (needs root)"), "{}", r.stdout);
    assert!(
        r.stdout.contains("used 100"),
        "df block printed: {}",
        r.stdout
    );

    let r = run(&rig, &[], &["status", "/definitely/not/here"], true);
    assert_eq!(r.code, 64);
    assert!(r.stdout.is_empty());
}

#[test]
fn kinds_should_print_the_table_with_availability_and_exit_0_and_json_should_return_the_same_rows()
{
    let rig = Rig::new();
    let c = caches();
    let r = run(&rig, &[&c], &["kinds"], true);
    assert_eq!(r.code, 0);
    let first = r.stdout.lines().next().unwrap();
    assert!(first.starts_with("ID") && first.contains("AVAILABLE") && first.contains("MIN-AGE"));
    let line = |id: &str| {
        r.stdout
            .lines()
            .find(|l| l.split_whitespace().next() == Some(id))
            .unwrap()
            .to_string()
    };
    assert!(line("caches").contains(" yes "));
    assert!(line("build-dirs").contains("not available in this build"));
    let j = run(&rig, &[&c], &["kinds", "--json"], true);
    let d: serde_json::Value = serde_json::from_str(&j.stdout).unwrap();
    assert_eq!(d["mode"], "kinds");
    assert_eq!(d["kinds"].as_array().unwrap().len(), SelectorId::ALL.len());
    assert_eq!(d["exit_code"], 0);
}

const WALK_FLAGS: [&str; 4] = ["--max-seconds", "3600", "--rescan", "--no-cache"];

#[test]
fn walk_flags_should_leave_the_plan_output_and_id_unchanged_When_the_deadline_is_not_hit() {
    let rig = Rig::new();
    let b = build_dirs();
    let kinds = ["--kinds", "build-dirs"];
    let bare = run(&rig, &[&b], &kinds, true);
    let flagged_args: Vec<&str> = kinds.iter().chain(&WALK_FLAGS).copied().collect();
    let flagged = run(&rig, &[&b], &flagged_args, true);
    assert_eq!(flagged.code, 0, "{flagged:?}");
    assert_eq!(no_scan_line(&flagged.stdout), no_scan_line(&bare.stdout));
    assert!(!flagged.stdout.contains("PARTIAL SCAN"));
    assert_eq!(
        plan_id_of(&rig, &[&b], &flagged_args),
        plan_id_of(&rig, &[&b], &kinds),
        "the walk flags are not part of the plan-id"
    );
}

#[test]
fn a_plan_should_print_the_partial_scan_banner_and_exit_0_When_a_walking_kind_hit_max_seconds() {
    let rig = Rig::new();
    let mut b = build_dirs();
    b.partial = Some(5);
    let r = run(
        &rig,
        &[&b],
        &["--kinds", "build-dirs", "--max-seconds", "5"],
        true,
    );
    assert_eq!(r.code, 0, "{r:?}");
    assert!(
        r.stdout
            .contains("PARTIAL SCAN (hit --max-seconds 5): sizes are lower bounds"),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("at least"), "{}", r.stdout);
    let j = run(
        &rig,
        &[&b],
        &["--kinds", "build-dirs", "--max-seconds", "5", "--json"],
        true,
    );
    let doc: serde_json::Value = serde_json::from_str(&j.stdout).unwrap();
    assert_eq!(doc["partial_scan"], true);
}

#[test]
fn an_apply_should_warn_that_the_scan_was_partial_When_a_walking_kind_hit_max_seconds() {
    let rig = Rig::new();
    let mut b = build_dirs();
    b.partial = Some(5);
    let id = plan_id_of(&rig, &[&b], &["--kinds", "build-dirs"]);
    let r = run(
        &rig,
        &[&b],
        &[
            "--apply",
            "--kinds",
            "build-dirs",
            "--plan-id",
            &id,
            "--max-seconds",
            "5",
        ],
        true,
    );
    assert_eq!(r.code, 0, "{r:?}");
    assert!(
        r.stderr
            .contains("WARNING: PARTIAL SCAN (hit --max-seconds 5)"),
        "{}",
        r.stderr
    );
}

#[test]
fn the_selection_should_carry_max_seconds_rescan_and_no_cache_When_the_flags_are_given() {
    let parse = |argv: &[&str]| match args::parse(argv.iter().copied()) {
        args::Parsed::Run(c) => c,
        other @ args::Parsed::Exit { .. } => panic!("{other:?}"),
    };
    let kinds = dunnage_core::selector::KindSelector::default();
    let none = parse(&[]).selection(&[], &kinds).unwrap();
    assert_eq!(none.scan, dunnage_core::selector::ScanControl::default());
    let all = parse(&WALK_FLAGS).selection(&[], &kinds).unwrap();
    assert_eq!(
        all.scan,
        dunnage_core::selector::ScanControl {
            max_seconds: Some(3600),
            rescan: true,
            no_cache: true
        }
    );
}
