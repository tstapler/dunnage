#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Marks, the quit prompt and the help overlay (Story 6.2.4, Task 6.2.4b; validation UX-T3, UX-T5, UX-T29).

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use disk_cleanup_core::explore::help::{help_lines, DELETE_RULE, DELETE_STEPS, SCREEN_READER};
use disk_cleanup_core::explore::input::{
    handle_key, Action, ExploreState, KeyEnv, MarkQuery, Outcome,
};
use disk_cleanup_core::explore::keymap::{Key, KEYMAP};
use disk_cleanup_core::explore::layout::{cells, Tier};
use disk_cleanup_core::explore::mark_check::MarkOk;
use disk_cleanup_core::explore::marks::{Mark, Marks, MARK_LIMIT};
use disk_cleanup_core::explore::msg::{MarkRefusal, NodeEvent, ScanMode, NO_PARENT};
use disk_cleanup_core::explore::tree::{ExploreTree, NodeId};
use disk_cleanup_core::explore::view::{Badge, Sort, ViewState};
use disk_cleanup_scan::estimate::Stats;
use disk_cleanup_scan::index::DirInfo;

fn stats(files: u64, alloc: u64) -> Stats {
    Stats {
        inodes: files,
        files,
        dir_entries: files,
        size_bytes: files * alloc,
        allocated_bytes: files * alloc,
        ..Stats::default()
    }
}

fn dir(id: u64, parent: u64, name: &str, s: Stats) -> NodeEvent {
    NodeEvent::Dir {
        id,
        parent,
        name: OsString::from(name),
        dev: 1,
        info: DirInfo {
            stats: s,
            ..DirInfo::default()
        },
        subdirs: 1_000_000,
        from_cache: false,
        painted: false,
    }
}

/// `/r` with `n` children `c00..`, the first three of which hold `.../x`, `.../y`, `.../z`.
fn tree_with(n: usize) -> ExploreTree {
    let mut ev = vec![dir(0, NO_PARENT, "/r", stats(0, 0))];
    for i in 0..n {
        ev.push(dir(
            1 + i as u64,
            0,
            &format!("c{i:02}"),
            stats(1, 1000 + i as u64),
        ));
    }
    let base = 1 + n as u64;
    for (i, name) in ["x", "y", "z"].iter().enumerate() {
        ev.push(dir(base + i as u64, 1, name, stats(1, 10)));
    }
    let mut t = ExploreTree::new();
    t.apply_batch(0, ScanMode::Fresh, ev).unwrap();
    t
}

struct Ctx {
    roots: Vec<PathBuf>,
    config: PathBuf,
    candidates: HashSet<NodeId>,
    refused: Vec<(PathBuf, MarkRefusal)>,
    now: u64,
}

impl Ctx {
    fn new() -> Ctx {
        Ctx {
            roots: vec![PathBuf::from("/r"), PathBuf::from("/q")],
            config: PathBuf::from("/etc/dc/config.toml"),
            candidates: HashSet::new(),
            refused: Vec::new(),
            now: 0,
        }
    }

    fn key(&mut self, st: &mut ExploreState, t: &ExploreTree, key: Key) -> Outcome {
        self.now += 5_000;
        let cands = self.candidates.clone();
        let badge = move |n: NodeId| {
            if cands.contains(&n) {
                Badge::Candidate
            } else {
                Badge::None
            }
        };
        let refused = self.refused.clone();
        let check = move |q: &MarkQuery<'_>| -> Result<MarkOk, MarkRefusal> {
            match refused.iter().find(|(p, _)| p == q.path) {
                Some((_, r)) => Err(r.clone()),
                None => Ok(MarkOk::default()),
            }
        };
        let env = KeyEnv {
            now_ms: self.now,
            tier: Tier::Compact,
            page: 10,
            tty: true,
            roots: &self.roots,
            config_path: &self.config,
            badge: &badge,
            mark_check: &check,
        };
        handle_key(st, t, key, &env)
    }

    /// Repaints the rows with this context's badges (a re-sort).
    fn open(&self, t: &ExploreTree) -> ExploreState {
        let cands = self.candidates.clone();
        let badge = move |n: NodeId| {
            if cands.contains(&n) {
                Badge::Candidate
            } else {
                Badge::None
            }
        };
        let mut v = ViewState::new(Sort::default());
        v.open_dir(t, t.roots().next().unwrap(), 0, &badge);
        ExploreState::new(v)
    }
}

fn goto(st: &mut ExploreState, name: &str) {
    let i = st
        .view
        .rows()
        .iter()
        .position(|r| r.display_name() == name)
        .unwrap();
    st.view.move_to(i, 0);
}

#[test]
fn space_marks_the_row_the_cursor_stays_and_a_plain_directory_is_markable() {
    let t = tree_with(4);
    let mut cx = Ctx::new();
    let mut st = cx.open(&t);
    goto(&mut st, "c02/");
    let at = st.view.cursor_index();
    let o = cx.key(&mut st, &t, Key::Char(' '));
    assert!(matches!(o.action, Action::None));
    assert_eq!(o.message.as_deref(), Some("marked /r/c02"));
    assert!(st.marks.contains(Path::new("/r/c02")));
    assert_eq!(st.view.cursor_index(), at);
    assert!(
        !st.marks.iter().next().unwrap().cleaner_item,
        "no badge: not a cleaner item"
    );
    // Space again unmarks.
    let o = cx.key(&mut st, &t, Key::Char(' '));
    assert_eq!(o.message.as_deref(), Some("unmarked /r/c02"));
    assert!(st.marks.is_empty());
}

#[test]
fn a_refusal_of_mark_check_shows_its_catalogue_text_and_marks_nothing() {
    let t = tree_with(4);
    let mut cx = Ctx::new();
    cx.refused = vec![(PathBuf::from("/r/c01"), MarkRefusal::OutsideRoot)];
    let mut st = cx.open(&t);
    goto(&mut st, "c01/");
    let o = cx.key(&mut st, &t, Key::Char(' '));
    let m = o.message.unwrap();
    assert!(
        m.starts_with("/r/c01 is outside the folders the cleaner may touch (/r, /q)."),
        "{m}"
    );
    assert!(m.contains("add its parent to 'roots' in /etc/dc/config.toml and run explore again"));
    assert!(st.marks.is_empty());
}

#[test]
fn a_directory_with_candidates_below_adds_the_count_to_the_mark_line() {
    let t = tree_with(4);
    let mut cx = Ctx::new();
    let c00 = t
        .children(t.roots().next().unwrap())
        .find(|c| t.name(*c) == "c00")
        .unwrap();
    for k in t.children(c00) {
        cx.candidates.insert(k);
    }
    let mut st = cx.open(&t);
    goto(&mut st, "c00/");
    let o = cx.key(&mut st, &t, Key::Char(' '));
    assert_eq!(
        o.message.as_deref(),
        Some("marked /r/c00; contains 3 candidate items (press f to show only candidates)")
    );
}

#[test]
fn a_path_over_512_bytes_is_refused_with_path_too_long() {
    let first = "a".repeat(300);
    let second = "b".repeat(300);
    let mut t = ExploreTree::new();
    t.apply_batch(
        0,
        ScanMode::Fresh,
        vec![
            dir(0, NO_PARENT, "/r", stats(0, 0)),
            dir(1, 0, &first, stats(0, 0)),
            dir(2, 1, &second, stats(1, 100)),
        ],
    )
    .unwrap();
    let mut cx = Ctx::new();
    let mut st = cx.open(&t);
    cx.key(&mut st, &t, Key::Enter);
    let o = cx.key(&mut st, &t, Key::Char(' '));
    let m = o.message.unwrap();
    assert!(m.contains("the --only line cannot carry it"), "{m}");
    assert!(st.marks.is_empty());
    // exactly 512 bytes is still carried
    let mut marks = Marks::new();
    let at_limit = format!("/{}", "x".repeat(511));
    assert!(marks
        .add(Mark {
            path: PathBuf::from(&at_limit),
            est_bytes: 1,
            cleaner_item: false
        })
        .is_ok());
    assert_eq!(
        marks.add(Mark {
            path: PathBuf::from(format!("{at_limit}x")),
            est_bytes: 1,
            cleaner_item: false
        }),
        Err(MarkRefusal::PathTooLong)
    );
}

#[test]
fn the_21st_mark_is_refused_x_clears_and_marks_stay_sorted() {
    let t = tree_with(24);
    let mut cx = Ctx::new();
    let mut st = cx.open(&t);
    // largest first: c23 down to c00; mark in that order
    for i in 0..MARK_LIMIT {
        st.view.move_to(i, 0);
        let o = cx.key(&mut st, &t, Key::Char(' '));
        assert!(o.message.unwrap().starts_with("marked "), "mark {i}");
    }
    assert_eq!(st.marks.len(), 20);
    st.view.move_to(20, 0);
    let o = cx.key(&mut st, &t, Key::Char(' '));
    assert_eq!(
        o.message.as_deref(),
        Some("20 items are marked, the limit for one plan. Delete these first, then mark more.")
    );
    assert_eq!(st.marks.len(), 20);
    let paths: Vec<_> = st.marks.iter().map(|m| m.path.clone()).collect();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted, "marks are kept in path order");

    // unmarking one makes room again
    st.view.move_to(0, 0);
    cx.key(&mut st, &t, Key::Char(' '));
    st.view.move_to(20, 0);
    assert!(cx
        .key(&mut st, &t, Key::Char(' '))
        .message
        .unwrap()
        .starts_with("marked "));

    let o = cx.key(&mut st, &t, Key::Char('x'));
    assert!(st.marks.is_empty());
    assert_eq!(o.message.as_deref(), Some("Forgot 20 marks."));
}

#[test]
fn marks_survive_every_filter() {
    let t = tree_with(6);
    let mut cx = Ctx::new();
    let c03 = t
        .children(t.roots().next().unwrap())
        .find(|c| t.name(*c) == "c03")
        .unwrap();
    cx.candidates.insert(c03);
    let mut st = cx.open(&t);
    goto(&mut st, "c05/");
    cx.key(&mut st, &t, Key::Char(' '));
    goto(&mut st, "c03/");
    cx.key(&mut st, &t, Key::Char(' '));
    let before = st.marks.identity();
    for k in [Key::Char('f'), Key::Char('f'), Key::Char('f')] {
        cx.key(&mut st, &t, k);
        assert_eq!(st.marks.identity(), before);
    }
    cx.key(&mut st, &t, Key::Char('/'));
    for ch in "zzz".chars() {
        cx.key(&mut st, &t, Key::Char(ch));
    }
    assert!(st
        .view
        .rows()
        .iter()
        .all(|r| !r.display_name().starts_with('c')));
    assert_eq!(st.marks.identity(), before);
    cx.key(&mut st, &t, Key::Esc);
    assert_eq!(st.marks.identity(), before);
    assert_eq!(st.marks.len(), 2);
    assert!(st.marks.iter().any(|m| m.cleaner_item));
}

#[test]
fn a_summary_row_cannot_be_marked() {
    let t = tree_with(3);
    let mut cx = Ctx::new();
    let mut st = cx.open(&t);
    // c00 has no files of its own beyond its 3 children: use the root-level (N files) row of /r/c00
    goto(&mut st, "c00/");
    cx.key(&mut st, &t, Key::Enter);
    let files_row = st
        .view
        .rows()
        .iter()
        .position(|r| r.display_name().ends_with(" files)"));
    if let Some(i) = files_row {
        st.view.move_to(i, 0);
        let o = cx.key(&mut st, &t, Key::Char(' '));
        assert!(o
            .message
            .unwrap()
            .starts_with("That row stands for several entries"));
        assert!(st.marks.is_empty());
    }
}

#[test]
fn a_marked_item_that_disappears_is_dropped_with_a_line() {
    let mut m = Marks::new();
    for p in ["/r/b", "/r/a"] {
        m.add(Mark {
            path: PathBuf::from(p),
            est_bytes: 5,
            cleaner_item: true,
        })
        .unwrap();
    }
    let notes = m.prune(&|p| p != Path::new("/r/a"), true);
    assert_eq!(notes, ["/r/a disappeared; unmarked"]);
    assert_eq!(m.len(), 1);
    assert!(m.contains(Path::new("/r/b")));
    // an unusual name is escaped
    let mut m = Marks::new();
    m.add(Mark {
        path: PathBuf::from("/r/a\nb"),
        est_bytes: 5,
        cleaner_item: true,
    })
    .unwrap();
    assert_eq!(
        m.prune(&|_| false, true),
        ["/r/a\\nb disappeared; unmarked"]
    );
}

#[test]
fn the_mark_set_identity_ignores_order_and_sizes_but_not_paths() {
    let mk = |p: &str, b: u64| Mark {
        path: PathBuf::from(p),
        est_bytes: b,
        cleaner_item: false,
    };
    let mut a = Marks::new();
    a.add(mk("/r/x", 1)).unwrap();
    a.add(mk("/r/y", 2)).unwrap();
    let mut b = Marks::new();
    b.add(mk("/r/y", 9)).unwrap();
    b.add(mk("/r/x", 8)).unwrap();
    assert_eq!(a.identity(), b.identity());
    b.add(mk("/r/z", 1)).unwrap();
    assert_ne!(a.identity(), b.identity());
    b.remove(Path::new("/r/z"));
    assert_eq!(a.identity(), b.identity());
    let paths: Vec<_> = a.identity().paths().to_vec();
    assert_eq!(paths, [PathBuf::from("/r/x"), PathBuf::from("/r/y")]);
}

#[test]
fn the_quit_prompt_says_what_enter_x_and_esc_do_and_names_marks_that_are_not_items() {
    let m = |p: &str, b: u64, item: bool| Mark {
        path: PathBuf::from(p),
        est_bytes: b,
        cleaner_item: item,
    };
    let gib = 1u64 << 30;
    let mut marks = Marks::new();
    assert_eq!(marks.quit_prompt(), None);
    marks.add(m("/r/a", 20 * gib, true)).unwrap();
    marks.add(m("/r/b", 10 * gib, true)).unwrap();
    marks.add(m("/r/c", 10 * gib + gib / 10, true)).unwrap();
    assert_eq!(
        marks.quit_prompt().unwrap(),
        "3 marked (40.1 GiB).  Enter: show the plan and leave   x: forget marks and leave   Esc: stay"
    );
    assert!(marks.not_cleaner_items().is_empty());
    marks.add(m("/r/d", 0, false)).unwrap();
    let p = marks.quit_prompt().unwrap();
    assert!(p.starts_with("4 marked (40.1 GiB)."), "{p}");
    assert!(
        p.ends_with("  1 is not a cleaner item: delete it here with d"),
        "{p}"
    );
    assert_eq!(marks.not_cleaner_items(), [Path::new("/r/d")]);
    marks.add(m("/r/e", 0, false)).unwrap();
    assert!(marks
        .quit_prompt()
        .unwrap()
        .ends_with("2 are not cleaner items: delete them here with d"));
}

#[test]
fn the_help_overlay_is_generated_from_the_keymap_and_pinned() {
    let lines = help_lines(80);
    let text = lines.join("\n");
    // sentences may wrap; compare them with the line breaks and indents folded to single spaces
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    // every entry of the table is listed with its label and text
    for d in KEYMAP {
        assert!(
            text.contains(d.label.split('/').next().unwrap()),
            "{} missing from the overlay",
            d.label
        );
        let first_word = d.help.split(' ').next().unwrap();
        assert!(text.contains(first_word), "{}", d.help);
    }
    assert!(flat.contains(DELETE_RULE));
    assert!(flat.contains(DELETE_STEPS));
    assert_eq!(
        DELETE_RULE,
        "d deletes after a plan and a typed count. D, Delete and y do nothing."
    );
    assert_eq!(
        DELETE_STEPS,
        "Deleting is permanent: mark, press p, read the plan, press d, type the count"
    );
    assert!(flat.contains("'+' candidate: the cleaner lists it"));
    assert!(flat.contains(
        "'?' possible candidate: p checks it with git or Steam data the explorer does not have"
    ));
    assert!(flat.contains("not screen-reader friendly"));
    assert!(flat.contains("disk-cleanup explore --text"));
    assert_eq!(
        SCREEN_READER,
        "This view is not screen-reader friendly: use 'disk-cleanup explore --text'."
    );
    assert!(!text.to_lowercase().contains("cleanable"));
    assert_eq!(
        text, EXPECTED_HELP_80,
        "the 80-column overlay changed:\n{text}"
    );
}

#[test]
fn the_help_overlay_fits_every_width_from_60_to_200() {
    for w in 60..=200 {
        for l in help_lines(w) {
            assert!(cells(&l) <= w, "{w}: {l:?}");
        }
    }
}

#[test]
fn every_keymap_entry_appears_in_full_in_the_overlay() {
    // The key lines come from KEYMAP alone, so each entry is present with its label and its text.
    let full = help_lines(80).join("\n");
    for d in KEYMAP {
        let needle = format!("{} {}", d.label, d.help);
        assert!(
            full.replace("\n          ", " ").contains(&needle),
            "{needle}"
        );
    }
}

const EXPECTED_HELP_80: &str = "\
EXPLORE HELP - nothing is deleted until you confirm.             Esc or ? closes

Move      j/Down down   k/Up up   PgDn/Ctrl-F page down   PgUp/Ctrl-B page up
          g/Home top   G/End bottom
Open/back l/Right/Enter open directory   h/Left/Backspace parent directory
Sort      s usage   a apparent   m metadata   c items   n name (again = reverse)
Columns   u toggle shared-extent estimate column   t toggle share bar/percent
Filter    / name contains   f cycle: all, candidates only, errors only
Scan      Esc close, clear filter, stop the scan (keeps what was found)
          r rescan this directory   R rescan everything (like --rescan)
          e list unreadable paths
Info      i details of the selected item
Plan      Space mark/unmark an item (max 20)   x clear marks
          p preview the plan (deletes nothing)
          d delete the marked items after a plan and a typed count
Quit      q quit (asks what to do with marks)   Ctrl-C quit now, exit 130
Screen    Ctrl-L redraw   ? this help

d deletes after a plan and a typed count. D, Delete and y do nothing.
Deleting is permanent: mark, press p, read the plan, press d, type the count

State     '.' still scanning (size is at least)   '!' unreadable (at least)
          'x' other filesystem (Enter scans it)
          '+' candidate: the cleaner lists it   '?' possible candidate: p checks
          it with git or Steam data the explorer does not have
Headers   USAGE bytes on disk   APPARENT file sizes   META est. btrfs metadata
          SHARE of parent
Access    This view is not screen-reader friendly: use 'disk-cleanup explore
          --text'.";

// ---- Story 6.6.1 (Task 6.6.1b): Space reaches the real `mark_check` --------------------------------------

mod real_check {
    use super::*;
    use disk_cleanup_core::explore::mark_check::{
        mark_check, MarkCtx, MarkEnv, MarkFacts, MarkMount, MountTable, OwnState, SymlinkFact,
    };
    use disk_cleanup_core::explore::refusal::{RefusalCtx, MARK_LIMIT as LIMIT};
    use disk_cleanup_core::Dirs;

    struct NoFacts;
    impl MarkFacts for NoFacts {
        fn symlink_in(&self, _: &Path) -> Option<SymlinkFact> {
            None
        }
        fn dev_of(&self, p: &Path) -> Option<u64> {
            Some(if p == Path::new("/r/c02/x") { 7 } else { 1 })
        }
        fn is_dir(&self, _: &Path) -> Option<bool> {
            Some(true)
        }
        fn is_boundary(&self, _: &Path) -> bool {
            false
        }
        fn scanning(&self, _: &Path) -> bool {
            false
        }
    }

    fn press(st: &mut ExploreState, t: &ExploreTree, key: Key, mounts: &MountTable) -> Outcome {
        let roots = vec![PathBuf::from("/r")];
        let own = OwnState::new(
            &Dirs {
                home: "/home/u".into(),
                state: "/home/u/.local/state".into(),
                cache: "/home/u/.cache".into(),
                config: "/home/u/.config".into(),
            },
            "/bin/dc".into(),
        );
        let env_facts = MarkEnv {
            roots: &roots,
            home: Path::new("/home/u"),
            mounts,
            own_state: &own,
            facts: &NoFacts,
            tty: false,
        };
        let check = |q: &MarkQuery<'_>| {
            mark_check(
                q.path,
                &MarkCtx {
                    env: &env_facts,
                    marks: q.marks,
                },
            )
        };
        let badge = |_: NodeId| Badge::None;
        let cfg = PathBuf::from("/etc/dc/config.toml");
        let env = KeyEnv {
            now_ms: 5_000,
            tier: Tier::Compact,
            page: 10,
            tty: false,
            roots: &roots,
            config_path: &cfg,
            badge: &badge,
            mark_check: &check,
        };
        handle_key(st, t, key, &env)
    }

    fn open_dir(t: &ExploreTree) -> ExploreState {
        let badge = |_: NodeId| Badge::None;
        let mut v = ViewState::new(Sort::default());
        v.open_dir(t, t.roots().next().unwrap(), 0, &badge);
        ExploreState::new(v)
    }

    #[test]
    fn space_should_refuse_inside_a_mark_then_above_one_with_the_catalogue_text() {
        let t = tree_with(4);
        let none = MountTable::default();
        let mut st = open_dir(&t);
        goto(&mut st, "c00/");
        press(&mut st, &t, Key::Char(' '), &none);
        assert!(st.marks.contains(Path::new("/r/c00")));
        // Open c00 and try x inside the marked directory.
        press(&mut st, &t, Key::Enter, &none);
        goto(&mut st, "x/");
        let o = press(&mut st, &t, Key::Char(' '), &none);
        let m = o.message.unwrap();
        assert!(
            m.starts_with("/r/c00/x is inside /r/c00, which is already marked."),
            "{m}"
        );
        assert_eq!(st.marks.len(), 1);
        // Unmark c00, mark x, then c00 holds one marked item.
        press(&mut st, &t, Key::Char('h'), &none);
        goto(&mut st, "c00/");
        press(&mut st, &t, Key::Char(' '), &none);
        assert!(st.marks.is_empty());
        press(&mut st, &t, Key::Enter, &none);
        goto(&mut st, "x/");
        press(&mut st, &t, Key::Char(' '), &none);
        assert!(st.marks.contains(Path::new("/r/c00/x")));
        press(&mut st, &t, Key::Char('h'), &none);
        goto(&mut st, "c00/");
        let o = press(&mut st, &t, Key::Char(' '), &none);
        assert!(o
            .message
            .unwrap()
            .starts_with("/r/c00 contains 1 marked item. Unmark it first."),);
        assert_eq!(st.marks.len(), 1);
    }

    #[test]
    fn space_should_mark_a_directory_over_another_filesystem_and_say_so_in_the_footer() {
        let t = tree_with(4);
        // c02 holds a mount of another device (x has st_dev 7): the directory is markable with a note.
        let mounts = MountTable::new(vec![MarkMount {
            point: "/r/c02/x".into(),
            source: "/dev/sdb1".into(),
        }]);
        let mut st = open_dir(&t);
        goto(&mut st, "c02/");
        let o = press(&mut st, &t, Key::Char(' '), &mounts);
        let m = o.message.unwrap();
        assert!(
            m.starts_with("marked /r/c02; contains another filesystem (/dev/sdb1); it is not entered and the directory stays"),
            "{m}"
        );
        assert!(st.marks.contains(Path::new("/r/c02")));
    }

    #[test]
    fn add_checked_should_change_nothing_on_a_refusal_and_stop_at_the_limit() {
        let roots = vec![PathBuf::from("/r")];
        let own = OwnState::new(
            &Dirs {
                home: "/home/u".into(),
                state: "/home/u/.local/state".into(),
                cache: "/home/u/.cache".into(),
                config: "/home/u/.config".into(),
            },
            "/bin/dc".into(),
        );
        let mounts = MountTable::default();
        let env = MarkEnv {
            roots: &roots,
            home: Path::new("/home/u"),
            mounts: &mounts,
            own_state: &own,
            facts: &NoFacts,
            tty: false,
        };
        let m = |p: String| Mark {
            path: p.into(),
            est_bytes: 1,
            cleaner_item: false,
        };
        let mut marks = Marks::new();
        for i in 0..LIMIT {
            marks.add_checked(m(format!("/r/p{i:02}")), &env).unwrap();
        }
        let err = marks.add_checked(m("/r/extra".into()), &env).unwrap_err();
        assert_eq!(err, MarkRefusal::LimitReached);
        assert_eq!(marks.len(), LIMIT);
        let err = marks
            .add_checked(m("/elsewhere/a".into()), &env)
            .unwrap_err();
        assert_eq!(err, MarkRefusal::OutsideRoot);
        let cx = RefusalCtx {
            path: Path::new("/r/extra"),
            roots: &roots,
            config_path: Path::new("/c"),
            real_path: None,
            tty: false,
        };
        assert_eq!(
            MarkRefusal::LimitReached.text(&cx),
            "20 items are marked, the limit for one plan. Delete these first, then mark more."
        );
    }
}
