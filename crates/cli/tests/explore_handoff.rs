#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Story 6.4.3: the explorer's `preview` and `finish` over fake ports. The planner is the cleaner's own
//! `Executor` in plan mode with scripted targets; nothing here can delete, and no test asks it to.

mod support;

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Write;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::TryRecvError;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use dunnage_cli::explore::actions::{ExplorerPorts, RealExplorerActions, ScanningProbe};
use dunnage_cli::explore::handoff::CLOSED_LINE;
use dunnage_core::candidate::BuildDirFlavor;
use dunnage_core::delete_types::{DeleteFailure, Freed};
use dunnage_core::explore::msg::{MarkRefusal, PlanPreview, ScanMsg};
use dunnage_core::explore::ports::ExplorerActions;
use dunnage_core::outcome::SkipReason;
use dunnage_core::plan::NO_PLAN_ID_ONLY_NOTE;
use dunnage_core::plan_state::{BuildInfo, SavedSelection};
use dunnage_core::ports::{Env, PlanStore};
use dunnage_core::selector::Selection;
use dunnage_core::shell_quote::sandbox::{find_on_path, run_printed_line};
use dunnage_core::target::{Guarded, KindGate, Target};
use dunnage_core::testing::{
    FakeAuditSink, FakeClock, FakeCommandRunner, FakeFs, FakePlanStore, FakeProbe, FakeRunGuard,
    FakeSpaceMeter,
};
use dunnage_core::{ApplyToken, Candidate, Dirs, DiskVerdict, RunRoots, SelectorId};
use support::{build_dir, cache, worktree, NOW};

const HOME: &str = "/home/u";

/// Serves `candidates` for `selectors`; `skips` are the guard's refusals; `gate` holds discovery until the
/// test lets it go (a planner that blocks); `executed` counts calls of `execute`, which must stay 0.
struct Scripted {
    selectors: &'static [SelectorId],
    candidates: Vec<Candidate>,
    skips: HashMap<PathBuf, SkipReason>,
    gate: Option<Arc<(Mutex<bool>, Condvar)>>,
    discovered: Arc<AtomicUsize>,
    executed: Arc<AtomicUsize>,
}

impl Scripted {
    fn new(selectors: &'static [SelectorId], candidates: Vec<Candidate>) -> Scripted {
        Scripted {
            selectors,
            candidates,
            skips: HashMap::new(),
            gate: None,
            discovered: Arc::new(AtomicUsize::new(0)),
            executed: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl Target for Scripted {
    fn name(&self) -> &'static str {
        "scripted"
    }
    fn selectors(&self) -> &'static [SelectorId] {
        self.selectors
    }
    fn gate(&self, _env: &Env) -> KindGate {
        KindGate::Available
    }
    fn discover(&self, _env: &Env, _sel: &Selection) -> Vec<Candidate> {
        self.discovered.fetch_add(1, Ordering::SeqCst);
        if let Some(g) = &self.gate {
            let (m, cv) = &**g;
            let mut open = m.lock().unwrap();
            while !*open {
                open = cv.wait(open).unwrap();
            }
        }
        self.candidates.clone()
    }
    fn guard(&self, c: &Candidate, _env: &Env) -> Guarded {
        match c.locator() {
            dunnage_core::candidate::Locator::Path(p) => self
                .skips
                .get(p)
                .map_or(Guarded::Allow, |r| Guarded::Skip(*r)),
            _ => Guarded::Allow,
        }
    }
    fn execute(
        &self,
        _c: &Candidate,
        _env: &Env,
        _proof: &ApplyToken,
    ) -> Result<Freed, DeleteFailure> {
        self.executed.fetch_add(1, Ordering::SeqCst);
        panic!("the explorer's planner must never execute an item");
    }
}

struct Wired {
    actions: RealExplorerActions,
    plans: Arc<FakePlanStore>,
    executed: Vec<Arc<AtomicUsize>>,
    discovered: Vec<Arc<AtomicUsize>>,
}

fn build() -> BuildInfo {
    BuildInfo {
        bin_version: "0.1.0".into(),
        git_commit: "abc".into(),
        dirty: false,
        metascope_rev: "none".into(),
        patched: false,
        metascope_dirty: false,
    }
}

fn wire_with(
    targets: Vec<Scripted>,
    plans: Arc<FakePlanStore>,
    probe: Option<Arc<ScanningProbe>>,
) -> Wired {
    let executed = targets.iter().map(|t| Arc::clone(&t.executed)).collect();
    let discovered = targets.iter().map(|t| Arc::clone(&t.discovered)).collect();
    let ports = ExplorerPorts {
        fs: Arc::new(FakeFs::new()),
        runner: Arc::new(FakeCommandRunner::new()),
        clock: Arc::new(FakeClock::at(NOW)),
        probe: Arc::new(FakeProbe::new()),
        space: Arc::new(FakeSpaceMeter::new(
            vec![FakeSpaceMeter::plain_snapshot("/home/u", 500, 1000)],
            FakeSpaceMeter::stable(),
        )),
        audit: Arc::new(FakeAuditSink::new()),
        guard: Arc::new(FakeRunGuard::new()),
        plans: Arc::clone(&plans) as Arc<dyn dunnage_core::ports::PlanStore>,
        dirs: Dirs {
            home: PathBuf::from(HOME),
            state: PathBuf::from("/home/u/.local/state/dunnage"),
            cache: PathBuf::from("/home/u/.cache/dunnage"),
            config: PathBuf::from("/home/u/.config/dunnage"),
        },
        targets: targets
            .into_iter()
            .map(|t| Box::new(t) as Box<dyn Target>)
            .collect(),
        build: build(),
        roots: RunRoots {
            scan_roots: vec![PathBuf::from("/home/u/code")],
            ..RunRoots::default()
        },
        run_id: "run-1".into(),
        jobs: 2,
    };
    let mut actions = RealExplorerActions::new(Arc::new(ports));
    if let Some(probe) = probe {
        actions = actions.with_scanning_probe(probe);
    }
    Wired {
        actions,
        plans,
        executed,
        discovered,
    }
}

fn wire(targets: Vec<Scripted>) -> Wired {
    wire_with(targets, Arc::new(FakePlanStore::new()), None)
}

const BUILD: &str = "/home/u/code/app/node_modules";
const OLD: &str = "/home/u/code/old/node_modules";
const CACHE: &str = "/home/u/.npm/_cacache";
const WT: &str = "/home/u/code/app/.claude/worktrees/agent-1";

fn standard() -> Vec<Scripted> {
    vec![
        Scripted::new(&[SelectorId::Caches], vec![cache(CACHE, 1.1)]),
        Scripted::new(
            &[SelectorId::BuildDirs],
            vec![
                build_dir(BUILD, 21.7, 30, BuildDirFlavor::NodeModules),
                build_dir(OLD, 3.0, 30, BuildDirFlavor::NodeModules),
            ],
        ),
        Scripted::new(&[SelectorId::Worktrees], vec![worktree(WT, 2.0, 30)]),
    ]
}

fn p(s: &str) -> PathBuf {
    PathBuf::from(s)
}

fn preview(w: &Wired, marks: &[&str]) -> PlanPreview {
    let marks: Vec<PathBuf> = marks.iter().map(|m| p(m)).collect();
    let h = w.actions.preview(&marks, Arc::new(AtomicBool::new(false)));
    match h
        .rx
        .recv_timeout(Duration::from_secs(20))
        .expect("a preview message")
    {
        ScanMsg::Preview(pv) => pv,
        other => panic!("not a preview: {other:?}"),
    }
}

fn report(w: &Wired, marks: &[&str]) -> Vec<String> {
    let marks: Vec<PathBuf> = marks.iter().map(|m| p(m)).collect();
    let h = w.actions.finish(&marks);
    assert_eq!(h.exit_code, 0);
    h.lines
        .into_iter()
        .map(|l| l.into_string().expect("report lines are UTF-8"))
        .collect()
}

fn sh_accepts(line: &str) {
    let mut child = Command::new("sh")
        .arg("-n")
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(line.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "sh -n rejects {line:?}: {:?}",
        out.stderr
    );
}

fn commands(lines: &[String]) -> Vec<String> {
    let mut in_next = false;
    let mut out = Vec::new();
    for l in lines {
        if l == "Next:" {
            in_next = true;
            continue;
        }
        if in_next && l.is_empty() {
            break;
        }
        if in_next && l.starts_with("  dunnage ") {
            out.push(l.trim().to_string());
        }
    }
    out
}

fn is_hex8(s: &str) -> bool {
    s.len() == 8 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

#[test]
fn the_preview_of_two_marks_should_hold_exactly_those_two_rows_saved_with_an_id_and_no_next_text() {
    let w = wire(standard());
    let pv = preview(&w, &[BUILD, CACHE]);
    let mut got: Vec<(String, String)> = pv
        .rows
        .iter()
        .map(|r| (r.path.display().to_string(), r.status.clone()))
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            (CACHE.to_string(), "FREE".to_string()),
            (BUILD.to_string(), "FREE".to_string())
        ]
    );
    assert!(is_hex8(&pv.plan_id), "{}", pv.plan_id);
    assert!(pv.saved);
    assert!(pv.est_bytes > 0);
    let all = format!("{} {} {:?}", pv.scope_line, pv.guards_line, pv.rows);
    assert!(!all.contains("Next:") && !all.contains("--apply"), "{all}");
    // The plan-id is in the store, merged through the reserve, with the flags string of the selection.
    let saved = w.plans.load_last().expect("a saved plan").entries;
    assert!(
        saved
            .iter()
            .any(|e: &SavedSelection| e.plan_id == pv.plan_id),
        "{saved:?}"
    );
    assert!(
        saved.iter().all(|e| e.flags.contains("--only")),
        "{saved:?}"
    );
    assert!(w.executed.iter().all(|e| e.load(Ordering::SeqCst) == 0));
}

#[test]
fn a_guarded_mark_should_show_skip_with_its_catalogue_reason_and_a_stale_mark_should_show_not_an_item(
) {
    let mut targets = standard();
    targets[1].skips.insert(p(BUILD), SkipReason::Dirty);
    let w = wire(targets);
    let gone = "/home/u/code/app/gone";
    let pv = preview(&w, &[BUILD, OLD, gone]);
    let status = |path: &str| {
        pv.rows
            .iter()
            .find(|r| r.path == Path::new(path))
            .map(|r| r.status.clone())
    };
    assert_eq!(status(BUILD).as_deref(), Some("SKIP"));
    assert_eq!(status(OLD).as_deref(), Some("FREE"));
    assert_eq!(status(gone).as_deref(), Some("SKIP"));
    assert!(
        pv.guards_line.contains(SkipReason::Dirty.reason_text()),
        "{}",
        pv.guards_line
    );
    assert!(
        pv.guards_line
            .contains("is not an item of any cleanable kind"),
        "{}",
        pv.guards_line
    );
}

#[test]
fn a_mark_still_being_scanned_should_make_preview_return_still_scanning_without_planning() {
    let probe: Arc<ScanningProbe> = Arc::new(|path: &Path| path.starts_with("/home/u/code/app"));
    let w = wire_with(standard(), Arc::new(FakePlanStore::new()), Some(probe));
    let cancel = Arc::new(AtomicBool::new(false));
    let marks = [p(BUILD), p(CACHE)];
    let err = w
        .actions
        .try_preview(&marks, Arc::clone(&cancel))
        .unwrap_err();
    assert_eq!(err, MarkRefusal::StillScanning);
    // Through the trait the same refusal arrives as the footer line.
    let h = w.actions.preview(&marks, cancel);
    match h.rx.recv_timeout(Duration::from_secs(5)).unwrap() {
        ScanMsg::PreviewFailed(t) => assert!(t.contains("still being scanned"), "{t}"),
        other => panic!("{other:?}"),
    }
    assert!(
        w.discovered.iter().all(|d| d.load(Ordering::SeqCst) == 0),
        "planned anyway"
    );
    assert!(w.plans.save_calls().is_empty());
}

#[test]
fn a_planner_that_blocks_should_return_a_handle_at_once_and_deliver_the_result_later() {
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let mut targets = standard();
    targets[1].gate = Some(Arc::clone(&gate));
    let w = wire(targets);
    let started = std::time::Instant::now();
    let h = w
        .actions
        .preview(&[p(BUILD)], Arc::new(AtomicBool::new(false)));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "preview blocked the caller"
    );
    assert!(matches!(h.rx.try_recv(), Err(TryRecvError::Empty)));
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    assert!(matches!(
        h.rx.recv_timeout(Duration::from_secs(20)),
        Ok(ScanMsg::Preview(_))
    ));
}

#[test]
fn esc_during_planning_should_drop_the_result_and_save_no_plan() {
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let mut targets = standard();
    targets[1].gate = Some(Arc::clone(&gate));
    let w = wire(targets);
    let cancel = Arc::new(AtomicBool::new(false));
    let h = w.actions.preview(&[p(BUILD)], Arc::clone(&cancel));
    h.cancel();
    assert!(cancel.load(Ordering::SeqCst));
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    assert!(matches!(
        h.rx.recv_timeout(Duration::from_secs(20)),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
    ));
    assert!(
        w.plans.save_calls().is_empty(),
        "a cancelled plan must not be saved"
    );
}

#[test]
fn a_later_plan_should_merge_beside_the_explorers_id_and_not_displace_it() {
    let w = wire(standard());
    let pv = preview(&w, &[BUILD]);
    // What a later dry run does: merge its own entries through the same store.
    let other = SavedSelection {
        plan_id: "deadbeef".into(),
        saved_at: NOW,
        euid: 1000,
        kinds: "caches".into(),
        flags: String::new(),
        items: Vec::new(),
        items_truncated: false,
    };
    let _ = w.plans.save_merged(vec![other], 1000, NOW);
    let ids: Vec<String> = w
        .plans
        .load_last()
        .unwrap()
        .entries
        .into_iter()
        .map(|e| e.plan_id)
        .collect();
    assert!(
        ids.contains(&pv.plan_id) && ids.contains(&"deadbeef".to_string()),
        "{ids:?}"
    );
}

#[test]
fn a_store_that_cannot_save_should_say_plan_id_not_usable_and_print_no_command_with_the_id() {
    let w = wire_with(standard(), Arc::new(FakePlanStore::failing(28)), None);
    let pv = preview(&w, &[BUILD]);
    assert!(!pv.saved);
    let lines = report(&w, &[BUILD]);
    let text = lines.join("\n");
    assert!(text.contains("plan-id NOT usable"), "{text}");
    assert!(
        !lines.iter().any(|l| l.contains("--plan-id")),
        "a command carries the unsaved id:\n{text}"
    );
    assert_eq!(lines.last().unwrap(), "DRY RUN - nothing was deleted.");
}

#[test]
fn the_report_should_be_the_dry_run_with_one_apply_line_per_tier() {
    let w = wire(standard());
    let lines = report(&w, &[BUILD, CACHE, WT]);
    let text = lines.join("\n");
    let first = &lines[0];
    let (head, id) = first.split_once("   plan-id ").expect(first);
    assert_eq!(head, "DRY RUN - nothing will be deleted.");
    assert!(is_hex8(id), "{first}");
    assert_eq!(lines.last().unwrap(), "DRY RUN - nothing was deleted.");
    assert!(!text.contains("--root"), "{text}");
    assert!(!text.bytes().any(|b| b == 0x1b));
    let cmds = commands(&lines);
    assert_eq!(cmds.len(), 3, "{text}");
    for c in &cmds {
        sh_accepts(c);
        assert!(c.starts_with("dunnage --apply --kinds "), "{c}");
        assert!(c.contains("--only"), "{c}");
    }
    let destructive = cmds
        .iter()
        .find(|c| c.contains("--kinds build-dirs"))
        .unwrap();
    assert!(
        destructive.contains(BUILD) && destructive.contains("--plan-id "),
        "{destructive}"
    );
    let tail = destructive.rsplit(' ').next().unwrap();
    assert!(
        is_hex8(tail),
        "a destructive line ends with its id: {destructive}"
    );
    for c in cmds.iter().filter(|c| !c.contains("--plan-id")) {
        let at = lines.iter().position(|l| l.trim() == c).unwrap();
        let after = lines[at + 1..]
            .iter()
            .take(4)
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        let squashed: String = after.split_whitespace().collect::<Vec<_>>().join(" ");
        let note: String = NO_PLAN_ID_ONLY_NOTE
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            squashed.contains(&note),
            "no explanation after {c}:\n{text}"
        );
    }
    assert_eq!(
        cmds.iter().filter(|c| c.contains("--kinds caches")).count(),
        1
    );
    assert_eq!(
        cmds.iter()
            .filter(|c| c.contains("--kinds worktrees"))
            .count(),
        1
    );
}

#[test]
fn a_destructive_line_without_one_of_its_only_values_should_not_carry_the_same_id() {
    let w = wire(standard());
    let both = commands(&report(&w, &[BUILD, OLD]));
    let one = commands(&report(&w, &[BUILD]));
    let id = |c: &str| c.rsplit(' ').next().unwrap().to_string();
    assert_eq!(both.len(), 1);
    assert_eq!(one.len(), 1);
    assert_ne!(
        id(&both[0]),
        id(&one[0]),
        "dropping a mark must change the id"
    );
}

#[test]
fn marks_the_cleaner_does_not_know_should_be_named_as_deleted_from_inside_explore_only() {
    let w = wire(standard());
    let lines = report(&w, &[BUILD, "/home/u/code/app/src"]);
    let text = lines.join("\n");
    assert!(text.contains("deleted from inside explore only"), "{text}");
    assert!(
        text.contains("/home/u/code/app/src") || text.contains("~/code/app/src"),
        "{text}"
    );
    assert_eq!(commands(&lines).len(), 1);
    // Only the unknown mark: nothing to apply, still a dry run.
    let only_unknown = report(&w, &["/home/u/code/app/src"]);
    assert!(commands(&only_unknown).is_empty());
    assert_eq!(
        only_unknown.last().unwrap(),
        "DRY RUN - nothing was deleted."
    );
}

#[test]
fn leaving_without_marks_should_print_only_the_closing_line() {
    let w = wire(standard());
    let lines = report(&w, &[]);
    assert_eq!(lines, vec![CLOSED_LINE.to_string()]);
    assert!(w.discovered.iter().all(|d| d.load(Ordering::SeqCst) == 0));
}

fn hostile_names() -> Vec<Vec<u8>> {
    let s = |t: &str| t.as_bytes().to_vec();
    vec![
        s("plain"),
        s("a b"),
        s("it's"),
        s("say \"hi\""),
        s("$(touch INJECTED)"),
        s("`touch INJECTED`"),
        s("a\nb"),
        s("-rf"),
        s("a\tb\x1b[2Jc"),
        s("bidi\u{202e}gpj"),
        b"bad\xffbyte".to_vec(),
        s("a'; rm -rf ~; '"),
        s("a;b&c|d"),
        s("*?[x]{a,b}"),
        s("=ls"),
        s("back\\slash"),
        s("!!"),
        s("caf\u{e9}"),
        s("\u{65e5}\u{672c}"),
    ]
}

#[test]
fn hostile_marked_names_should_reach_the_cleaner_as_one_argument_and_inject_nothing() {
    let mut shells = vec!["bash", "zsh"];
    if find_on_path("dash").is_some() {
        shells.push("dash");
    }
    let mut runs = 0;
    for name in hostile_names() {
        let mut path = b"/home/u/.cache/".to_vec();
        path.extend_from_slice(&name);
        let path_os = PathBuf::from(OsString::from_vec(path.clone()));
        let c = Candidate::new(
            dunnage_core::candidate::Locator::Path(path_os.clone()),
            dunnage_core::candidate::KindFacts::Cache(dunnage_core::candidate::CacheFacts {
                label: "cache".into(),
                apt: false,
            }),
            dunnage_core::AllocatedBytes(1 << 30),
            None,
            dunnage_core::ids::Identity { dev: 1, ino: 77 },
        );
        let w = wire(vec![Scripted::new(&[SelectorId::Caches], vec![c])]);
        let h = w.actions.finish(std::slice::from_ref(&path_os));
        let raw: Vec<Vec<u8>> = h.lines.iter().map(|l| l.as_bytes().to_vec()).collect();
        assert!(
            raw.iter().all(|l| !l.contains(&0x1b)),
            "an escape byte reached the report: {name:?}"
        );
        let cmds: Vec<String> = raw
            .iter()
            .filter(|l| l.starts_with(b"  dunnage --apply"))
            .map(|l| {
                String::from_utf8(l.clone())
                    .expect("command lines are UTF-8")
                    .trim()
                    .to_string()
            })
            .collect();
        assert_eq!(cmds.len(), 1, "{name:?}: {:?}", h.lines);
        for shell in &shells {
            let dir = tempfile::tempdir().unwrap();
            let run = run_printed_line(dir.path(), shell, &cmds[0])
                .unwrap()
                .unwrap();
            if *shell == "dash" && cmds[0].contains("$'") {
                // dash has no `$'..'`: the argument cannot arrive intact, but the line must stay inert
                assert!(run.injected.is_empty(), "{shell}: {:?}", run.injected);
                assert!(!dir.path().join("INJECTED").exists());
                continue;
            }
            let argv = run
                .argv_of("dunnage")
                .unwrap_or_else(|| panic!("{shell}: {} did not run: {}", cmds[0], run.stderr));
            let at = argv.iter().position(|a| a == b"--only").expect("--only");
            assert_eq!(argv[at + 1], path, "{shell}: {}", cmds[0]);
            assert!(run.injected.is_empty(), "{shell}: {:?}", run.injected);
            assert!(!dir.path().join("INJECTED").exists());
            runs += 1;
        }
    }
    assert!(runs >= 19 * 2, "{runs}");
}

#[test]
fn the_planner_should_never_execute_an_item() {
    let w = wire(standard());
    let _ = preview(&w, &[BUILD, CACHE, WT]);
    let _ = report(&w, &[BUILD, CACHE, WT]);
    assert!(w.executed.iter().all(|e| e.load(Ordering::SeqCst) == 0));
    let _ = (
        DiskVerdict::Unknown,
        Dirs {
            home: PathBuf::new(),
            state: PathBuf::new(),
            cache: PathBuf::new(),
            config: PathBuf::new(),
        },
    );
}
