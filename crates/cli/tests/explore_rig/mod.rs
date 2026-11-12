#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cloned_ref_to_slice_refs
)]
//! A rig for the explorer's apply: a real fixture tree in a tempdir, the real `lstat`, mount table and
//! directory operations, and fakes for the audit sink, the run lock, the plan store and the clock.
//! Nothing outside the tempdir is ever read for deletion or written.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use dunnage_adapters::dirops::DirOps;
use dunnage_adapters::treestat::TreestatSizer;
use dunnage_adapters::{RealDirOps, RealProbe};
use dunnage_cli::explore::apply::{
    compose_explorer_apply, ExplorerApplyPorts, ExplorerReport, ExplorerRun,
};
use dunnage_cli::wiring::compose::Trace;
use dunnage_cli::wiring::dirs::real_canon;
use dunnage_cli::wiring::env::RealFs;
use dunnage_core::explore::confirm::{Confirm, ConfirmedPlan, ReviewedPlan};
use dunnage_core::explore::mark_check::OwnState;
use dunnage_core::explorer_plan::{
    explorer_plan, ExplorerPlanCtx, NoCleanerView, TreeSize, TreeSizer,
};
use dunnage_core::plan::OnlyMode;
use dunnage_core::ports::PlanStore;
use dunnage_core::roots::RootSet;
use dunnage_core::testing::{
    FakeAuditSink, FakeClock, FakeCommandRunner, FakePlanStore, FakeRunGuard, FakeSpaceMeter,
};
use dunnage_core::{AllocatedBytes, ApplyToken, BuildInfo, Dirs};

pub const NOW: i64 = 1_800_000_000;

pub struct Ex {
    pub td: tempfile::TempDir,
    /// Canonical base of the fixture.
    pub base: PathBuf,
    pub home: PathBuf,
    /// The one scan root, `base/hhh/code`.
    pub code: PathBuf,
    /// The XDG bases (before `dunnage` is appended).
    pub base_dirs: Dirs,
    /// The tool's own directories.
    pub dirs: Dirs,
    pub fs: RealFs,
    pub runner: FakeCommandRunner,
    pub clock: FakeClock,
    pub probe: RealProbe,
    pub space: FakeSpaceMeter,
    pub audit: FakeAuditSink,
    pub guard: FakeRunGuard,
    pub plans: FakePlanStore,
}

impl Ex {
    pub fn new() -> Ex {
        let td = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(td.path()).unwrap();
        let home = base.join("hhh");
        let code = home.join("code");
        for d in [
            &code,
            &base.join("state"),
            &base.join("cache"),
            &base.join("config"),
        ] {
            std::fs::create_dir_all(d).unwrap();
        }
        let base_dirs = Dirs {
            home: home.clone(),
            state: base.join("state"),
            cache: base.join("cache"),
            config: base.join("config"),
        };
        let dirs = Dirs {
            state: base.join("state/dunnage"),
            cache: base.join("cache/dunnage"),
            config: base.join("config/dunnage"),
            ..base_dirs.clone()
        };
        Ex {
            td,
            base,
            home,
            code,
            base_dirs,
            dirs,
            fs: RealFs,
            runner: FakeCommandRunner::new(),
            clock: FakeClock::at(NOW),
            probe: RealProbe,
            space: FakeSpaceMeter::new(
                vec![FakeSpaceMeter::plain_snapshot("/x", 500, 1000)],
                FakeSpaceMeter::stable(),
            ),
            audit: FakeAuditSink::new(),
            guard: FakeRunGuard::new(),
            plans: FakePlanStore::new(),
        }
    }

    pub fn p(&self, rel: &str) -> PathBuf {
        self.base.join(rel)
    }

    /// A directory below the root with `files` files of `bytes` bytes each.
    pub fn tree(&self, rel: &str, files: usize, bytes: usize) -> PathBuf {
        let d = self.code.join(rel);
        std::fs::create_dir_all(d.join("sub")).unwrap();
        for i in 0..files {
            std::fs::write(d.join(format!("f{i}")), vec![b'x'; bytes]).unwrap();
            std::fs::write(d.join("sub").join(format!("g{i}")), vec![b'y'; bytes]).unwrap();
        }
        d
    }

    /// The roots as `RootSet::resolve_on` gives them, with `code` the only configured root.
    pub fn roots(&self) -> RootSet {
        self.roots_of(&[self.code.clone()])
    }

    pub fn roots_of(&self, config: &[PathBuf]) -> RootSet {
        RootSet::resolve_on(
            &self.base_dirs,
            &[],
            Some(config),
            &self.base,
            &real_canon,
            &|_| None,
        )
        .unwrap()
    }

    pub fn own_state(&self) -> OwnState {
        OwnState::new(&self.base_dirs, self.base.join("bin/dunnage"))
    }

    pub fn real_sizer(&self) -> TreestatSizer<RealDirOps> {
        TreestatSizer::new(RealDirOps::confined_to(&self.base))
    }

    /// What `p` does: plans `marks`, saves the plan in the plan store, returns the reviewed plan.
    pub fn review(&self, marks: &[PathBuf]) -> ReviewedPlan {
        self.review_with(marks, &self.roots(), &self.real_sizer())
    }

    pub fn review_with(
        &self,
        marks: &[PathBuf],
        roots: &RootSet,
        sizer: &dyn TreeSizer,
    ) -> ReviewedPlan {
        let plan = self.plan_with(marks, roots, sizer, &self.own_state());
        let euid = dunnage_core::ports::SystemProbe::euid(&self.probe);
        self.plans
            .save_merged(vec![plan.saved_selection(euid, NOW)], euid, NOW);
        ReviewedPlan::from_explorer_plan(&plan)
    }

    /// The explorer plan of `marks` (rows and all), not saved.
    pub fn plan_with(
        &self,
        marks: &[PathBuf],
        roots: &RootSet,
        sizer: &dyn TreeSizer,
        own: &OwnState,
    ) -> dunnage_core::explorer_plan::ExplorerPlan {
        let ctx = ExplorerPlanCtx {
            roots,
            own_state: own,
            fs: &self.fs,
            probe: &self.probe,
            clock: &self.clock,
            sizer,
            extents: None,
            cleaner: &NoCleanerView,
        };
        explorer_plan(marks, OnlyMode::Explorer, &ctx).unwrap()
    }

    /// A reviewed plan for paths `p` could never have produced (a hostile list), saved so the plan-id gate
    /// passes: the identity of each item is what `lstat` says now, or the first value when given.
    pub fn forge(
        &self,
        items: &[(PathBuf, Option<dunnage_core::plan::ItemIdentity>)],
    ) -> ReviewedPlan {
        use dunnage_core::explore::confirm::ReviewedItem;
        let list: Vec<ReviewedItem> = items
            .iter()
            .map(|(p, id)| ReviewedItem {
                path: p.clone(),
                identity: id.unwrap_or_else(|| {
                    dunnage_core::plan::ItemIdentity::of(
                        &dunnage_core::ports::Fs::lstat(&self.fs, p).unwrap(),
                    )
                }),
            })
            .collect();
        let roots = self.roots();
        let reviewed = ReviewedPlan::new(list, roots.root_identities().to_vec(), 0);
        let sel = dunnage_core::selector::Selection::explorer(
            reviewed.items().iter().map(|i| i.path.clone()).collect(),
        );
        let euid = dunnage_core::ports::SystemProbe::euid(&self.probe);
        self.plans.save_merged(
            vec![dunnage_core::plan_state::SavedSelection {
                plan_id: reviewed.plan_id().to_string(),
                saved_at: NOW,
                euid,
                kinds: sel.kinds_resolved(),
                flags: sel.flags(),
                items: Vec::new(),
                items_truncated: false,
            }],
            euid,
            NOW,
        );
        reviewed
    }

    pub fn confirm(r: ReviewedPlan) -> ConfirmedPlan {
        let n = r.count().to_string();
        Confirm::new(r).accepts(&n).unwrap()
    }

    /// Reviews and confirms `marks`.
    pub fn confirmed(&self, marks: &[PathBuf]) -> ConfirmedPlan {
        Ex::confirm(self.review(marks))
    }

    pub fn run<D: DirOps + 'static>(
        &self,
        confirmed: ConfirmedPlan,
        ops: impl Fn(&ApplyToken) -> D,
    ) -> Ran {
        self.run_with(confirmed, ops, &Knobs::default())
    }

    pub fn env(&self) -> dunnage_core::ports::Env<'_> {
        dunnage_core::ports::Env {
            fs: &self.fs,
            runner: &self.runner,
            clock: &self.clock,
            probe: &self.probe,
            space: &self.space,
            audit: &self.audit,
            guard: &self.guard,
            plans: &self.plans,
            dirs: self.dirs.clone(),
        }
    }

    pub fn run_with<D: DirOps + 'static>(
        &self,
        confirmed: ConfirmedPlan,
        ops: impl Fn(&ApplyToken) -> D,
        knobs: &Knobs,
    ) -> Ran {
        let mut env = self.env();
        if let Some(a) = &knobs.audit {
            env.audit = &**a;
        }
        let hooked = GateHook {
            inner: env.audit,
            hook: &knobs.after_gate,
        };
        env.audit = &hooked;
        let minted = AtomicUsize::new(0);
        let mint = || {
            minted.fetch_add(1, Ordering::SeqCst);
            ApplyToken::for_tests()
        };
        let default_roots = self.roots();
        let resolve_calls = AtomicUsize::new(0);
        let resolve = || {
            let late = resolve_calls.fetch_add(1, Ordering::SeqCst) > 0;
            match (&knobs.roots_after_gate, &knobs.roots) {
                (Some(r), _) if late => r.clone(),
                (_, Some(r)) => r.clone(),
                _ => Ok(default_roots.clone()),
            }
        };
        let real_sizer = self.real_sizer();
        let sizer: &dyn TreeSizer = match &knobs.sizer {
            Some(s) => s,
            None => &real_sizer,
        };
        let own = knobs.own_state.clone().unwrap_or_else(|| self.own_state());
        let ports = ExplorerApplyPorts {
            env: &env,
            mint: &mint,
            ops: &ops,
            resolve_roots: &resolve,
            own_state: own,
            sizer,
            extents: None,
            build: BuildInfo {
                bin_version: "0.1.0".into(),
                git_commit: "abc".into(),
                dirty: false,
                metascope_rev: "none".into(),
                patched: false,
                metascope_dirty: false,
            },
            run_id: "run-x".into(),
            tty: true,
        };
        let run = ExplorerRun {
            abort: knobs.abort.clone().unwrap_or_default(),
            events: knobs.events.clone(),
        };
        let mut trace = Trace::default();
        let report = compose_explorer_apply(confirmed, &ports, &run, &mut trace);
        Ran {
            report,
            steps: trace.steps().to_vec(),
            minted: minted.load(Ordering::SeqCst),
        }
    }

    pub fn confined_ops(&self) -> impl Fn(&ApplyToken) -> RealDirOps + '_ {
        move |_| RealDirOps::confined_to(&self.base)
    }
}

pub struct Knobs {
    pub sizer: Option<FixedSizer>,
    pub roots: Option<Result<RootSet, String>>,
    pub own_state: Option<OwnState>,
    /// The caller's abort switch (the loop sets it on a signal or Esc).
    pub abort: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Replaces the fake audit sink (the real one, to read the log back with `audit verify`).
    pub audit: Option<std::sync::Arc<dyn dunnage_core::ports::AuditSink>>,
    /// Where the per-item messages go (the TUI's delete handle).
    pub events: Option<std::sync::mpsc::SyncSender<dunnage_core::explore::msg::DeleteMsg>>,
    /// Runs once, right after `explorer_confirm` is written: a change made here happens after the plan-id gate,
    /// in the window the per-item identity compare defends.
    pub after_gate: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Roots returned from the second `resolve_roots` call on (the first is the plan-id gate's), so a root
    /// change after the gate can be modelled.
    pub roots_after_gate: Option<Result<RootSet, String>>,
}

impl Knobs {
    pub fn after_gate(change: impl FnOnce() + Send + 'static) -> Knobs {
        Knobs {
            after_gate: std::sync::Mutex::new(Some(Box::new(change))),
            ..Knobs::default()
        }
    }
}

/// Wraps the audit sink the run uses and fires the `after_gate` hook once the `explorer_confirm` line is in.
struct GateHook<'a> {
    inner: &'a dyn dunnage_core::ports::AuditSink,
    hook: &'a std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl dunnage_core::ports::AuditSink for GateHook<'_> {
    fn run_start(&self, r: &dunnage_core::RunStart) -> std::io::Result<()> {
        self.inner.run_start(r)
    }
    fn intent(&self, r: &dunnage_core::Intent) -> std::io::Result<()> {
        self.inner.intent(r)
    }
    fn result(&self, r: &dunnage_core::ItemResult) -> std::io::Result<()> {
        self.inner.result(r)
    }
    fn run_end(&self, r: &dunnage_core::RunEnd) -> std::io::Result<()> {
        self.inner.run_end(r)
    }
    fn explorer_confirm(
        &self,
        r: &dunnage_core::audit_types::ExplorerConfirm,
    ) -> std::io::Result<()> {
        self.inner.explorer_confirm(r)?;
        if let Some(h) = self.hook.lock().unwrap().take() {
            h();
        }
        Ok(())
    }
}

impl Default for Knobs {
    fn default() -> Knobs {
        Knobs {
            sizer: None,
            roots: None,
            own_state: None,
            abort: None,
            audit: None,
            events: None,
            after_gate: std::sync::Mutex::new(None),
            roots_after_gate: None,
        }
    }
}

pub struct Ran {
    pub report: ExplorerReport,
    pub steps: Vec<&'static str>,
    pub minted: usize,
}

/// A sizer that answers fixed figures (paths not listed measure as 0).
#[derive(Default)]
pub struct FixedSizer(pub HashMap<PathBuf, u64>);

impl TreeSizer for FixedSizer {
    fn measure(&self, path: &Path) -> std::io::Result<TreeSize> {
        Ok(TreeSize {
            allocated: AllocatedBytes(self.0.get(path).copied().unwrap_or(0)),
            newest_mtime: None,
            git_repos: 0,
        })
    }
}

/// `path -> description` of everything below `dir`: type, content hash and link target.
pub fn manifest(dir: &Path) -> std::collections::BTreeMap<PathBuf, String> {
    use std::os::unix::fs::MetadataExt;
    fn walk(p: &Path, out: &mut std::collections::BTreeMap<PathBuf, String>) {
        let m = std::fs::symlink_metadata(p).unwrap();
        let body = if m.file_type().is_symlink() {
            format!("-> {}", std::fs::read_link(p).unwrap().display())
        } else if m.is_file() {
            format!("{}", std::fs::read(p).unwrap().len())
        } else {
            String::new()
        };
        out.insert(p.to_path_buf(), format!("ino={} {body}", m.ino()));
        if m.is_dir() {
            let mut kids: Vec<_> = std::fs::read_dir(p)
                .unwrap()
                .map(|e| e.unwrap().path())
                .collect();
            kids.sort();
            for k in kids {
                walk(&k, out);
            }
        }
    }
    let mut out = std::collections::BTreeMap::new();
    walk(dir, &mut out);
    out
}

/// The `status` field of every `result` line the fake audit sink recorded.
pub fn kinds_of(audit: &FakeAuditSink) -> Vec<&'static str> {
    use dunnage_core::AuditRecord as R;
    audit
        .records()
        .iter()
        .map(|r| match r {
            R::RunStart(_) => "run_start",
            R::ExplorerConfirm(_) => "explorer_confirm",
            R::Intent(_) => "intent",
            R::Result(_) => "result",
            R::RunEnd(_) => "run_end",
        })
        .collect()
}
