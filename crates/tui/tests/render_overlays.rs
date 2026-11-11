#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The overlays (Task 6.5.3b; UX-T4, UX-T17, UX-T19): info pane, `e` list, plan preview, quit prompt, help,
//! the too-small message and the huge-directory footer.

mod common;

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use common::*;
use disk_cleanup_core::explore::input::Mode;
use disk_cleanup_core::explore::msg::{NodeEvent, PlanPreview, PreviewRow, ScanMode, NO_PARENT};
use disk_cleanup_core::explore::rows::{Sort, SortKey};
use disk_cleanup_core::explore::tree::ExploreTree;
use disk_cleanup_core::ports::SharedEstimate;
use disk_cleanup_tui::overlays::{errno_words, UnreadableEntry};

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn info_fixture() -> Fixture {
    let mut f = Fixture::new(sample_tree(true));
    f.state.input.show_info = true;
    f
}

#[test]
fn the_info_pane_shows_every_fact_wrapped_and_nothing_cut_at_every_width() {
    for (w, h) in [(120u16, 24u16), (80, 24), (60, 14), (60, 30)] {
        let f = info_fixture();
        let mut s = f.screen();
        s.kind_line = Some("build-dirs (destructive tier): the cleaner lists it".into());
        s.shared = SharedEstimate::Estimated {
            bytes: GIB + GIB / 5,
            files_probed: 10,
            unreadable: 0,
            probed_usage: 38,
            dir_usage: 100,
            partial: false,
        }
        .pane_text();
        s.owner = Some((1000, 1000));
        let text = draw_text(&s, w, h);
        insta::assert_snapshot!(format!("overlay_info_{w}x{h}"), text);
        assert_fits(&text, usize::from(w));
        let flat = collapse(&text);
        for needle in [
            "-- Info: /home/u/code/app/node_modules",
            "Kind build-dirs (destructive tier): the cleaner lists it",
            "Usage 21.7 GiB on disk Apparent 24.9 GiB Metadata 21.7 MiB (est.)",
            "Shared est. 1.2 GiB of the 10 largest files (38% of usage) also referenced elsewhere; a delete may free less",
            "Items 11 (files 10, dirs 0) Share of parent 65%",
            "(newest directory mtime below)",
            "uid 1000 gid 1000",
            "Scan complete, 0 unreadable",
            "Guards not checked here; p runs them",
        ] {
            // the pane may wrap a phrase across lines; compare with whitespace collapsed
            assert!(flat.contains(&collapse(needle)), "{w}x{h}: missing {needle:?} in\n{text}");
        }
    }
}

#[test]
fn below_20_rows_the_pane_replaces_the_list_and_the_age_says_directory_mtime() {
    let f = info_fixture();
    let text = draw_text(&f.screen(), 80, 19);
    assert!(!text.contains("target/"), "the list is replaced: {text}");
    assert!(
        text.contains("Age     63d ago (newest directory mtime below)"),
        "{text}"
    );
    let list = draw_text(&f.screen(), 80, 30);
    assert!(
        list.contains("target/") && list.contains("-- Info:"),
        "above 20 rows the pane shares the screen"
    );
}

#[test]
fn the_rootless_estimate_is_said_when_the_probe_has_no_answer() {
    let f = info_fixture();
    let text = draw_text(&f.screen(), 100, 24);
    assert!(text.contains("rootless estimate"), "{text}");
}

#[test]
fn the_e_overlay_lists_each_unreadable_path_with_its_reason_ls_and_the_no_sudo_sentence() {
    let mut f = Fixture::new(sample_tree(true));
    f.state.input.mode = Mode::Errors;
    let entries = vec![
        UnreadableEntry {
            path: PathBuf::from("/var/lib/docker"),
            errno: Some(13),
        },
        UnreadableEntry {
            path: PathBuf::from("/root"),
            errno: None,
        },
        UnreadableEntry {
            path: PathBuf::from(OsString::from_vec(b"/x/a\nb\x1b[31m".to_vec())),
            errno: Some(1),
        },
    ];
    let mut s = f.screen();
    s.unreadable = &entries;
    let text = draw_text(&s, 100, 24);
    assert!(text.contains("3 unreadable"));
    assert!(
        text.contains("EACCES (Permission denied): /var/lib/docker"),
        "{text}"
    );
    assert!(text.contains("reason not recorded: /root"));
    assert!(
        text.contains("EPERM (Operation not permitted): /x/a\\nb\\x1b[31m"),
        "{text}"
    );
    assert!(
        text.contains("  ls -ld -- /var/lib/docker\n"),
        "each command on its own line: {text}"
    );
    assert!(text.contains("The tool never retries with sudo."));
    assert!(
        !text.contains("chown"),
        "no ownership command is ever printed"
    );
    assert!(!text.contains('\x1b'));
    assert_eq!(errno_words(9999), "errno 9999");
    assert_fits(&text, 100);
}

#[test]
fn the_ls_line_delivers_each_hostile_path_as_one_argument_in_bash_and_zsh() {
    use disk_cleanup_core::shell_quote::sandbox::run_printed_line;
    let names: Vec<Vec<u8>> = vec![
        b"/x/plain".to_vec(),
        b"/x/a\nb".to_vec(),
        b"/x/a\tb\x1b[2Jc".to_vec(),
        b"/x/$(touch INJECTED)".to_vec(),
        b"/x/a'; touch INJECTED; '".to_vec(),
        b"/x/bad\xffbyte".to_vec(),
        "/x/a\u{9b}31m".as_bytes().to_vec(),
        "/x/a\u{9d}0;t\x07".as_bytes().to_vec(),
        "/x/x\u{202e}gpj".as_bytes().to_vec(),
        "/x/zw\u{200b}".as_bytes().to_vec(),
        "/x/\u{4e2d}\u{6587}\u{1f4a5}".as_bytes().to_vec(),
        b"/x/-rf".to_vec(),
    ];
    for tty in [true, false] {
        for name in &names {
            let mut f = Fixture::new(sample_tree(true));
            f.state.input.mode = Mode::Errors;
            let entries = vec![UnreadableEntry {
                path: PathBuf::from(OsString::from_vec(name.clone())),
                errno: Some(13),
            }];
            let mut s = f.screen();
            s.tty = tty;
            s.unreadable = &entries;
            let text = draw_text(&s, 200, 24);
            let line = text
                .lines()
                .find(|l| l.trim_start().starts_with("ls -ld -- "))
                .unwrap_or_else(|| panic!("no ls line for {name:?}: {text}"))
                .trim();
            for shell in ["bash", "zsh"] {
                let dir = std::env::temp_dir().join(format!(
                    "tui-ls-{}-{shell}-{}",
                    std::process::id(),
                    names.iter().position(|n| n == name).unwrap() * 2 + usize::from(tty)
                ));
                let _ = std::fs::remove_dir_all(&dir);
                std::fs::create_dir_all(&dir).unwrap();
                let run = run_printed_line(&dir, shell, line).unwrap().unwrap();
                assert_eq!(
                    run.argv_of("ls").map(<[Vec<u8>]>::to_vec),
                    Some(vec![b"-ld".to_vec(), b"--".to_vec(), name.clone()]),
                    "{shell} tty={tty}: {line:?}"
                );
                assert!(
                    run.injected.is_empty() && !dir.join("INJECTED").exists(),
                    "{shell}: {line:?}"
                );
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
    }
}

fn plan(saved: bool, rows: usize) -> PlanPreview {
    PlanPreview {
        plan_id: "5d2e77a1".into(),
        saved,
        rows: (0..rows)
            .map(|i| PreviewRow {
                path: PathBuf::from(format!("/home/u/code/app/node_modules{i}")),
                kind: "build-dirs".into(),
                est_bytes: 21 * GIB / 10,
                status: "FREE".into(),
            })
            .collect(),
        est_bytes: 22 * GIB,
        scope_line: "2 marked items, tier destructive, explorer-selection".into(),
        guards_line:
            "Guards run now: inside ~/code, not a mount point, not a symlink, not own state.".into(),
    }
}

#[test]
fn the_plan_preview_carries_its_title_the_d_line_and_no_next_line() {
    let f = Fixture::new(sample_tree(true));
    let p = plan(true, 2);
    let mut s = f.screen();
    s.plan = Some(&p);
    let text = draw_text(&s, 80, 24);
    insta::assert_snapshot!("overlay_plan_preview_80x24", text);
    assert!(text.starts_with("PLAN PREVIEW - nothing is deleted.   plan-id 5d2e77a1 (saved)"));
    assert!(text.contains("d      delete these items (asks you to type the count)"));
    assert!(text.contains("Esc    back to the list (marks are kept)"));
    assert!(!text.contains("Next:"), "a wrapped command would not paste");
    assert!(!text.contains("DRY RUN") && !text.contains("APPLYING"));
}

#[test]
fn a_plan_that_could_not_be_saved_shows_the_plan_id_not_usable_block() {
    let f = Fixture::new(sample_tree(true));
    let p = plan(false, 1);
    let mut s = f.screen();
    s.plan = Some(&p);
    let text = draw_text(&s, 80, 24);
    assert!(text.contains("plan-id NOT usable"), "{text}");
    assert!(text.contains("(NOT saved)"));
    assert_fits(&text, 80);
}

#[test]
fn a_long_plan_keeps_the_key_lines_and_counts_the_rows_it_could_not_show() {
    let f = Fixture::new(sample_tree(true));
    let p = plan(true, 40);
    let mut s = f.screen();
    s.plan = Some(&p);
    let text = draw_text(&s, 80, 24);
    assert!(text.contains("more items"), "{text}");
    assert!(text.contains("d      delete these items"));
    assert_eq!(text.lines().count(), 24);
}

#[test]
fn the_quit_prompt_is_the_footer_line_of_the_wireframe() {
    let mut f = Fixture::new(sample_tree(true));
    f.mark("/home/u/code/app/node_modules", 217 * GIB / 10);
    f.mark("/home/u/code/app/target", 9 * GIB);
    f.mark("/home/u/code/app/dist", 80 * MIB);
    f.state.input.mode = Mode::QuitPrompt;
    let text = draw_text(&f.screen(), 120, 24);
    assert!(
        text.contains("3 marked (30.8 GiB).  Enter: show the plan and leave   x: forget marks and leave   Esc: stay"),
        "{text}"
    );
}

#[test]
fn the_help_overlay_replaces_the_screen_and_fits() {
    let mut f = Fixture::new(sample_tree(true));
    f.state.input.mode = Mode::Help;
    let text = draw_text(&f.screen(), 80, 40);
    assert!(text.starts_with("EXPLORE HELP - nothing is deleted until you confirm."));
    assert!(text.contains("D, Delete and y do nothing") || text.contains("Deleting is permanent"));
    assert_fits(&text, 80);
}

#[test]
fn the_too_small_message_is_centred_and_a_zero_by_zero_terminal_does_not_panic() {
    let f = Fixture::new(sample_tree(true));
    let text = draw_text(&f.screen(), 40, 10);
    assert!(text.contains("Too small: need 60x12, have 40x10."));
    assert!(text.contains("or run: disk-cleanup explore --text"));
    for (w, h) in [(1u16, 1u16), (60, 11), (59, 30), (2, 2)] {
        let _ = draw_text(&f.screen(), w, h);
    }
}

#[test]
fn a_huge_directory_says_that_name_sort_covers_the_kept_entries() {
    let mut ev: Vec<NodeEvent> = vec![dir_ev(
        0,
        NO_PARENT,
        "/big",
        stats(0, 5_100, 0, 0),
        5_100,
        0,
    )];
    for i in 0..5_100u64 {
        ev.push(dir_ev(
            1 + i,
            0,
            &format!("part-{i:05}"),
            stats(1, 0, 4096 + i, 10),
            0,
            0,
        ));
    }
    let mut t = ExploreTree::new();
    t.apply_batch(0, ScanMode::Fresh, ev).unwrap();
    t.finish(false);
    let mut f = Fixture::new(t);
    {
        let Fixture { tree, state, .. } = &mut f;
        state.view.press_sort(SortKey::Name, tree, 40_000, &|_| {
            disk_cleanup_core::explore::rows::Badge::None
        });
    }
    assert_eq!(f.state.view.sort(), Sort::new(SortKey::Name));
    let text = draw_text(&f.screen(), 100, 24);
    assert!(
        text.contains("name sort covers the 5000 largest entries"),
        "{text}"
    );
    let last = f.state.view.rows().last().unwrap();
    assert!(
        last.display_name().starts_with("(+") && last.display_name().ends_with(" smaller)"),
        "the folded row closes the list: {}",
        last.display_name()
    );
}

#[test]
fn the_boundary_row_footer_prints_a_pasteable_explore_command_only_when_it_fits() {
    let ev = vec![
        dir_ev(0, NO_PARENT, "/r", stats(0, 1, 4096, 0), 1, 0),
        NodeEvent::Boundary {
            id: 1,
            parent: 0,
            name: "mnt".into(),
            dev: 9,
        },
    ];
    let mut t = ExploreTree::new();
    t.apply_batch(0, ScanMode::Fresh, ev).unwrap();
    let mut f = Fixture::new(t);
    f.state.view.move_cursor(1, 1);
    let wide = draw_text(&f.screen(), 120, 24);
    assert!(
        wide.contains("From the shell: disk-cleanup explore /r/mnt"),
        "{wide}"
    );
    let narrow = draw_text(&f.screen(), 60, 24);
    assert!(
        narrow.contains("mnt/ is another filesystem: Enter scans it."),
        "{narrow}"
    );
    assert!(
        !narrow.contains("disk-cleanup explore"),
        "a cut command is never printed: {narrow}"
    );
}
