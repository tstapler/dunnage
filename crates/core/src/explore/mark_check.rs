//! Mark rules M1 to M8 (Story 6.6.1; ADR-006 section 3): one pure check that decides whether a row may be
//! marked. It reads no file and holds no `Fs`; the facts a rule needs arrive through [`MarkFacts`], the mount
//! table and [`OwnState`]. The rules are the engine's own, not copies: M1 calls [`strictly_below`] (the
//! function [`RootSet::admits`] is built on), M2 calls [`floor_protected`] (the function behind
//! `DeletePolicy::validate`) and the scan-root and `.git` refusals are `guard::protected_position` (the one
//! function `decide_path_common` calls too), so the explorer cannot admit a path the engine's floor refuses.
//! The device is compared with the ROOT's, as `decide_path_common` does (`Facts.root_dev`). Marks are
//! directories only (review C3).
//!
//! The first rule that applies wins, in the order M1, M2, M3, M4, M5, M6, M7, M8.

use std::path::{Path, PathBuf};

use super::marks::{Marks, MARK_LIMIT, PATH_MAX_BYTES};
use super::msg::MarkRefusal;
use crate::dirs::Dirs;
use crate::escape::escape_os;
use crate::floor_rule::floor_protected;
use crate::guard::protected_position;
use crate::outcome::SkipReason;
use crate::roots::{lexically_clean, strictly_below};
use crate::selector::{KindSelector, Selection, ONLY_QUOTED_MAX_BYTES};

/// One row of the mount table: where it is mounted and what the owner calls the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkMount {
    pub point: PathBuf,
    /// The source as `mountinfo` names it (`/dev/sdb1`, `tmpfs`), for the note of M3.
    pub source: String,
}

/// The mount table (`/proc/self/mountinfo`) as the check needs it. Same-device or not is decided from
/// [`MarkFacts::dev_of`] (the engine's `st_dev` comparison), never from the table's own device field.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MountTable {
    mounts: Vec<MarkMount>,
}

impl MountTable {
    pub fn new(mounts: Vec<MarkMount>) -> MountTable {
        MountTable { mounts }
    }

    /// The entry mounted exactly at `path`.
    pub fn at(&self, path: &Path) -> Option<&MarkMount> {
        self.mounts.iter().find(|m| m.point == path)
    }

    /// Entries strictly below `path`, at any depth, in table order.
    pub fn below<'a>(&'a self, path: &'a Path) -> impl Iterator<Item = &'a MarkMount> + 'a {
        self.mounts
            .iter()
            .filter(move |m| strictly_below(path, &m.point))
    }
}

/// The tool's own state (rule M5): the state, cache and config directories and the running binary. A path is
/// refused when it is inside one of the directories, is one of them or is above one (their ancestors), or is
/// the binary or one of its ancestors (deleting the binary's `target/` directory also breaks the stamp check).
///
/// Marked paths are canonical (M4 refuses symlink components), so the comparison must be too: build it with
/// [`OwnState::canonical`] in production; [`OwnState::new`] compares the spellings it was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnState {
    dirs: Vec<PathBuf>,
    binaries: Vec<PathBuf>,
}

/// The name of the directory each XDG base gets.
const APP_DIR: &str = "disk-cleanup";

impl OwnState {
    /// `dirs` comes from the same resolution as the run (so `XDG_STATE_HOME` and `sudo` are honoured);
    /// `binary` is the running executable. Lexical: see [`OwnState::canonical`].
    pub fn new(dirs: &Dirs, binary: PathBuf) -> OwnState {
        OwnState {
            dirs: [&dirs.state, &dirs.cache, &dirs.config]
                .into_iter()
                .map(|d| d.join(APP_DIR))
                .collect(),
            binaries: vec![binary],
        }
    }

    /// As [`OwnState::new`], plus the spelling `canon` gives each directory and the binary (symlinked XDG
    /// directories, a binary reached through a link). `canon` must resolve links and tolerate a path that
    /// does not exist yet (it is given `<base>/disk-cleanup` before the state directory is created).
    #[must_use]
    pub fn canonical(dirs: &Dirs, binary: PathBuf, canon: &dyn Fn(&Path) -> PathBuf) -> OwnState {
        let mut own = OwnState::new(dirs, binary);
        for d in [&dirs.state, &dirs.cache, &dirs.config] {
            own.dirs.push(canon(d).join(APP_DIR));
        }
        let canon_binary = canon(&own.binaries[0]);
        own.binaries.push(canon_binary);
        own.dirs.sort();
        own.dirs.dedup();
        own.binaries.sort();
        own.binaries.dedup();
        own
    }

    pub fn covers(&self, path: &Path) -> bool {
        self.binaries.iter().any(|b| b.starts_with(path))
            || self
                .dirs
                .iter()
                .any(|d| path.starts_with(d) || d.starts_with(path))
    }
}

/// A symbolic link found on the way to a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymlinkFact {
    /// What the link resolves to, when that is known.
    pub real: Option<PathBuf>,
}

/// What a rule needs and a pure function cannot read. The explorer's adapter answers from `lstat`; a test
/// answers from a table. Any method may be called more than once.
pub trait MarkFacts {
    /// The first symlink among the components of `path`, the last one included.
    fn symlink_in(&self, path: &Path) -> Option<SymlinkFact>;
    /// `st_dev` from `lstat`, `None` when it could not be read.
    fn dev_of(&self, path: &Path) -> Option<u64>;
    /// `lstat` says directory (`Some(true)`), anything else (`Some(false)`), or could not be read (`None`).
    fn is_dir(&self, path: &Path) -> Option<bool>;
    /// The row is the boundary row of another filesystem (an `x` row).
    fn is_boundary(&self, path: &Path) -> bool;
    /// The path or something below it is still being scanned.
    fn scanning(&self, path: &Path) -> bool;
}

/// The facts that do not change while the marks do.
#[derive(Clone, Copy)]
pub struct MarkEnv<'a> {
    /// The cleaner's canonical scan roots (`RootSet::scan_roots`). The explore PATH is never one.
    pub roots: &'a [PathBuf],
    /// Canonical `$HOME`, or that of `SUDO_USER` under sudo (`Dirs.home`).
    pub home: &'a Path,
    pub mounts: &'a MountTable,
    pub own_state: &'a OwnState,
    pub facts: &'a dyn MarkFacts,
    pub tty: bool,
}

/// [`MarkEnv`] and the marks the new one is checked against.
#[derive(Clone, Copy)]
pub struct MarkCtx<'a> {
    pub env: &'a MarkEnv<'a>,
    pub marks: &'a Marks,
}

/// What a mark that passes carries to the footer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MarkOk {
    /// `contains another filesystem (<device>); it is not entered and the directory stays`.
    pub note: Option<String>,
}

/// Decides rules M1 to M8 for `path`. A path that is already marked passes (marking it again changes nothing).
pub fn mark_check(path: &Path, ctx: &MarkCtx<'_>) -> Result<MarkOk, MarkRefusal> {
    let env = ctx.env;
    // M1: strictly below one of the cleaner's roots.
    if !lexically_clean(path) || !env.roots.iter().any(|r| strictly_below(r, path)) {
        return Err(MarkRefusal::OutsideRoot);
    }
    // M2: rule F, and the `.git` refusal of `decide_path_common`.
    if floor_protected(path, env.home, env.roots) || protected_position(path, env.roots) {
        return Err(MarkRefusal::Engine(SkipReason::ProtectedPath));
    }
    // M3, M4, M5.
    let note = mount_rule(path, env)?;
    if env.facts.symlink_in(path).is_some() {
        return Err(MarkRefusal::SymlinkRow);
    }
    match env.facts.is_dir(path) {
        Some(true) => {}
        Some(false) => return Err(MarkRefusal::NotADirectory),
        None => return Err(MarkRefusal::Engine(SkipReason::Gone)),
    }
    if env.own_state.covers(path) {
        return Err(MarkRefusal::OwnState);
    }
    // M6 to M8.
    marks_rule(path, ctx.marks)?;
    if env.facts.scanning(path) {
        return Err(MarkRefusal::StillScanning);
    }
    limits_rule(path, ctx.marks)?;
    Ok(MarkOk { note })
}

/// M3. Returns the note for a directory that holds mounts of another device.
fn mount_rule(path: &Path, env: &MarkEnv<'_>) -> Result<Option<String>, MarkRefusal> {
    let facts = env.facts;
    let dev = facts.dev_of(path);
    // The device of the longest scan root that holds the path: what the engine compares with (`root_dev`).
    let root_dev = env
        .roots
        .iter()
        .filter(|r| strictly_below(r, path))
        .max_by_key(|r| r.components().count())
        .and_then(|r| facts.dev_of(r));
    if facts.is_boundary(path) || env.mounts.at(path).is_some() {
        return Err(MarkRefusal::MountPoint);
    }
    if matches!((dev, root_dev), (Some(a), Some(b)) if a != b) {
        return Err(MarkRefusal::Engine(SkipReason::CrossDevice));
    }
    let mut other_devices: Vec<&str> = Vec::new();
    for m in env.mounts.below(path) {
        // An unreadable device counts as the same one: refusing is the safe answer.
        let same = match (dev, facts.dev_of(&m.point)) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        };
        if same {
            return Err(MarkRefusal::BindMountBelow(m.point.clone()));
        }
        if !other_devices.contains(&m.source.as_str()) {
            other_devices.push(&m.source);
        }
    }
    Ok((!other_devices.is_empty()).then(|| {
        let shown: Vec<String> = other_devices
            .iter()
            .map(|d| escape_os(std::ffi::OsStr::new(d), env.tty))
            .collect();
        format!(
            "contains another filesystem ({}); it is not entered and the directory stays",
            shown.join(", ")
        )
    }))
}

/// M6.
fn marks_rule(path: &Path, marks: &Marks) -> Result<(), MarkRefusal> {
    if marks.contains(path) {
        return Ok(());
    }
    if let Some(outer) = marks.iter().find(|m| strictly_below(&m.path, path)) {
        return Err(MarkRefusal::InsideMarked(outer.path.clone()));
    }
    let inner = marks
        .iter()
        .filter(|m| strictly_below(path, &m.path))
        .count();
    if inner > 0 {
        return Err(MarkRefusal::ContainsMarked(
            u32::try_from(inner).unwrap_or(u32::MAX),
        ));
    }
    Ok(())
}

/// M8: the path length, the quoted `--only` text this mark would make, and the number of marks.
fn limits_rule(path: &Path, marks: &Marks) -> Result<(), MarkRefusal> {
    if marks.contains(path) {
        return Ok(());
    }
    if path.as_os_str().len() > PATH_MAX_BYTES {
        return Err(MarkRefusal::PathTooLong);
    }
    let mut sel = Selection::new(KindSelector::default(), KindSelector::default());
    sel.only = marks.iter().map(|m| m.path.clone()).collect();
    sel.only.push(path.to_path_buf());
    if sel.only_flags_len() > ONLY_QUOTED_MAX_BYTES {
        return Err(MarkRefusal::PathTooLong);
    }
    if marks.len() >= MARK_LIMIT {
        return Err(MarkRefusal::LimitReached);
    }
    Ok(())
}
