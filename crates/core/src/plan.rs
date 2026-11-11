//! `Plan`, `PlanItem`, `PlanId` and the Destructive-tier plan-id gate (Story 1.1.4, Task 1.1.4a). Pure: the
//! impure part (asking a target's guard) is passed in as a closure, and the saved plans arrive as data.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::candidate::{Candidate, Locator};
use crate::delete_types::{FileKind, StatInfo};
use crate::kind::SelectorId;
use crate::outcome::SkipReason;
use crate::plan_state::{ItemKey, SaveOutcome, SavedPlan, SavedSelection};
use crate::ports::SAVED_TTL_SECS;
use crate::selector::Selection;
use crate::target::{kind_order, Guarded};

/// The name `Next:` lines and refusal texts print for the binary.
pub const BIN: &str = "disk-cleanup";

/// What the plan decided for one item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Planned {
    Will,
    WillNot(SkipReason),
}

#[derive(Debug, Clone)]
pub struct PlanItem {
    pub candidate: Candidate,
    /// The target's guard at plan time.
    pub guard: Guarded,
    /// The decision under the selection the plan was built with.
    pub planned: Planned,
}

/// Ordered, filtered candidates with their decisions. A dry run stops here.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub items: Vec<PlanItem>,
    /// Unix seconds the plan was built at: the age rule is evaluated against it.
    pub now: i64,
}

/// `Path` equality is per component (`/a/b/` equals `/a/b`), `--only` matches bytes.
fn same_bytes(a: &Path, b: &Path) -> bool {
    a.as_os_str() == b.as_os_str()
}

/// The `--only` filter (Story 6.4.2): with no values every candidate stays; otherwise a candidate stays only
/// when its path equals a value byte for byte (no `realpath`, no prefix, no case folding, no normalisation).
/// A Docker object has no path, so it never matches a value.
pub fn only_keeps(only: &[PathBuf], c: &Candidate) -> bool {
    if only.is_empty() {
        return true;
    }
    match c.locator() {
        Locator::Path(p) | Locator::Steam(_, p) => only.iter().any(|o| same_bytes(o, p)),
        Locator::Docker(_) => false,
    }
}

/// The `--only` values that are not an item of the selected kinds among `candidates`, in sorted order.
pub fn non_items<'a>(sel: &'a Selection, candidates: &[Candidate]) -> Vec<&'a Path> {
    sel.only_sorted()
        .into_iter()
        .filter(|o| {
            !candidates.iter().any(|c| {
                sel.selects(c.selector())
                    && matches!(c.locator(), Locator::Path(p) | Locator::Steam(_, p) if same_bytes(p, o))
            })
        })
        .collect()
}

/// The exit-64 text for an `--only` value that is not an item (`design/ux.md` 1.1).
pub fn not_an_item_text(sel: &Selection, only: &Path) -> String {
    format!(
        "error: --only {} is not an item of the selected kinds ({}). It may be new, under the age floor, kept as the newest duplicate, or not a cleanable item at all, or spelled differently from the path the cleaner prints (no symlinks, no '.' or '..', no trailing slash); dry run without --only to list the items. Nothing was deleted.",
        crate::shell_quote::shell_quote(only.as_os_str(), false),
        sel.kinds_resolved()
    )
}

/// Printed after an `--apply ... --only` line that needs no plan-id (Safe tier, worktrees without
/// `--worktree-ignored discard`): there `--only` is a filter and nothing more (UX-T6).
pub const NO_PLAN_ID_ONLY_NOTE: &str = "This line needs no plan-id; --only limits it to what you marked. Without --only it would apply to every item of those kinds.";

/// How a planner treats an `--only` value that is no longer an item: the CLI refuses (`Strict`), the
/// explorer's exit report plans the survivors (`Tolerant`), and the explorer's own selection plan keeps a
/// vanished mark as a `gone` row (`Explorer`, Story 6.6.2: the marks are the items, nothing is discovered).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnlyMode {
    Strict,
    Tolerant,
    Explorer,
}

/// A plan restricted to the `--only` values that are items. `selection` is the one the plan was built with:
/// under `Tolerant` its `only` holds the survivors alone, so its flags and `PlanId` cover exactly them.
#[derive(Debug, Clone)]
pub struct OnlyPlan {
    pub plan: Plan,
    pub selection: Selection,
    /// Values that were not items (always empty under `Strict`, which refuses instead).
    pub stale: Vec<PathBuf>,
}

/// Plan `sel` after discovery. Strict: the first value that is not an item is an `Err` carrying the
/// exit-64 text. Tolerant: such values are returned in `stale`; with no survivor the plan holds no item
/// (an empty `only` would mean "everything").
pub fn plan_only(
    sel: &Selection,
    now: i64,
    candidates: Vec<Candidate>,
    guard: &dyn Fn(&Candidate) -> Guarded,
    mode: OnlyMode,
) -> Result<OnlyPlan, String> {
    let stale: Vec<PathBuf> = non_items(sel, &candidates)
        .into_iter()
        .map(Path::to_path_buf)
        .collect();
    // `Explorer` is meaningful to `explorer_plan` only; over discovered candidates it plans the survivors.
    if mode == OnlyMode::Strict {
        if let Some(first) = stale.first() {
            return Err(not_an_item_text(sel, first));
        }
        let plan = Plan::build(sel, now, candidates, guard);
        return Ok(OnlyPlan {
            plan,
            selection: sel.clone(),
            stale,
        });
    }
    let mut narrowed = sel.clone();
    narrowed
        .only
        .retain(|o| !stale.iter().any(|s| same_bytes(s, o)));
    let any_asked = !sel.only.is_empty();
    if any_asked && narrowed.only.is_empty() {
        return Ok(OnlyPlan {
            plan: Plan {
                items: Vec::new(),
                now,
            },
            selection: narrowed,
            stale,
        });
    }
    let plan = Plan::build(&narrowed, now, candidates, guard);
    Ok(OnlyPlan {
        plan,
        selection: narrowed,
        stale,
    })
}

fn decide(sel: &Selection, c: &Candidate, guard: Guarded, now: i64) -> Planned {
    if let Err(r) = sel.admits(c, now) {
        return Planned::WillNot(r);
    }
    match guard {
        Guarded::Allow => Planned::Will,
        Guarded::Skip(r) => Planned::WillNot(r),
    }
}

fn locator_text(l: &Locator) -> String {
    match l {
        Locator::Path(p) | Locator::Steam(_, p) => p.to_string_lossy().into_owned(),
        Locator::Docker(d) => d.0.clone(),
    }
}

impl Plan {
    /// Drop what the selection drops outright, then decide each remaining item. Items are ordered by kind
    /// (Cache, Worktree, build dirs, Steam, Docker, Btrfs), discovery order within a kind.
    pub fn build(
        sel: &Selection,
        now: i64,
        candidates: Vec<Candidate>,
        guard: &dyn Fn(&Candidate) -> Guarded,
    ) -> Plan {
        let mut items: Vec<PlanItem> = candidates
            .into_iter()
            .filter(|c| sel.keeps(c))
            .map(|c| {
                let g = guard(&c);
                let planned = decide(sel, &c, g, now);
                PlanItem {
                    candidate: c,
                    guard: g,
                    planned,
                }
            })
            .collect();
        items.sort_by_key(|i| kind_order(i.candidate.kind()));
        Plan { items, now }
    }

    /// Items that would be removed under `sel`: kept, selected, old enough and guard-clean.
    fn effective<'a>(&'a self, sel: &'a Selection) -> impl Iterator<Item = &'a PlanItem> + 'a {
        self.items.iter().filter(move |i| {
            sel.keeps(&i.candidate)
                && i.guard == Guarded::Allow
                && sel.admits(&i.candidate, self.now).is_ok()
        })
    }

    /// `<kind>:<path or docker id>` of every effective item, sorted.
    pub fn keys(&self, sel: &Selection) -> Vec<ItemKey> {
        let mut keys: Vec<String> = self
            .effective(sel)
            .map(|i| {
                format!(
                    "{}:{}",
                    i.candidate.kind().id(),
                    locator_text(i.candidate.locator())
                )
            })
            .collect();
        keys.sort();
        keys.iter().map(|k| ItemKey::new(k)).collect()
    }

    /// The entry plan mode saves in `last_plan` for `sel` (the store stamps its time and effective uid).
    pub fn saved_selection(&self, sel: &Selection, euid: u32, now: i64) -> SavedSelection {
        SavedSelection {
            plan_id: PlanId::for_selection(self, sel).to_string(),
            saved_at: now,
            euid,
            kinds: sel.kinds_resolved(),
            flags: sel.flags(),
            items: self.keys(sel),
            items_truncated: false,
        }
    }

    /// The `n` largest items by size at discovery (what `--top n` prints).
    pub fn top(&self, n: usize) -> Vec<&PlanItem> {
        let mut v: Vec<&PlanItem> = self.items.iter().collect();
        v.sort_by_key(|i| std::cmp::Reverse(i.candidate.est_bytes()));
        v.truncate(n);
        v
    }

    pub fn will_count(&self) -> usize {
        self.items
            .iter()
            .filter(|i| i.planned == Planned::Will)
            .count()
    }
}

/// Eight hex digits of a hash of the effective item set (kinds and ids, never sizes, which drift between a
/// dry run and the apply).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PlanId(u32);

/// 64-bit FNV-1a, folded to the 32 bits a plan-id shows.
struct Fnv(u64);

impl Fnv {
    fn new() -> Fnv {
        Fnv(0xcbf2_9ce4_8422_2325)
    }

    fn feed(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }

    /// A length-prefixed field, so two different lists of fields never feed the same bytes.
    fn field(&mut self, bytes: &[u8]) {
        self.feed(&(bytes.len() as u64).to_le_bytes());
        self.feed(bytes);
    }

    fn id(&self) -> PlanId {
        PlanId((self.0 >> 32) as u32 ^ self.0 as u32)
    }
}

/// What `lstat` said a marked path is at plan time (Story 6.6.2, UX-T34): the device, the inode and the file
/// type. A directory removed and recreated under the same name has another inode, so a plan reviewed for the
/// old one cannot authorise deleting the new one. Sizes and times are left out on purpose: they drift between
/// the review and the delete and would make every plan-id stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemIdentity {
    pub dev: u64,
    pub ino: u64,
    pub kind: FileKind,
}

impl ItemIdentity {
    pub fn of(st: &StatInfo) -> ItemIdentity {
        ItemIdentity {
            dev: st.ident.dev,
            ino: st.ident.ino,
            kind: st.kind,
        }
    }

    fn kind_tag(self) -> u8 {
        match self.kind {
            FileKind::Dir => 1,
            FileKind::File => 2,
            FileKind::Symlink => 3,
            FileKind::Fifo => 4,
            FileKind::Socket => 5,
            FileKind::CharDev => 6,
            FileKind::BlockDev => 7,
            FileKind::Other => 8,
        }
    }
}

impl PlanId {
    /// Computable for any selection from one discovery: a guard decision does not depend on the selection
    /// except through `KindNotSelected` and the age rule, which `Selection::admits` re-evaluates.
    pub fn for_selection(plan: &Plan, sel: &Selection) -> PlanId {
        let mut pairs: Vec<(&'static str, String)> = plan
            .effective(sel)
            .map(|i| (i.candidate.kind().id(), i.candidate.id().as_hex()))
            .collect();
        pairs.sort();
        let mut h = Fnv::new();
        for (kind, id) in &pairs {
            h.feed(kind.as_bytes());
            h.feed(&[0]);
            h.feed(id.as_bytes());
            h.feed(&[1]);
        }
        h.id()
    }

    /// The explorer's plan-id (Story 6.6.2): the kind name, the sorted marked paths with their identities and
    /// the resolved scan roots. Another set, another inode behind a path, another root list or another kind
    /// (a cleaner plan over the same paths) gives another id; the order the marks were made in does not.
    pub fn for_explorer<'a>(
        items: impl IntoIterator<Item = (&'a Path, ItemIdentity)>,
        roots: &[crate::roots::RootIdentity],
    ) -> PlanId {
        use std::os::unix::ffi::OsStrExt;
        let mut items: Vec<(&Path, ItemIdentity)> = items.into_iter().collect();
        items.sort_by(|a, b| a.0.as_os_str().cmp(b.0.as_os_str()));
        let mut roots: Vec<&crate::roots::RootIdentity> = roots.iter().collect();
        roots.sort_by(|a, b| a.path.as_os_str().cmp(b.path.as_os_str()));
        let mut h = Fnv::new();
        h.field(crate::kind::Kind::ExplorerSelection.id().as_bytes());
        for (path, id) in &items {
            h.field(path.as_os_str().as_bytes());
            h.feed(&id.dev.to_le_bytes());
            h.feed(&id.ino.to_le_bytes());
            h.feed(&[id.kind_tag()]);
        }
        h.feed(&[0xff]);
        for r in roots {
            h.field(r.path.as_os_str().as_bytes());
            h.feed(&r.ident.dev.to_le_bytes());
            h.feed(&r.ident.ino.to_le_bytes());
        }
        h.id()
    }

    pub fn from_hex(s: &str) -> Option<PlanId> {
        if s.len() == 8 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
            u32::from_str_radix(s, 16).ok().map(PlanId)
        } else {
            None
        }
    }
}

impl std::fmt::Display for PlanId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:08x}", self.0)
    }
}

/// `--plan-id` as typed: exactly eight hex digits, upper case lowered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanIdArg(PlanId);

/// The `malformed` text of `design/ux.md` 1.1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanIdArgError(pub String);

impl std::fmt::Display for PlanIdArgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "error: invalid value '{}' for '--plan-id <ID>': a plan-id is 8 hex digits (for example 7f3a91c2), copied from the Next: line of a dry run.",
            self.0
        )
    }
}

impl std::error::Error for PlanIdArgError {}

impl std::str::FromStr for PlanIdArg {
    type Err = PlanIdArgError;

    fn from_str(s: &str) -> Result<PlanIdArg, PlanIdArgError> {
        PlanId::from_hex(s)
            .map(PlanIdArg)
            .ok_or_else(|| PlanIdArgError(s.to_string()))
    }
}

impl PlanIdArg {
    pub fn plan_id(self) -> PlanId {
        self.0
    }
}

/// Why the gate refused `--apply`. The exit class is usage (64) for every variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateRefusal {
    MissingPlanId {
        recent: Vec<RecentPlan>,
    },
    /// The id is not saved, or nothing is saved.
    NoRecentPlan {
        supplied: PlanId,
        recent: Vec<RecentPlan>,
    },
    /// Saved, but over 24 h ago.
    Stale {
        supplied: PlanId,
        age_secs: i64,
        recent: Vec<RecentPlan>,
    },
    Changed {
        supplied: PlanId,
        recomputed: PlanId,
        cause: ChangeCause,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentPlan {
    pub plan_id: String,
    /// `--kinds ...` plus the flags, as the dry run was typed.
    pub args: String,
    pub age_secs: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeCause {
    ItemsMoved {
        vanished: Vec<ItemKey>,
        appeared: Vec<ItemKey>,
        incomplete: bool,
    },
    SelectionDiffers {
        saved_kinds: String,
        saved_flags: String,
        now_kinds: String,
        now_flags: String,
    },
}

fn entry_args(kinds: &str, flags: &str) -> String {
    if flags.is_empty() {
        format!("--kinds {kinds}")
    } else {
        format!("--kinds {kinds} {flags}")
    }
}

/// The three newest unexpired entries, never the one named `except`.
fn recent_of(saved: Option<&SavedPlan>, now: i64, except: Option<&str>) -> Vec<RecentPlan> {
    let mut v: Vec<&SavedSelection> = saved
        .map(|s| s.entries.iter())
        .into_iter()
        .flatten()
        .filter(|e| {
            (0..=SAVED_TTL_SECS).contains(&(now - e.saved_at))
                && Some(e.plan_id.as_str()) != except
                && !e.kinds.contains(crate::kind::Kind::ExplorerSelection.id())
        })
        .collect();
    v.sort_by_key(|e| std::cmp::Reverse(e.saved_at));
    v.into_iter()
        .take(3)
        .map(|e| RecentPlan {
            plan_id: e.plan_id.clone(),
            args: entry_args(&e.kinds, &e.flags),
            age_secs: now - e.saved_at,
        })
        .collect()
}

fn entry_of<'a>(saved: Option<&'a SavedPlan>, id: &str) -> Option<&'a SavedSelection> {
    saved?
        .entries
        .iter()
        .filter(|e| e.plan_id == id)
        .max_by_key(|e| e.saved_at)
}

/// The Destructive-tier gate. Pure.
///
/// `Ok` when the selection needs no id and none was supplied; otherwise only when `supplied` names an entry
/// of `saved` that is at most 24 h old (boundary included) and equals `recomputed`. A supplied id is verified
/// for any selection. `current_keys` are the keys of the items the selection selects now
/// (`Plan::keys`); they let a refusal say what moved.
pub fn destructive_gate(
    sel: &Selection,
    supplied: Option<PlanId>,
    saved: Option<&SavedPlan>,
    recomputed: PlanId,
    current_keys: &[ItemKey],
    now: i64,
) -> Result<(), GateRefusal> {
    let Some(id) = supplied else {
        return if sel.needs_plan_id() {
            Err(GateRefusal::MissingPlanId {
                recent: recent_of(saved, now, None),
            })
        } else {
            Ok(())
        };
    };
    let hex = id.to_string();
    let Some(entry) = entry_of(saved, &hex) else {
        return Err(GateRefusal::NoRecentPlan {
            supplied: id,
            recent: recent_of(saved, now, None),
        });
    };
    let age = now - entry.saved_at;
    // A `saved_at` in the future (a clock stepped back) would never age out: treat it as stale (review C10).
    if age > SAVED_TTL_SECS || age < 0 {
        return Err(GateRefusal::Stale {
            supplied: id,
            age_secs: age,
            recent: recent_of(saved, now, Some(&hex)),
        });
    }
    if id == recomputed {
        return Ok(());
    }
    let (now_kinds, now_flags) = (sel.kinds_resolved(), sel.flags());
    let cause = if entry.kinds != now_kinds || entry.flags != now_flags {
        ChangeCause::SelectionDiffers {
            saved_kinds: entry.kinds.clone(),
            saved_flags: entry.flags.clone(),
            now_kinds,
            now_flags,
        }
    } else if entry.items.is_empty() {
        // no saved keys: nothing to diff against, so the text carries no lists
        ChangeCause::ItemsMoved {
            vanished: Vec::new(),
            appeared: Vec::new(),
            incomplete: entry.items_truncated,
        }
    } else {
        ChangeCause::ItemsMoved {
            vanished: entry
                .items
                .iter()
                .filter(|k| !current_keys.contains(k))
                .cloned()
                .collect(),
            appeared: current_keys
                .iter()
                .filter(|k| !entry.items.contains(k))
                .cloned()
                .collect(),
            incomplete: entry.items_truncated,
        }
    };
    Err(GateRefusal::Changed {
        supplied: id,
        recomputed,
        cause,
    })
}

/// Why the explorer's `d` was refused before anything else ran (Story 6.6.2). Every variant means nothing was
/// deleted; the text always ends with the way forward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExplorerGateRefusal {
    /// No saved plan of this user has this id.
    NoSavedPlan,
    /// The plan was saved more than 24 h ago.
    Stale { age_secs: i64 },
    /// A plan with this id exists, but it was made for other marks, other things behind the marks or other
    /// roots.
    Changed,
    /// The preview could not save its plan (`plan-id NOT usable`), so nothing can authorise a delete.
    NotUsable,
}

impl ExplorerGateRefusal {
    pub fn text(&self) -> String {
        match self {
            ExplorerGateRefusal::NoSavedPlan => "error: no saved plan for these marks in the last 24 h. Review the plan, then press d again. Nothing was deleted.".into(),
            ExplorerGateRefusal::Stale { age_secs } => format!(
                "error: the plan was made {} and a plan is good for 24 h. Review the plan, then press d again. Nothing was deleted.",
                age_text(*age_secs)
            ),
            ExplorerGateRefusal::Changed => "error: the marks, or the items they point at, changed since the plan was made. Review the plan, then press d again. Nothing was deleted.".into(),
            ExplorerGateRefusal::NotUsable => "error: the plan could not be saved (plan-id NOT usable), so it cannot authorise a delete. Free space in the state directory, review the plan, then press d again. Nothing was deleted.".into(),
        }
    }
}

/// The explorer's use of the Destructive-tier gate (Story 6.6.2, UX-T34). It is [`destructive_gate`], the
/// function `--apply` uses, called on the entries of this user only, plus one check the cleaner's gate does
/// not make: the stored kinds and flags (the exact marked paths) must equal the recomputed ones, so a
/// collision of the 32-bit id cannot authorise another set.
///
/// `saved_as` is what the preview's `save_merged` returned: a plan that was not saved authorises nothing.
pub fn check_plan_id(
    sel: &Selection,
    supplied: PlanId,
    saved_as: SaveOutcome,
    saved: Option<&SavedPlan>,
    euid: u32,
    recomputed: PlanId,
    now: i64,
) -> Result<(), ExplorerGateRefusal> {
    if matches!(saved_as, SaveOutcome::NotSaved(_)) {
        return Err(ExplorerGateRefusal::NotUsable);
    }
    let own = saved.map(|s| SavedPlan {
        entries: s
            .entries
            .iter()
            .filter(|e| e.euid == euid)
            .cloned()
            .collect(),
    });
    match destructive_gate(sel, Some(supplied), own.as_ref(), recomputed, &[], now) {
        Ok(()) => {}
        Err(GateRefusal::Stale { age_secs, .. }) => {
            return Err(ExplorerGateRefusal::Stale { age_secs });
        }
        Err(GateRefusal::Changed { .. }) => return Err(ExplorerGateRefusal::Changed),
        Err(GateRefusal::NoRecentPlan { .. } | GateRefusal::MissingPlanId { .. }) => {
            return Err(ExplorerGateRefusal::NoSavedPlan);
        }
    }
    let hex = supplied.to_string();
    let same_set = entry_of(own.as_ref(), &hex)
        .is_some_and(|e| e.kinds == sel.kinds_resolved() && e.flags == sel.flags());
    if same_set {
        Ok(())
    } else {
        Err(ExplorerGateRefusal::Changed)
    }
}

/// What a refusal text needs besides the refusal itself.
pub struct GateRenderCtx<'a> {
    pub selection: &'a Selection,
    pub saved: Option<&'a SavedPlan>,
    /// Effective uid of this run.
    pub euid: u32,
    /// Set when the state directory has under 1 MiB free: its display path and the free bytes.
    pub state_dir_low: Option<(String, u64)>,
    /// Selectors the availability step found unreachable now.
    pub unavailable: Vec<SelectorId>,
    /// Shown as `~` in listed paths.
    pub home: Option<&'a Path>,
}

fn age_text(secs: i64) -> String {
    let s = secs.max(0);
    if s >= 3600 {
        format!("{}h ago", s / 3600)
    } else if s >= 60 {
        format!("{}m ago", s / 60)
    } else {
        format!("{s}s ago")
    }
}

fn bytes_text(b: u64) -> String {
    if b >= 1 << 10 {
        format!("{} KiB", b >> 10)
    } else {
        format!("{b} B")
    }
}

fn shown(key: &ItemKey, home: Option<&Path>) -> String {
    let path = key
        .as_str()
        .split_once(':')
        .map_or(key.as_str(), |(_, p)| p);
    match home.and_then(|h| Path::new(path).strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.to_string(),
    }
}

fn list_text(keys: &[ItemKey], home: Option<&Path>) -> String {
    let mut out: Vec<String> = keys.iter().take(3).map(|k| shown(k, home)).collect();
    if keys.len() > 3 {
        out.push(format!("+{} more", keys.len() - 3));
    }
    out.join(", ")
}

impl GateRefusal {
    /// The `error:` text of `design/ux.md` 1.1, ending with the paste-ready dry-run command and
    /// `Nothing was deleted.`
    pub fn render(&self, ctx: &GateRenderCtx) -> String {
        let args = ctx.selection.dry_run_args();
        let mut extras: Vec<String> = Vec::new();
        let body = match self {
            GateRefusal::MissingPlanId { recent } => missing_text(ctx, recent, &args),
            GateRefusal::NoRecentPlan { supplied, recent }
            | GateRefusal::Stale {
                supplied, recent, ..
            } => format!(
                "error: no dry run with plan-id {supplied} in the last 24h ({}). Dry run again, then use the plan-id on its Next: line: {BIN} {args}.",
                recent_inline(recent)
            ),
            GateRefusal::Changed {
                supplied,
                recomputed,
                cause,
            } => {
                if let Some(first) = ctx.unavailable.first() {
                    extras.push(format!(
                        "{0} was not reachable now, which changes the id (it was in your dry run). Start Docker and re-run the same command, or dry run again without {0}.",
                        first.id()
                    ));
                }
                changed_text(*supplied, *recomputed, cause, ctx, &args)
            }
        };
        let saved_uid = match self {
            GateRefusal::Changed { supplied, .. } => {
                entry_of(ctx.saved, &supplied.to_string()).map(|e| e.euid)
            }
            _ => None,
        };
        if let Some(uid) = saved_uid.filter(|u| *u != ctx.euid) {
            let me = if ctx.euid == 0 {
                "root".to_string()
            } else {
                format!("uid {}", ctx.euid)
            };
            extras.push(format!(
                "That dry run was made as uid {uid}; this run is {me}, which sees a different HOME and skips your user caches, so its ids differ: dry run and apply as the same user."
            ));
        }
        if let Some((dir, free)) = &ctx.state_dir_low {
            extras.push(format!(
                "The state directory {dir} has {} free: free about 1 MiB there, then dry run again.",
                bytes_text(*free)
            ));
        }
        if extras.is_empty() {
            format!("{body} Nothing was deleted.")
        } else {
            format!("{body} {} Nothing was deleted.", extras.join(" "))
        }
    }
}

fn missing_text(ctx: &GateRenderCtx, recent: &[RecentPlan], args: &str) -> String {
    let mut names: Vec<String> = ctx
        .selection
        .destructive_selectors()
        .iter()
        .map(|s| s.id().to_string())
        .collect();
    if ctx.selection.worktree_ignored_discard() {
        names.push("--worktree-ignored discard".into());
    }
    let list = if recent.is_empty() {
        "none".to_string()
    } else {
        recent
            .iter()
            .map(|r| format!("{} ('{}', {})", r.plan_id, r.args, age_text(r.age_secs)))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "error: destructive kinds ({}) need --plan-id from a dry run made in the last 24h. Run '{BIN} {args}' first and use the plan-id on its Next: line. Recent dry runs: {list}.",
        names.join(", ")
    )
}

fn recent_inline(recent: &[RecentPlan]) -> String {
    if recent.is_empty() {
        return "recent: none".into();
    }
    let items: Vec<String> = recent
        .iter()
        .map(|r| format!("{} for '{}', {}", r.plan_id, r.args, age_text(r.age_secs)))
        .collect();
    format!("recent: {}", items.join("; "))
}

fn moved_lists(
    supplied: PlanId,
    vanished: &[ItemKey],
    appeared: &[ItemKey],
    incomplete: bool,
    ctx: &GateRenderCtx,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !vanished.is_empty() {
        parts.push(format!(
            "{} {} vanished ({})",
            vanished.len(),
            if vanished.len() == 1 { "item" } else { "items" },
            list_text(vanished, ctx.home)
        ));
    }
    if !appeared.is_empty() {
        parts.push(format!(
            "{} appeared ({})",
            appeared.len(),
            list_text(appeared, ctx.home)
        ));
    }
    if parts.is_empty() {
        return String::new();
    }
    let mut lists = format!(": {}", parts.join(", "));
    if incomplete {
        let kept = entry_of(ctx.saved, &supplied.to_string()).map_or(0, |e| e.items.len());
        let _ = write!(
            lists,
            " (list incomplete: the saved plan keeps the first {kept} items)"
        );
    }
    lists
}

fn changed_text(
    supplied: PlanId,
    recomputed: PlanId,
    cause: &ChangeCause,
    ctx: &GateRenderCtx,
    args: &str,
) -> String {
    match cause {
        ChangeCause::SelectionDiffers {
            saved_kinds,
            saved_flags,
            now_kinds,
            now_flags,
        } => format!(
            "error: plan-id {supplied} was a dry run of '{}'; this command selects '{}', so the ids differ. Use the Next: line of that dry run unchanged, or dry run this selection: {BIN} {args}.",
            entry_args(saved_kinds, saved_flags),
            entry_args(now_kinds, now_flags)
        ),
        ChangeCause::ItemsMoved {
            vanished,
            appeared,
            incomplete,
        } => {
            let lists = moved_lists(supplied, vanished, appeared, *incomplete, ctx);
            format!(
                "error: plan-id {supplied} does not match the plan just computed ({recomputed}){lists}. Dry run again, then use the new plan-id on its Next: line: {BIN} {args}. If an earlier apply was interrupted or partly failed, this is expected."
            )
        }
    }
}
