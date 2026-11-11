#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! The keymap and the input state machine (Story 6.2.4, Task 6.2.4a; validation UX-T1, UX-T12, UX-T20, UX-T29).

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use disk_cleanup_core::explore::input::{
    handle_key, scan_title, Action, ExitKind, ExploreState, KeyEnv, MarkQuery, Mode, Outcome,
    Rescan, MSG_NOTHING_MARKED, MSG_ONLY_D_DELETES, MSG_TOP_OF_SCAN,
};
use disk_cleanup_core::explore::keymap::{all_keys, lookup, Cmd, Key, KEYMAP};
use disk_cleanup_core::explore::layout::Tier;
use disk_cleanup_core::explore::mark_check::MarkOk;
use disk_cleanup_core::explore::msg::{
    FileRow, FileRows, MarkRefusal, NodeEvent, ScanMode, StopCause, NO_PARENT,
};
use disk_cleanup_core::explore::tree::{ExploreTree, NodeId, ScanStatus};
use disk_cleanup_core::explore::view::{no_badge, Badge, Sort, ViewState};
use disk_cleanup_scan::estimate::Stats;
use disk_cleanup_scan::index::DirInfo;
use proptest::prelude::*;

fn own(files: u64, subdirs: u64, alloc: u64) -> Stats {
    Stats {
        inodes: files + subdirs,
        files,
        dirs: subdirs,
        dir_entries: files + subdirs,
        name_bytes: 8 * (files + subdirs),
        size_bytes: files * alloc,
        allocated_bytes: files * alloc,
        ..Stats::default()
    }
}

fn dir(id: u64, parent: u64, name: &str, stats: Stats) -> NodeEvent {
    NodeEvent::Dir {
        id,
        parent,
        name: OsString::from(name),
        dev: 1,
        info: DirInfo {
            stats,
            ..DirInfo::default()
        },
        subdirs: 1_000_000,
        from_cache: false,
        painted: false,
    }
}

/// `/r` holds alpha (with `node_modules` inside), bravo, charlie and the other-filesystem entry `mnt`.
fn world_tree() -> ExploreTree {
    let mut t = ExploreTree::new();
    t.apply_batch(
        0,
        ScanMode::Fresh,
        vec![
            dir(0, NO_PARENT, "/r", own(0, 4, 0)),
            dir(1, 0, "alpha", own(2, 1, 4000)),
            dir(2, 1, "node_modules", own(5, 0, 1000)),
            dir(3, 0, "bravo", own(1, 0, 3000)),
            dir(4, 0, "charlie", own(1, 0, 2000)),
            NodeEvent::Boundary {
                id: 5,
                parent: 0,
                name: OsString::from("mnt"),
                dev: 2,
            },
        ],
    )
    .unwrap();
    t
}

struct Ctx {
    roots: Vec<PathBuf>,
    config: PathBuf,
    candidates: HashSet<NodeId>,
    refused: Vec<(PathBuf, MarkRefusal)>,
    tier: Tier,
    now: u64,
}

impl Ctx {
    fn new() -> Ctx {
        Ctx {
            roots: vec![PathBuf::from("/r")],
            config: PathBuf::from("/etc/dc/config.toml"),
            candidates: HashSet::new(),
            refused: Vec::new(),
            tier: Tier::Compact,
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
            tier: self.tier,
            page: 10,
            tty: true,
            roots: &self.roots,
            config_path: &self.config,
            badge: &badge,
            mark_check: &check,
        };
        handle_key(st, t, key, &env)
    }
}

fn open_root(t: &ExploreTree) -> ExploreState {
    let mut v = ViewState::new(Sort::default());
    let root = t.roots().next().unwrap();
    let req = v.open_dir(t, root, 0, &no_badge);
    let files = FileRows {
        rows: vec![FileRow {
            name: OsString::from("big.iso"),
            allocated_bytes: 100,
            size_bytes: 100,
            second_link: false,
        }],
        smaller_count: 0,
        smaller_bytes: 0,
        partial: false,
    };
    assert!(v.accept_files(req.generation, root, files, &|_| true));
    v.order_snapshot(t, 10_000, &no_badge);
    ExploreState::new(v)
}

fn names(st: &ExploreState) -> Vec<String> {
    st.view
        .rows()
        .iter()
        .map(disk_cleanup_core::explore::rows::Row::display_name)
        .collect()
}

fn goto(st: &mut ExploreState, name: &str) {
    let i = names(st).iter().position(|n| n == name).unwrap();
    st.view.move_to(i, 0);
}

fn snapshot(st: &ExploreState) -> String {
    format!(
        "{:?}|{}|{:?}|{:?}|{:?}|{}|{}",
        st.view.cursor(),
        st.view.cursor_index(),
        names(st),
        st.marks,
        st.input.mode,
        st.input.show_info,
        st.input.shared_column,
    )
}

#[test]
fn only_d_deletes_D_Delete_and_y_explain_and_b_says_no_shell() {
    let t = world_tree();
    let mut cx = Ctx::new();
    let mut st = open_root(&t);
    goto(&mut st, "bravo/");
    cx.key(&mut st, &t, Key::Char(' '));
    let before = snapshot(&st);
    for k in [Key::Char('D'), Key::Delete, Key::Char('y')] {
        let o = cx.key(&mut st, &t, k);
        assert!(matches!(o.action, Action::None), "{k:?}");
        assert_eq!(o.message.as_deref(), Some(MSG_ONLY_D_DELETES));
        assert_eq!(snapshot(&st), before, "{k:?} changed the state");
    }
    assert_eq!(
        MSG_ONLY_D_DELETES,
        "Press d to review and delete the marked items. Space marks an item, p previews the plan."
    );
    let o = cx.key(&mut st, &t, Key::Char('b'));
    assert!(matches!(o.action, Action::None));
    assert_eq!(o.message.as_deref(), Some("No shell from here"));
    assert_eq!(snapshot(&st), before);

    // d with a mark is the one Delete; with none it is a message.
    let o = cx.key(&mut st, &t, Key::Char('d'));
    assert!(matches!(o.action, Action::Delete));
    st.marks.clear();
    let o = cx.key(&mut st, &t, Key::Char('d'));
    assert!(matches!(o.action, Action::None));
    assert_eq!(o.message.as_deref(), Some(MSG_NOTHING_MARKED));
    assert_eq!(
        MSG_NOTHING_MARKED,
        "Nothing is marked. Space marks an item."
    );
}

#[test]
fn no_other_key_returns_delete_and_the_action_enum_has_no_surprise_variant() {
    // The match has no wildcard: a new variant breaks this test until it is classified here.
    fn class(a: &Action) -> &'static str {
        match a {
            Action::None | Action::Redraw | Action::ListFiles(_) => "view",
            Action::Rescan(_) | Action::NewRoot(_) | Action::PopRoot | Action::StopScan => "scan",
            Action::Exit(_) => "exit",
            Action::Preview => "plan request",
            Action::Delete => "delete request",
        }
    }
    let t = world_tree();
    let mut cx = Ctx::new();
    let mut st = open_root(&t);
    goto(&mut st, "bravo/");
    cx.key(&mut st, &t, Key::Char(' '));
    let mut extra = vec![Key::Char('D'), Key::Delete, Key::Char('y'), Key::Char('b')];
    extra.extend(all_keys());
    let mut deletes = Vec::new();
    for k in extra {
        let mut s2 = open_root(&t);
        goto(&mut s2, "bravo/");
        cx.key(&mut s2, &t, Key::Char(' '));
        let o = cx.key(&mut s2, &t, k);
        if class(&o.action) == "delete request" {
            deletes.push(k);
        }
    }
    assert_eq!(deletes, vec![Key::Char('d')]);
}

#[test]
fn the_keymap_has_an_entry_for_every_key_of_the_table_and_for_no_other() {
    let doc: Vec<Key> = {
        let mut v = vec![
            Key::Char('j'),
            Key::Char('k'),
            Key::Down,
            Key::Up,
            Key::PageDown,
            Key::PageUp,
            Key::Ctrl('f'),
            Key::Ctrl('b'),
            Key::Char('g'),
            Key::Char('G'),
            Key::Home,
            Key::End,
            Key::Char('l'),
            Key::Char('h'),
            Key::Left,
            Key::Right,
            Key::Enter,
            Key::Backspace,
        ];
        v.extend("sacmnut/fi xprdrR?eq".chars().map(Key::Char));
        v.extend([Key::Esc, Key::Ctrl('l'), Key::Ctrl('c')]);
        v
    };
    let have: HashSet<Key> = all_keys().collect();
    let want: HashSet<Key> = doc.into_iter().collect();
    assert_eq!(have, want);
    // No key twice, and no key without help text.
    assert_eq!(all_keys().count(), have.len());
    assert!(KEYMAP
        .iter()
        .all(|d| !d.help.is_empty() && !d.label.is_empty()));
    // The refused keys are not commands.
    for k in [Key::Char('D'), Key::Delete, Key::Char('y'), Key::Char('b')] {
        assert!(lookup(k).is_none(), "{k:?}");
    }
    assert!(matches!(lookup(Key::Char('d')).unwrap().cmd, Cmd::Delete));
}

#[test]
fn every_key_of_the_table_returns_its_documented_action() {
    let t = world_tree();
    let mut cx = Ctx::new();
    let fresh = |t: &ExploreTree| {
        let mut st = open_root(t);
        goto(&mut st, "bravo/");
        st
    };
    let act = |cx: &mut Ctx, key: Key| {
        let mut st = fresh(&t);
        cx.key(&mut st, &t, Key::Char(' '));
        cx.key(&mut st, &t, key).action
    };
    assert!(
        matches!(act(&mut cx, Key::Char('r')), Action::Rescan(Rescan::Directory(p)) if p == Path::new("/r"))
    );
    assert!(matches!(
        act(&mut cx, Key::Char('R')),
        Action::Rescan(Rescan::All)
    ));
    assert!(matches!(act(&mut cx, Key::Ctrl('l')), Action::Redraw));
    assert!(matches!(act(&mut cx, Key::Char('p')), Action::Preview));
    assert!(matches!(act(&mut cx, Key::Char('d')), Action::Delete));
    assert!(matches!(
        act(&mut cx, Key::Ctrl('c')),
        Action::Exit(ExitKind::Interrupted)
    ));
    for k in [Key::Char('l'), Key::Right, Key::Enter] {
        assert!(matches!(act(&mut cx, k), Action::ListFiles(_)), "{k:?}");
    }
    for k in [Key::Char('h'), Key::Left, Key::Backspace] {
        // bravo is a child of the root: leaving the root is the top of the scan
        assert!(matches!(act(&mut cx, k), Action::None), "{k:?}");
    }
    for k in "jksacmntfi".chars().map(Key::Char) {
        assert!(matches!(act(&mut cx, k), Action::None), "{k:?}");
    }
    for k in [
        Key::Down,
        Key::Up,
        Key::PageDown,
        Key::PageUp,
        Key::Ctrl('f'),
        Key::Ctrl('b'),
        Key::Home,
        Key::End,
        Key::Char('g'),
        Key::Char('G'),
        Key::Char('x'),
    ] {
        assert!(matches!(act(&mut cx, k), Action::None), "{k:?}");
    }
    // q with marks opens the prompt, with none exits.
    assert!(matches!(act(&mut cx, Key::Char('q')), Action::None));
    let mut st = fresh(&t);
    assert!(matches!(
        cx.key(&mut st, &t, Key::Char('q')).action,
        Action::Exit(ExitKind::Closed)
    ));
}

#[test]
fn navigation_moves_the_cursor_pages_and_reports_the_ends() {
    let t = world_tree();
    let mut cx = Ctx::new();
    let mut st = open_root(&t);
    assert_eq!(st.view.rows().len(), 5);
    cx.key(&mut st, &t, Key::Char('G'));
    assert_eq!(st.view.cursor_index(), 4);
    let o = cx.key(&mut st, &t, Key::Down);
    assert_eq!(o.message.as_deref(), Some("last entry"));
    cx.key(&mut st, &t, Key::Home);
    assert_eq!(st.view.cursor_index(), 0);
    cx.key(&mut st, &t, Key::Char('j'));
    assert_eq!(st.view.cursor_index(), 1);
    cx.key(&mut st, &t, Key::PageDown);
    assert_eq!(st.view.cursor_index(), 4);
    cx.key(&mut st, &t, Key::PageUp);
    assert_eq!(st.view.cursor_index(), 0);
    cx.key(&mut st, &t, Key::Char('k'));
    assert_eq!(st.view.cursor_index(), 0);
}

#[test]
fn enter_on_a_file_says_not_a_directory_and_on_an_x_row_starts_a_new_root() {
    let t = world_tree();
    let mut cx = Ctx::new();
    let mut st = open_root(&t);
    goto(&mut st, "big.iso");
    let o = cx.key(&mut st, &t, Key::Enter);
    assert!(matches!(o.action, Action::None));
    assert_eq!(o.message.as_deref(), Some("not a directory"));

    goto(&mut st, "mnt/");
    let o = cx.key(&mut st, &t, Key::Enter);
    assert!(
        matches!(&o.action, Action::NewRoot(p) if p == Path::new("/r/mnt")),
        "{o:?}"
    );

    // h at the scan root, then at a pushed root.
    let o = cx.key(&mut st, &t, Key::Char('h'));
    assert!(matches!(o.action, Action::None));
    assert_eq!(o.message.as_deref(), Some(MSG_TOP_OF_SCAN));
    assert_eq!(
        MSG_TOP_OF_SCAN,
        "top of the scan: re-run with a wider PATH to go higher"
    );
    st.input.root_depth = 1;
    let o = cx.key(&mut st, &t, Key::Left);
    assert!(matches!(o.action, Action::PopRoot));
}

#[test]
fn enter_opens_a_directory_and_h_returns_with_the_cursor_on_it() {
    let t = world_tree();
    let mut cx = Ctx::new();
    let mut st = open_root(&t);
    goto(&mut st, "alpha/");
    let o = cx.key(&mut st, &t, Key::Enter);
    let Action::ListFiles(req) = o.action else {
        panic!("{o:?}")
    };
    assert_eq!(t.path_of(req.dir), Path::new("/r/alpha"));
    assert_eq!(names(&st), ["(2 files)", "node_modules/"]);
    let o = cx.key(&mut st, &t, Key::Char('h'));
    assert!(matches!(o.action, Action::ListFiles(_)));
    assert_eq!(st.view.cursor_row().unwrap().display_name(), "alpha/");
}

#[test]
fn esc_acts_on_the_first_applicable_of_overlay_prompt_filter_and_scan() {
    let t = world_tree();
    let mut cx = Ctx::new();
    let mut st = open_root(&t);
    st.input.scan_running = true;
    goto(&mut st, "bravo/");
    cx.key(&mut st, &t, Key::Char(' '));

    // scan running + filter active + overlay + prompt all open
    cx.key(&mut st, &t, Key::Char('f'));
    assert!(st.view.filter_active());
    cx.key(&mut st, &t, Key::Char('?'));
    assert_eq!(st.input.mode, Mode::Help);

    // 1: the overlay closes, the filter and the scan stay
    let o = cx.key(&mut st, &t, Key::Esc);
    assert!(matches!(o.action, Action::None));
    assert_eq!(st.input.mode, Mode::Normal);
    assert!(st.view.filter_active());
    assert!(!st.input.stop_requested);

    // prompt first as well
    cx.key(&mut st, &t, Key::Char('q'));
    assert_eq!(st.input.mode, Mode::QuitPrompt);
    cx.key(&mut st, &t, Key::Esc);
    assert_eq!(st.input.mode, Mode::Normal);
    assert!(st.view.filter_active());

    // 2: the filter clears, the scan keeps running
    let o = cx.key(&mut st, &t, Key::Esc);
    assert!(matches!(o.action, Action::None));
    assert!(!st.view.filter_active());
    assert!(!st.input.stop_requested);
    assert_eq!(st.marks.len(), 1, "marks survive the filter and Esc");

    // 3: the scan stops, once
    let o = cx.key(&mut st, &t, Key::Esc);
    assert!(matches!(o.action, Action::StopScan));
    assert!(st.input.stop_requested);
    assert_eq!(
        scan_title(&st.input, ScanStatus::Scanning, StopCause::None, None, 31),
        "PARTIAL SCAN (stopped by you)"
    );
    let o = cx.key(&mut st, &t, Key::Esc);
    assert!(matches!(o.action, Action::None));
}

#[test]
fn the_title_names_what_the_scan_is_doing_or_why_it_stopped() {
    let st = open_root(&world_tree());
    let w = |status, stop, max| scan_title(&st.input, status, stop, max, 31);
    assert_eq!(
        w(ScanStatus::Scanning, StopCause::None, None),
        "SCANNING 3.1s"
    );
    assert_eq!(
        w(ScanStatus::Complete, StopCause::None, None),
        "SCAN DONE 3.1s"
    );
    assert_eq!(
        w(ScanStatus::Truncated, StopCause::Deadline, Some(120)),
        "PARTIAL SCAN (hit --max-seconds 120)"
    );
    assert_eq!(
        w(ScanStatus::Truncated, StopCause::Cancelled, None),
        "PARTIAL SCAN (stopped by you)"
    );
}

#[test]
fn the_filter_prompt_edits_keeps_and_clears() {
    let t = world_tree();
    let mut cx = Ctx::new();
    let mut st = open_root(&t);
    cx.key(&mut st, &t, Key::Char('/'));
    assert_eq!(st.input.mode, Mode::FilterEdit);
    // keys are text here, not commands
    for c in ['b', 'r', 'd'] {
        let o = cx.key(&mut st, &t, Key::Char(c));
        assert!(matches!(o.action, Action::None));
    }
    assert_eq!(st.view.text_filter(), "brd");
    cx.key(&mut st, &t, Key::Backspace);
    assert_eq!(st.view.text_filter(), "br");
    assert_eq!(names(&st), ["bravo/"]);
    cx.key(&mut st, &t, Key::Enter);
    assert_eq!(st.input.mode, Mode::Normal);
    assert_eq!(names(&st), ["bravo/"]);
    // Esc in normal mode clears it
    cx.key(&mut st, &t, Key::Esc);
    assert_eq!(names(&st).len(), 5);
    // Esc while typing clears and leaves the prompt
    cx.key(&mut st, &t, Key::Char('/'));
    cx.key(&mut st, &t, Key::Char('z'));
    assert!(names(&st).is_empty() || names(&st).iter().all(|n| n.contains('z')));
    cx.key(&mut st, &t, Key::Esc);
    assert_eq!(st.input.mode, Mode::Normal);
    assert_eq!(st.view.text_filter(), "");
}

#[test]
fn u_toggles_the_estimate_column_in_the_wide_tier_and_opens_the_info_pane_elsewhere() {
    let t = world_tree();
    let mut cx = Ctx::new();
    let mut st = open_root(&t);
    cx.tier = Tier::Wide;
    cx.key(&mut st, &t, Key::Char('u'));
    assert!(st.input.shared_column && !st.input.show_info);
    cx.key(&mut st, &t, Key::Char('u'));
    assert!(!st.input.shared_column);
    cx.tier = Tier::Narrow;
    cx.key(&mut st, &t, Key::Char('u'));
    assert!(st.input.show_info && !st.input.shared_column);
    cx.key(&mut st, &t, Key::Char('i'));
    assert!(!st.input.show_info, "i toggles");
    cx.key(&mut st, &t, Key::Char('t'));
    assert!(st.input.percent_only);
}

#[test]
fn ctrl_c_exits_with_interrupted_from_every_state() {
    let t = world_tree();
    let mut cx = Ctx::new();
    for pre in [
        vec![],
        vec![Key::Char('?')],
        vec![Key::Char('e')],
        vec![Key::Char(' '), Key::Char('q')],
        vec![Key::Char('/'), Key::Char('a')],
    ] {
        let mut st = open_root(&t);
        goto(&mut st, "bravo/");
        for k in pre {
            cx.key(&mut st, &t, k);
        }
        let o = cx.key(&mut st, &t, Key::Ctrl('c'));
        assert!(matches!(o.action, Action::Exit(ExitKind::Interrupted)));
    }
}

#[test]
fn the_quit_prompt_enter_x_and_esc() {
    let t = world_tree();
    let mut cx = Ctx::new();
    let setup = |cx: &mut Ctx| {
        let mut st = open_root(&t);
        goto(&mut st, "bravo/");
        cx.key(&mut st, &t, Key::Char(' '));
        cx.key(&mut st, &t, Key::Char('q'));
        assert_eq!(st.input.mode, Mode::QuitPrompt);
        st
    };
    let mut st = setup(&mut cx);
    assert!(st.footer().unwrap().starts_with("1 marked (2.9 KiB)."));
    assert!(matches!(
        cx.key(&mut st, &t, Key::Enter).action,
        Action::Exit(ExitKind::Plan)
    ));
    let mut st = setup(&mut cx);
    assert!(matches!(
        cx.key(&mut st, &t, Key::Char('x')).action,
        Action::Exit(ExitKind::Closed)
    ));
    let mut st = setup(&mut cx);
    // other keys do nothing inside the prompt
    assert!(matches!(
        cx.key(&mut st, &t, Key::Char('d')).action,
        Action::None
    ));
    assert_eq!(st.input.mode, Mode::QuitPrompt);
    assert!(matches!(cx.key(&mut st, &t, Key::Esc).action, Action::None));
    assert_eq!(st.input.mode, Mode::Normal);
    assert_eq!(st.marks.len(), 1);
}

#[test]
fn no_text_in_core_or_tui_says_cleanable_in_a_user_visible_string() {
    // Story 6.4.1's catalogue text `is not an item of any cleanable kind` predates this rule and is pinned by
    // its own test; it is the one exemption (see phase5-findings.md).
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut hits = Vec::new();
    for sub in ["core/src/explore", "tui/src"] {
        let Ok(rd) = std::fs::read_dir(root.join(sub)) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_none_or(|x| x != "rs") || p.ends_with("refusal.rs") {
                continue;
            }
            let src = std::fs::read_to_string(&p).unwrap();
            for (n, l) in src.lines().enumerate() {
                let code = l.trim_start();
                if !code.starts_with("//") && l.to_lowercase().contains("cleanable") {
                    hits.push(format!("{}:{}", p.display(), n + 1));
                }
            }
        }
    }
    assert!(hits.is_empty(), "{hits:?}");
}

fn any_key() -> impl Strategy<Value = Key> {
    prop_oneof![
        proptest::sample::select(all_keys().collect::<Vec<_>>()),
        proptest::sample::select(vec![
            Key::Char('D'),
            Key::Char('y'),
            Key::Char('b'),
            Key::Delete,
            Key::Char('z'),
            Key::Char('1'),
        ]),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn a_random_key_sequence_never_leaves_the_view_model(keys in proptest::collection::vec(any_key(), 0..120)) {
        let t = world_tree();
        let mut cx = Ctx::new();
        cx.candidates.insert(t.children(t.roots().next().unwrap()).next().unwrap());
        let mut st = open_root(&t);
        st.input.scan_running = true;
        for k in keys {
            let o = cx.key(&mut st, &t, k);
            if matches!(o.action, Action::Exit(_)) {
                break;
            }
            let rows = st.view.rows();
            if rows.is_empty() {
                prop_assert!(st.view.cursor().is_none());
            } else {
                prop_assert!(st.view.cursor_index() < rows.len());
                prop_assert_eq!(st.view.cursor(), Some(&rows[st.view.cursor_index()].id));
            }
            prop_assert!(st.marks.len() <= 20);
            let paths: Vec<_> = st.marks.iter().map(|m| m.path.clone()).collect();
            let mut sorted = paths.clone();
            sorted.sort_by(|a, b| a.as_os_str().as_encoded_bytes().cmp(b.as_os_str().as_encoded_bytes()));
            prop_assert_eq!(paths, sorted);
        }
    }
}
