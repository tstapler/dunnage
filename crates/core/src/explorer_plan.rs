//! The plan of the explorer's `explorer-selection` kind (Story 6.6.2, ADR-006): the marks are the items,
//! nothing is discovered, every guard runs again on fresh `lstat` and mount data, and the plan-id binds the
//! identity of what each mark points at. Pure but for the ports it is handed; it deletes nothing and holds
//! no apply token (the apply path is Story 6.6.3 and goes through `compose_apply`).

use std::io;
use std::path::{Path, PathBuf};

use crate::candidate::{Candidate, ExplorerFacts, KindFacts, Locator};
use crate::delete_types::FileKind;
use crate::explore::mark_check::{
    mark_check, MarkCtx, MarkEnv, MarkFacts, MarkMount, MountTable, OwnState, SymlinkFact,
};
use crate::explore::marks::Marks;
use crate::explore::msg::MarkRefusal;
use crate::guard::{decide_path_common, Facts, GuardDecision};
use crate::ids::{AllocatedBytes, RootDev};
use crate::kind::Kind;
use crate::outcome::SkipReason;
use crate::plan::{not_an_item_text, ItemIdentity, OnlyMode, Plan, PlanId, Planned};
use crate::plan_state::{ItemKey, SavedSelection};
use crate::ports::{Clock, ExtentAttribution, ExtentProbe, Fs, SystemProbe};
use crate::roots::RootSet;
use crate::selector::Selection;
use crate::target::Guarded;

/// The most files the extent probe may open for one plan (the cleaner's own figure for a report).
pub const EXTENT_BUDGET: usize = 4096;

/// What a tree walk (`treestat`) measured below one marked path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeSize {
    pub allocated: AllocatedBytes,
    pub newest_mtime: Option<i64>,
}

/// `treestat` as a port, so `core` does not depend on the adapters.
pub trait TreeSizer: Send + Sync {
    fn measure(&self, path: &Path) -> io::Result<TreeSize>;
}

/// What the cleaner itself says about a path: `None` when it would not list it, else its guard's answer.
/// Only used for advice on a row; it never changes a status.
pub trait CleanerView: Send + Sync {
    fn guard_of(&self, path: &Path) -> Option<Guarded>;
}

/// A [`CleanerView`] that lists nothing (no advice).
pub struct NoCleanerView;

impl CleanerView for NoCleanerView {
    fn guard_of(&self, _path: &Path) -> Option<Guarded> {
        None
    }
}

/// Everything `explorer_plan` reads, all through ports.
pub struct ExplorerPlanCtx<'a> {
    pub roots: &'a RootSet,
    pub own_state: &'a OwnState,
    pub fs: &'a dyn Fs,
    pub probe: &'a dyn SystemProbe,
    pub clock: &'a dyn Clock,
    pub sizer: &'a dyn TreeSizer,
    pub extents: Option<&'a dyn ExtentProbe>,
    pub cleaner: &'a dyn CleanerView,
}

/// One row of the plan overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplorerRow {
    pub path: PathBuf,
    pub planned: Planned,
    /// `lstat` at plan time; `None` for a mark that is gone or could not be read.
    pub identity: Option<ItemIdentity>,
    /// Allocated bytes less the shared extents the probe found, where it ran.
    pub attributed_bytes: u64,
    pub shared_bytes: Option<u64>,
    /// `contains another filesystem (<device>); it is not entered and the directory stays`.
    pub note: Option<String>,
    /// `the cleaner would skip this: <reason>`; never changes the status.
    pub advice: Option<String>,
    /// `git repository: unpushed work is lost`; informational, never blocks.
    pub risk: Option<String>,
}

/// Why no plan was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExplorerPlanError {
    /// A mark is over the 512-byte, 20-value or 24 KiB limit of the `--only` line, or not canonical.
    PathTooLong(String),
    /// `OnlyMode::Strict` and a mark is not there any more.
    NotAnItem(String),
}

impl ExplorerPlanError {
    pub fn text(&self) -> &str {
        match self {
            ExplorerPlanError::PathTooLong(t) | ExplorerPlanError::NotAnItem(t) => t,
        }
    }
}

/// The plan of one marked set.
#[derive(Debug, Clone)]
pub struct ExplorerPlan {
    /// The items for the engine: one per mark that still exists.
    pub plan: Plan,
    /// The selection the plan was built with: its flags are `--only <every mark, quoted>`.
    pub selection: Selection,
    pub rows: Vec<ExplorerRow>,
    /// The scan roots the plan was made against (bound into the id).
    pub roots: Vec<PathBuf>,
}

impl ExplorerPlan {
    /// Allocated bytes less shared extents of the rows that would be removed.
    pub fn total_bytes(&self) -> u64 {
        self.rows
            .iter()
            .filter(|r| r.planned == Planned::Will)
            .map(|r| r.attributed_bytes)
            .sum()
    }

    pub fn will_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| r.planned == Planned::Will)
            .count()
    }

    /// The identity-bound id of the rows that would be removed.
    pub fn plan_id(&self) -> PlanId {
        PlanId::for_explorer(
            self.rows
                .iter()
                .filter(|r| r.planned == Planned::Will)
                .filter_map(|r| r.identity.map(|i| (r.path.as_path(), i))),
            &self.roots,
        )
    }

    /// The entry `p` saves in `last_plan` (the store stamps its time and effective uid).
    pub fn saved_selection(&self, euid: u32, now: i64) -> SavedSelection {
        let mut keys: Vec<String> = self
            .rows
            .iter()
            .filter(|r| r.planned == Planned::Will)
            .map(|r| format!("{}:{}", Kind::ExplorerSelection.id(), r.path.display()))
            .collect();
        keys.sort();
        SavedSelection {
            plan_id: self.plan_id().to_string(),
            saved_at: now,
            euid,
            kinds: self.selection.kinds_resolved(),
            flags: self.selection.flags(),
            items: keys.iter().map(|k| ItemKey::new(k)).collect(),
            items_truncated: false,
        }
    }
}

/// [`MarkFacts`] over the `Fs` port: symlinks and devices from fresh `lstat`.
struct FsMarkFacts<'a> {
    fs: &'a dyn Fs,
}

impl MarkFacts for FsMarkFacts<'_> {
    fn symlink_in(&self, path: &Path) -> Option<SymlinkFact> {
        let mut at = PathBuf::new();
        for comp in path.components() {
            at.push(comp);
            if self
                .fs
                .lstat(&at)
                .is_ok_and(|st| st.kind == FileKind::Symlink)
            {
                return Some(SymlinkFact { real: None });
            }
        }
        None
    }

    fn dev_of(&self, path: &Path) -> Option<u64> {
        self.fs.lstat(path).ok().map(|st| st.ident.dev)
    }

    fn is_boundary(&self, _path: &Path) -> bool {
        false
    }

    fn scanning(&self, _path: &Path) -> bool {
        false
    }
}

/// The skip reason the engine's vocabulary has for a mark refusal.
fn skip_of(r: &MarkRefusal) -> SkipReason {
    match r {
        MarkRefusal::OutsideRoot => SkipReason::OutsideRoot,
        MarkRefusal::SymlinkRow => SkipReason::Symlink,
        MarkRefusal::MountPoint | MarkRefusal::BindMountBelow(_) => SkipReason::MountPoint,
        MarkRefusal::Engine(s) => *s,
        _ => SkipReason::ProtectedPath,
    }
}

struct Judged {
    guard: Guarded,
    note: Option<String>,
}

/// Rules M1 to M5 for one existing mark: the engine's `decide_path_common` on fresh facts, then `mark_check`
/// (the same rules the screen applied when the mark was made).
fn judge(
    path: &Path,
    st: &crate::delete_types::StatInfo,
    mounts: &MountTable,
    mount_points: &[PathBuf],
    ctx: &ExplorerPlanCtx<'_>,
) -> Judged {
    let root_dev = ctx
        .roots
        .scan_roots()
        .iter()
        .find(|r| path.starts_with(r))
        .and_then(|r| ctx.fs.lstat(r).ok())
        .map_or(RootDev(st.ident.dev), |s| RootDev(s.ident.dev));
    let facts = Facts {
        kind: Kind::ExplorerSelection,
        path: path.to_path_buf(),
        ident: st.ident,
        is_symlink: st.kind == FileKind::Symlink,
        root_dev,
        is_mount_point: mount_points.iter().any(|m| m == path),
    };
    if let GuardDecision::Skip(r) = decide_path_common(&facts, ctx.roots) {
        return Judged {
            guard: Guarded::Skip(r),
            note: None,
        };
    }
    let mark_facts = FsMarkFacts { fs: ctx.fs };
    let env = MarkEnv {
        roots: ctx.roots.scan_roots(),
        home: ctx.roots.home(),
        mounts,
        own_state: ctx.own_state,
        facts: &mark_facts,
        tty: false,
    };
    let empty = Marks::new();
    match mark_check(
        path,
        &MarkCtx {
            env: &env,
            marks: &empty,
        },
    ) {
        Ok(ok) => Judged {
            guard: Guarded::Allow,
            note: ok.note,
        },
        Err(r) => Judged {
            guard: Guarded::Skip(skip_of(&r)),
            note: None,
        },
    }
}

fn gone_like(e: &io::Error) -> bool {
    // ENOENT, or ENOTDIR when a parent was replaced by a file.
    matches!(e.kind(), io::ErrorKind::NotFound) || e.raw_os_error() == Some(20)
}

fn shared_of(extents: &dyn ExtentProbe, path: &Path) -> Option<u64> {
    match extents.attribute(&[path], EXTENT_BUDGET) {
        ExtentAttribution::Measured(items) => Some(
            items
                .iter()
                .flat_map(|i| i.shared.iter())
                .map(|e| e.length)
                .sum(),
        ),
        ExtentAttribution::Unavailable(_) => None,
    }
}

fn mount_facts(probe: &dyn SystemProbe) -> (MountTable, Vec<PathBuf>) {
    let Ok(entries) = probe.mounts() else {
        return (MountTable::default(), Vec::new());
    };
    let table = MountTable::new(
        entries
            .iter()
            .map(|m| MarkMount {
                point: m.mount_point.clone(),
                source: format!("device {}", m.dev),
            })
            .collect(),
    );
    (table, entries.into_iter().map(|m| m.mount_point).collect())
}

/// One mark, planned: its row, and the candidate and guard when it still exists.
struct Planned1 {
    row: ExplorerRow,
    item: Option<(Candidate, Guarded)>,
}

fn plan_one(
    path: &Path,
    gone_is_error: bool,
    mounts: &(MountTable, Vec<PathBuf>),
    ctx: &ExplorerPlanCtx<'_>,
) -> Result<Planned1, ()> {
    let mut row = ExplorerRow {
        path: path.to_path_buf(),
        planned: Planned::WillNot(SkipReason::Gone),
        identity: None,
        attributed_bytes: 0,
        shared_bytes: None,
        note: None,
        advice: None,
        risk: None,
    };
    let st = match ctx.fs.lstat(path) {
        Ok(st) => st,
        Err(e) if gone_like(&e) => {
            return if gone_is_error {
                Err(())
            } else {
                Ok(Planned1 { row, item: None })
            };
        }
        Err(_) => {
            // Unreadable: fail closed.
            row.planned = Planned::WillNot(SkipReason::ProtectedPath);
            return Ok(Planned1 { row, item: None });
        }
    };
    row.identity = Some(ItemIdentity::of(&st));
    let judged = judge(path, &st, &mounts.0, &mounts.1, ctx);
    row.note = judged.note;
    if let Some(Guarded::Skip(why)) = ctx.cleaner.guard_of(path) {
        row.advice = Some(format!(
            "the cleaner would skip this: {}",
            why.reason_text()
        ));
    }
    let is_dir = st.kind == FileKind::Dir;
    if is_dir && ctx.fs.lstat(&path.join(".git")).is_ok() {
        row.risk = Some("git repository: unpushed work is lost".into());
    }
    let (allocated, mtime) = match ctx.sizer.measure(path) {
        Ok(t) => (t.allocated, t.newest_mtime),
        Err(_) => (AllocatedBytes(st.blocks.saturating_mul(512)), None),
    };
    row.shared_bytes = ctx.extents.and_then(|e| shared_of(e, path));
    row.attributed_bytes = allocated.0.saturating_sub(row.shared_bytes.unwrap_or(0));
    let candidate = Candidate::new(
        Locator::Path(path.to_path_buf()),
        KindFacts::Explorer(ExplorerFacts { is_dir }),
        allocated,
        mtime,
        st.ident,
    );
    Ok(Planned1 {
        row,
        item: Some((candidate, judged.guard)),
    })
}

/// Plans exactly `marks` as the kind `explorer-selection`.
///
/// `OnlyMode::Explorer` keeps a vanished mark as a `SKIP gone` row; `Strict` refuses it (the apply's re-plan
/// of Story 6.6.3); any other mode drops it from the rows.
pub fn explorer_plan(
    marks: &[PathBuf],
    mode: OnlyMode,
    ctx: &ExplorerPlanCtx<'_>,
) -> Result<ExplorerPlan, ExplorerPlanError> {
    let selection = Selection::explorer(marks.to_vec());
    selection
        .validate_only()
        .map_err(|e| ExplorerPlanError::PathTooLong(e.0))?;
    let mounts = mount_facts(ctx.probe);
    let mut rows: Vec<ExplorerRow> = Vec::new();
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut guards: Vec<(PathBuf, Guarded)> = Vec::new();
    for path in selection.only_sorted() {
        let one = plan_one(path, mode == OnlyMode::Strict, &mounts, ctx)
            .map_err(|()| ExplorerPlanError::NotAnItem(not_an_item_text(&selection, path)))?;
        let gone = one.item.is_none() && one.row.planned == Planned::WillNot(SkipReason::Gone);
        if let Some((c, g)) = one.item {
            candidates.push(c);
            guards.push((path.to_path_buf(), g));
        }
        if !gone || mode == OnlyMode::Explorer {
            rows.push(one.row);
        }
    }
    let guard_of = |c: &Candidate| -> Guarded {
        match c.locator() {
            Locator::Path(p) => guards
                .iter()
                .find(|(g, _)| g == p)
                .map_or(Guarded::Skip(SkipReason::ProtectedPath), |(_, g)| *g),
            _ => Guarded::Skip(SkipReason::ProtectedPath),
        }
    };
    let plan = Plan::build(&selection, ctx.clock.now_epoch(), candidates, &guard_of);
    for item in &plan.items {
        if let Locator::Path(p) = item.candidate.locator() {
            if let Some(row) = rows.iter_mut().find(|r| &r.path == p) {
                row.planned = item.planned;
            }
        }
    }
    Ok(ExplorerPlan {
        plan,
        selection,
        rows,
        roots: ctx.roots.scan_roots().to_vec(),
    })
}
