//! Plain data behind the `PlanStore` port and the audit `run_start`: `SavedPlan`, `BuildInfo`.
//! The merge rule (`merge_saved`) is Task 1.1.2a, the real store Story 2.2.1.

use serde::{Deserialize, Serialize};

/// Longest item key kept in a saved plan.
pub const ITEM_KEY_MAX: usize = 200;

/// `<kind>:<path or id>` of one planned item, cut on a character boundary at [`ITEM_KEY_MAX`] bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ItemKey(String);

impl ItemKey {
    pub fn new(s: &str) -> ItemKey {
        let mut end = s.len().min(ITEM_KEY_MAX);
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        ItemKey(s[..end].to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One plan-id a plan-mode run printed, with what it was made from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedSelection {
    pub plan_id: String,
    pub saved_at: i64,
    pub euid: u32,
    /// The `--kinds` string.
    pub kinds: String,
    /// Canonical string of the selection-shaping flags.
    pub flags: String,
    pub items: Vec<ItemKey>,
    pub items_truncated: bool,
}

/// `last_plan`: a merged set of entries, one per plan-id.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedPlan {
    pub entries: Vec<SavedSelection>,
}

/// What `PlanStore::save_merged` did. `NotSaved` is a warning, never a failure of the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveOutcome {
    Saved,
    SavedAfterReserve,
    NotSaved(i32),
}

/// The binary that wrote an audit line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildInfo {
    pub bin_version: String,
    pub git_commit: String,
    pub dirty: bool,
    pub metascope_rev: String,
    /// A local `[patch]` was in force when it was built.
    pub patched: bool,
    /// `crates/scan` had uncommitted changes when it was built.
    #[serde(default)]
    pub metascope_dirty: bool,
}
