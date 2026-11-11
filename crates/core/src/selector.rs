//! `KindSelector` and `Selection` (Story 1.1.4, Task 1.1.4c): what `--kinds`, `--except`, `--min-age`,
//! `--min-size` and the kind-specific flags select, and the canonical strings `Next:` lines and saved plans
//! echo (`Selection::kinds_resolved`, `Selection::flags`).

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::candidate::{Candidate, Locator};
use crate::kind::SelectorId;
use crate::outcome::SkipReason;
use crate::shell_quote::shell_quote;

/// A build directory is never removed below this age, whatever `--min-age` says (Story 3.2.1).
pub const BUILD_DIR_FLOOR_SECS: u64 = 60 * 60;
/// A worktree is never removed below this age (`WORKTREE_MIN_AGE`, Story 3.1.2).
pub const WORKTREE_FLOOR_SECS: u64 = 24 * 60 * 60;
const DAY: u64 = 86_400;
/// `--only` limits (Story 6.4.2): distinct values, bytes per value, and the quoted text of all of them.
pub const ONLY_MAX_VALUES: usize = 20;
pub const ONLY_MAX_BYTES: usize = 512;
pub const ONLY_QUOTED_MAX_BYTES: usize = 24 * 1024;

/// An `--only` value that is not an absolute canonical path within the limits (exit 64).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnlyValueError(pub String);

impl std::fmt::Display for OnlyValueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for OnlyValueError {}

/// Why one `--only` value is not in canonical lexical form, if it is not.
fn only_form_problem(p: &Path) -> Option<&'static str> {
    use std::os::unix::ffi::OsStrExt;
    let b = p.as_os_str().as_bytes();
    if b.is_empty() {
        return Some("it is empty");
    }
    if b.len() > ONLY_MAX_BYTES {
        return Some("it is longer than 512 bytes");
    }
    if b[0] != b'/' {
        return Some("it is not an absolute path");
    }
    if b.len() == 1 {
        return Some("it names the root directory");
    }
    if b[b.len() - 1] == b'/' {
        return Some("it ends with a slash");
    }
    for comp in b[1..].split(|c| *c == b'/') {
        match comp {
            [] => return Some("it has an empty component ('//')"),
            b"." => return Some("it has a '.' component"),
            b".." => return Some("it has a '..' component"),
            _ => {}
        }
    }
    None
}

/// Why `--kinds` (or `--except`) did not parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectorError {
    Unknown {
        id: String,
        suggestion: Option<&'static str>,
    },
    NotBuilt {
        id: &'static str,
    },
    Empty,
}

impl std::fmt::Display for SelectorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SelectorError::Unknown {
                id,
                suggestion: Some(s),
            } => write!(
                f,
                "unknown kind '{id}'. Did you mean '{s}'? See 'disk-cleanup kinds'."
            ),
            SelectorError::Unknown {
                id,
                suggestion: None,
            } => {
                write!(f, "unknown kind '{id}'. See 'disk-cleanup kinds'.")
            }
            SelectorError::NotBuilt { id } => write!(
                f,
                "error: '{id}' is not available in this build (see 'disk-cleanup kinds')"
            ),
            SelectorError::Empty => {
                write!(f, "a kind list is empty. See 'disk-cleanup kinds'.")
            }
        }
    }
}

impl std::error::Error for SelectorError {}

/// A parsed `--kinds` (or `--except`) list: a set of [`SelectorId`]s. `--kinds X` replaces the default set,
/// it never adds to it. A family id names exactly the selector of the same name (`docker` is the guarded
/// selector alone; `docker-images`, `docker-volumes` and `btrfs-trim` need their own id).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KindSelector {
    ids: BTreeSet<SelectorId>,
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != *cb);
            cur.push(sub.min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

impl KindSelector {
    /// The default `--apply` set: `caches,worktrees`.
    pub fn default_apply() -> KindSelector {
        KindSelector::of(&[SelectorId::Caches, SelectorId::Worktrees])
    }

    pub fn of(ids: &[SelectorId]) -> KindSelector {
        KindSelector {
            ids: ids.iter().copied().collect(),
        }
    }

    /// `registered` is the set of ids this build can run; naming another is `NotBuilt`.
    pub fn parse(input: &str, registered: &[SelectorId]) -> Result<KindSelector, SelectorError> {
        if input.is_empty() {
            return Err(SelectorError::Empty);
        }
        let mut ids = BTreeSet::new();
        for word in input.split(',') {
            let known = || SelectorId::ALL.iter().chain(SelectorId::TEST_ONLY.iter());
            let Some(id) = known().copied().find(|s| s.id() == word) else {
                let suggestion = known()
                    .map(|s| (edit_distance(word, s.id()), s.id()))
                    .filter(|(d, _)| *d <= 2)
                    .min_by_key(|(d, _)| *d)
                    .map(|(_, id)| id);
                return Err(SelectorError::Unknown {
                    id: word.to_string(),
                    suggestion,
                });
            };
            if !registered.contains(&id) {
                return Err(SelectorError::NotBuilt { id: id.id() });
            }
            ids.insert(id);
        }
        Ok(KindSelector { ids })
    }

    pub fn contains(&self, id: SelectorId) -> bool {
        self.ids.contains(&id)
    }

    pub fn ids(&self) -> impl Iterator<Item = SelectorId> + '_ {
        self.ids.iter().copied()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// `--except`: remove every id of `other`.
    #[must_use]
    pub fn without(mut self, other: &KindSelector) -> KindSelector {
        self.ids.retain(|i| !other.ids.contains(i));
        self
    }

    /// Sorted, comma-joined ids (the `--kinds` value `Next:` lines echo).
    pub fn resolved(&self) -> String {
        let mut names: Vec<&str> = self.ids.iter().map(|s| s.id()).collect();
        names.sort_unstable();
        names.join(",")
    }
}

/// The per-kind `--min-age` default of Task 1.1.4c.
pub fn default_min_age_secs(id: SelectorId) -> Option<u64> {
    match id {
        SelectorId::Worktrees | SelectorId::BuildDirs => Some(14 * DAY),
        SelectorId::DockerImages | SelectorId::DockerVolumes | SelectorId::Steam => Some(30 * DAY),
        SelectorId::Caches
        | SelectorId::CacheApt
        | SelectorId::Docker
        | SelectorId::Btrfs
        | SelectorId::BtrfsTrim => None,
        #[cfg(feature = "test-support")]
        SelectorId::FixtureDestructive => None,
    }
}

fn floor_secs(id: SelectorId) -> u64 {
    match id {
        SelectorId::BuildDirs => BUILD_DIR_FLOOR_SECS,
        SelectorId::Worktrees => WORKTREE_FLOOR_SECS,
        _ => 0,
    }
}

/// Kinds whose items `--root` narrows.
fn is_rooted(id: SelectorId) -> bool {
    match id {
        SelectorId::BuildDirs | SelectorId::Worktrees => true,
        #[cfg(feature = "test-support")]
        SelectorId::FixtureDestructive => true,
        _ => false,
    }
}

/// Whether `--keep-newest` has a kind to apply to: build dirs, or the test-only `fixture-destructive`.
fn selects_keep_newest_kind(kinds: &KindSelector) -> bool {
    kinds.contains(SelectorId::BuildDirs)
        || SelectorId::TEST_ONLY.iter().any(|s| kinds.contains(*s))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockerVolumes {
    Anonymous,
    Named,
}

/// Value of `--steam-saves` and `--worktree-ignored`: the one opt-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Discard {
    Discard,
}

/// A flag that belongs to one kind was given while that kind is not selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlagWithoutKind {
    pub flag: &'static str,
    pub needs: SelectorId,
    pub kinds: String,
}

impl std::fmt::Display for FlagWithoutKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "error: {} only applies to {}, which is not in --kinds ({}). Add {} to --kinds or drop the flag. Nothing was deleted.",
            self.flag,
            self.needs.id(),
            self.kinds,
            self.needs.id()
        )
    }
}

impl std::error::Error for FlagWithoutKind {}

/// Everything that decides which items a run selects. The fields that shape the item set are bound into the
/// plan-id and echoed by [`Selection::flags`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    kinds: KindSelector,
    excluded: KindSelector,
    pub roots: Vec<PathBuf>,
    /// `--min-age` (or the hidden `--stale-days`), in seconds.
    pub min_age_secs: Option<u64>,
    pub min_size_bytes: Option<u64>,
    pub keep_newest: Option<u32>,
    pub docker_volumes: Option<DockerVolumes>,
    pub volumes: Vec<String>,
    pub steam_saves: Option<Discard>,
    pub worktree_ignored: Option<Discard>,
    pub only: Vec<PathBuf>,
}

impl Selection {
    /// The `--only` values as `flags()` prints them: sorted, deduplicated.
    pub fn only_sorted(&self) -> Vec<&Path> {
        let mut only: Vec<&Path> = self.only.iter().map(PathBuf::as_path).collect();
        // `Path` equality and order are per component (`/a/b/` equals `/a/b`); the values are compared as bytes.
        only.sort_by(|a, b| a.as_os_str().cmp(b.as_os_str()));
        only.dedup_by(|a, b| a.as_os_str() == b.as_os_str());
        only
    }

    /// Value errors of `--only` (Story 6.4.2): absolute canonical form, at most 512 bytes each, at most 20
    /// distinct values, and at most 24 KiB of quoted flag text (what binds the plan-id and `last_plan` keeps).
    /// Item membership is checked after discovery, not here.
    pub fn validate_only(&self) -> Result<(), OnlyValueError> {
        let only = self.only_sorted();
        for p in &only {
            if let Some(why) = only_form_problem(p) {
                let shown = if p.as_os_str().len() > ONLY_MAX_BYTES {
                    format!("<{} bytes>", p.as_os_str().len())
                } else {
                    shell_quote(p.as_os_str(), false)
                };
                return Err(OnlyValueError(format!(
                    "error: invalid value '{shown}' for '--only <PATH>': {why}. Give the absolute path exactly as the cleaner prints it (no '.', '..', '//' or trailing slash). Nothing was deleted."
                )));
            }
        }
        if only.len() > ONLY_MAX_VALUES {
            return Err(OnlyValueError(format!(
                "error: too many values for '--only <PATH>': {} given, at most {ONLY_MAX_VALUES} are allowed. Run the cleaner in several steps. Nothing was deleted.",
                only.len()
            )));
        }
        let quoted = self.only_flags_len();
        if quoted > ONLY_QUOTED_MAX_BYTES {
            return Err(OnlyValueError(format!(
                "error: the '--only <PATH>' values need {quoted} bytes once quoted for the shell, over the {ONLY_QUOTED_MAX_BYTES} byte limit. Give fewer or shorter paths. Nothing was deleted."
            )));
        }
        Ok(())
    }

    /// Length of the quoted `--only ...` flag text, as `flags()` prints it.
    pub fn only_flags_len(&self) -> usize {
        self.only_sorted()
            .iter()
            .map(|p| "--only ".len() + shell_quote(p.as_os_str(), false).len() + 1)
            .sum::<usize>()
            .saturating_sub(1)
    }

    /// `kinds` minus `except`.
    pub fn new(kinds: KindSelector, except: KindSelector) -> Selection {
        Selection {
            kinds: kinds.without(&except),
            excluded: except,
            roots: Vec::new(),
            min_age_secs: None,
            min_size_bytes: None,
            keep_newest: None,
            docker_volumes: None,
            volumes: Vec::new(),
            steam_saves: None,
            worktree_ignored: None,
            only: Vec::new(),
        }
    }

    /// The hidden `--stale-days N`, equal to `--min-age Nd`.
    pub fn stale_days(&mut self, days: u64) {
        self.min_age_secs = Some(days * DAY);
    }

    pub fn kinds(&self) -> &KindSelector {
        &self.kinds
    }

    pub fn selects(&self, id: SelectorId) -> bool {
        self.kinds.contains(id)
    }

    /// Sorted comma list of the selectors, `--except` applied.
    pub fn kinds_resolved(&self) -> String {
        self.kinds.resolved()
    }

    /// A flag that belongs to one kind is a usage error when that kind is not selected: it is bound into
    /// the plan-id and must never be silently ignored.
    pub fn validate(&self) -> Result<(), FlagWithoutKind> {
        let checks = [
            (
                self.docker_volumes.is_some(),
                "--docker-volumes",
                SelectorId::DockerVolumes,
            ),
            (
                !self.volumes.is_empty(),
                "--volume",
                SelectorId::DockerVolumes,
            ),
            (
                self.steam_saves.is_some(),
                "--steam-saves",
                SelectorId::Steam,
            ),
            (
                self.worktree_ignored.is_some(),
                "--worktree-ignored",
                SelectorId::Worktrees,
            ),
            (
                self.keep_newest.is_some(),
                "--keep-newest",
                SelectorId::BuildDirs,
            ),
        ];
        for (given, flag, needs) in checks {
            let kind_present = if flag == "--keep-newest" {
                selects_keep_newest_kind(&self.kinds)
            } else {
                self.selects(needs)
            };
            if given && !kind_present {
                return Err(FlagWithoutKind {
                    flag,
                    needs,
                    kinds: self.kinds_resolved(),
                });
            }
        }
        Ok(())
    }

    /// `--worktree-ignored discard`: the flag, not the `worktrees` kind, can delete `.env` and SQLite files.
    pub fn worktree_ignored_discard(&self) -> bool {
        self.worktree_ignored.is_some()
    }

    /// Whether the plan-id gate applies: a destructive selector is selected or the discard flag is given.
    pub fn needs_plan_id(&self) -> bool {
        self.kinds.ids().any(SelectorId::is_destructive) || self.worktree_ignored_discard()
    }

    /// The destructive selectors in the selection, in id order.
    pub fn destructive_selectors(&self) -> Vec<SelectorId> {
        let mut v: Vec<SelectorId> = self.kinds.ids().filter(|s| s.is_destructive()).collect();
        v.sort_by_key(|s| s.id());
        v
    }

    /// The effective `--min-age` for a selector: the override or the default, never below the floor
    /// (60 minutes for build dirs, 24 hours for worktrees).
    pub fn effective_min_age(&self, id: SelectorId) -> Option<u64> {
        let base = self.min_age_secs.or_else(|| default_min_age_secs(id));
        let floor = floor_secs(id);
        match base {
            None if floor == 0 => None,
            b => Some(b.unwrap_or(0).max(floor)),
        }
    }

    /// Items this selection drops outright (shown nowhere): an excluded kind, a size under `--min-size`,
    /// a build dir or worktree outside every `--root`, a path that is not one of the `--only` values.
    pub fn keeps(&self, c: &Candidate) -> bool {
        if self.excluded.contains(c.selector()) {
            return false;
        }
        if self.min_size_bytes.is_some_and(|m| c.est_bytes().0 < m) {
            return false;
        }
        let path = match c.locator() {
            Locator::Path(p) | Locator::Steam(_, p) => Some(p.as_path()),
            Locator::Docker(_) => None,
        };
        if let Some(p) = path {
            let rooted = is_rooted(c.selector());
            if rooted && !self.roots.is_empty() && !self.roots.iter().any(|r| p.starts_with(r)) {
                return false;
            }
        }
        crate::plan::only_keeps(&self.only, c)
    }

    /// Whether a kept item is selected (`KindNotSelected` otherwise) and old enough (`TooRecent`). An item
    /// with no known mtime under an age rule fails closed as `TooRecent`.
    pub fn admits(&self, c: &Candidate, now: i64) -> Result<(), SkipReason> {
        if !self.selects(c.selector()) {
            return Err(SkipReason::KindNotSelected);
        }
        if let Some(min) = self.effective_min_age(c.selector()) {
            let old_enough = c
                .newest_mtime()
                .is_some_and(|m| now.saturating_sub(m) >= i64::try_from(min).unwrap_or(i64::MAX));
            if !old_enough {
                return Err(SkipReason::TooRecent);
            }
        }
        Ok(())
    }

    /// The canonical selection-flag string, fixed order, sorted and deduplicated, values shell-quoted.
    /// It is what `Next:` lines echo and `SavedSelection` stores.
    pub fn flags(&self) -> String {
        let mut out: Vec<String> = Vec::new();
        let mut paths = |flag: &str, v: &[PathBuf]| {
            let mut v: Vec<&Path> = v.iter().map(PathBuf::as_path).collect();
            v.sort();
            v.dedup();
            for p in v {
                out.push(format!("{flag} {}", shell_quote(p.as_os_str(), false)));
            }
        };
        paths("--root", &self.roots);
        if let Some(s) = self.min_age_secs {
            out.push(format!("--min-age {}", format_age(s)));
        }
        if let Some(b) = self.min_size_bytes {
            out.push(format!("--min-size {}", format_size(b)));
        }
        if let Some(n) = self.keep_newest {
            out.push(format!("--keep-newest {n}"));
        }
        match self.docker_volumes {
            Some(DockerVolumes::Anonymous) => out.push("--docker-volumes anonymous".into()),
            Some(DockerVolumes::Named) => out.push("--docker-volumes named".into()),
            None => {}
        }
        let mut vols: Vec<&String> = self.volumes.iter().collect();
        vols.sort();
        vols.dedup();
        for v in vols {
            out.push(format!("--volume {}", shell_quote(OsStr::new(v), false)));
        }
        if self.steam_saves.is_some() {
            out.push("--steam-saves discard".into());
        }
        if self.worktree_ignored.is_some() {
            out.push("--worktree-ignored discard".into());
        }
        for p in self.only_sorted() {
            out.push(format!("--only {}", shell_quote(p.as_os_str(), false)));
        }
        out.join(" ")
    }

    /// `--kinds <resolved>` followed by the flags: the dry-run command (minus the binary name) of this
    /// selection, as printed in `Next:` lines and refusals.
    pub fn dry_run_args(&self) -> String {
        let flags = self.flags();
        if flags.is_empty() {
            format!("--kinds {}", self.kinds_resolved())
        } else {
            format!("--kinds {} {flags}", self.kinds_resolved())
        }
    }
}

/// `1h`, `7d`, `60m`: the largest unit that divides the value exactly.
pub fn format_age(secs: u64) -> String {
    if secs != 0 && secs.is_multiple_of(DAY) {
        format!("{}d", secs / DAY)
    } else if secs != 0 && secs.is_multiple_of(3600) {
        format!("{}h", secs / 3600)
    } else if secs != 0 && secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// `1GiB`, `512MiB`: the largest binary unit that divides the value exactly.
pub fn format_size(bytes: u64) -> String {
    for (unit, name) in [
        (1u64 << 40, "TiB"),
        (1 << 30, "GiB"),
        (1 << 20, "MiB"),
        (1 << 10, "KiB"),
    ] {
        if bytes != 0 && bytes.is_multiple_of(unit) {
            return format!("{}{name}", bytes / unit);
        }
    }
    format!("{bytes}B")
}
