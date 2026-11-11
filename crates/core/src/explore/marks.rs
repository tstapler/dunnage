//! The marked set (Task 6.2.4b; UX-T3): at most [`MARK_LIMIT`] paths in a fixed order, with the facts the quit
//! prompt and the plan need. Which rows may be marked at all is `mark_check` (Story 6.6.1); this file only
//! holds the set, its cap and its identity. Marks are paths, not arena ids, so a rescan cannot invalidate them.

use std::path::{Path, PathBuf};

use super::fmt::fmt_size_cell;
use super::msg::MarkRefusal;
use crate::escape::escape_os;

pub use super::refusal::MARK_LIMIT;

/// A path longer than this many bytes cannot ride on the printed `--only` line (UX-T3).
pub const PATH_MAX_BYTES: usize = 512;

/// One marked path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mark {
    pub path: PathBuf,
    /// The row's usage when it was marked: the quit prompt's total, not a promise of what a delete frees.
    pub est_bytes: u64,
    /// The hint called the row a cleaner item (`+`) when it was marked.
    pub cleaner_item: bool,
}

/// The identity of a marked set: the sorted paths. Sizes and flags do not change it, so a plan saved for one
/// set cannot be taken for another.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MarkSetId(Vec<PathBuf>);

impl MarkSetId {
    pub fn paths(&self) -> &[PathBuf] {
        &self.0
    }
}

/// The marked set, kept sorted by path bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Marks {
    items: Vec<Mark>,
}

impl Marks {
    pub fn new() -> Marks {
        Marks::default()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.position(path).is_ok()
    }

    /// The marks in path order.
    pub fn iter(&self) -> impl Iterator<Item = &Mark> {
        self.items.iter()
    }

    fn position(&self, path: &Path) -> Result<usize, usize> {
        self.items.binary_search_by(|m| {
            m.path
                .as_os_str()
                .as_encoded_bytes()
                .cmp(path.as_os_str().as_encoded_bytes())
        })
    }

    /// Adds a mark. An already marked path is left as it is. Refuses a path over [`PATH_MAX_BYTES`] and the
    /// 21st mark; nothing changes on a refusal.
    pub fn add(&mut self, mark: Mark) -> Result<(), MarkRefusal> {
        let Err(at) = self.position(&mark.path) else {
            return Ok(());
        };
        if mark.path.as_os_str().len() > PATH_MAX_BYTES {
            return Err(MarkRefusal::PathTooLong);
        }
        if self.items.len() >= MARK_LIMIT {
            return Err(MarkRefusal::LimitReached);
        }
        self.items.insert(at, mark);
        Ok(())
    }

    /// Returns whether the path was marked.
    pub fn remove(&mut self, path: &Path) -> bool {
        match self.position(path) {
            Ok(i) => {
                self.items.remove(i);
                true
            }
            Err(_) => false,
        }
    }

    /// Forgets every mark and returns how many there were.
    pub fn clear(&mut self) -> usize {
        let n = self.items.len();
        self.items.clear();
        n
    }

    pub fn total_bytes(&self) -> u64 {
        self.items
            .iter()
            .map(|m| m.est_bytes)
            .fold(0, u64::saturating_add)
    }

    /// Marks the cleaner does not know as items; the exit report names them as deleted from inside explore.
    pub fn not_cleaner_items(&self) -> Vec<&Path> {
        self.items
            .iter()
            .filter(|m| !m.cleaner_item)
            .map(|m| m.path.as_path())
            .collect()
    }

    pub fn identity(&self) -> MarkSetId {
        MarkSetId(self.items.iter().map(|m| m.path.clone()).collect())
    }

    /// Drops every mark whose path `exists` no longer finds and returns one footer line for each.
    pub fn prune(&mut self, exists: &dyn Fn(&Path) -> bool, tty: bool) -> Vec<String> {
        let mut notes = Vec::new();
        self.items.retain(|m| {
            let keep = exists(&m.path);
            if !keep {
                notes.push(format!(
                    "{} disappeared; unmarked",
                    escape_os(m.path.as_os_str(), tty)
                ));
            }
            keep
        });
        notes
    }

    /// The footer prompt of `q` with marks (E7), or `None` when nothing is marked (quit at once). Enter is
    /// not a deletion confirmation: it prints the plan and leaves.
    pub fn quit_prompt(&self) -> Option<String> {
        if self.items.is_empty() {
            return None;
        }
        let mut s = format!(
            "{} marked ({}).  Enter: show the plan and leave   x: forget marks and leave   Esc: stay",
            self.items.len(),
            fmt_size_cell(self.total_bytes())
        );
        let unknown = self.not_cleaner_items().len();
        match unknown {
            0 => {}
            1 => s.push_str("  1 is not a cleaner item: delete it here with d"),
            n => s = format!("{s}  {n} are not cleaner items: delete them here with d"),
        }
        Some(s)
    }

    /// The footer of the marks: `3 marked (40.1 GiB)`, empty when nothing is marked.
    pub fn summary(&self) -> Option<String> {
        (!self.items.is_empty()).then(|| {
            format!(
                "{} marked ({})",
                self.items.len(),
                fmt_size_cell(self.total_bytes())
            )
        })
    }
}
