//! Hostile-mark suite of the explorer-deletion apply path (Task 6.6.3d, ADR-003 process requirement 2).
//!
//! Written by a fresh author from the specification alone: Story 6.6.3 (the TOCTOU list and the adversarial
//! suites AC), the rule table M1 to M8 of `ux-explorer.md` 1.2, ADR-006 with its 2026-11-11 amendment, ADR-003
//! and the two architecture reviews read as threat lists. The implementation bodies were not read.
//!
//! Two layers are driven for every hostile mark:
//!  * the plan (`explorer_plan`): the row of the mark must not be `Will`;
//!  * the apply (`compose_explorer_apply`) over a *forged* `ReviewedPlan` that lists the hostile path with the
//!    identity `lstat` gives it now, i.e. a plan that lied or went stale. The apply re-checks M1 to M5 itself,
//!    so it must refuse the item and leave the tree intact.
//!
//! Every case ends with the fixture tree intact and no deletion without an `intent` line in the audit log,
//! except the one allowed directory, which is deleted and logged.
//!
//! Run: `GATE=D TMPDIR=/tmp cargo test -p dunnage --features dunnage/gate-d --test explore_hostile`.
//! Mount cases re-run themselves under `unshare -rm` (a named SKIP where that does not work;
//! `DC_REQUIRE_UNSHARE=1` or `DC_REQUIRE_BIND_MOUNT=1` makes the skip a failure).
#![cfg(feature = "gate-d")]
#![allow(
    clippy::too_many_lines,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cloned_ref_to_slice_refs,
    clippy::unnecessary_debug_formatting,
    clippy::unused_self,
    non_snake_case
)]

mod common;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{symlink, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use common::Fixture;
use dunnage_adapters::dirops::{DirOps, RealDir, RealReadDir};
use dunnage_adapters::{RealDirOps, RealProbe};
use dunnage_cli::explore::apply::{
    compose_explorer_apply, skip_text, ExplorerApplyPorts, ExplorerReport, ExplorerRun,
};
use dunnage_cli::wiring::audit::StampOutcome;
use dunnage_cli::wiring::compose::Trace;
use dunnage_cli::wiring::env::{tool_dirs, RealFs, SystemClock, Wired};
use dunnage_cli::wiring::explore::canonical_lenient;
use dunnage_core::audit_types::{ExplorerConfirm, Intent, ItemResult, RunEnd, RunStart};
use dunnage_core::delete_types::{DirEntryInfo, StatInfo};
use dunnage_core::dirs::Dirs;
use dunnage_core::explore::confirm::{Confirm, ConfirmedPlan, ReviewedItem, ReviewedPlan};
use dunnage_core::explore::mark_check::OwnState;
use dunnage_core::explorer_plan::{
    explorer_plan, ExplorerPlan, ExplorerPlanCtx, NoCleanerView, TreeSize, TreeSizer,
};
use dunnage_core::ids::AllocatedBytes;
use dunnage_core::kind::Kind;
use dunnage_core::outcome::Status;
use dunnage_core::plan::{ItemIdentity, OnlyMode, Planned};
use dunnage_core::plan_state::{BuildInfo, ItemKey, SaveOutcome, SavedSelection};
use dunnage_core::ports::{AuditSink, Clock, Env, Fs, SystemProbe};
use dunnage_core::roots::{RootIdentity, RootSet};
use dunnage_core::selector::Selection;
use dunnage_core::{ApplyToken, Identity};

// ---------------------------------------------------------------------------------------------------------
// Namespace plumbing
// ---------------------------------------------------------------------------------------------------------

const NS_ENV: &str = "DC_HOSTILE_IN_NS";

/// Runs the calling test again under `unshare -rm` and returns `false` (the caller returns); inside the child
/// it returns `true` and the body runs. Without a working `unshare` it is a named SKIP unless a require
/// variable is set.
fn enter_ns(test: &str, require_var: &str) -> bool {
    if std::env::var_os(NS_ENV).is_some() {
        return true;
    }
    let works = Command::new("unshare")
        .args(["-rm", "true"])
        .output()
        .is_ok_and(|o| o.status.success());
    if !works {
        assert!(
            std::env::var_os(require_var).is_none()
                && std::env::var_os("DC_REQUIRE_UNSHARE").is_none(),
            "{require_var} or DC_REQUIRE_UNSHARE is set and `unshare -rm` does not work here"
        );
        eprintln!(
            "SKIP {test}: `unshare -rm` does not work here ({require_var}=1 makes this a failure)"
        );
        return false;
    }
    let exe = std::env::current_exe().expect("test exe");
    let out = Command::new("unshare")
        .args(["-rm"])
        .arg(exe)
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(NS_ENV, "1")
        .env(
            "DC_HOSTILE_REQUIRE",
            if std::env::var_os(require_var).is_some() {
                "1"
            } else {
                ""
            },
        )
        .output()
        .expect("spawn unshare");
    let so = String::from_utf8_lossy(&out.stdout);
    let se = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "{test} failed under unshare -rm\n--- stdout ---\n{so}\n--- stderr ---\n{se}"
    );
    for l in so
        .lines()
        .chain(se.lines())
        .filter(|l| l.starts_with("SKIP"))
    {
        eprintln!("{l}");
    }
    false
}

/// Inside the namespace: runs `mount args`; `false` after a named SKIP when the kernel refuses and nothing
/// requires the mount.
fn mount_or_skip(what: &str, args: &[&str]) -> bool {
    let ok = Command::new("mount")
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        assert!(
            std::env::var_os("DC_HOSTILE_REQUIRE").is_none_or(|v| v.is_empty()),
            "DC_REQUIRE_BIND_MOUNT=1 and `mount {args:?}` failed ({what})"
        );
        println!("SKIP {what}: `mount {args:?}` is refused here (DC_REQUIRE_BIND_MOUNT=1 makes this a failure)");
    }
    ok
}

// ---------------------------------------------------------------------------------------------------------
// Fixture, ports, rig
// ---------------------------------------------------------------------------------------------------------

/// Sums apparent bytes (so a sparse file made with `truncate` counts without real data), counts `.git`.
struct WalkSizer;

impl WalkSizer {
    fn walk(p: &Path, acc: &mut (u64, i64, u64)) {
        let Ok(m) = std::fs::symlink_metadata(p) else {
            return;
        };
        acc.0 += m.len();
        acc.1 = acc.1.max(m.mtime());
        if p.file_name().is_some_and(|n| n == ".git") {
            acc.2 += 1;
        }
        if m.is_dir() {
            if let Ok(rd) = std::fs::read_dir(p) {
                for e in rd.flatten() {
                    WalkSizer::walk(&e.path(), acc);
                }
            }
        }
    }
}

impl TreeSizer for WalkSizer {
    fn measure(&self, path: &Path) -> io::Result<TreeSize> {
        let mut acc = (0u64, 0i64, 0u64);
        WalkSizer::walk(path, &mut acc);
        Ok(TreeSize {
            allocated: AllocatedBytes(acc.0),
            newest_mtime: Some(acc.1),
            git_repos: acc.2,
        })
    }
}

type Hook = Box<dyn Fn() + Send + Sync>;

/// The real audit sink with optional faults: a hook run once the `explorer_confirm` line is written (after
/// the plan-id gate, before the re-measure and any item), a panic in `result` (a crash between `intent` and
/// `result`), and an error at `explorer_confirm`.
struct FaultSink<'a> {
    inner: &'a dyn AuditSink,
    after_confirm: Mutex<Option<Hook>>,
    panic_in_result: AtomicBool,
}

impl AuditSink for FaultSink<'_> {
    fn run_start(&self, rec: &RunStart) -> io::Result<()> {
        self.inner.run_start(rec)
    }
    fn intent(&self, rec: &Intent) -> io::Result<()> {
        self.inner.intent(rec)
    }
    fn result(&self, rec: &ItemResult) -> io::Result<()> {
        assert!(
            !self.panic_in_result.load(Ordering::SeqCst),
            "injected crash between intent and result"
        );
        self.inner.result(rec)
    }
    fn run_end(&self, rec: &RunEnd) -> io::Result<()> {
        self.inner.run_end(rec)
    }
    fn explorer_confirm(&self, rec: &ExplorerConfirm) -> io::Result<()> {
        self.inner.explorer_confirm(rec)?;
        if let Some(h) = self.after_confirm.lock().unwrap().take() {
            h();
        }
        Ok(())
    }
}

/// What a run left behind.
struct Applied {
    report: Option<ExplorerReport>,
    trace: Trace,
    log: Vec<serde_json::Value>,
    panicked: bool,
}

/// When the plan the owner reviewed was saved in `last_plan`, relative to now.
#[derive(Default, Clone, Copy)]
enum SavedAt {
    #[default]
    Now,
    /// Seconds in the future (a clock that stepped back after the save).
    Future(i64),
    /// Never saved.
    Never,
}

#[derive(Default)]
struct RunOpts {
    after_confirm: Option<Hook>,
    panic_in_result: bool,
    binary: Option<PathBuf>,
    saved: SavedAt,
    /// Save the entry of this other set instead of the reviewed one (a plan-id presented for another set).
    saved_marks: Option<Vec<PathBuf>>,
}

struct Rig {
    fx: Fixture,
    base: Dirs,
    cfg_roots: Vec<PathBuf>,
    /// The "running binary" the own-state rule must protect.
    binary: PathBuf,
}

fn canon(p: &Path) -> io::Result<(PathBuf, Identity)> {
    let c = std::fs::canonicalize(p)?;
    let m = std::fs::metadata(&c)?;
    Ok((c, Identity::of(&m)))
}

impl Rig {
    /// `$HOME` is `<fixture>/home`, the one scan root `~/code`.
    fn new() -> Rig {
        Rig::with(|home| vec![home.join("code")])
    }

    fn with(roots: impl FnOnce(&Path) -> Vec<PathBuf>) -> Rig {
        Rig::with_home("home", |_top, home| roots(home))
    }

    /// `$HOME` is `<fixture>/<home_rel>`; `roots` gets the fixture top and `$HOME`.
    fn with_home(home_rel: &str, roots: impl FnOnce(&Path, &Path) -> Vec<PathBuf>) -> Rig {
        let fx = Fixture::new();
        let root = std::fs::canonicalize(fx.root()).unwrap();
        let home = root.join(home_rel);
        std::fs::create_dir_all(&home).unwrap();
        let base = Dirs {
            home: home.clone(),
            state: root.join("xdg/state"),
            cache: root.join("xdg/cache"),
            config: root.join("xdg/config"),
        };
        let cfg_roots = roots(&root, &home);
        for r in &cfg_roots {
            if !r.starts_with("/proc") {
                std::fs::create_dir_all(r).unwrap();
            }
        }
        let binary = root.join("opt/dunnage/bin/dunnage");
        Rig {
            fx,
            base,
            cfg_roots,
            binary,
        }
    }

    fn top(&self) -> PathBuf {
        std::fs::canonicalize(self.fx.root()).unwrap()
    }

    fn home(&self) -> PathBuf {
        self.base.home.clone()
    }

    fn dir(&self, p: impl AsRef<Path>) -> PathBuf {
        let p = p.as_ref();
        let full = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.top().join(p)
        };
        std::fs::create_dir_all(&full).unwrap();
        full
    }

    fn file(&self, p: impl AsRef<Path>, bytes: &[u8]) -> PathBuf {
        let p = p.as_ref();
        let full = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.top().join(p)
        };
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, bytes).unwrap();
        full
    }

    /// A directory with a witness file, below `~/code` by default.
    fn victim(&self, rel: &str) -> PathBuf {
        let d = self.dir(self.home().join("code").join(rel));
        self.file(d.join("witness.bin"), &[7u8; 4096]);
        d
    }

    fn roots(&self) -> Result<RootSet, String> {
        RootSet::resolve_on(
            &self.base,
            &[],
            Some(&self.cfg_roots),
            &self.home(),
            &canon,
            &|p| RealFs.fs_type(p),
        )
        .map_err(|e| format!("{e:?}"))
    }

    fn own_state(&self, binary: &Path) -> OwnState {
        OwnState::canonical(&self.base, binary.to_path_buf(), &canonical_lenient)
    }

    fn plan(&self, marks: &[PathBuf]) -> Result<ExplorerPlan, String> {
        let roots = self.roots()?;
        let own = self.own_state(&self.binary);
        let ctx = ExplorerPlanCtx {
            roots: &roots,
            own_state: &own,
            fs: &RealFs,
            probe: &RealProbe,
            clock: &SystemClock,
            sizer: &WalkSizer,
            extents: None,
            cleaner: &NoCleanerView,
        };
        explorer_plan(marks, OnlyMode::Explorer, &ctx).map_err(|e| e.text().to_string())
    }

    /// The row of `path` in the plan of `marks`: `Some(planned)`, or `None` when the plan refused the set.
    fn planned_of(&self, marks: &[PathBuf], path: &Path) -> Option<Planned> {
        let p = self.plan(marks).ok()?;
        p.rows.iter().find(|r| r.path == path).map(|r| r.planned)
    }

    fn root_identities(&self) -> Vec<RootIdentity> {
        let Some(first) = self.cfg_roots.first().filter(|r| !r.starts_with("/proc")) else {
            return Vec::new();
        };
        let anchor = self.dir(first.join(".dc-anchor-for-roots"));
        let roots = self
            .plan(&[anchor.clone()])
            .map(|p| p.roots)
            .unwrap_or_default();
        std::fs::remove_dir_all(&anchor).unwrap();
        roots
    }

    /// The honest plan of `marks`, reviewed: only what the plan says `Will`.
    fn reviewed(&self, marks: &[PathBuf]) -> ReviewedPlan {
        ReviewedPlan::from_explorer_plan(&self.plan(marks).expect("plan"))
    }

    /// A plan that lists every mark with the identity `lstat` reports now, whatever the rules say: what a
    /// stale or lying plan would hold.
    fn forged(&self, marks: &[PathBuf]) -> ReviewedPlan {
        let roots = self.root_identities();
        let bytes = marks
            .iter()
            .map(|p| WalkSizer.measure(p).map_or(0, |t| t.allocated.0))
            .sum();
        let items = marks
            .iter()
            .map(|p| ReviewedItem {
                path: p.clone(),
                identity: ItemIdentity::of(&RealFs.lstat(p).expect("lstat of a forged mark")),
            })
            .collect();
        ReviewedPlan::new(items, roots, bytes)
    }

    fn confirm(reviewed: ReviewedPlan) -> ConfirmedPlan {
        let n = reviewed.count().to_string();
        Confirm::new(reviewed)
            .accepts(&n)
            .unwrap_or_else(|e| panic!("typed count refused: {e}"))
    }

    fn audit_path(&self) -> PathBuf {
        tool_dirs(&self.base).state.join("audit.jsonl")
    }

    fn apply(&self, reviewed: ReviewedPlan, opts: RunOpts) -> Applied {
        let top = self.top();
        self.apply_ops(reviewed, opts, &|_t: &ApplyToken| {
            RealDirOps::confined_to(&top)
        })
    }

    fn apply_ops<D: DirOps + 'static>(
        &self,
        reviewed: ReviewedPlan,
        opts: RunOpts,
        ops: &dyn Fn(&ApplyToken) -> D,
    ) -> Applied {
        let wired = Wired::build(&self.base, true);
        match &opts.saved_marks {
            Some(m) => self.save(&wired, &self.forged(m), opts.saved),
            None => self.save(&wired, &reviewed, opts.saved),
        }
        let confirmed = Rig::confirm(reviewed);
        let sink = FaultSink {
            inner: &wired.audit,
            after_confirm: Mutex::new(opts.after_confirm),
            panic_in_result: AtomicBool::new(opts.panic_in_result),
        };
        let mut env: Env<'_> = wired.env();
        env.audit = &sink;
        let binary = opts.binary.clone().unwrap_or_else(|| self.binary.clone());
        let own = self.own_state(&binary);
        let stamp = |_gate: char| Ok(StampOutcome::Confined);
        let mint = ApplyToken::for_tests;
        let resolve = || self.roots();
        let ports = ExplorerApplyPorts {
            env: &env,
            stamp: &stamp,
            mint: &mint,
            ops,
            resolve_roots: &resolve,
            own_state: own,
            sizer: &WalkSizer,
            extents: None,
            build: BuildInfo {
                bin_version: "test".into(),
                git_commit: "test".into(),
                dirty: false,
                metascope_rev: "test".into(),
                patched: false,
                metascope_dirty: false,
            },
            run_id: "hostile-run".into(),
            tty: true,
        };
        let run = ExplorerRun::headless();
        let mut trace = Trace::default();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            compose_explorer_apply(confirmed, &ports, &run, &mut trace)
        }));
        let (report, panicked) = match r {
            Ok(rep) => (Some(rep), false),
            Err(_) => (None, true),
        };
        Applied {
            report,
            trace,
            log: self.log(),
            panicked,
        }
    }

    /// Saves the reviewed set in `last_plan` the way a plan-mode run does (the gate needs a saved entry).
    fn save(&self, wired: &Wired, reviewed: &ReviewedPlan, when: SavedAt) {
        let now = SystemClock.now_epoch();
        let at = match when {
            SavedAt::Now => now,
            SavedAt::Future(s) => now + s,
            SavedAt::Never => return,
        };
        let marks: Vec<PathBuf> = reviewed.items().iter().map(|i| i.path.clone()).collect();
        let sel = Selection::explorer(marks.clone());
        let mut keys: Vec<String> = marks
            .iter()
            .map(|p| format!("{}:{}", Kind::ExplorerSelection.id(), p.display()))
            .collect();
        keys.sort();
        let entry = SavedSelection {
            plan_id: reviewed.plan_id().to_string(),
            saved_at: at,
            euid: RealProbe.euid(),
            kinds: sel.kinds_resolved(),
            flags: sel.flags(),
            items: keys.iter().map(|k| ItemKey::new(k)).collect(),
            items_truncated: false,
        };
        let out = wired.plans.save_merged(vec![entry], RealProbe.euid(), at);
        assert!(
            matches!(out, SaveOutcome::Saved | SaveOutcome::SavedAfterReserve),
            "the plan was not saved: {out:?}"
        );
    }

    fn log(&self) -> Vec<serde_json::Value> {
        std::fs::read_to_string(self.audit_path())
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }
}

// ---------------------------------------------------------------------------------------------------------
// Observations
// ---------------------------------------------------------------------------------------------------------

type Snap = BTreeMap<PathBuf, String>;

/// Everything under the fixture except the tool's own XDG directories (they change with every run).
fn snap(rig: &Rig) -> Snap {
    snap_at(&rig.top())
}

fn snap_at(top: &Path) -> Snap {
    fn walk(p: &Path, skip: &Path, out: &mut Snap) {
        if p == skip || p.file_name().is_some_and(|n| n == "xdg") {
            return;
        }
        let Ok(m) = std::fs::symlink_metadata(p) else {
            return;
        };
        let what = if m.file_type().is_symlink() {
            format!("link->{:?}", std::fs::read_link(p).unwrap_or_default())
        } else {
            format!(
                "{}:{}:{}",
                if m.is_dir() { "d" } else { "f" },
                m.ino(),
                m.len()
            )
        };
        out.insert(p.to_path_buf(), what);
        if m.is_dir() {
            if let Ok(rd) = std::fs::read_dir(p) {
                for e in rd.flatten() {
                    walk(&e.path(), skip, out);
                }
            }
        }
    }
    let mut out = Snap::new();
    walk(top, &top.join("xdg"), &mut out);
    out
}

fn strings_of(v: &serde_json::Value, out: &mut Vec<String>) {
    match v {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(a) => a.iter().for_each(|x| strings_of(x, out)),
        serde_json::Value::Object(o) => o.values().for_each(|x| strings_of(x, out)),
        _ => {}
    }
}

fn line_type(v: &serde_json::Value) -> &str {
    if let Some(t) = v.get("type").and_then(|t| t.as_str()) {
        return t;
    }
    if v.get("status").is_some() {
        "result"
    } else {
        "other"
    }
}

fn intent_paths(log: &[serde_json::Value]) -> Vec<String> {
    let mut out = Vec::new();
    for v in log.iter().filter(|v| line_type(v) == "intent") {
        strings_of(v, &mut out);
    }
    out
}

fn count_type(log: &[serde_json::Value], t: &str) -> usize {
    log.iter().filter(|v| line_type(v) == t).count()
}

/// Nothing that existed before is gone.
fn assert_intact(before: &Snap, after: &Snap, what: &str) {
    let gone: Vec<_> = before.keys().filter(|k| !after.contains_key(*k)).collect();
    assert!(gone.is_empty(), "{what}: the tree lost {gone:?}");
    let changed: Vec<_> = before
        .iter()
        .filter(|(k, v)| after.get(*k).is_some_and(|a| a != *v))
        .map(|(k, _)| k)
        .collect();
    assert!(
        changed.is_empty(),
        "{what}: the tree changed at {changed:?}"
    );
}

/// Every vanished path lies below a path with an `intent` line: no unlogged deletion.
fn assert_all_deletions_logged(before: &Snap, after: &Snap, log: &[serde_json::Value], what: &str) {
    let intents = intent_paths(log);
    for k in before.keys().filter(|k| !after.contains_key(*k)) {
        let ks = k.to_string_lossy();
        assert!(
            intents.iter().any(|i| ks.starts_with(i.as_str())),
            "{what}: {k:?} was deleted with no intent line (intents: {intents:?})"
        );
    }
}

fn no_removal(a: &Applied) -> bool {
    a.report
        .as_ref()
        .is_none_or(|r| r.outcomes.iter().all(|o| o.status != Status::Removed))
}

/// The run deleted nothing: whole-run refusal, or every item skipped or errored; tree intact, no `intent`
/// without its refusal, nothing removed.
fn assert_refused_and_intact(rig: &Rig, before: &Snap, a: &Applied, what: &str) {
    let after = snap(rig);
    assert!(!a.panicked, "{what}: the apply panicked");
    assert!(no_removal(a), "{what}: an item was removed: {:?}", a.report);
    assert_intact(before, &after, what);
    assert_all_deletions_logged(before, &after, &a.log, what);
}

fn assert_skipped_with(a: &Applied, needle: &str, what: &str) {
    let r = a.report.as_ref().expect("report");
    let texts: Vec<String> = r
        .outcomes
        .iter()
        .map(|o| match &o.status {
            Status::Skipped(s) => skip_text(*s),
            other => format!("{other:?}"),
        })
        .collect();
    assert!(
        !texts.is_empty() && texts.iter().all(|t| t.to_lowercase().contains(needle)),
        "{what}: expected every item to read `{needle}`, got {texts:?} (refused: {:?})",
        r.refused
    );
}

fn refused_text(a: &Applied) -> String {
    a.report
        .as_ref()
        .and_then(|r| r.refused.clone())
        .unwrap_or_default()
}

fn outcome_paths(a: &Applied) -> Vec<PathBuf> {
    a.report
        .as_ref()
        .map(|r| r.outcomes.iter().filter_map(|o| o.path.clone()).collect())
        .unwrap_or_default()
}

/// A mark that must not be deleted: the plan does not say `Will`, and a forged plan that does is refused by
/// the apply, with the tree intact.
fn assert_hostile_mark(rig: &Rig, mark: &Path, what: &str) {
    let before = snap(rig);
    let planned = rig.planned_of(&[mark.to_path_buf()], mark);
    assert!(
        !matches!(planned, Some(Planned::Will)),
        "{what}: the plan lists {mark:?} as Will"
    );
    assert_intact(&before, &snap(rig), &format!("{what} (plan)"));
    let a = rig.apply(rig.forged(&[mark.to_path_buf()]), RunOpts::default());
    assert_refused_and_intact(
        rig,
        &before,
        &a,
        &format!("{what} (apply of a forged plan)"),
    );
    assert!(
        !refused_text(&a).contains("Changed since you confirmed"),
        "{what}: the forged plan was stopped by the re-measure, not by the rules: {:?}",
        a.report
    );
}

fn dir_size_bytes_sparse(path: &Path, bytes: u64) {
    let f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .unwrap();
    f.set_len(bytes).unwrap();
}

// ---------------------------------------------------------------------------------------------------------
// The one allowed case (control): a directory with no sibling marker below a root is deleted and logged
// ---------------------------------------------------------------------------------------------------------

#[test]
fn safety_explorer_hostile_should_delete_and_log_a_plain_directory_When_it_has_no_sibling_marker_below_a_root(
) {
    let rig = Rig::new();
    let app = rig.dir(rig.home().join("code/app"));
    let keep = rig.file(app.join("src/main.rs"), b"fn main() {}");
    let dist = rig.victim("app/dist");
    let before = snap(&rig);
    let plan = rig.plan(&[dist.clone()]).expect("plan");
    assert_eq!(plan.rows.len(), 1);
    assert_eq!(
        plan.rows[0].planned,
        Planned::Will,
        "a plain directory below a root is allowed and listed"
    );
    let a = rig.apply(ReviewedPlan::from_explorer_plan(&plan), RunOpts::default());
    let after = snap(&rig);
    let r = a.report.as_ref().expect("report");
    assert_eq!(r.outcomes.len(), 1, "{r:?}");
    assert_eq!(r.outcomes[0].status, Status::Removed, "{r:?}");
    assert!(!dist.exists(), "the allowed directory is deleted");
    assert!(keep.exists(), "its sibling survives");
    assert_all_deletions_logged(&before, &after, &a.log, "allowed delete");
    assert_eq!(count_type(&a.log, "intent"), 1, "{:?}", a.log);
    assert_eq!(count_type(&a.log, "result"), 1, "{:?}", a.log);
    assert_eq!(count_type(&a.log, "run_end"), 1, "{:?}", a.log);
    assert_eq!(count_type(&a.log, "explorer_confirm"), 1, "{:?}", a.log);
    assert_eq!(outcome_paths(&a), vec![dist]);
}

// ---------------------------------------------------------------------------------------------------------
// More helpers
// ---------------------------------------------------------------------------------------------------------

fn tname() -> String {
    std::thread::current()
        .name()
        .expect("test thread name")
        .to_string()
}

fn lazy_umount(p: &Path) {
    let _ = Command::new("umount").arg("-l").arg(p).output();
}

fn s(p: &Path) -> &str {
    p.to_str().expect("utf-8 fixture path")
}

/// `DirOps` that hides the mount id of every `fstat` (a kernel without `statx` mount ids): the engine's
/// `root_mnt` is then unknown and the item must fail closed.
struct NoMntOps(RealDirOps);

impl DirOps for NoMntOps {
    type Dir = RealDir;
    type ReadDir = RealReadDir;
    fn open_root(&self, canonical_base: &Path) -> io::Result<RealDir> {
        self.0.open_root(canonical_base)
    }
    fn open_dir(&self, parent: &RealDir, name: &std::ffi::OsStr) -> io::Result<RealDir> {
        self.0.open_dir(parent, name)
    }
    fn fstat(&self, d: &RealDir) -> io::Result<StatInfo> {
        self.0.fstat(d).map(|mut st| {
            st.mnt_id = None;
            st
        })
    }
    fn stat_at(&self, d: &RealDir, name: &std::ffi::OsStr) -> io::Result<StatInfo> {
        self.0.stat_at(d, name)
    }
    fn read_dir(&self, d: &RealDir) -> io::Result<RealReadDir> {
        self.0.read_dir(d)
    }
    fn unlinkat(&self, d: &RealDir, name: &std::ffi::OsStr, dir: bool) -> io::Result<()> {
        self.0.unlinkat(d, name, dir)
    }
    fn fchmod(&self, d: &RealDir, mode: u32) -> io::Result<()> {
        self.0.fchmod(d, mode)
    }
}

#[allow(dead_code)]
fn _entry_type_is_used(_: DirEntryInfo) {}

/// A TOCTOU case: plan an allowed directory, change the world, apply. `hook` runs after the plan-id gate and
/// the `explorer_confirm` line (the window between the confirmation and the first `fstat`); otherwise the
/// change is made before the apply starts. Returns the snapshot taken right after the change and the run.
fn toctou(
    rig: &Rig,
    item: &Path,
    change: impl Fn() + Send + Sync + 'static,
    hook: bool,
) -> (Snap, Applied) {
    let reviewed = rig.reviewed(&[item.to_path_buf()]);
    assert_eq!(
        reviewed.count(),
        1,
        "the item must be allowed before the change"
    );
    let mid: Arc<Mutex<Option<Snap>>> = Arc::new(Mutex::new(None));
    if hook {
        let (m, top) = (mid.clone(), rig.top());
        let a = rig.apply(
            reviewed,
            RunOpts {
                after_confirm: Some(Box::new(move || {
                    change();
                    *m.lock().unwrap() = Some(snap_at(&top));
                })),
                ..RunOpts::default()
            },
        );
        let mid = mid.lock().unwrap().take().expect("the hook ran");
        (mid, a)
    } else {
        change();
        let mid = snap(rig);
        (mid, rig.apply(reviewed, RunOpts::default()))
    }
}

/// After a swap nothing may have been deleted or touched, wherever the swap was made.
fn assert_swap_refused(rig: &Rig, mid: &Snap, a: &Applied, what: &str) {
    assert_refused_and_intact(rig, mid, a, what);
}

fn swap_for_symlink(dist: PathBuf, keep: PathBuf) -> impl Fn() + Send + Sync + 'static {
    move || {
        std::fs::rename(&dist, dist.with_extension("orig")).unwrap();
        symlink(&keep, &dist).unwrap();
    }
}

fn swap_for_new_inode(dist: PathBuf) -> impl Fn() + Send + Sync + 'static {
    move || {
        let fresh = dist.with_extension("new");
        std::fs::create_dir(&fresh).unwrap();
        std::fs::write(fresh.join("witness.bin"), [9u8; 4096]).unwrap();
        std::fs::rename(&dist, dist.with_extension("old")).unwrap();
        std::fs::rename(&fresh, &dist).unwrap();
    }
}

// ---------------------------------------------------------------------------------------------------------
// M2: mount points and bind mounts
// ---------------------------------------------------------------------------------------------------------

#[test]
fn safety_explorer_hostile_should_refuse_a_mount_point_When_a_tmpfs_is_mounted_on_the_mark() {
    if !enter_ns(&tname(), "DC_REQUIRE_UNSHARE") {
        return;
    }
    let rig = Rig::new();
    let mnt = rig.dir(rig.home().join("code/app/mnt"));
    if !mount_or_skip("mount point", &["-t", "tmpfs", "tmpfs", s(&mnt)]) {
        return;
    }
    rig.file(mnt.join("inside.bin"), b"data");
    assert_hostile_mark(&rig, &mnt, "tmpfs mount point");
    assert!(mnt.join("inside.bin").exists());
    lazy_umount(&mnt);
}

#[test]
fn safety_explorer_hostile_should_refuse_a_bind_mount_When_it_is_marked_and_leave_its_source_intact(
) {
    if !enter_ns(&tname(), "DC_REQUIRE_BIND_MOUNT") {
        return;
    }
    let rig = Rig::new();
    let keep = rig.victim("keep");
    let bound = rig.dir(rig.home().join("code/app/bound"));
    if !mount_or_skip("bind mount", &["--bind", s(&keep), s(&bound)]) {
        return;
    }
    assert_hostile_mark(&rig, &bound, "bind mount");
    assert!(
        keep.join("witness.bin").exists(),
        "the bind mount's source survives"
    );
    lazy_umount(&bound);
}

// ---------------------------------------------------------------------------------------------------------
// M1: roots, $HOME and its ancestors
// ---------------------------------------------------------------------------------------------------------

#[test]
fn safety_explorer_hostile_should_refuse_an_ancestor_of_home_When_a_root_of_data_has_home_below_it()
{
    let rig = Rig::with_home("data/home/u", |top, _| vec![top.join("data")]);
    let home = rig.home();
    rig.file(home.join("precious.txt"), b"mine");
    let ancestor = rig.top().join("data/home");
    let before = snap(&rig);
    let err = rig.roots().expect_err("a root that holds $HOME is refused");
    assert!(err.contains("HomeOrAncestor"), "{err}");
    assert!(
        rig.plan(&[ancestor.clone()]).is_err(),
        "no plan without roots"
    );
    let a = rig.apply(rig.forged(&[ancestor.clone()]), RunOpts::default());
    assert!(
        a.report.as_ref().is_some_and(|r| r.refused.is_some()),
        "{:?}",
        a.report
    );
    assert_refused_and_intact(&rig, &before, &a, "ancestor of $HOME, root /data");
    assert!(home.join("precious.txt").exists());
}

#[test]
fn safety_explorer_hostile_should_refuse_an_ancestor_of_home_When_another_root_does_not_cover_it() {
    let rig = Rig::with_home("data/home/u", |top, _| vec![top.join("data/other")]);
    rig.file(rig.home().join("precious.txt"), b"mine");
    assert_hostile_mark(&rig, &rig.top().join("data/home"), "ancestor of $HOME");
    assert_hostile_mark(&rig, &rig.home(), "$HOME itself");
}

#[test]
fn safety_explorer_hostile_should_refuse_the_roots_When_a_root_equals_the_parent_of_home() {
    let rig = Rig::with_home("home", |top, _| vec![top.to_path_buf()]);
    let home = rig.home();
    rig.file(home.join("precious.txt"), b"mine");
    let before = snap(&rig);
    let err = rig
        .roots()
        .expect_err("a root equal to $HOME's parent is refused");
    assert!(err.contains("HomeOrAncestor"), "{err}");
    assert!(rig.plan(&[home.clone()]).is_err());
    let a = rig.apply(rig.forged(&[home.clone()]), RunOpts::default());
    assert!(
        a.report.as_ref().is_some_and(|r| r.refused.is_some()),
        "{:?}",
        a.report
    );
    assert_refused_and_intact(&rig, &before, &a, "root = parent of $HOME");
}

#[test]
fn safety_explorer_hostile_should_refuse_a_proc_root_When_config_names_a_pseudo_filesystem() {
    let rig = Rig::with_home("home", |_, _| vec![PathBuf::from("/proc")]);
    let err = rig.roots().expect_err("/proc is not a usable root");
    assert!(err.contains("PseudoFs") && err.contains("proc"), "{err}");
    let mark = PathBuf::from("/proc/self/attr");
    assert!(rig.plan(&[mark.clone()]).is_err());
    let before = snap(&rig);
    let a = rig.apply(rig.forged(&[mark]), RunOpts::default());
    assert!(
        a.report.as_ref().is_some_and(|r| r.refused.is_some()),
        "{:?}",
        a.report
    );
    assert_refused_and_intact(&rig, &before, &a, "/proc root");
    assert!(Path::new("/proc/self/attr").exists());
}

#[test]
fn safety_explorer_hostile_should_refuse_scan_roots_and_delete_the_same_item_When_roots_are_nested_in_either_order(
) {
    for swapped in [false, true] {
        let rig = Rig::with(|h| {
            let (a, b) = (h.join("code"), h.join("code/app"));
            if swapped {
                vec![b, a]
            } else {
                vec![a, b]
            }
        });
        let dist = rig.victim("app/dist");
        let sibling = rig.victim("other");
        assert_hostile_mark(
            &rig,
            &rig.home().join("code/app"),
            &format!("inner scan root (swapped={swapped})"),
        );
        assert_hostile_mark(
            &rig,
            &rig.home().join("code"),
            &format!("outer scan root (swapped={swapped})"),
        );
        let before = snap(&rig);
        let plan = rig.plan(&[dist.clone()]).expect("plan");
        assert_eq!(plan.rows[0].planned, Planned::Will, "swapped={swapped}");
        let a = rig.apply(ReviewedPlan::from_explorer_plan(&plan), RunOpts::default());
        let after = snap(&rig);
        assert_eq!(
            a.report.as_ref().unwrap().outcomes[0].status,
            Status::Removed,
            "swapped={swapped}: {:?}",
            a.report
        );
        assert!(!dist.exists() && sibling.join("witness.bin").exists());
        assert_all_deletions_logged(&before, &after, &a.log, "nested roots");
    }
}

// ---------------------------------------------------------------------------------------------------------
// M3 and M5: .git, files, symlinks
// ---------------------------------------------------------------------------------------------------------

#[test]
fn safety_explorer_hostile_should_refuse_a_dot_git_directory_When_it_is_marked() {
    let rig = Rig::new();
    let app = rig.dir(rig.home().join("code/app"));
    let git = rig.dir(app.join(".git"));
    rig.file(git.join("HEAD"), b"ref: refs/heads/main");
    let hooks = rig.dir(git.join("objects"));
    rig.file(hooks.join("pack.bin"), &[1u8; 4096]);
    assert_hostile_mark(&rig, &git, ".git directory");
    assert_hostile_mark(&rig, &hooks, "a directory inside .git");
    assert!(git.join("HEAD").exists());
}

#[test]
fn safety_explorer_hostile_should_refuse_a_file_mark_When_a_forged_plan_lists_a_regular_file_or_a_fifo(
) {
    let rig = Rig::new();
    let app = rig.dir(rig.home().join("code/app"));
    let file = rig.file(app.join("notes.txt"), b"notes");
    assert_hostile_mark(&rig, &file, "regular file");
    let fifo = app.join("pipe");
    if Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .is_ok_and(|st| st.success())
    {
        assert_hostile_mark(&rig, &fifo, "fifo");
    }
    assert!(file.exists());
}

#[test]
fn safety_explorer_hostile_should_refuse_a_symlink_mark_When_it_points_at_a_directory_outside_the_marks(
) {
    let rig = Rig::new();
    let keep = rig.victim("keep");
    let link = rig.home().join("code/app/linkdir");
    rig.dir(link.parent().unwrap());
    symlink(&keep, &link).unwrap();
    assert_hostile_mark(&rig, &link, "symlink to a directory");
    assert!(keep.join("witness.bin").exists());
    let below = link.join("sub");
    rig.dir(keep.join("sub"));
    assert_hostile_mark(&rig, &below, "directory reached through a symlinked parent");
}

// ---------------------------------------------------------------------------------------------------------
// M7: own state, config, cache, the running binary
// ---------------------------------------------------------------------------------------------------------

/// Roots are the XDG tree itself so the tool's own directories are *below* a root: only the own-state rule
/// can keep them.
fn xdg_rig() -> Rig {
    Rig::with_home("home", |top, _| vec![top.join("xdg")])
}

#[test]
fn safety_explorer_hostile_should_refuse_the_audit_log_directory_When_it_is_marked() {
    let rig = xdg_rig();
    let wired = Wired::build(&rig.base, true);
    drop(wired);
    let state = tool_dirs(&rig.base).state;
    let log = state.join("audit.jsonl");
    rig.file(&log, b"{\"type\":\"run_start\"}\n");
    let bytes = std::fs::read(&log).unwrap();
    for mark in [state.clone(), state.parent().unwrap().to_path_buf()] {
        assert!(
            !matches!(rig.planned_of(&[mark.clone()], &mark), Some(Planned::Will)),
            "{mark:?}"
        );
        let a = rig.apply(rig.forged(&[mark.clone()]), RunOpts::default());
        assert!(no_removal(&a), "{:?}", a.report);
        assert!(!a.panicked);
        assert!(
            std::fs::read(&log).unwrap().starts_with(&bytes),
            "the audit log lost its history ({mark:?})"
        );
    }
}

#[test]
fn safety_explorer_hostile_should_refuse_the_cache_and_config_directories_When_they_are_marked() {
    let rig = xdg_rig();
    let cache = rig.dir(rig.base.cache.join("dunnage"));
    let cfg = rig.dir(rig.base.config.join("dunnage"));
    let cache_file = rig.file(cache.join("explorer.cache"), b"cache");
    let cfg_file = rig.file(cfg.join("config.toml"), b"roots = []");
    for mark in [&cache, &cfg, &rig.base.cache, &rig.base.config] {
        assert!(
            !matches!(rig.planned_of(&[mark.clone()], mark), Some(Planned::Will)),
            "{mark:?}"
        );
        let a = rig.apply(rig.forged(&[mark.clone()]), RunOpts::default());
        assert!(no_removal(&a), "{:?}", a.report);
        assert!(
            cache_file.exists() && cfg_file.exists(),
            "{mark:?} lost its files"
        );
    }
    assert_eq!(std::fs::read(&cache_file).unwrap(), b"cache");
    assert_eq!(std::fs::read(&cfg_file).unwrap(), b"roots = []");
}

#[test]
fn safety_explorer_hostile_should_refuse_the_running_binary_and_its_ancestors_When_they_are_marked()
{
    let mut rig = Rig::new();
    let app = rig.dir(rig.home().join("code/app"));
    let debug = rig.dir(app.join("target/debug"));
    rig.binary = debug.join("dunnage");
    rig.file(&rig.binary, b"\x7fELF");
    let dist = rig.victim("app/dist");
    assert_hostile_mark(&rig, &debug, "directory of the running binary");
    assert_hostile_mark(
        &rig,
        &app.join("target"),
        "target/ above the running binary",
    );
    assert!(rig.binary.exists());
    assert_eq!(
        rig.planned_of(&[dist.clone()], &dist),
        Some(Planned::Will),
        "an unrelated sibling stays allowed"
    );
}

#[test]
fn safety_explorer_hostile_should_refuse_own_state_When_xdg_state_home_is_a_symlink_to_a_data_directory(
) {
    let mut rig = Rig::with_home("home", |top, _| vec![top.join("data")]);
    let real = rig.dir(rig.top().join("data/st"));
    let link = rig.top().join("xdg-state-link");
    symlink(&real, &link).unwrap();
    rig.base.state = link;
    let own = real.join("dunnage");
    let a0 = Wired::build(&rig.base, true);
    drop(a0);
    rig.dir(&own);
    let log = rig.file(own.join("audit.jsonl"), b"{\"type\":\"run_start\"}\n");
    for mark in [own.clone(), real.clone()] {
        assert!(
            !matches!(rig.planned_of(&[mark.clone()], &mark), Some(Planned::Will)),
            "{mark:?}"
        );
        let a = rig.apply(rig.forged(&[mark.clone()]), RunOpts::default());
        assert!(no_removal(&a), "{:?}", a.report);
        assert!(
            log.exists(),
            "the audit log was deleted through the symlinked XDG_STATE_HOME ({mark:?})"
        );
    }
}

// ---------------------------------------------------------------------------------------------------------
// The gate: saved plan, timestamps, plan-id agreement
// ---------------------------------------------------------------------------------------------------------

#[test]
fn safety_explorer_hostile_should_refuse_the_plan_When_it_was_saved_with_a_future_timestamp() {
    let rig = Rig::new();
    let dist = rig.victim("app/dist");
    let before = snap(&rig);
    let a = rig.apply(
        rig.reviewed(&[dist.clone()]),
        RunOpts {
            saved: SavedAt::Future(3600),
            ..RunOpts::default()
        },
    );
    assert!(
        a.report.as_ref().is_some_and(|r| r.refused.is_some()),
        "{:?}",
        a.report
    );
    assert_refused_and_intact(&rig, &before, &a, "future saved_at");
    assert!(
        !a.trace.steps().contains(&"mint"),
        "no capability before the gate passes: {:?}",
        a.trace.steps()
    );
    assert_eq!(count_type(&a.log, "intent"), 0);
}

#[test]
fn safety_explorer_hostile_should_refuse_the_plan_When_no_matching_entry_was_ever_saved() {
    let rig = Rig::new();
    let dist = rig.victim("app/dist");
    let before = snap(&rig);
    let a = rig.apply(
        rig.reviewed(&[dist.clone()]),
        RunOpts {
            saved: SavedAt::Never,
            ..RunOpts::default()
        },
    );
    assert!(
        a.report.as_ref().is_some_and(|r| r.refused.is_some()),
        "{:?}",
        a.report
    );
    assert_refused_and_intact(&rig, &before, &a, "unsaved plan");
    assert!(!a.trace.steps().contains(&"mint"), "{:?}", a.trace.steps());
}

#[test]
fn safety_explorer_hostile_should_refuse_set_b_When_only_the_plan_id_of_set_a_was_saved() {
    let rig = Rig::new();
    let (a1, b1) = (rig.victim("app/a"), rig.victim("app/b"));
    let before = snap(&rig);
    let a = rig.apply(
        rig.reviewed(&[b1.clone()]),
        RunOpts {
            saved_marks: Some(vec![a1.clone()]),
            ..RunOpts::default()
        },
    );
    assert!(
        a.report.as_ref().is_some_and(|r| r.refused.is_some()),
        "{:?}",
        a.report
    );
    assert_refused_and_intact(&rig, &before, &a, "A's id for B");
    assert!(!a.trace.steps().contains(&"mint"), "{:?}", a.trace.steps());
    assert!(a1.exists() && b1.exists());
}

#[test]
fn safety_explorer_hostile_should_refuse_a_plus_one_path_When_only_the_plan_id_of_set_a_was_saved()
{
    let rig = Rig::new();
    let (a1, extra) = (rig.victim("app/a"), rig.victim("app/extra"));
    let before = snap(&rig);
    let a = rig.apply(
        rig.reviewed(&[a1.clone(), extra.clone()]),
        RunOpts {
            saved_marks: Some(vec![a1.clone()]),
            ..RunOpts::default()
        },
    );
    assert!(
        a.report.as_ref().is_some_and(|r| r.refused.is_some()),
        "{:?}",
        a.report
    );
    assert_refused_and_intact(&rig, &before, &a, "A plus one path");
    assert!(a1.exists() && extra.exists());
}

#[test]
fn safety_explorer_hostile_should_refuse_the_whole_set_When_one_directory_of_it_was_replaced_after_review(
) {
    let rig = Rig::new();
    let (d1, d2) = (rig.victim("app/d1"), rig.victim("app/d2"));
    let reviewed = rig.reviewed(&[d1.clone(), d2.clone()]);
    assert_eq!(reviewed.count(), 2);
    swap_for_new_inode(d2.clone())();
    let before = snap(&rig);
    let a = rig.apply(reviewed, RunOpts::default());
    assert_refused_and_intact(&rig, &before, &a, "A with one directory replaced");
    assert!(
        d1.join("witness.bin").exists(),
        "the unchanged member of the set survives too"
    );
}

// ---------------------------------------------------------------------------------------------------------
// Shapes of the set: 21 marks, a mark inside a mark, hostile names
// ---------------------------------------------------------------------------------------------------------

#[test]
fn safety_explorer_hostile_should_refuse_21_marks_When_the_limit_is_20() {
    let rig = Rig::new();
    let marks: Vec<PathBuf> = (0..21)
        .map(|i| rig.victim(&format!("app/d{i:02}")))
        .collect();
    let before = snap(&rig);
    assert!(rig.plan(&marks).is_err(), "the plan refuses 21 marks");
    assert!(rig.plan(&marks[..20]).is_ok(), "20 marks are fine");
    let a = rig.apply(rig.forged(&marks), RunOpts::default());
    assert!(
        a.report.as_ref().is_some_and(|r| r.refused.is_some()),
        "{:?}",
        a.report
    );
    assert_refused_and_intact(&rig, &before, &a, "21 marks");
}

#[test]
fn safety_explorer_hostile_should_refuse_the_set_When_a_mark_is_inside_another_mark() {
    for reversed in [false, true] {
        let rig = Rig::new();
        let outer = rig.victim("app/outer");
        let inner = rig.victim("app/outer/inner");
        let marks = if reversed {
            vec![inner.clone(), outer.clone()]
        } else {
            vec![outer.clone(), inner.clone()]
        };
        let before = snap(&rig);
        assert!(rig.plan(&marks).is_err(), "reversed={reversed}");
        let a = rig.apply(rig.forged(&marks), RunOpts::default());
        assert!(
            a.report.as_ref().is_some_and(|r| r.refused.is_some()),
            "reversed={reversed}: {:?}",
            a.report
        );
        assert_refused_and_intact(&rig, &before, &a, "mark inside a mark");
        assert!(inner.exists() && outer.exists());
    }
}

#[test]
fn safety_explorer_hostile_should_escape_the_text_and_keep_the_log_one_record_per_line_When_a_name_holds_a_newline_and_esc(
) {
    let rig = Rig::new();
    let nasty = rig.victim("app/ev\nil\u{1b}[31mred\u{1b}]0;title\u{7}");
    let inner = rig.victim("app/ev\nil\u{1b}[31mred\u{1b}]0;title\u{7}/in");
    let before = snap(&rig);
    // a refusal text (a mark inside a mark) names the path: no raw control byte may reach the terminal
    let err = rig
        .plan(&[nasty.clone(), inner.clone()])
        .expect_err("overlap");
    assert!(
        !err.contains('\u{1b}') && !err.contains('\u{7}') && !err.contains('\n'),
        "raw control byte in {err:?}"
    );
    let a = rig.apply(
        rig.forged(&[nasty.clone(), inner.clone()]),
        RunOpts::default(),
    );
    assert!(!refused_text(&a).contains('\u{1b}') && !refused_text(&a).contains('\u{7}'));
    assert_refused_and_intact(&rig, &before, &a, "hostile name, overlap");
    // alone, the directory may be deleted (it is a plain directory) but the log must stay well formed
    std::fs::remove_dir_all(&inner).unwrap();
    let before = snap(&rig);
    let a = rig.apply(rig.forged(&[nasty.clone()]), RunOpts::default());
    let after = snap(&rig);
    assert!(!a.panicked);
    assert_all_deletions_logged(&before, &after, &a.log, "hostile name");
    let raw = std::fs::read(rig.audit_path()).unwrap();
    assert!(
        !raw.contains(&0x1b) && !raw.contains(&0x07),
        "a raw control byte is in the audit log"
    );
    let text = String::from_utf8(raw).expect("the audit log is utf-8");
    assert!(
        text.lines()
            .all(|l| serde_json::from_str::<serde_json::Value>(l).is_ok()),
        "a record spans lines or is torn"
    );
    let _ = OsString::from_vec(vec![0xff]);
}

// ---------------------------------------------------------------------------------------------------------
// The capability types: a confirmation cannot be forged (compile-fail through rustc)
// ---------------------------------------------------------------------------------------------------------

fn newest_rlib(deps: &Path, krate: &str) -> PathBuf {
    let prefix = format!("lib{krate}-");
    std::fs::read_dir(deps)
        .expect("deps dir")
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with(&prefix) && Path::new(&n).extension().is_some_and(|x| x == "rlib")
        })
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .unwrap_or_else(|| panic!("no rlib of {krate} in {deps:?}"))
        .path()
}

/// Compiles `src` as a library against the crates this test links; returns (ok, stderr).
fn rustc_check(src: &str) -> (bool, String) {
    let exe = std::env::current_exe().unwrap();
    let deps = exe.parent().unwrap().to_path_buf();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("probe.rs");
    std::fs::write(&file, src).unwrap();
    let mut c = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()));
    c.args([
        "--edition",
        "2021",
        "--crate-type",
        "lib",
        "--emit=metadata",
        "--crate-name",
        "probe",
    ])
    .arg("-o")
    .arg(dir.path().join("probe.rmeta"))
    .arg("-L")
    .arg(format!("dependency={}", deps.display()));
    for k in ["dunnage_core", "dunnage_cli", "dunnage_adapters"] {
        c.arg("--extern")
            .arg(format!("{k}={}", newest_rlib(&deps, k).display()));
    }
    let out = c.arg(&file).output().expect("rustc");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

const CONFIRM_PRELUDE: &str =
    "use dunnage_core::explore::confirm::{Confirm, ConfirmRecord, ConfirmedPlan, ReviewedPlan};\n";

#[test]
fn safety_explorer_hostile_should_not_compile_a_forged_confirmation_When_it_does_not_come_from_accepts(
) {
    // Bazel does not lay the built rlibs out next to the test binary: the Bazel target sets DC_SKIP_COMPILE_FAIL=1
    // and `make gate` runs this test through cargo instead (scripts/gate.sh, "cargo-only tests").
    if std::env::var_os("DC_SKIP_COMPILE_FAIL").is_some() {
        assert!(
            std::env::var_os("DC_REQUIRE_COMPILE_FAIL").is_none(),
            "DC_REQUIRE_COMPILE_FAIL is set and this build cannot run the compile-fail probes"
        );
        eprintln!("SKIP: compile-fail probes need Cargo's rlib layout (DC_SKIP_COMPILE_FAIL=1); `make gate` runs them through cargo");
        return;
    }
    let (ok, err) = rustc_check(&format!(
        "{CONFIRM_PRELUDE}pub fn good(r: ReviewedPlan) -> Option<ConfirmedPlan> {{ Confirm::new(r).accepts(\"1\").ok() }}"
    ));
    assert!(
        ok,
        "the control program must compile, or this test proves nothing: {err}"
    );
    let bad: [(&str, &str, &str); 6] = [
        ("a struct literal of ConfirmedPlan", "pub fn f(reviewed: ReviewedPlan) -> ConfirmedPlan { ConfirmedPlan { reviewed, typed: \"1\".into() } }", "E0451"),
        ("ConfirmedPlan::default()", "pub fn f() -> ConfirmedPlan { ConfirmedPlan::default() }", "E0599"),
        ("a clone of a ConfirmedPlan", "pub fn f(c: ConfirmedPlan) -> (ConfirmedPlan, ConfirmedPlan) { (c.clone(), c) }", "E0599"),
        ("reuse after the move", "pub fn f(c: ConfirmedPlan) { drop(c); let _ = c.count(); }", "E0382"),
        ("a second accepts on one Confirm", "pub fn f(c: Confirm) { let _ = c.accepts(\"1\"); let _ = c.accepts(\"1\"); }", "E0382"),
        (
            "a ConfirmRecord where a ConfirmedPlan is required",
            "use dunnage_adapters::dirops::DirOps;\nuse dunnage_cli::explore::apply::{compose_explorer_apply, ExplorerApplyPorts, ExplorerReport, ExplorerRun};\nuse dunnage_cli::wiring::compose::Trace;\npub fn f<D: DirOps + 'static>(r: ConfirmRecord, w: &ExplorerApplyPorts<'_, D>, run: &ExplorerRun, t: &mut Trace) -> ExplorerReport { compose_explorer_apply(r, w, run, t) }",
            "E0308",
        ),
    ];
    for (what, body, code) in bad {
        let (ok, err) = rustc_check(&format!("{CONFIRM_PRELUDE}{body}"));
        assert!(!ok, "{what} compiles, so a confirmation can be forged");
        assert!(err.contains(code), "{what}: expected {code}, got {err}");
    }
    let (ok, err) = rustc_check(&format!(
        "{CONFIRM_PRELUDE}use dunnage_adapters::dirops::DirOps;\nuse dunnage_cli::explore::apply::{{compose_explorer_apply, ExplorerApplyPorts, ExplorerReport, ExplorerRun}};\nuse dunnage_cli::wiring::compose::Trace;\npub fn f<D: DirOps + 'static>(r: ReviewedPlan, w: &ExplorerApplyPorts<'_, D>, run: &ExplorerRun, t: &mut Trace) -> ExplorerReport {{ compose_explorer_apply(r, w, run, t) }}"
    ));
    assert!(
        !ok && err.contains("E0308"),
        "an unconfirmed ReviewedPlan must not be accepted by the apply: {err}"
    );
}

// ---------------------------------------------------------------------------------------------------------
// A crash between intent and result
// ---------------------------------------------------------------------------------------------------------

#[test]
fn safety_explorer_hostile_should_leave_an_intent_without_result_or_run_end_When_the_process_dies_after_the_delete(
) {
    let rig = Rig::new();
    let dist = rig.victim("app/dist");
    let before = snap(&rig);
    let a = rig.apply(
        rig.reviewed(&[dist.clone()]),
        RunOpts {
            panic_in_result: true,
            ..RunOpts::default()
        },
    );
    let after = snap(&rig);
    assert!(a.panicked, "the injected crash must reach the caller");
    assert_eq!(count_type(&a.log, "intent"), 1, "{:?}", a.log);
    assert_eq!(count_type(&a.log, "result"), 0, "{:?}", a.log);
    assert_eq!(count_type(&a.log, "run_end"), 0, "{:?}", a.log);
    assert_eq!(
        count_type(&a.log, "explorer_confirm"),
        1,
        "the paths were synced before the first intent"
    );
    assert!(
        intent_paths(&a.log).iter().any(|p| Path::new(p) == dist),
        "{:?}",
        a.log
    );
    assert_all_deletions_logged(&before, &after, &a.log, "crash after the delete");
}

// ---------------------------------------------------------------------------------------------------------
// TOCTOU between the plan and the apply (UX-T37)
// ---------------------------------------------------------------------------------------------------------

#[test]
fn safety_explorer_hostile_should_skip_the_item_When_a_symlink_to_keep_is_swapped_in_after_the_plan(
) {
    for hook in [false, true] {
        let rig = Rig::new();
        let keep = rig.victim("keep");
        let dist = rig.victim("app/dist");
        let (mid, a) = toctou(
            &rig,
            &dist,
            swap_for_symlink(dist.clone(), keep.clone()),
            hook,
        );
        assert_swap_refused(
            &rig,
            &mid,
            &a,
            &format!("(a) symlink swapped in, hook={hook}"),
        );
        assert!(keep.join("witness.bin").exists());
    }
}

#[test]
fn safety_explorer_hostile_should_skip_the_item_When_the_directory_is_replaced_by_a_new_inode_after_the_plan(
) {
    for hook in [false, true] {
        let rig = Rig::new();
        let dist = rig.victim("app/dist");
        let (mid, a) = toctou(&rig, &dist, swap_for_new_inode(dist.clone()), hook);
        assert_swap_refused(&rig, &mid, &a, &format!("(b) replaced inode, hook={hook}"));
        assert!(
            dist.join("witness.bin").exists(),
            "the new directory survives"
        );
    }
}

#[test]
fn safety_explorer_hostile_should_skip_the_item_When_a_tmpfs_is_mounted_on_it_after_the_plan() {
    if !enter_ns(&tname(), "DC_REQUIRE_UNSHARE") {
        return;
    }
    for hook in [false, true] {
        let rig = Rig::new();
        let dist = rig.victim("app/dist");
        let probe = rig.dir(rig.top().join("probe"));
        if !mount_or_skip("tmpfs on the item", &["-t", "tmpfs", "tmpfs", s(&probe)]) {
            return;
        }
        lazy_umount(&probe);
        let target = dist.clone();
        let (mid, a) = toctou(
            &rig,
            &dist,
            move || {
                let ok = Command::new("mount")
                    .args(["-t", "tmpfs", "tmpfs"])
                    .arg(&target)
                    .status()
                    .unwrap()
                    .success();
                assert!(ok, "mounting a tmpfs on the item");
                std::fs::write(target.join("fresh.bin"), b"x").unwrap();
            },
            hook,
        );
        assert_swap_refused(
            &rig,
            &mid,
            &a,
            &format!("(c) tmpfs on the item, hook={hook}"),
        );
        assert!(dist.join("fresh.bin").exists());
        lazy_umount(&dist);
    }
}

#[test]
fn safety_explorer_hostile_should_skip_the_item_When_a_parent_component_becomes_a_symlink_to_the_same_directory(
) {
    for hook in [false, true] {
        let rig = Rig::new();
        let dist = rig.victim("app/sub/dist");
        let sub = rig.home().join("code/app/sub");
        let moved = rig.home().join("code/app/sub.moved");
        let (s2, m2) = (sub.clone(), moved.clone());
        // The path still resolves to the very same inode: only the O_NOFOLLOW walk of the parents can notice.
        let (mid, a) = toctou(
            &rig,
            &dist,
            move || {
                std::fs::rename(&s2, &m2).unwrap();
                symlink("sub.moved", &s2).unwrap();
            },
            hook,
        );
        assert_refused_and_intact(
            &rig,
            &mid,
            &a,
            &format!("(d) symlinked parent, hook={hook}"),
        );
        assert!(moved.join("dist/witness.bin").exists());
        assert!(!a
            .report
            .as_ref()
            .is_some_and(|r| r.outcomes.iter().any(|o| o.status == Status::Removed)));
    }
}

fn grow_case(extra: u64, hook: bool) -> (Rig, PathBuf, Snap, Applied) {
    let rig = Rig::new();
    let dist = rig.victim("app/dist");
    dir_size_bytes_sparse(&dist.join("big.bin"), 10 << 30);
    let d2 = dist.clone();
    let (mid, a) = toctou(
        &rig,
        &dist,
        move || dir_size_bytes_sparse(&d2.join("grown.bin"), extra),
        hook,
    );
    (rig, dist, mid, a)
}

#[test]
fn safety_explorer_hostile_should_delete_nothing_and_say_changed_since_you_confirmed_When_the_item_grew_by_2_gib(
) {
    for hook in [false, true] {
        let (rig, dist, mid, a) = grow_case(2 << 30, hook);
        assert_refused_and_intact(&rig, &mid, &a, &format!("(e) growth, hook={hook}"));
        let text = refused_text(&a);
        assert!(
            text.contains("Changed since you confirmed"),
            "hook={hook}: {text:?}"
        );
        assert!(text.contains("Nothing was deleted"), "{text:?}");
        assert!(dist.join("big.bin").exists() && dist.join("grown.bin").exists());
        assert_eq!(
            count_type(&a.log, "intent"),
            0,
            "a refused re-measure writes no intent"
        );
        assert_eq!(count_type(&a.log, "run_end"), 1, "{:?}", a.log);
    }
}

#[test]
fn growth_should_not_refuse_the_delete_When_the_item_grew_by_less_than_the_limit() {
    let (rig, dist, _mid, a) = grow_case(1 << 20, true);
    let r = a.report.as_ref().expect("report");
    assert!(r.refused.is_none(), "{r:?}");
    assert_eq!(r.outcomes[0].status, Status::Removed, "{r:?}");
    assert!(!dist.exists());
    drop(rig);
}

fn root_swap_case(as_symlink: bool, hook: bool) -> (Rig, Snap, Applied) {
    let rig = Rig::new();
    let dist = rig.victim("app/dist");
    let code = rig.home().join("code");
    let old = rig.home().join("code.old");
    let (mid, a) = toctou(
        &rig,
        &dist,
        move || {
            std::fs::rename(&code, &old).unwrap();
            if as_symlink {
                symlink("code.old", &code).unwrap();
            } else {
                std::fs::create_dir_all(code.join("app/dist")).unwrap();
                std::fs::write(code.join("app/dist/witness.bin"), [5u8; 4096]).unwrap();
            }
        },
        hook,
    );
    (rig, mid, a)
}

#[test]
fn safety_explorer_hostile_should_delete_nothing_When_the_scan_root_is_replaced_by_a_symlink_after_the_plan(
) {
    for hook in [false, true] {
        let (rig, mid, a) = root_swap_case(true, hook);
        assert_swap_refused(
            &rig,
            &mid,
            &a,
            &format!("(f) scan root replaced by a symlink, hook={hook}"),
        );
        assert!(rig.home().join("code.old/app/dist/witness.bin").exists());
    }
}

#[test]
fn safety_explorer_hostile_should_delete_nothing_When_the_scan_root_is_replaced_by_a_new_directory_after_the_plan(
) {
    for hook in [false, true] {
        let (rig, mid, a) = root_swap_case(false, hook);
        assert_swap_refused(
            &rig,
            &mid,
            &a,
            &format!("(f) scan root replaced by a new directory, hook={hook}"),
        );
        assert!(rig.home().join("code.old/app/dist/witness.bin").exists());
        assert!(
            rig.home().join("code/app/dist/witness.bin").exists(),
            "the new directory was never planned"
        );
    }
}

// The wording the specification gives for the swaps is `SKIP changed since the plan`. These checks are apart
// from the data-loss guards above so that a different (still safe) reason shows up as a wording finding only.

#[test]
fn skip_text_should_read_changed_since_the_plan_When_a_symlink_replaces_the_item_after_the_confirmation(
) {
    let rig = Rig::new();
    let keep = rig.victim("keep");
    let dist = rig.victim("app/dist");
    let (_, a) = toctou(&rig, &dist, swap_for_symlink(dist.clone(), keep), true);
    assert_skipped_with(&a, "changed since the plan", "(a)");
}

#[test]
fn skip_text_should_read_changed_since_the_plan_When_a_new_inode_replaces_the_item_after_the_confirmation(
) {
    let rig = Rig::new();
    let dist = rig.victim("app/dist");
    let (_, a) = toctou(&rig, &dist, swap_for_new_inode(dist.clone()), true);
    assert_skipped_with(&a, "changed since the plan", "(b)");
}

#[test]
fn skip_text_should_read_changed_since_the_plan_When_a_tmpfs_is_mounted_on_the_item_after_the_confirmation(
) {
    if !enter_ns(&tname(), "DC_REQUIRE_UNSHARE") {
        return;
    }
    let rig = Rig::new();
    let dist = rig.victim("app/dist");
    let target = dist.clone();
    let (_, a) = toctou(
        &rig,
        &dist,
        move || {
            let ok = Command::new("mount")
                .args(["-t", "tmpfs", "tmpfs"])
                .arg(&target)
                .status()
                .unwrap()
                .success();
            assert!(ok, "mounting a tmpfs on the item");
        },
        true,
    );
    assert_skipped_with(&a, "changed since the plan", "(c)");
    lazy_umount(&dist);
}

#[test]
fn skip_text_should_read_changed_since_the_plan_When_the_scan_root_is_replaced_after_the_confirmation(
) {
    let mut findings = Vec::new();
    for as_symlink in [true, false] {
        let (_rig, _mid, a) = root_swap_case(as_symlink, true);
        let r = a.report.as_ref().expect("report");
        let texts: Vec<String> = r
            .outcomes
            .iter()
            .map(|o| match &o.status {
                Status::Skipped(s) => skip_text(*s),
                other => format!("{other:?}"),
            })
            .collect();
        if texts.is_empty()
            || !texts
                .iter()
                .all(|t| t.to_lowercase().contains("changed since the plan"))
        {
            findings.push(format!(
                "symlink={as_symlink}: {texts:?} refused={:?}",
                r.refused
            ));
        }
    }
    assert!(findings.is_empty(), "(f) wording: {findings:?}");
}

#[test]
fn safety_explorer_hostile_should_not_delete_a_bind_mount_source_When_the_mount_is_below_the_item_at_plan_time(
) {
    if !enter_ns(&tname(), "DC_REQUIRE_BIND_MOUNT") {
        return;
    }
    let rig = Rig::new();
    let keep = rig.victim("keep");
    let dist = rig.victim("app/dist");
    let inner = rig.dir(dist.join("inner"));
    if !mount_or_skip(
        "bind mount below the item",
        &["--bind", s(&keep), s(&inner)],
    ) {
        return;
    }
    let before = snap(&rig);
    assert!(
        !matches!(rig.planned_of(&[dist.clone()], &dist), Some(Planned::Will)),
        "the plan lists a tree holding a bind mount"
    );
    let a = rig.apply(rig.forged(&[dist.clone()]), RunOpts::default());
    let after = snap(&rig);
    assert!(!a.panicked && no_removal(&a), "{:?}", a.report);
    assert!(
        keep.join("witness.bin").exists(),
        "the bind mount's source was deleted"
    );
    assert_all_deletions_logged(&before, &after, &a.log, "(g) bind mount below the item");
    lazy_umount(&inner);
}

#[test]
fn safety_explorer_hostile_should_not_delete_a_bind_mount_source_When_the_mount_appears_after_the_plan(
) {
    if !enter_ns(&tname(), "DC_REQUIRE_BIND_MOUNT") {
        return;
    }
    let rig = Rig::new();
    let keep = rig.victim("keep");
    let dist = rig.victim("app/dist");
    let inner = rig.dir(dist.join("inner"));
    let probe = rig.dir(rig.top().join("probe"));
    if !mount_or_skip(
        "bind mount after the plan",
        &["--bind", s(&keep), s(&probe)],
    ) {
        return;
    }
    lazy_umount(&probe);
    let (k2, i2) = (keep.clone(), inner.clone());
    let reviewed = rig.reviewed(&[dist.clone()]);
    assert_eq!(reviewed.count(), 1);
    let before = snap(&rig);
    let a = rig.apply(
        reviewed,
        RunOpts {
            after_confirm: Some(Box::new(move || {
                let ok = Command::new("mount")
                    .arg("--bind")
                    .arg(&k2)
                    .arg(&i2)
                    .status()
                    .unwrap()
                    .success();
                assert!(ok, "mount --bind after the plan");
            })),
            ..RunOpts::default()
        },
    );
    let after = snap(&rig);
    assert!(!a.panicked, "the apply panicked");
    assert!(
        no_removal(&a),
        "the item with a bind mount below it must end Partial or refused: {:?}",
        a.report
    );
    assert!(
        keep.join("witness.bin").exists(),
        "(g) the bind mount's source was deleted"
    );
    assert_all_deletions_logged(
        &before,
        &after,
        &a.log,
        "(g) bind mount appearing after the plan",
    );
    lazy_umount(&inner);
}

#[test]
fn safety_explorer_hostile_should_fail_closed_When_the_root_mount_id_is_unknown() {
    let rig = Rig::new();
    let dist = rig.victim("app/dist");
    let reviewed = rig.reviewed(&[dist.clone()]);
    let before = snap(&rig);
    let top = rig.top();
    let a = rig.apply_ops(reviewed, RunOpts::default(), &|_t: &ApplyToken| {
        NoMntOps(RealDirOps::confined_to(&top))
    });
    assert_refused_and_intact(&rig, &before, &a, "no root_mnt");
    let r = a.report.as_ref().unwrap();
    assert!(
        !r.outcomes.is_empty(),
        "the item is reported as skipped: {r:?}"
    );
    assert!(dist.join("witness.bin").exists());
}

#[test]
fn safety_explorer_hostile_should_refuse_an_opened_entry_When_it_is_home_reached_through_a_bind_mount_of_its_ancestor(
) {
    if !enter_ns(&tname(), "DC_REQUIRE_BIND_MOUNT") {
        return;
    }
    let rig = Rig::new();
    rig.file(rig.home().join("precious.txt"), b"mine");
    let up = rig.dir(rig.home().join("code/app/up"));
    if !mount_or_skip(
        "bind mount of an ancestor of $HOME",
        &["--bind", s(&rig.top()), s(&up)],
    ) {
        return;
    }
    let spelled = up.join("home");
    assert!(
        spelled.join("precious.txt").exists(),
        "$HOME is reachable under the other spelling"
    );
    assert_hostile_mark(&rig, &spelled, "$HOME under another spelling");
    assert_hostile_mark(&rig, &up, "the bound ancestor of $HOME");
    assert!(rig.home().join("precious.txt").exists());
    lazy_umount(&up);
}
