//! The rows of one directory (Task 6.2.2b): subdirectories from the arena, one synthetic row for the files
//! the arena only counts, the lister's file rows once they arrive, the 5,000-entry cap with its `(+N
//! smaller)` row, the empty directory and the filters. Pure: a function of the tree, a sort, a filter and
//! the listed files, so the TUI and the tests draw the same rows.
//!
//! The columns add up (UX-T18). A directory row counts itself and everything below (`inodes + 1`), the
//! synthetic files row is what is left of the directory total after its children (`total - sum of
//! children`), and the cap row is the exact sum of the entries it replaces, so with no hard-linked file the
//! usage, apparent and item columns of a complete directory's rows equal the directory's own total.

use std::cmp::Ordering;
use std::ffi::{OsStr, OsString};

use disk_cleanup_scan::estimate::Stats;

use super::msg::{FileRow, FileRows};
use super::tree::{stats_add, stats_sub, ExploreTree, NodeId};
use crate::escape::escape_os;

/// Entries kept in one directory's list; the rest fold into one `(+N smaller)` row.
pub const ROW_CAP: usize = 5_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Usage,
    Apparent,
    Meta,
    Items,
    Name,
}

/// A sort key and its direction. Size keys start descending, `Name` ascending; the same key again reverses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sort {
    pub key: SortKey,
    pub descending: bool,
}

impl Default for Sort {
    fn default() -> Sort {
        Sort::new(SortKey::Usage)
    }
}

impl Sort {
    pub fn new(key: SortKey) -> Sort {
        Sort {
            key,
            descending: key != SortKey::Name,
        }
    }

    /// What pressing `key` does: a new key takes its default direction, the active one reverses.
    #[must_use]
    pub fn pressed(self, key: SortKey) -> Sort {
        if self.key == key {
            Sort {
                key,
                descending: !self.descending,
            }
        } else {
            Sort::new(key)
        }
    }

    /// `usage, largest first` (the path line adds `sort: `).
    pub fn words(self) -> String {
        match (self.key, self.descending) {
            (SortKey::Usage, true) => "usage, largest first".into(),
            (SortKey::Usage, false) => "usage, smallest first".into(),
            (SortKey::Apparent, true) => "apparent size, largest first".into(),
            (SortKey::Apparent, false) => "apparent size, smallest first".into(),
            (SortKey::Meta, true) => "metadata, largest first".into(),
            (SortKey::Meta, false) => "metadata, smallest first".into(),
            (SortKey::Items, true) => "items, most first".into(),
            (SortKey::Items, false) => "items, fewest first".into(),
            (SortKey::Name, true) => "name, Z to A".into(),
            (SortKey::Name, false) => "name, A to Z".into(),
        }
    }
}

/// `f` cycles these; marks are not affected by any of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Filter {
    #[default]
    All,
    /// Rows the hint badges `+` or `?`.
    Candidates,
    /// Unreadable directories.
    Errors,
}

impl Filter {
    #[must_use]
    pub fn next(self) -> Filter {
        match self {
            Filter::All => Filter::Candidates,
            Filter::Candidates => Filter::Errors,
            Filter::Errors => Filter::All,
        }
    }

    pub fn words(self) -> &'static str {
        match self {
            Filter::All => "none",
            Filter::Candidates => "candidates",
            Filter::Errors => "errors",
        }
    }
}

/// What the cleaner knows about a row (filled from the hint, Story 6.4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Badge {
    #[default]
    None,
    /// `+`
    Candidate,
    /// `?`
    Possible,
}

/// The identity a cursor follows: it survives re-sorts and size changes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RowId {
    Dir(NodeId),
    /// A file of the directory, by name (files are not arena nodes).
    File(NodeId, OsString),
    /// The `(N files)` row standing for the files the arena only counts.
    Files(NodeId),
    /// The `(+N smaller)` row.
    Smaller(NodeId),
    Empty(NodeId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Dir,
    File,
    Files,
    Smaller,
    Empty,
}

/// The state flags are independent facts a drawn row reads together.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: RowId,
    pub kind: RowKind,
    /// The raw name; the drawn text is [`Row::display_name`].
    pub name: OsString,
    pub usage: u64,
    pub apparent: u64,
    pub meta: Option<u64>,
    /// `None` reads `n/a` (a file).
    pub items: Option<u64>,
    /// Entries a synthetic row stands for.
    pub count: u64,
    pub complete: bool,
    pub unreadable: bool,
    pub boundary: bool,
    pub at_least: bool,
    pub second_link: bool,
    pub badge: Badge,
}

impl Row {
    /// Escaped, with the marks of the kind: `name/`, `(412390 files)`, `(+12 smaller)`, `(empty directory)`.
    pub fn display_name(&self) -> String {
        match self.kind {
            RowKind::Dir => format!("{}/", escape_os(&self.name, true)),
            RowKind::File => escape_os(&self.name, true),
            RowKind::Files => format!("({} files)", self.count),
            RowKind::Smaller => format!("(+{} smaller)", self.count),
            RowKind::Empty => "(empty directory)".into(),
        }
    }

    /// Items for sums and the items sort: a file is one.
    pub fn item_count(&self) -> u64 {
        self.items.unwrap_or(u64::from(self.kind == RowKind::File))
    }

    /// Whether Enter opens it.
    pub fn is_directory(&self) -> bool {
        self.kind == RowKind::Dir
    }

    pub(super) fn pinned_last(&self) -> bool {
        matches!(self.kind, RowKind::Smaller | RowKind::Empty)
    }
}

/// The files of the viewed directory as the lister reported them and the view accepted them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListedFiles {
    pub rows: Vec<FileRow>,
    pub smaller_count: u64,
    pub partial: bool,
}

impl ListedFiles {
    /// Keeps the rows `exists` still confirms: a file that vanished between the listing and the display is
    /// dropped.
    pub fn from_listing(rows: FileRows, exists: &dyn Fn(&OsStr) -> bool) -> ListedFiles {
        ListedFiles {
            rows: rows.rows.into_iter().filter(|r| exists(&r.name)).collect(),
            smaller_count: rows.smaller_count,
            partial: rows.partial,
        }
    }
}

/// Everything a build reads.
pub struct RowSource<'a> {
    pub tree: &'a ExploreTree,
    pub dir: NodeId,
    pub files: Option<&'a ListedFiles>,
    pub badge: &'a dyn Fn(NodeId) -> Badge,
}

/// The rows of one directory in display order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RowList {
    pub rows: Vec<Row>,
    /// Entries folded into the `(+N smaller)` row by the cap.
    pub folded: u64,
    /// A listed file is a second link of an inode seen earlier in its listing.
    pub hard_links: bool,
    /// The files have not been listed yet and the directory holds some (`listing files...`).
    pub files_unlisted: bool,
}

impl RowList {
    /// The footer sentence that qualifies the rows, if one applies.
    pub fn footer_note(&self, sort: Sort) -> Option<String> {
        if self.folded > 0 && sort.key == SortKey::Name {
            return Some(format!("name sort covers the {ROW_CAP} largest entries"));
        }
        self.hard_links
            .then(|| "hard links are counted once in totals".to_string())
    }
}

/// One entry before the cap: a directory by its totals, or a ready row.
struct Cand {
    usage: u64,
    apparent: u64,
    items: u64,
    what: What,
}

enum What {
    Dir(NodeId),
    Row(Box<Row>),
}

fn dir_row(src: &RowSource<'_>, id: NodeId) -> Row {
    let t = src.tree;
    let total = t.total(id);
    Row {
        id: RowId::Dir(id),
        kind: RowKind::Dir,
        name: t.name(id).to_os_string(),
        usage: total.allocated_bytes,
        apparent: total.size_bytes,
        meta: Some(total.metadata_cost().total()),
        items: Some(total.inodes + 1),
        count: 0,
        complete: t.is_complete(id),
        unreadable: t.is_unreadable(id),
        boundary: t.is_boundary(id),
        at_least: t.is_at_least(id),
        second_link: false,
        badge: (src.badge)(id),
    }
}

fn synthetic(id: RowId, kind: RowKind, sums: [u64; 3], count: u64) -> Row {
    Row {
        id,
        kind,
        name: OsString::new(),
        usage: sums[0],
        apparent: sums[1],
        meta: None,
        items: Some(sums[2]),
        count,
        complete: true,
        unreadable: false,
        boundary: false,
        at_least: false,
        second_link: false,
        badge: Badge::None,
    }
}

fn file_row(dir: NodeId, f: &FileRow) -> Row {
    Row {
        id: RowId::File(dir, f.name.clone()),
        kind: RowKind::File,
        name: f.name.clone(),
        usage: f.allocated_bytes,
        apparent: f.size_bytes,
        meta: None,
        items: None,
        count: 0,
        complete: true,
        unreadable: false,
        boundary: false,
        at_least: false,
        second_link: f.second_link,
        badge: Badge::None,
    }
}

fn name_bytes<'a>(src: &'a RowSource<'_>, c: &'a Cand) -> &'a [u8] {
    match &c.what {
        What::Dir(id) => src.tree.name(*id).as_encoded_bytes(),
        What::Row(r) => r.name.as_encoded_bytes(),
    }
}

pub(super) fn order(a: &Row, b: &Row, sort: Sort) -> Ordering {
    let by_key = match sort.key {
        SortKey::Usage => a.usage.cmp(&b.usage),
        SortKey::Apparent => a.apparent.cmp(&b.apparent),
        SortKey::Meta => a.meta.unwrap_or(0).cmp(&b.meta.unwrap_or(0)),
        SortKey::Items => a.item_count().cmp(&b.item_count()),
        SortKey::Name => a.name.as_encoded_bytes().cmp(b.name.as_encoded_bytes()),
    };
    let by_key = if sort.descending {
        by_key.reverse()
    } else {
        by_key
    };
    // Ties break by name ascending whatever the direction; the synthetic files row has an empty name and
    // two rows of one directory never share both name and kind, so the order is total.
    by_key
        .then_with(|| a.name.as_encoded_bytes().cmp(b.name.as_encoded_bytes()))
        .then_with(|| (a.kind as u8).cmp(&(b.kind as u8)))
}

fn keeps(row: &Row, filter: Filter, text: &str) -> bool {
    let by_kind = match filter {
        Filter::All => true,
        Filter::Candidates => row.badge != Badge::None,
        Filter::Errors => row.unreadable,
    };
    by_kind
        && (text.is_empty()
            || row
                .display_name()
                .to_lowercase()
                .contains(&text.to_lowercase()))
}

/// Builds the rows of `src.dir`. `text` is the `/` filter, matched case-insensitively against the escaped
/// name; any active filter hides the synthetic rows (they stand for entries the filter cannot judge).
pub fn build_rows(src: &RowSource<'_>, sort: Sort, filter: Filter, text: &str) -> RowList {
    let tree = src.tree;
    let dir = src.dir;
    let filtering = filter != Filter::All || !text.is_empty();
    let mut cands: Vec<Cand> = Vec::new();
    let mut below = Stats::default();
    let mut children = 0u64;
    for c in tree.children(dir) {
        let t = tree.total(c);
        stats_add(&mut below, t);
        children += 1;
        cands.push(Cand {
            usage: t.allocated_bytes,
            apparent: t.size_bytes,
            items: t.inodes + 1,
            what: What::Dir(c),
        });
    }

    // What the directory holds beyond its subdirectories: its own entries (files, symlinks) and the
    // directory entries of the subdirectories, which the rows above count as their `+ 1`.
    let mut own = *tree.total(dir);
    stats_sub(&mut own, &below);
    let own_items = own.inodes.saturating_sub(children);
    let own_files = own.files + own.symlinks;

    let mut hard_links = false;
    let mut smaller_files: Option<([u64; 3], u64)> = None;
    let mut files_unlisted = false;
    match src.files {
        Some(listed) => {
            let (mut u, mut a) = (0u64, 0u64);
            for f in &listed.rows {
                u += f.allocated_bytes;
                a += f.size_bytes;
                hard_links |= f.second_link;
                cands.push(Cand {
                    usage: f.allocated_bytes,
                    apparent: f.size_bytes,
                    items: 1,
                    what: What::Row(Box::new(file_row(dir, f))),
                });
            }
            let rest = [
                own.allocated_bytes.saturating_sub(u),
                own.size_bytes.saturating_sub(a),
                own_items.saturating_sub(listed.rows.len() as u64),
            ];
            if rest.iter().any(|&v| v > 0) || listed.smaller_count > 0 {
                smaller_files = Some((rest, listed.smaller_count));
            }
        }
        None => {
            files_unlisted = own_files > 0;
            if own_files > 0 || own.allocated_bytes > 0 {
                let sums = [own.allocated_bytes, own.size_bytes, own_items];
                cands.push(Cand {
                    usage: sums[0],
                    apparent: sums[1],
                    items: sums[2],
                    what: What::Row(Box::new(synthetic(
                        RowId::Files(dir),
                        RowKind::Files,
                        sums,
                        own_files,
                    ))),
                });
            }
        }
    }

    // The cap keeps the largest by usage whatever the sort, so a name sort covers the kept entries.
    let mut folded = [0u64; 3];
    let mut folded_entries = 0u64;
    if cands.len() > ROW_CAP {
        cands.select_nth_unstable_by(ROW_CAP - 1, |a, b| {
            b.usage
                .cmp(&a.usage)
                .then_with(|| name_bytes(src, a).cmp(name_bytes(src, b)))
        });
        for c in &cands[ROW_CAP..] {
            folded[0] += c.usage;
            folded[1] += c.apparent;
            folded[2] += c.items;
            folded_entries += 1;
        }
        cands.truncate(ROW_CAP);
    }

    let mut rows: Vec<Row> = cands
        .into_iter()
        .map(|c| match c.what {
            What::Dir(id) => dir_row(src, id),
            What::Row(r) => *r,
        })
        .filter(|r| keeps(r, filter, text))
        .collect();
    rows.sort_by(|a, b| order(a, b, sort));

    if !filtering {
        let mut tail = folded;
        let mut count = folded[2];
        if let Some((sums, n)) = smaller_files {
            for (t, s) in tail.iter_mut().zip(sums) {
                *t += s;
            }
            count += n;
        }
        if folded_entries > 0 || smaller_files.is_some() {
            rows.push(synthetic(
                RowId::Smaller(dir),
                RowKind::Smaller,
                tail,
                count,
            ));
        } else if rows.is_empty() && children == 0 && own.inodes == 0 && tree.is_complete(dir) {
            rows.push(synthetic(RowId::Empty(dir), RowKind::Empty, [0; 3], 0));
        }
    }
    RowList {
        rows,
        folded: folded_entries,
        hard_links,
        files_unlisted,
    }
}
