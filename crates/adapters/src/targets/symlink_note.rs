//! The `symlinked roots:` note (Story 1.3.3): when a plan row is `Skip(Symlink)` because a parent directory
//! below `$HOME` is a symlink, say which one and where it points, so the skip is explained and not silent.
//! Read-only: one `lstat` per path component and one `readlink` per symlinked root found.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use disk_cleanup_core::candidate::Locator;
use disk_cleanup_core::delete_types::FileKind;
use disk_cleanup_core::kind::Kind;
use disk_cleanup_core::plan::{PlanItem, Planned};
use disk_cleanup_core::ports::Fs;
use disk_cleanup_core::SkipReason;

/// The first symlinked directory strictly between `home` and `path`, walking down from `home`.
fn symlinked_ancestor(fs: &dyn Fs, home: &Path, path: &Path) -> Option<PathBuf> {
    let rel = path.strip_prefix(home).ok()?;
    let mut cur = home.to_path_buf();
    let mut parts = rel.components().peekable();
    while let Some(c) = parts.next() {
        parts.peek()?;
        cur.push(c);
        if fs.lstat(&cur).is_ok_and(|st| st.kind == FileKind::Symlink) {
            return Some(cur);
        }
    }
    None
}

/// One note per symlinked root that made at least one row `Skip(Symlink)`. `read_link` is a parameter so a
/// test can stand in for the filesystem.
#[must_use]
pub fn symlinked_root_notes(
    fs: &dyn Fs,
    home: &Path,
    items: &[PlanItem],
    read_link: &dyn Fn(&Path) -> Option<PathBuf>,
) -> Vec<String> {
    let mut roots: BTreeMap<PathBuf, (usize, bool)> = BTreeMap::new();
    for item in items {
        if item.planned != Planned::WillNot(SkipReason::Symlink) {
            continue;
        }
        let Locator::Path(p) = item.candidate.locator() else {
            continue;
        };
        let Some(root) = symlinked_ancestor(fs, home, p) else {
            continue;
        };
        let e = roots.entry(root).or_insert((0, true));
        e.0 += 1;
        e.1 &= item.candidate.kind() == Kind::Cache;
    }
    roots
        .into_iter()
        .map(|(root, (n, all_caches))| {
            let shown = root.strip_prefix(home).map_or_else(
                |_| root.display().to_string(),
                |r| format!("~/{}", r.display()),
            );
            let target = read_link(&root).map_or_else(
                || "(unreadable)".to_string(),
                |t| {
                    let t = if t.is_relative() {
                        root.parent().unwrap_or(home).join(t)
                    } else {
                        t
                    };
                    t.display().to_string()
                },
            );
            let what = match (all_caches, n) {
                (true, 1) => "1 cache kind is".to_string(),
                (true, n) => format!("{n} cache kinds are"),
                (false, 1) => "1 item is".to_string(),
                (false, n) => format!("{n} items are"),
            };
            format!(
                "symlinked roots: {shown} -> {target} ({what} skipped: the tool never follows a symlinked parent; point --root at the real directory or move the data)"
            )
        })
        .collect()
}

/// [`symlinked_root_notes`] over the real `readlink`.
#[must_use]
pub fn symlinked_root_notes_real(fs: &dyn Fs, home: &Path, items: &[PlanItem]) -> Vec<String> {
    symlinked_root_notes(fs, home, items, &|p| std::fs::read_link(p).ok())
}
