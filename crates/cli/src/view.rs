//! The data both renderers draw from: rows, subtotals and `Next:` commands of a plan (`design/ux.md` S1).
//! The human report and the JSON document are built from the same [`PlanView`], so the ids on the `Next:`
//! lines, in `next[]` and in the entries handed to `PlanStore` cannot disagree.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use disk_cleanup_core::candidate::{BtrfsFacts, Candidate, DockerFacts, KindFacts, Locator};
use disk_cleanup_core::ids::CandidateId;
use disk_cleanup_core::kind::{Kind, Risk, SelectorId};
use disk_cleanup_core::outcome::SkipReason;
use disk_cleanup_core::plan::{Plan, PlanId, Planned, BIN};
use disk_cleanup_core::plan_state::{SaveOutcome, SavedSelection};
use disk_cleanup_core::selector::{DockerVolumes, KindSelector, Selection};
use disk_cleanup_core::snapshot_types::FsSnapshot;
use disk_cleanup_core::summary::DiskVerdict;
use disk_cleanup_core::target::kind_order;

use crate::escape::{escape, shell_quote};
use crate::reasontext::{self, ReasonFacts, TextCtx};
use crate::units;

/// The family id of a kind: what the KIND column shows.
pub fn family(kind: Kind) -> &'static str {
    kind.family()
}

/// The word of the STATUS column (`design/ux.md` 3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowStatus {
    Free,
    Freed,
    Skip,
    Risk,
    Err,
}

impl RowStatus {
    pub fn word(self) -> &'static str {
        match self {
            RowStatus::Free => "FREE",
            RowStatus::Freed => "FREED",
            RowStatus::Skip => "SKIP",
            RowStatus::Risk => "RISK",
            RowStatus::Err => "ERR",
        }
    }

    /// The `status` value of a JSON item.
    pub fn json(self) -> &'static str {
        match self {
            RowStatus::Free => "would_free",
            RowStatus::Freed => "freed",
            RowStatus::Skip => "skipped",
            RowStatus::Risk => "risk_not_selected",
            RowStatus::Err => "error",
        }
    }
}

/// One item as either renderer shows it.
#[derive(Debug, Clone)]
pub struct ItemRow {
    pub id: CandidateId,
    pub kind: Kind,
    pub selector: SelectorId,
    pub status: RowStatus,
    pub est_bytes: u64,
    /// Apply rows: the bytes the FREED column shows.
    pub freed_bytes: Option<u64>,
    pub measured_bytes: Option<u64>,
    pub elapsed_ms: u64,
    pub age_secs: Option<i64>,
    /// The filesystem path, when the item has one.
    pub path: Option<PathBuf>,
    /// What the ITEM column shows: the path, or the name of a docker or btrfs object.
    pub label: OsString,
    pub reason: Option<SkipReason>,
    pub reason_text: Option<String>,
    pub next_action: Option<String>,
    /// An `ERR` row's own text.
    pub error_text: Option<String>,
    /// Extra clauses of a RISK row, shown inside its parentheses.
    pub risk_notes: Vec<String>,
}

impl ItemRow {
    pub fn tier(&self) -> Risk {
        self.kind.risk()
    }
}

/// How the object of a docker, steam or btrfs candidate is named when it has no path.
pub fn label_of(c: &Candidate) -> OsString {
    match (c.locator(), c.facts()) {
        (Locator::Path(p) | Locator::Steam(_, p), _) => p.as_os_str().to_owned(),
        (Locator::Docker(d), KindFacts::Docker(DockerFacts::Volume { name, anonymous })) => {
            let _ = d;
            OsString::from(format!(
                "volume {name}{}",
                if *anonymous { " (anonymous)" } else { "" }
            ))
        }
        (Locator::Docker(_), KindFacts::Docker(DockerFacts::BuildCache)) => {
            OsString::from("docker build cache")
        }
        (Locator::Docker(_), KindFacts::Docker(DockerFacts::DanglingImage { image_id })) => {
            OsString::from(format!("dangling image {image_id}"))
        }
        (Locator::Docker(_), KindFacts::Docker(DockerFacts::UnusedImage { image_id, tags })) => {
            OsString::from(format!(
                "image {}",
                tags.first().map_or(image_id.as_str(), String::as_str)
            ))
        }
        (Locator::Docker(d), _) => OsString::from(d.0.clone()),
    }
}

fn btrfs_label(c: &Candidate) -> Option<OsString> {
    match c.facts() {
        KindFacts::Btrfs(BtrfsFacts::Balance { usage_pct }) => Some(OsString::from(format!(
            "btrfs balance (usage under {usage_pct}%)"
        ))),
        KindFacts::Btrfs(BtrfsFacts::Trim) => Some(OsString::from("btrfs trim")),
        _ => None,
    }
}

fn label_for(c: &Candidate) -> OsString {
    btrfs_label(c).unwrap_or_else(|| label_of(c))
}

fn path_of(c: &Candidate) -> Option<PathBuf> {
    match c.locator() {
        Locator::Path(p) | Locator::Steam(_, p) => Some(p.clone()),
        Locator::Docker(_) => None,
    }
}

/// `$HOME` as `~` (human output only), then the escaping rule.
pub fn display_path(p: &OsStr, home: Option<&Path>, tty: bool) -> String {
    if let Some(h) = home.filter(|h| !h.as_os_str().is_empty()) {
        let path = Path::new(p);
        if let Ok(rest) = path.strip_prefix(h) {
            let mut bytes = b"~".to_vec();
            if !rest.as_os_str().is_empty() {
                bytes.push(b'/');
                bytes.extend_from_slice(rest.as_os_str().as_bytes());
            }
            return escape(&bytes, tty);
        }
    }
    escape(p.as_bytes(), tty)
}

/// What a plan run knows about its scan.
#[derive(Debug, Clone)]
pub struct ScanFacts {
    pub roots: usize,
    pub secs: f64,
    pub cache_warm: bool,
    pub backend: String,
    pub dirs: u64,
    pub unreadable: u64,
}

/// A filesystem snapshot with the verdict of `metascope_core::health::assess`.
#[derive(Debug, Clone)]
pub struct FsBlock {
    pub snapshot: FsSnapshot,
    pub verdict: DiskVerdict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Size,
    Age,
    Kind,
}

/// `--top`, `--sort`, `-v`, `-q`.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// 0 prints every row.
    pub top: usize,
    pub sort: SortKey,
    pub verbose: bool,
    pub quiet: bool,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            top: 10,
            sort: SortKey::Size,
            verbose: false,
            quiet: false,
        }
    }
}

/// An `errors[]` element of a JSON document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorEntry {
    pub code: String,
    pub path: Option<PathBuf>,
    pub message: String,
}

/// Everything a plan report is built from.
pub struct PlanInput<'a> {
    pub plan: &'a Plan,
    pub selection: &'a Selection,
    pub saved: SaveOutcome,
    pub scan: ScanFacts,
    pub fs: Option<FsBlock>,
    /// `Some(N)` when discovery hit `--max-seconds N`.
    pub partial_scan: Option<u64>,
    /// Kinds this binary does not ship (`not available in this build: ...`).
    pub not_built: Vec<String>,
    /// Pre-formatted `Notes:` lines (Docker SKIPPED, btrfs n/a, snapshots, rootless, plan-during-apply).
    pub notes: Vec<String>,
    /// The wrapper `NOTE:` of `design/ux.md` S10.
    pub wrapper_note: Option<String>,
    /// `std::env::current_exe()`.
    pub exe: PathBuf,
    pub euid: u32,
    pub state_dir: PathBuf,
    pub home: Option<PathBuf>,
    pub facts: HashMap<CandidateId, ReasonFacts>,
    pub opts: Options,
    pub errors: Vec<ErrorEntry>,
    /// Whether stdout is a TTY: decides ASCII-only quoting in printed commands.
    pub tty: bool,
}

/// One subtotal of the decision block.
#[derive(Debug, Clone)]
pub struct Group {
    pub bytes: u64,
    pub items: usize,
    /// Distinct selectors behind the group.
    pub kinds: Vec<SelectorId>,
}

impl Group {
    fn of(rows: &[&ItemRow]) -> Group {
        let kinds: BTreeSet<SelectorId> = rows.iter().map(|r| r.selector).collect();
        Group {
            bytes: rows.iter().map(|r| r.est_bytes).sum(),
            items: rows.len(),
            kinds: kinds.into_iter().collect(),
        }
    }

    pub fn kinds_text(&self) -> String {
        let mut v: Vec<&str> = self.kinds.iter().map(|k| k.id()).collect();
        v.sort_unstable();
        v.join(",")
    }
}

/// One `Next:` command with its explanation.
#[derive(Debug, Clone)]
pub struct NextCmd {
    pub command: String,
    pub explain: String,
    /// Set when the command's selection needs the gate.
    pub plan_id: Option<PlanId>,
    /// The selection the plan-id belongs to (saved with it).
    pub selection: Selection,
    /// A `--apply` line with no plan-id that applies the safe set.
    pub safe_apply: bool,
}

/// The `plan-id NOT usable` block of `design/ux.md` 1.4.
#[derive(Debug, Clone)]
pub struct NotUsable {
    pub errno: i32,
    pub state_path: String,
    pub state_dir: String,
    pub safe_apply_printed: bool,
}

pub struct PlanView {
    pub header_id: PlanId,
    pub plan_saved: bool,
    /// Every row, in the default (or requested) order.
    pub rows: Vec<ItemRow>,
    pub selected: Group,
    pub add_on: Option<Group>,
    pub needs: Option<Group>,
    pub next: Vec<NextCmd>,
    pub not_usable: Option<NotUsable>,
}

fn status_of(item: &disk_cleanup_core::plan::PlanItem) -> (RowStatus, Option<SkipReason>) {
    match item.planned {
        Planned::Will => (RowStatus::Free, None),
        Planned::WillNot(SkipReason::KindNotSelected) => {
            (RowStatus::Risk, Some(SkipReason::KindNotSelected))
        }
        Planned::WillNot(r) => (RowStatus::Skip, Some(r)),
    }
}

/// The selectors whose RISK rows need more than a name in `--kinds`: an opt-in flag or Steam's
/// per-prefix conditions.
fn needs_naming_selector(s: SelectorId) -> bool {
    matches!(s, SelectorId::DockerVolumes | SelectorId::Steam)
}

/// Selectors that only run as root: their `Next:` line is a `sudo` line, never part of a subtotal.
fn root_selector(s: SelectorId) -> bool {
    matches!(
        s,
        SelectorId::Btrfs | SelectorId::BtrfsTrim | SelectorId::CacheApt
    )
}

fn is_add_on_row(r: &ItemRow) -> bool {
    r.status == RowStatus::Risk && !needs_naming_selector(r.selector) && !root_selector(r.selector)
}

fn is_needs_row(r: &ItemRow) -> bool {
    r.status == RowStatus::Risk && needs_naming_selector(r.selector)
}

fn text_ctx<'a>(input: &'a PlanInput, selector: SelectorId) -> TextCtx<'a> {
    TextCtx {
        selection: input.selection,
        selector,
        exe: &input.exe,
        ascii_only: !input.tty,
    }
}

fn build_rows(input: &PlanInput) -> Vec<ItemRow> {
    let none = ReasonFacts::default();
    input
        .plan
        .items
        .iter()
        .map(|item| {
            let c = &item.candidate;
            let (status, reason) = status_of(item);
            let facts = input.facts.get(&c.id()).unwrap_or(&none);
            let ctx = text_ctx(input, c.selector());
            let mut risk_notes = Vec::new();
            if let Some((files, bytes)) = facts.discards {
                risk_notes.push(format!(
                    "discards {files} ignored file{}, {}",
                    if files == 1 { "" } else { "s" },
                    crate::units::size(bytes)
                ));
            }
            if status == RowStatus::Risk {
                risk_notes.push("not selected".to_string());
                if c.kind() == Kind::DockerVolume && input.selection.docker_volumes.is_none() {
                    risk_notes.push("also needs --docker-volumes".to_string());
                }
                if let Some(n) = facts.duplicates {
                    risk_notes.push(format!("the newest of its {n} duplicates is kept"));
                }
            }
            ItemRow {
                id: c.id(),
                kind: c.kind(),
                selector: c.selector(),
                status,
                est_bytes: c.est_bytes().0,
                freed_bytes: None,
                measured_bytes: None,
                elapsed_ms: 0,
                age_secs: c.newest_mtime().map(|m| input.plan.now.saturating_sub(m)),
                path: path_of(c),
                label: label_for(c),
                reason,
                reason_text: reason.map(|r| reasontext::reason(r, facts, &ctx)),
                next_action: reason.map(|r| reasontext::next_action(r, facts, &ctx)),
                error_text: None,
                risk_notes,
            }
        })
        .collect()
}

/// Order rows for display: size descending by default.
pub fn sort_rows(rows: &mut [ItemRow], key: SortKey) {
    match key {
        SortKey::Size => rows.sort_by(|a, b| {
            b.est_bytes
                .cmp(&a.est_bytes)
                .then_with(|| a.label.cmp(&b.label))
        }),
        SortKey::Age => rows.sort_by(|a, b| {
            b.age_secs
                .cmp(&a.age_secs)
                .then_with(|| b.est_bytes.cmp(&a.est_bytes))
                .then_with(|| a.label.cmp(&b.label))
        }),
        SortKey::Kind => rows.sort_by(|a, b| {
            kind_order(a.kind)
                .cmp(&kind_order(b.kind))
                .then_with(|| b.est_bytes.cmp(&a.est_bytes))
                .then_with(|| a.label.cmp(&b.label))
        }),
    }
}

/// A selection over `ids` that keeps the flags of `base` which still apply to those kinds, so a pasted
/// line is not a usage error and reproduces the reviewed plan.
pub fn selection_over(base: &Selection, ids: &[SelectorId]) -> Selection {
    let has = |s: SelectorId| ids.contains(&s);
    let mut s = Selection::new(KindSelector::of(ids), KindSelector::default());
    s.roots.clone_from(&base.roots);
    s.min_age_secs = base.min_age_secs;
    s.min_size_bytes = base.min_size_bytes;
    s.only.clone_from(&base.only);
    if has(SelectorId::BuildDirs) {
        s.keep_newest = base.keep_newest;
    }
    if has(SelectorId::DockerVolumes) {
        s.docker_volumes = base.docker_volumes;
        s.volumes.clone_from(&base.volumes);
    }
    if has(SelectorId::Steam) {
        s.steam_saves = base.steam_saves;
    }
    if has(SelectorId::Worktrees) {
        s.worktree_ignored = base.worktree_ignored;
    }
    s
}

/// `--kinds ...` (omitted for the default set) followed by the selection flags.
pub fn args_of(sel: &Selection) -> String {
    let flags = sel.flags();
    let kinds = (sel.kinds_resolved() != KindSelector::default_apply().resolved())
        .then(|| format!("--kinds {}", sel.kinds_resolved()));
    [kinds.unwrap_or_default(), flags]
        .iter()
        .filter(|p| !p.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join(" ")
}

fn command(parts: &[&str]) -> String {
    parts
        .iter()
        .filter(|p| !p.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" ")
}

fn root_explain(s: SelectorId) -> &'static str {
    match s {
        SelectorId::Btrfs => "return freed chunks to unallocated, after an apply",
        SelectorId::BtrfsTrim => "trim free space on the filesystem (needs root)",
        SelectorId::CacheApt => "clear the apt cache (needs root)",
        SelectorId::DockerVolumes => {
            "dry run the volumes whose contents only root can read (its plan-id is root's, so apply as root too)"
        }
        _ => "needs root",
    }
}

fn group_count(g: &Group, noun: &str) -> String {
    units::count(g.items, noun, &format!("{noun}s"))
}

/// Every `Next:` command of the plan, before any NOT-saved filtering. The ids are the ones
/// `PlanId::for_selection` gives for exactly each command's selection.
pub fn commands(input: &PlanInput) -> Vec<NextCmd> {
    let rows = build_rows(input);
    let sel = input.selection;
    let plan = input.plan;
    let free: Vec<&ItemRow> = rows
        .iter()
        .filter(|r| r.status == RowStatus::Free)
        .collect();
    let mut out = Vec::new();
    let with_id = |s: &Selection| s.needs_plan_id().then(|| PlanId::for_selection(plan, s));
    let line = |s: &Selection, apply: bool, id: Option<PlanId>, root: bool| {
        let id_text = id.map(|i| format!("--plan-id {i}"));
        let exe = shell_quote(input.exe.as_os_str(), !input.tty);
        let head = if root {
            format!("sudo {exe}")
        } else {
            BIN.to_string()
        };
        command(&[
            &head,
            if apply { "--apply" } else { "" },
            &args_of(s),
            id_text.as_deref().unwrap_or(""),
        ])
    };

    if !free.is_empty() {
        let id = with_id(sel);
        out.push(NextCmd {
            command: line(sel, true, id, false),
            explain: format!("the {} FREE rows (selected now)", free.len()),
            plan_id: id,
            selection: sel.clone(),
            safe_apply: id.is_none(),
        });
    }

    // A default kind this build does not ship (`worktrees` before Gate C) must not be echoed: naming it exits 64.
    let selected_ids: Vec<SelectorId> = sel
        .kinds()
        .ids()
        .filter(|id| !input.not_built.iter().any(|n| n == id.id()))
        .collect();
    let add_rows: Vec<&ItemRow> = rows.iter().filter(|r| is_add_on_row(r)).collect();
    if !add_rows.is_empty() {
        let mut ids = selected_ids.clone();
        ids.extend(add_rows.iter().map(|r| r.selector));
        ids.sort();
        ids.dedup();
        let s = selection_over(sel, &ids);
        let id = with_id(&s);
        out.push(NextCmd {
            command: line(&s, true, id, false),
            explain: format!("adds the {} RISK rows", add_rows.len()),
            plan_id: id,
            selection: s,
            safe_apply: false,
        });
    }

    let named: BTreeSet<SelectorId> = rows
        .iter()
        .filter(|r| r.status == RowStatus::Risk)
        .map(|r| r.selector)
        .filter(|s| matches!(s, SelectorId::DockerVolumes | SelectorId::Steam))
        .collect();
    if !named.is_empty() {
        let ids: Vec<SelectorId> = named.into_iter().collect();
        let mut s = selection_over(sel, &ids);
        if ids.contains(&SelectorId::DockerVolumes) && s.docker_volumes.is_none() {
            s.docker_volumes = Some(DockerVolumes::Anonymous);
        }
        out.push(NextCmd {
            command: line(&s, false, None, false),
            explain: "a dry run of the kinds that need naming (prints its own plan-id)".into(),
            plan_id: None,
            selection: s,
            safe_apply: false,
        });
    }

    let mut root: BTreeMap<SelectorId, bool> = BTreeMap::new();
    for r in &rows {
        match r.reason {
            Some(SkipReason::NeedsRoot) => {
                root.insert(r.selector, r.selector.is_destructive());
            }
            Some(SkipReason::ContentsUnknown) => {
                root.insert(SelectorId::DockerVolumes, true);
            }
            _ if r.status == RowStatus::Risk && root_selector(r.selector) => {
                root.insert(r.selector, false);
            }
            _ => {}
        }
    }
    for (s, dry) in root {
        let sel_s = selection_over(sel, &[s]);
        out.push(NextCmd {
            command: line(&sel_s, !dry, None, true),
            explain: root_explain(s).to_string(),
            plan_id: None,
            selection: sel_s,
            safe_apply: false,
        });
    }
    out
}

/// The entries plan mode hands to `PlanStore::save_merged`: its own selection and every `Next:` line that
/// carries an id.
pub fn saved_entries(input: &PlanInput) -> Vec<SavedSelection> {
    let mut sels = vec![input.selection.clone()];
    sels.extend(
        commands(input)
            .into_iter()
            .filter(|c| c.plan_id.is_some())
            .map(|c| c.selection),
    );
    let mut seen = BTreeSet::new();
    sels.iter()
        .map(|s| input.plan.saved_selection(s, input.euid, input.plan.now))
        .filter(|e| seen.insert(e.plan_id.clone()))
        .collect()
}

impl PlanView {
    pub fn build(input: &PlanInput) -> PlanView {
        let mut rows = build_rows(input);
        sort_rows(&mut rows, input.opts.sort);
        let by = |pred: &dyn Fn(&ItemRow) -> bool| -> Vec<&ItemRow> {
            rows.iter().filter(|r| pred(r)).collect()
        };
        let selected = Group::of(&by(&|r| r.status == RowStatus::Free));
        let add = by(&is_add_on_row);
        let needs = by(&is_needs_row);
        let add_on = (!add.is_empty()).then(|| Group::of(&add));
        let needs_g = (!needs.is_empty()).then(|| Group::of(&needs));

        let plan_saved = !matches!(input.saved, SaveOutcome::NotSaved(_));
        let mut next = commands(input);
        let mut not_usable = None;
        if let SaveOutcome::NotSaved(errno) = input.saved {
            let before = next.len();
            next.retain(|c| c.plan_id.is_none());
            let dropped = before != next.len();
            let header_has_id = input.selection.needs_plan_id();
            if dropped || header_has_id {
                not_usable = Some(NotUsable {
                    errno,
                    state_path: display_path(
                        input.state_dir.join("last_plan").as_os_str(),
                        input.home.as_deref(),
                        input.tty,
                    ),
                    state_dir: display_path(
                        input.state_dir.as_os_str(),
                        input.home.as_deref(),
                        input.tty,
                    ),
                    safe_apply_printed: next.iter().any(|c| c.safe_apply),
                });
            }
        }
        PlanView {
            header_id: PlanId::for_selection(input.plan, input.selection),
            plan_saved,
            rows,
            selected,
            add_on,
            needs: needs_g,
            next,
            not_usable,
        }
    }

    /// Items counted by the quiet line and the totals.
    pub fn selected_text(&self, partial: bool) -> String {
        format!(
            "Selected now: {}, would free {}",
            group_count(&self.selected, "item"),
            units::size_lower_bound(self.selected.bytes, partial)
        )
    }
}
