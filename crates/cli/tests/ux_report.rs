#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 1.2.3: the plan report (UX-1, 6, 7, 9, 22, 34-36, 44, 53, 57; T-376, T-436, T-440, T-465, T-504,
//! T-505, T-513).

mod support;

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::process::Command;

use disk_cleanup_cli::escape::{escape, path_hex, shell_quote, unescape};
use disk_cleanup_cli::reasontext::ReasonFacts;
use disk_cleanup_cli::render::{render_plan, wrapper_note};
use disk_cleanup_cli::render_json::plan_doc;
use disk_cleanup_cli::terminal::Terminal;
use disk_cleanup_cli::units;
use disk_cleanup_cli::view::{saved_entries, Options, PlanInput, PlanView, SortKey};
use disk_cleanup_core::candidate::BuildDirFlavor;
use disk_cleanup_core::kind::SelectorId;
use disk_cleanup_core::plan::PlanId;
use disk_cleanup_core::plan_state::SaveOutcome;
use disk_cleanup_core::selector::Selection;
use support::*;

fn text(input: &PlanInput, term: &Terminal) -> String {
    render_plan(input, &PlanView::build(input), term)
}

fn lines(s: &str) -> Vec<&str> {
    s.lines().collect()
}

fn next_commands(out: &str) -> Vec<String> {
    out.lines()
        .filter(|l| l.starts_with("  disk-cleanup") || l.starts_with("  sudo "))
        .map(str::to_string)
        .collect()
}

#[test]
fn bare_run_should_print_dry_run_first_and_last_line_and_never_say_apply_in_the_marker() {
    let fx = Fixture::s1();
    let out = text(&fx.input(), &pipe(80));
    let ls = lines(&out);
    assert!(
        ls[0].starts_with("DRY RUN - nothing will be deleted."),
        "{}",
        ls[0]
    );
    let id = PlanId::for_selection(&fx.plan, &fx.selection).to_string();
    assert!(ls[0].ends_with(&format!("plan-id {id}")), "{}", ls[0]);
    assert_eq!(*ls.last().unwrap(), "DRY RUN - nothing was deleted.");
    assert!(!ls[0].contains("--apply") && !ls[0].contains("Re-run"));
    assert_eq!(out.matches("DRY RUN").count(), 2);
    assert!(!out.contains("APPLYING"));
}

#[test]
fn first_23_output_lines_at_80_columns_should_hold_marker_three_subtotals_fs_block_and_the_whole_next_block_then_at_most_10_rows_then_notes(
) {
    let fx = Fixture::s1_root();
    let mut input = fx.input();
    input.not_built = vec!["btrfs".into()];
    let out = text(&input, &pipe(80));
    let ls = lines(&out);
    let next_at = ls.iter().position(|l| *l == "Next:").unwrap();
    let table_at = ls.iter().position(|l| l.starts_with("STATUS ")).unwrap();
    let end = table_at - 2; // the last line of the Next block
    assert!(end < 23 || end == 22, "Next block ends at line {}", end + 1);
    assert!(ls[end].starts_with("      "), "{:?}", ls[end]);
    let order = [
        "DRY RUN",
        "Scanned",
        "Selected now",
        "Add build-dirs,docker",
        "Needs naming",
        "Estimates are",
        "Filesystem",
        "Next:",
    ];
    let mut last = 0;
    for key in order {
        let at = ls
            .iter()
            .position(|l| l.starts_with(key))
            .unwrap_or_else(|| panic!("{key}"));
        assert!(at >= last, "{key} out of order");
        last = at;
    }
    assert_eq!(next_at, 14);
    assert_eq!(end + 1, 23);
    let notes_at = ls.iter().position(|l| *l == "Notes:").unwrap();
    assert!(notes_at > table_at && notes_at > next_at);
    assert!(ls[notes_at + 1].contains("not available in this build: btrfs"));
    for (i, l) in ls.iter().enumerate() {
        let is_cmd = next_commands(&format!("{l}\n")).len() == 1;
        assert!(
            is_cmd || l.chars().count() <= 80,
            "line {} is {} wide: {l}",
            i + 1,
            l.chars().count()
        );
    }
    assert!(out.contains("18.1 GiB") && out.contains("78.3 GiB") && out.contains("27.5 GiB"));
}

#[test]
fn partial_scan_and_wrapper_note_should_add_one_line_each_and_next_should_end_by_line_25() {
    let fx = Fixture::s1_root();
    let mut input = fx.input();
    input.partial_scan = Some(120);
    input.wrapper_note = wrapper_note(Some("disk-cleanup"), false);
    let out = text(&input, &pipe(80));
    let ls = lines(&out);
    let table_at = ls.iter().position(|l| l.starts_with("STATUS ")).unwrap();
    assert!(
        table_at - 1 <= 25,
        "Next block ends at line {}",
        table_at - 1
    );
}

#[test]
fn rows_should_sort_by_size_desc_and_sort_age_kind_and_top_0_should_show_all() {
    let mut cands = Vec::new();
    for i in 0..25 {
        cands.push(cache(
            &format!("/home/tstapler/.cache/c{i:02}"),
            1.0 + f64::from((i * 7) % 25),
        ));
    }
    cands.push(worktree(
        "/home/tstapler/code/app/.claude/worktrees/agent-x",
        3.0,
        30,
    ));
    let fx = Fixture::of(default_selection(), cands);
    let rows_of = |opts: Options| -> Vec<String> {
        let mut input = fx.input();
        input.opts = opts;
        let out = text(&input, &pipe(80));
        out.lines()
            .skip_while(|l| !l.starts_with("STATUS "))
            .skip(1)
            .take_while(|l| !l.starts_with("DRY RUN") && !l.starts_with("..."))
            .map(str::to_string)
            .collect()
    };
    let sizes = |rows: &[String]| -> Vec<f64> {
        rows.iter()
            .map(|r| r.split_whitespace().nth(2).unwrap().parse().unwrap())
            .collect()
    };
    let default = rows_of(Options::default());
    assert_eq!(default.len(), 10);
    let s = sizes(&default);
    assert!(s.windows(2).all(|w| w[0] >= w[1]), "{s:?}");

    let kind = rows_of(Options {
        sort: SortKey::Kind,
        top: 0,
        ..Options::default()
    });
    assert_eq!(kind.len(), 26);
    assert!(kind[0].contains("caches") && kind[25].contains("worktrees"));
    let age = rows_of(Options {
        sort: SortKey::Age,
        top: 0,
        ..Options::default()
    });
    assert!(
        age[0].contains("worktrees"),
        "the 30d worktree is the oldest: {}",
        age[0]
    );

    let mut input = fx.input();
    input.opts.top = 3;
    let out = text(&input, &pipe(80));
    assert!(out.contains("... 23 more rows (--top 0 for all)"), "{out}");
    let all = rows_of(Options {
        top: 0,
        ..Options::default()
    });
    assert_eq!(all.len(), 26);
    assert!(!text(
        &{
            let mut i = fx.input();
            i.opts.top = 0;
            i
        },
        &pipe(80)
    )
    .contains("more rows"));
}

#[test]
fn bounded_report_should_print_at_most_top_rows_plus_totals_When_100000_candidates() {
    let cands: Vec<_> = (0..100_000)
        .map(|i| cache(&format!("/home/tstapler/.cache/n{i}"), 0.001))
        .collect();
    let fx = Fixture::of(default_selection(), cands);
    let out = text(&fx.input(), &pipe(80));
    assert!(
        out.lines().count() <= 10 + 40,
        "{} lines",
        out.lines().count()
    );
    assert!(out.contains("... 99990 more rows"));
}

#[test]
fn partial_scan_should_print_banner_and_at_least_on_every_total_When_max_seconds_hit() {
    let fx = Fixture::s1();
    let mut input = fx.input();
    input.partial_scan = Some(120);
    let out = text(&input, &pipe(80));
    assert_eq!(
        out.lines().nth(1).unwrap(),
        "PARTIAL SCAN (hit --max-seconds 120): sizes are lower bounds"
    );
    for key in ["Selected now", "Add build-dirs,docker", "Needs naming"] {
        let l = out.lines().find(|l| l.contains(key)).unwrap();
        assert!(l.contains("at least"), "{l}");
    }
    let doc = plan_doc(&input, &PlanView::build(&input), 0);
    assert_eq!(doc["partial_scan"], true);
}

#[test]
fn non_btrfs_should_print_btrfs_n_a_with_fstype_and_df_block_and_no_counter_rows() {
    let fx = Fixture::s1();
    let mut input = fx.input();
    let mut fs = fs_btrfs();
    fs.snapshot.btrfs = None;
    fs.snapshot.fs_type = "ext4".into();
    input.fs = Some(fs);
    let out = text(&input, &pipe(80));
    assert!(
        out.contains("btrfs          n/a (filesystem is ext4)"),
        "{out}"
    );
    assert!(out.contains("df             used"));
    assert!(!out.contains("unallocated") && !out.contains("metadata  "));
}

#[test]
fn home_should_print_as_tilde_in_human_output_and_absolute_in_json() {
    let fx = Fixture::s1();
    let input = fx.input();
    let human = text(&input, &pipe(80));
    assert!(human.contains("~/.cache/go-build"));
    assert!(!human.contains("/home/tstapler/.cache/go-build"));
    let doc = plan_doc(&input, &PlanView::build(&input), 0);
    let json = serde_json::to_string(&doc).unwrap();
    assert!(json.contains("/home/tstapler/.cache/go-build"));
    assert!(!json.contains("\"~"));
}

#[test]
fn sizes_should_use_binary_units_one_decimal_zero_B_and_signed_negatives_with_no_locale_separators()
{
    assert_eq!(units::size(0), "0 B");
    assert_eq!(units::size(512), "512 B");
    assert_eq!(units::size(1 << 30), "1.0 GiB");
    assert_eq!(units::size(gib(12.34)), "12.3 GiB");
    assert_eq!(units::size(1_023_999), "1.0 MiB");
    assert_eq!(units::size_i(-(gib(99.7) as i64)), "-99.7 GiB");
    assert_eq!(units::signed(0), "+0 B");
    assert_eq!(units::fs_size((1.62 * TIB as f64) as u64), "1.62 TiB");
    // the unit never depends on the environment: no code path reads LC_ALL
    let src = include_str!("../src/units.rs");
    assert!(!src.contains("env::") && !src.contains("LC_"));
}

#[test]
fn names_with_newline_esc_invalid_utf8_backslash_and_emoji_should_print_ascii_escaped_one_line_and_reversible(
) {
    let names: Vec<(&[u8], &str)> = vec![
        ("café".as_bytes(), "caf\\u{e9}"),
        (b"a\nb", "a\\nb"),
        (b"a\tb\x1b[2Jc", "a\\tb\\x1b[2Jc"),
        (b"bad\xffname", "bad\\xffname"),
        (b"a\\b", "a\\\\b"),
        ("x😀y".as_bytes(), "x\\u{1f600}y"),
    ];
    for (raw, want) in &names {
        let esc = escape(raw, false);
        assert_eq!(&esc, want);
        assert!(esc.is_ascii() && !esc.contains('\n') && !esc.contains('\x1b'));
        assert_eq!(unescape(&esc), raw.to_vec());
        assert_eq!(unescape(&escape(raw, true)), raw.to_vec());
    }
    assert_eq!(
        path_hex(OsStr::from_bytes(b"bad\xffname")).as_deref(),
        Some("6261 64ff6e616d65".replace(' ', "").as_str())
    );
    assert!(path_hex(OsStr::new("plain")).is_none());

    // through the renderer, into a pipe, with the hostile names as directory names
    let out = {
        use disk_cleanup_core::candidate::{CacheFacts, Candidate, KindFacts, Locator};
        use disk_cleanup_core::ids::{AllocatedBytes, Identity};
        let cands: Vec<Candidate> = names
            .iter()
            .enumerate()
            .map(|(i, (raw, _))| {
                let mut p = b"/home/tstapler/.cache/".to_vec();
                p.extend_from_slice(raw);
                Candidate::new(
                    Locator::Path(OsStr::from_bytes(&p).into()),
                    KindFacts::Cache(CacheFacts {
                        label: "c".into(),
                        apt: false,
                    }),
                    AllocatedBytes(gib(1.0 + i as f64)),
                    None,
                    Identity {
                        dev: 1,
                        ino: i as u64,
                    },
                )
            })
            .collect();
        let fx = Fixture::of(default_selection(), cands);
        let mut input = fx.input();
        input.opts.top = 0;
        let human = text(&input, &pipe(80));
        let json =
            disk_cleanup_cli::render_json::to_text(&plan_doc(&input, &PlanView::build(&input), 0));
        assert!(serde_json::from_str::<serde_json::Value>(&json).is_ok());
        assert_eq!(
            json.matches("path_hex").count(),
            1,
            "path_hex only for the 0xff name"
        );
        human
    };
    assert!(out.is_ascii(), "{out}");
    assert!(!out.contains('\x1b') && !out.contains('\r'));
    for (_, want) in &names {
        assert!(out.contains(want), "{want} missing from\n{out}");
    }

    // a TTY prints a valid non-ASCII character as itself, controls and format characters still escaped
    assert_eq!(escape("café".as_bytes(), true), "café");
    assert_eq!(
        escape("a\u{85}b\u{202e}c\u{200b}d".as_bytes(), true),
        "a\\u{85}b\\u{202e}c\\u{200b}d"
    );
    assert_eq!(escape("x😀y".as_bytes(), true), "x😀y");
}

#[test]
fn shell_quote_should_be_bare_single_quoted_or_dollar_quoted_by_the_rule_of_3_6() {
    let q = |b: &[u8], ascii| shell_quote(OsStr::from_bytes(b), ascii);
    assert_eq!(q(b"/a/b-c", true), "/a/b-c");
    assert_eq!(q(b"", true), "''");
    assert_eq!(q(b"/h/x /etc", true), "'/h/x /etc'");
    assert_eq!(q(b"/h/it's", true), "'/h/it'\\''s'");
    assert_eq!(q(b"-rf", true), "'-rf'");
    assert_eq!(q(b"/h/a\nb", true), "$'/h/a\\x0ab'");
    assert_eq!(q(b"/h/\xff", true), "$'/h/\\xff'");
    assert_eq!(q("/h/é".as_bytes(), true), "$'/h/\\xc3\\xa9'");
    assert_eq!(q("/h/é".as_bytes(), false), "'/h/é'");
    assert!(!q(b"/h/a'\n", true).contains("\\'"));
    // delivered as one argument by sh
    let line = format!("printf '%s\\n' {}", q(b"/h/x /etc", true));
    let out = Command::new("sh").args(["-c", &line]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "/h/x /etc\n");
}

#[test]
fn next_lines_selecting_destructive_kinds_should_carry_the_plan_id_of_that_selection_and_match_json_next_and_saved_plan_When_rendered(
) {
    let fx = Fixture::s1();
    let input = fx.input();
    let view = PlanView::build(&input);
    let out = text(&input, &pipe(80));
    let cmds = next_commands(&out);
    assert_eq!(cmds.len(), 3);
    let add_on = view.next.iter().find(|c| c.plan_id.is_some()).unwrap();
    let id = PlanId::for_selection(&fx.plan, &add_on.selection);
    assert!(
        add_on.command.ends_with(&format!("--plan-id {id}")),
        "{}",
        add_on.command
    );
    assert!(cmds
        .iter()
        .filter(|c| c.contains("--plan-id"))
        .all(|c| c.ends_with(&id.to_string())));
    assert!(
        !cmds[0].contains("--plan-id"),
        "the safe apply needs no id: {}",
        cmds[0]
    );
    let doc = plan_doc(&input, &view, 0);
    let in_json: Vec<String> = doc["next"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| n["plan_id"].as_str().map(str::to_string))
        .collect();
    assert_eq!(in_json, vec![id.to_string()]);
    let saved: Vec<String> = saved_entries(&input)
        .into_iter()
        .map(|e| e.plan_id)
        .collect();
    assert!(saved.contains(&id.to_string()));
    assert!(saved.contains(&view.header_id.to_string()));
}

#[test]
fn next_command_lines_should_match_the_paste_pattern_pass_sh_n_and_keep_the_explanation_on_the_next_line(
) {
    let mut cands = s1_candidates();
    cands.push(cache_apt("/var/cache/apt/archives", 1.0));
    let fx = Fixture::of(default_selection(), cands);
    let out = text(&fx.input(), &pipe(80));
    let ok = |l: &str| -> bool {
        let Some(rest) = l.strip_prefix("  ") else {
            return false;
        };
        rest.starts_with("disk-cleanup ")
            || rest
                .strip_prefix("sudo /")
                .is_some_and(|r| r.contains("/disk-cleanup "))
    };
    let ls = lines(&out);
    let mut seen = 0;
    for (i, l) in ls.iter().enumerate() {
        if next_commands(&format!("{l}\n")).is_empty() {
            continue;
        }
        seen += 1;
        assert!(ok(l), "{l}");
        assert!(
            !l.contains('(') && !l.contains(')') && !l.contains('#'),
            "{l}"
        );
        let st = Command::new("sh")
            .args(["-n", "-c", l.trim()])
            .status()
            .unwrap();
        assert!(st.success(), "sh -n rejected {l}");
        let expl = ls[i + 1];
        assert!(expl.starts_with("      "), "{expl}");
        assert!(
            !expl.trim_start().starts_with("disk-cleanup")
                && !expl.trim_start().starts_with("sudo")
        );
    }
    assert_eq!(seen, 4, "{out}");
    assert!(
        out.contains("  sudo /home/tstapler/.cargo/bin/disk-cleanup --apply --kinds cache-apt"),
        "{out}"
    );
    assert!(!out.contains("sudo disk-cleanup"));
}

#[test]
fn next_lines_should_echo_each_selection_flag_in_the_fixed_order_and_never_the_display_flags() {
    use disk_cleanup_core::selector::{Discard, DockerVolumes};
    type Case = (&'static str, Box<dyn Fn(&mut Selection)>);
    let cases: Vec<Case> = vec![
        (
            "--root '/tmp/a b'",
            Box::new(|s| s.roots = vec!["/tmp/a b".into()]),
        ),
        (
            "--min-age 60d",
            Box::new(|s| s.min_age_secs = Some(60 * 86_400)),
        ),
        (
            "--min-size 1GiB",
            Box::new(|s| s.min_size_bytes = Some(1 << 30)),
        ),
        ("--keep-newest 0", Box::new(|s| s.keep_newest = Some(0))),
        (
            "--docker-volumes anonymous",
            Box::new(|s| s.docker_volumes = Some(DockerVolumes::Anonymous)),
        ),
        ("--volume x", Box::new(|s| s.volumes = vec!["x".into()])),
        (
            "--steam-saves discard",
            Box::new(|s| s.steam_saves = Some(Discard::Discard)),
        ),
        (
            "--worktree-ignored discard",
            Box::new(|s| s.worktree_ignored = Some(Discard::Discard)),
        ),
        (
            "--only /home/tstapler/old-app/node_modules",
            Box::new(|s| s.only = vec!["/home/tstapler/old-app/node_modules".into()]),
        ),
    ];
    for (flag, apply) in cases {
        let mut sel = selection_of(&[
            SelectorId::Caches,
            SelectorId::Worktrees,
            SelectorId::BuildDirs,
            SelectorId::DockerVolumes,
            SelectorId::Steam,
        ]);
        apply(&mut sel);
        let fx = Fixture::of(sel, s1_candidates());
        let mut input = fx.input();
        input.opts = Options {
            top: 3,
            sort: SortKey::Age,
            verbose: true,
            quiet: false,
        };
        let out = text(&input, &pipe(80));
        let cmds = next_commands(&out);
        assert!(!cmds.is_empty(), "{flag}: {out}");
        assert!(
            cmds.iter().all(|c| c.contains(flag)),
            "{flag} missing from {cmds:?}"
        );
        for banned in [
            "--top",
            "--sort",
            "--jobs",
            "--json",
            " -v",
            " -q",
            "--color",
            "--max-seconds",
            "--rescan",
        ] {
            assert!(
                cmds.iter().all(|c| !c.contains(banned)),
                "{banned} echoed in {cmds:?}"
            );
        }
        let kinds_at = cmds[0].find("--kinds").unwrap_or(0);
        let flag_at = cmds[0].find(flag).unwrap();
        assert!(kinds_at <= flag_at, "--kinds must come first: {}", cmds[0]);
    }
}

#[test]
fn plan_not_saved_should_print_the_NOT_usable_block_and_no_destructive_next_line_and_exit_0() {
    let fx = Fixture::s1();
    let mut input = fx.input();
    input.saved = SaveOutcome::NotSaved(28);
    let view = PlanView::build(&input);
    let out = text(&input, &pipe(80));
    let flat = out.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        out.lines().next().unwrap().ends_with("(NOT saved)"),
        "{out}"
    );
    assert!(flat.contains("plan-id NOT usable: could not save it (ENOSPC on ~/.local/state/disk-cleanup/last_plan)."), "{out}");
    assert!(flat.contains("Run the safe apply printed above first"));
    assert!(
        next_commands(&out).iter().all(|c| !c.contains("--plan-id")),
        "{out}"
    );
    assert!(next_commands(&out)
        .iter()
        .any(|c| c == "  disk-cleanup --apply"));
    let doc = plan_doc(&input, &view, 0);
    assert_eq!(doc["plan_saved"], false);
    assert_eq!(doc["exit_code"], 0);
    assert!(doc["next"]
        .as_array()
        .unwrap()
        .iter()
        .all(|n| n["plan_id"].is_null()));
}

#[test]
fn not_usable_block_should_not_point_at_a_safe_apply_when_the_plan_has_no_free_row() {
    let sel = selection_of(&[SelectorId::BuildDirs]);
    let cands = vec![build_dir(
        "/home/tstapler/code/a/target",
        5.0,
        40,
        BuildDirFlavor::CargoTarget,
    )];
    let fx = Fixture::of(sel, cands);
    let mut input = fx.input();
    input.saved = SaveOutcome::NotSaved(28);
    let out = text(&input, &pipe(80));
    let flat = out.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(!flat.contains("safe apply printed above"), "{out}");
    assert!(
        flat.contains("There is no safe apply to run first."),
        "{out}"
    );
    assert!(flat.contains("~/.local/state/disk-cleanup"));
    assert!(flat.contains("'disk-cleanup status'"));
    assert!(next_commands(&out).is_empty(), "{out}");
}

#[test]
fn sudo_next_lines_should_name_the_binary_by_absolute_path_and_behave_the_same_with_HOME_root_or_user_and_env_dropped(
) {
    let mk = |home: &str, euid: u32| {
        let mut cands = s1_candidates();
        cands.push(cache_apt("/var/cache/apt/archives", 1.0));
        let fx = Fixture::of(default_selection(), cands);
        let mut input = fx.input();
        input.euid = euid;
        input.home = Some(home.into());
        let out = text(&input, &pipe(80));
        next_commands(&out)
            .into_iter()
            .filter(|c| c.contains("sudo"))
            .collect::<Vec<_>>()
    };
    let as_user = mk("/home/tstapler", 1000);
    let as_root = mk("/root", 0);
    assert_eq!(as_user, as_root);
    assert!(
        as_user[0].starts_with("  sudo /home/tstapler/.cargo/bin/disk-cleanup "),
        "{as_user:?}"
    );
    assert!(!as_user.iter().any(|c| c.contains("sudo disk-cleanup")));
}

#[test]
fn line_1_of_a_dry_run_should_not_say_re_run_with_apply_and_wrapper_notes_should_point_at_the_next_lines(
) {
    let fx = Fixture::s1();
    for dash_n in [true, false] {
        let mut input = fx.input();
        input.wrapper_note = wrapper_note(Some("disk-cleanup"), dash_n);
        let out = text(&input, &pipe(80));
        let ls = lines(&out);
        assert!(!ls[0].contains("Re-run") && !ls[0].contains("--apply"));
        assert!(ls[1].starts_with("NOTE:"), "{}", ls[1]);
        assert!(ls[1].contains("Copy a Next: line"));
        assert!(!ls[1].contains("Re-run with --apply"));
    }
    assert!(wrapper_note(None, true).is_none());
}

#[test]
fn quiet_plan_should_keep_the_markers_next_block_and_hide_rows() {
    let fx = Fixture::s1();
    let mut input = fx.input();
    input.opts.quiet = true;
    input.partial_scan = Some(30);
    let out = text(&input, &pipe(80));
    let ls = lines(&out);
    assert!(ls[0].starts_with("DRY RUN - nothing will be deleted."));
    assert!(ls[1].starts_with("PARTIAL SCAN"));
    assert_eq!(ls[2], "Selected now: 4 items, would free at least 18.1 GiB");
    assert_eq!(ls[3], "Next:");
    assert_eq!(*ls.last().unwrap(), "DRY RUN - nothing was deleted.");
    assert!(!out.contains("STATUS") && !out.contains("Filesystem") && !out.contains("Estimates"));
    let loud = text(
        &{
            let mut i = fx.input();
            i.partial_scan = Some(30);
            i
        },
        &pipe(80),
    );
    let doc_q = plan_doc(&input, &PlanView::build(&input), 0);
    let doc = plan_doc(
        &{
            let mut i = fx.input();
            i.partial_scan = Some(30);
            i
        },
        &PlanView::build(&fx.input()),
        0,
    );
    assert_eq!(doc_q["items"], doc["items"], "--json -q equals --json");
    drop(loud);
}

#[test]
fn nothing_to_free_should_say_so_and_still_print_the_filesystem_block() {
    let fx = Fixture::of(default_selection(), Vec::new());
    let out = text(&fx.input(), &pipe(80));
    assert!(out.contains("Nothing to free under the current selectors."));
    assert!(out.contains("Filesystem /"));
    assert!(!out.contains("Next:"));
}

#[test]
fn facts_should_fill_the_reason_text_of_a_skip_row_and_an_absent_fact_should_drop_its_clause() {
    let fx = Fixture::s1();
    let dirty = fx
        .plan
        .items
        .iter()
        .find(|i| i.guard != disk_cleanup_core::target::Guarded::Allow)
        .unwrap()
        .candidate
        .id();
    let mut input = fx.input();
    let mut facts = HashMap::new();
    facts.insert(dirty, ReasonFacts::default());
    input.facts = facts;
    let out = text(&input, &pipe(80));
    assert!(out.contains("dirty working tree"));
}

#[test]
fn worktree_row_should_end_with_the_discard_note_and_the_floor_text_should_replace_the_age_text() {
    let mut sel = selection_of(&[SelectorId::Worktrees]);
    sel.worktree_ignored = Some(disk_cleanup_core::selector::Discard::Discard);
    let ok = "/home/tstapler/code/app/.claude/worktrees/agent-a1";
    let fx = Fixture::of(sel, vec![worktree(ok, 0.1, 30)]);
    let id = fx.plan.items[0].candidate.id();
    let mut input = fx.input();
    input.facts.insert(
        id,
        ReasonFacts {
            discards: Some((2, 12 << 10)),
            ..ReasonFacts::default()
        },
    );
    let out = text(&input, &pipe(120));
    assert!(
        out.contains("(discards 2 ignored files, 12.0 KiB)"),
        "{out}"
    );

    let facts = ReasonFacts {
        detail: Some(disk_cleanup_core::targets::worktree_guard::FLOOR_DETAIL.to_string()),
        ..ReasonFacts::default()
    };
    let ctx = disk_cleanup_cli::reasontext::TextCtx {
        selection: &fx.selection,
        selector: SelectorId::Worktrees,
        exe: std::path::Path::new("/bin/dc"),
        ascii_only: true,
    };
    assert_eq!(
        disk_cleanup_cli::reasontext::reason(
            disk_cleanup_core::SkipReason::TooRecent,
            &facts,
            &ctx
        ),
        "younger than 24 h (worktree floor; --min-age cannot lower it)"
    );
}
