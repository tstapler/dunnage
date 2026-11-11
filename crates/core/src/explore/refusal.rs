//! The catalogue texts of [`MarkRefusal`] (Story 6.4.1; `tui/plan.md` Story 6.4.1, `design/ux.md` 3.5).
//! Each text says what happened, why and the next action, and escapes every path it prints (UX-57). The
//! variants Story 6.6.1 adds get their arms there.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::msg::MarkRefusal;
use super::tree::{ExploreTree, NodeId};
use crate::escape::escape_os;
use crate::shell_quote::shell_quote;

/// A directory shows `99+` candidate items rather than a count past this.
pub const CONTAINS_CAP: u32 = 99;

/// The most marks one plan carries (owner decision O-5).
pub const MARK_LIMIT: usize = 20;

/// Roots named in the `OutsideRoot` text before `+N more`.
const ROOTS_SHOWN: usize = 3;

/// The facts a text needs besides the variant.
#[derive(Debug, Clone, Copy)]
pub struct RefusalCtx<'a> {
    pub path: &'a Path,
    /// The cleaner's resolved scan roots.
    pub roots: &'a [PathBuf],
    /// Where the `roots` key lives, for the advice to add one.
    pub config_path: &'a Path,
    /// The canonical path a symlink row points at, when known.
    pub real_path: Option<&'a Path>,
    /// Non-ASCII characters print as themselves on a terminal.
    pub tty: bool,
}

fn esc(p: &OsStr, tty: bool) -> String {
    escape_os(p, tty)
}

/// `a, b, c, +2 more`.
fn roots_list(roots: &[PathBuf], tty: bool) -> String {
    let mut shown: Vec<String> = roots
        .iter()
        .take(ROOTS_SHOWN)
        .map(|r| esc(r.as_os_str(), tty))
        .collect();
    if roots.len() > ROOTS_SHOWN {
        shown.push(format!("+{} more", roots.len() - ROOTS_SHOWN));
    }
    shown.join(", ")
}

impl MarkRefusal {
    /// The line the footer or the plan shows for this refusal.
    pub fn text(&self, cx: &RefusalCtx<'_>) -> String {
        let path = esc(cx.path.as_os_str(), cx.tty);
        match self {
            MarkRefusal::NotAnItem => format!(
                "{path} is not an item of any cleanable kind, so the cleaner never touches it. \
                 Dry run without --only to list the items."
            ),
            MarkRefusal::OutsideRoot => format!(
                "{path} is outside the folders the cleaner may touch ({}). The explorer never widens \
                 them; to include it add its parent to 'roots' in {} and run explore again.",
                roots_list(cx.roots, cx.tty),
                esc(cx.config_path.as_os_str(), cx.tty)
            ),
            MarkRefusal::InsideBuildOrGit => format!(
                "{path} is inside a build directory or .git that the cleaner lists as one item. \
                 Mark the outer directory instead."
            ),
            MarkRefusal::SymlinkRow => match cx.real_path {
                Some(real) => format!(
                    "{path} is a symbolic link and the explorer never follows links. The cleaner's \
                     folders are named by their real paths ({}); open the real path: \
                     disk-cleanup explore {}",
                    roots_list(cx.roots, cx.tty),
                    shell_quote(real.as_os_str(), !cx.tty)
                ),
                None => format!(
                    "{path} is a symbolic link and the explorer never follows links. The cleaner's \
                     folders are named by their real paths ({}); open the real path with \
                     disk-cleanup explore.",
                    roots_list(cx.roots, cx.tty)
                ),
            },
            MarkRefusal::ContainsItems(n) => format!(
                "{path} contains {} candidate items (press f to show only candidates)",
                count_text(*n)
            ),
            MarkRefusal::LimitReached => format!(
                "{MARK_LIMIT} items are marked, the limit for one plan. Delete these first, then mark more."
            ),
            MarkRefusal::PathTooLong => format!(
                "{path} is too long to mark: the --only line cannot carry it. Mark a shorter path."
            ),
            MarkRefusal::StillScanning => {
                "1 marked item is still being scanned. Wait, or Esc to stop the scan.".to_string()
            }
            MarkRefusal::Engine(reason) => {
                format!("{path}: {}. {}", reason.reason_text(), reason.next_action())
            }
        }
    }
}

/// `3`, or `99+` once the count reached the cap.
pub fn count_text(n: u32) -> String {
    if n >= CONTAINS_CAP {
        format!("{CONTAINS_CAP}+")
    } else {
        n.to_string()
    }
}

/// How many descendants of `dir` (not `dir`) satisfy `is_item`, stopping at [`CONTAINS_CAP`]. The walk ends
/// when the cap is reached, so a directory holding many candidates costs a few of them, not the subtree.
pub fn count_items_below(tree: &ExploreTree, dir: NodeId, is_item: &dyn Fn(NodeId) -> bool) -> u32 {
    let mut found = 0;
    let mut stack: Vec<NodeId> = tree.children(dir).collect();
    while let Some(id) = stack.pop() {
        if is_item(id) {
            found += 1;
            if found >= CONTAINS_CAP {
                return CONTAINS_CAP;
            }
        }
        stack.extend(tree.children(id));
    }
    found
}
