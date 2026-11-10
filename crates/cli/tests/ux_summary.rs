#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 1.2.3: the apply screens (UX-2, 13, 15, 44, 48, 56; T-032, T-440, T-461).

mod support;

use disk_cleanup_cli::apply::{render_apply, render_refused, LastDryRun};
use disk_cleanup_core::kind::{Kind, SelectorId};
use disk_cleanup_core::outcome::{SkipReason, Status};
use disk_cleanup_core::plan::PlanId;
use disk_cleanup_core::summary::DiskVerdict;
use support::*;

fn apply_text(fx: &RunFx, exit: i32) -> String {
    render_apply(&fx.input(exit), &pipe(80))
}

#[test]
fn summary_should_open_with_bytes_freed_and_df_avail_and_list_the_three_largest_unselected_risk_rows_when_the_verdict_is_warn_or_crit(
) {
    let out = apply_text(&RunFx::s3(), 1);
    let ls: Vec<&str> = out.lines().collect();
    let at = ls.iter().position(|l| l.starts_with("Freed")).unwrap();
    assert_eq!(
        ls[at],
        "Freed 72.6 GiB in 5 items.  df avail is now 145.6 GiB (was 70.2 GiB)."
    );
    assert_eq!(
        ls[at + 1],
        "1 item skipped by guard, 1 error.  Exit code 1 (partial failure)."
    );
    assert!(
        out.contains(
            "Disk still WARN (unallocated 9.8 GiB). The largest RISK rows this run did not select:"
        ),
        "{out}"
    );
    let block: Vec<&str> = ls
        .iter()
        .skip_while(|l| !l.starts_with("Disk still"))
        .skip(1)
        .take(3)
        .copied()
        .collect();
    assert!(
        block[0].contains("16.5 GiB") && block[0].contains("steam"),
        "{block:?}"
    );
    assert!(block[1].contains("11.0 GiB") && block[1].contains("docker-volumes"));
    assert!(block[2].contains("5.0 GiB") && block[2].contains("docker"));
    assert!(
        out.contains(
            "      disk-cleanup --kinds docker,docker-volumes,steam --docker-volumes anonymous"
        ),
        "{out}"
    );
    let after = out.split("Disk still").nth(1).unwrap();
    assert!(
        !after.contains("--apply"),
        "the block never offers an apply command: {after}"
    );

    // WARN with none of them
    let mut o = s3_outcomes();
    o.retain(|x| x.status != Status::Skipped(SkipReason::KindNotSelected));
    let none = apply_text(&RunFx::with(o.clone(), DiskVerdict::Warn), 1);
    let flat = none.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains(
            "No unselected RISK rows remain. Run 'disk-cleanup status' for the filesystem state."
        ),
        "{none}"
    );
    // OK: the block is absent
    let ok = apply_text(&RunFx::with(s3_outcomes(), DiskVerdict::Ok), 1);
    assert!(!ok.contains("Disk still") && !ok.contains("No unselected"));
    // nothing attributed
    let mut nothing = vec![outcome(
        Kind::Cache,
        SelectorId::Caches,
        Some("/home/tstapler/.cache/x"),
        Status::Skipped(SkipReason::Gone),
        1.0,
    )];
    nothing.truncate(1);
    let zero = apply_text(&RunFx::with(nothing, DiskVerdict::Ok), 0);
    assert!(
        zero.lines().any(|l| l.starts_with("Freed 0 B in 0 items.")),
        "{zero}"
    );
}

#[test]
fn closing_line_should_be_printed_exactly_when_L1_was_printed_so_lock_audit_and_starvation_refusals_show_two_stdout_lines(
) {
    let id = PlanId::from_hex("91be04d7").unwrap();
    for exit in [2, 3] {
        let out = render_refused(id, None, exit, &pipe(80));
        let ls: Vec<&str> = out.lines().collect();
        assert_eq!(
            ls,
            vec![
                "APPLYING - deleting. plan-id 91be04d7".to_string(),
                format!("APPLY FINISHED - exit {exit}.")
            ]
        );
    }
    let with_warning = render_refused(
        id,
        Some("WARNING: gate check bypassed (disk full)"),
        2,
        &pipe(80),
    );
    assert_eq!(with_warning.lines().count(), 3);
    assert_eq!(
        with_warning.lines().nth(1).unwrap(),
        "WARNING: gate check bypassed (disk full)"
    );

    let fx = RunFx::s3();
    for exit in [0, 1, 2, 3, 130, 143] {
        let out = apply_text(&fx, exit);
        let ls: Vec<&str> = out.lines().collect();
        assert_eq!(ls[0], "APPLYING - deleting. plan-id 91be04d7");
        assert_eq!(
            *ls.last().unwrap(),
            format!("APPLY FINISHED - exit {exit}.")
        );
        assert_eq!(out.matches("APPLYING").count(), 1);
        assert_eq!(out.matches("APPLY FINISHED").count(), 1);
        assert!(!out.contains("DRY RUN"));
        assert!(ls.iter().any(|l| l.contains("Attribution ")), "{exit}");
        assert!(
            ls.iter()
                .any(|l| l.trim_start().starts_with("Unattributed remainder +")
                    || l.trim_start().starts_with("Unattributed remainder -")),
            "{exit}"
        );
    }
}

#[test]
fn apply_header_should_print_in_order_warning_audit_selected_and_last_dry_run() {
    let fx = RunFx::s3();
    let mut input = fx.input(1);
    input.gate_bypass = Some("WARNING: gate check bypassed (no room to build)".into());
    let out = render_apply(&input, &pipe(80));
    let ls: Vec<&str> = out.lines().collect();
    assert!(ls[1].starts_with("WARNING: gate check bypassed"));
    assert_eq!(ls[2], "audit log: ~/.local/state/disk-cleanup/audit.jsonl");
    assert_eq!(
        ls[3],
        "Selected: caches, worktrees, build-dirs   (7 items, est. 96.4 GiB)"
    );
    assert_eq!(ls[4], "Last dry run: matches.");
    input.last_dry_run = Some(LastDryRun::Differs("4be012aa".into()));
    assert!(render_apply(&input, &pipe(80))
        .contains("Last dry run: differs (it was plan-id 4be012aa)."));
    input.last_dry_run = None;
    assert!(!render_apply(&input, &pipe(80)).contains("Last dry run"));
}

#[test]
fn eacces_item_should_print_one_ERR_row_and_an_errors_section_with_the_ls_command_and_no_ownership_command(
) {
    let out = apply_text(&RunFx::s3(), 1);
    assert_eq!(
        out.lines().filter(|l| l.starts_with("ERR")).count(),
        1,
        "{out}"
    );
    let at = out.lines().position(|l| l == "Errors:").unwrap();
    let ls: Vec<&str> = out.lines().collect();
    assert!(
        ls[at + 1].starts_with("  ~/old-app/node_modules   permission denied (EACCES)."),
        "{}",
        ls[at + 1]
    );
    assert!(out.contains("      ls -ld -- /home/tstapler/old-app/node_modules"));
    assert!(!out.contains("chown") && !out.contains("chmod"));
    let clean = apply_text(
        &RunFx::with(vec![s3_outcomes().remove(0)], DiskVerdict::Ok),
        0,
    );
    assert!(!clean.contains("Errors:"));
}

#[test]
fn errors_section_should_quote_a_path_with_a_space_and_deliver_it_as_one_argument() {
    let mut o = s3_outcomes();
    o.push(outcome(
        Kind::NodeModules,
        SelectorId::BuildDirs,
        Some("/h/x /etc"),
        Status::Error("permission denied (EACCES)".into()),
        1.0,
    ));
    let out = apply_text(&RunFx::with(o, DiskVerdict::Ok), 1);
    let cmd = out
        .lines()
        .find(|l| l.contains("ls -ld -- '/h/x /etc'"))
        .expect(&out);
    let run = cmd.trim().replacen("ls -ld --", "printf '%s\\n'", 1);
    let got = std::process::Command::new("sh")
        .args(["-c", &run])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&got.stdout), "/h/x /etc\n");
}

#[test]
fn quiet_apply_should_keep_the_markers_attribution_lines_errors_and_guard_skips() {
    let fx = RunFx::s3();
    let mut input = fx.input(1);
    input.quiet = true;
    let out = render_apply(&input, &pipe(80));
    let ls: Vec<&str> = out.lines().collect();
    assert_eq!(ls[0], "APPLYING - deleting. plan-id 91be04d7");
    assert_eq!(*ls.last().unwrap(), "APPLY FINISHED - exit 1.");
    assert!(ls.iter().any(|l| l.starts_with("Selected:")));
    assert!(ls.iter().any(|l| l.starts_with("Freed 72.6 GiB")));
    assert!(ls.iter().any(|l| l.starts_with("ERR")));
    assert!(ls.iter().any(|l| l.starts_with("SKIP")));
    assert!(!ls.iter().any(|l| l.starts_with("FREED")));
    assert!(ls.iter().any(|l| l.starts_with("Measured")));
    assert!(ls.iter().any(|l| l.starts_with("Attribution")));
    assert!(ls.iter().any(|l| l.starts_with("Unattributed remainder")));
    assert!(ls.contains(&"Errors:"));
    assert!(!out.contains("Filesystem /"));
}

#[test]
fn spent_plan_id_should_be_named_after_a_destructive_apply_that_removed_items_and_not_otherwise() {
    let fx = RunFx::s3();
    for exit in [130, 1] {
        let out = apply_text(&fx, exit);
        let ls: Vec<&str> = out.lines().collect();
        let n = ls.len();
        assert!(ls[n - 2].starts_with("plan-id 91be04d7 is spent (5 items removed). Dry run again, then use the new plan-id: disk-cleanup --kinds build-dirs,caches,worktrees"), "{}", ls[n - 2]);
        assert_eq!(ls[n - 1], format!("APPLY FINISHED - exit {exit}."));
    }
    let mut safe = RunFx::s3();
    safe.selection = selection_of(&[SelectorId::Caches, SelectorId::Worktrees]);
    assert!(!apply_text(&safe, 130).contains("is spent"));
    let none = RunFx::with(vec![s3_outcomes().remove(5)], DiskVerdict::Ok);
    assert!(!apply_text(&none, 130).contains("is spent"));
}

#[test]
fn interrupted_apply_should_say_how_many_items_were_not_attempted() {
    let fx = RunFx::s3();
    let mut input = fx.input(130);
    input.not_attempted = Some(4);
    assert!(render_apply(&input, &pipe(80)).contains("INTERRUPTED: 4 items not attempted"));
}

#[test]
fn apply_rows_should_never_exceed_the_width_and_never_print_a_risk_row() {
    for w in [20, 39, 59, 80, 120] {
        let out = render_apply(&RunFx::s3().input(1), &pipe(w));
        for l in out.lines().filter(|l| {
            !l.trim_start().starts_with("sudo")
                && !l.trim_start().starts_with("disk-cleanup")
                && !l.contains("ls -ld")
                && !l.starts_with("Disk still")
                && !l.starts_with("plan-id")
        }) {
            assert!(
                l.chars().count() <= w.max(32) || l.starts_with("  "),
                "{w}: {l}"
            );
        }
        let rows = out.split("Freed ").next().unwrap();
        assert!(!rows.lines().any(|l| l.starts_with("RISK")), "{rows}");
    }
}
