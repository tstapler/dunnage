//! `ViewState` (Task 6.2.2a): the viewed directory, the sort, the cursor and the throttled snapshot of the
//! rows. The cursor follows a row identity, not an index (UX-T11), and the order of the rows changes only
//! when [`ViewState::order_snapshot`] says so: at most once a second and not within two seconds of a key.
//! Between re-sorts sizes change in place and new rows go to the end, so a key pressed after a pause acts on
//! what the owner was looking at. Time arrives as milliseconds on a monotonic clock the loop owns, so a test
//! needs no sleep.

use std::ffi::OsStr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::layout::{ColumnKind, SortMark, TierLayout};
use super::msg::FileRows;
pub use super::rows::{
    build_rows, Badge, Filter, ListedFiles, Row, RowId, RowKind, RowList, RowSource, Sort, SortKey,
    ROW_CAP,
};
use super::tree::{ExploreTree, NodeId};

/// Rows are re-sorted at most this often (UX-T11).
pub const RESORT_EVERY_MS: u64 = 1_000;
/// ...and never this soon after a key press.
pub const QUIET_AFTER_KEY_MS: u64 = 2_000;

/// A badge function for callers with no hint to ask yet.
pub fn no_badge(_: NodeId) -> Badge {
    Badge::None
}

/// What the loop needs to list a directory's files: the generation to echo, and a switch that stops the
/// listing when the owner moves on.
#[derive(Debug, Clone)]
pub struct ListingRequest {
    pub dir: NodeId,
    pub generation: u32,
    pub cancel: Arc<AtomicBool>,
}

#[derive(Debug)]
struct Listing {
    generation: u32,
    dir: NodeId,
    cancel: Arc<AtomicBool>,
}

#[derive(Debug)]
pub struct ViewState {
    sort: Sort,
    filter: Filter,
    text: String,
    dir: Option<NodeId>,
    list: RowList,
    cursor: Option<RowId>,
    cursor_index: usize,
    last_key_ms: Option<u64>,
    last_sort_ms: Option<u64>,
    generation: u32,
    listing: Option<Listing>,
    /// The files in the rows now.
    listed: Option<ListedFiles>,
    /// Files that arrived and wait for the next re-sort.
    arrived: Option<ListedFiles>,
}

impl ViewState {
    pub fn new(sort: Sort) -> ViewState {
        ViewState {
            sort,
            filter: Filter::All,
            text: String::new(),
            dir: None,
            list: RowList::default(),
            cursor: None,
            cursor_index: 0,
            last_key_ms: None,
            last_sort_ms: None,
            generation: 0,
            listing: None,
            listed: None,
            arrived: None,
        }
    }

    pub fn dir(&self) -> Option<NodeId> {
        self.dir
    }

    pub fn sort(&self) -> Sort {
        self.sort
    }

    pub fn filter(&self) -> Filter {
        self.filter
    }

    pub fn text_filter(&self) -> &str {
        &self.text
    }

    pub fn rows(&self) -> &[Row] {
        &self.list.rows
    }

    pub fn list(&self) -> &RowList {
        &self.list
    }

    pub fn cursor(&self) -> Option<&RowId> {
        self.cursor.as_ref()
    }

    pub fn cursor_index(&self) -> usize {
        self.cursor_index
    }

    pub fn cursor_row(&self) -> Option<&Row> {
        self.list.rows.get(self.cursor_index)
    }

    /// The directory Enter opens: `None` for a file, a synthetic row and the empty-directory row.
    pub fn activate(&self) -> Option<NodeId> {
        match self.cursor.as_ref()? {
            RowId::Dir(id) => Some(*id),
            _ => None,
        }
    }

    /// A key press: rows hold still for the next two seconds.
    pub fn note_key(&mut self, now_ms: u64) {
        self.last_key_ms = Some(now_ms);
    }

    /// Opens `dir`: the rows are built at once, the cursor lands on the first row and the listing of the
    /// directory's files starts (the previous listing is cancelled).
    pub fn open_dir(
        &mut self,
        tree: &ExploreTree,
        dir: NodeId,
        now_ms: u64,
        badge: &dyn Fn(NodeId) -> Badge,
    ) -> ListingRequest {
        self.note_key(now_ms);
        self.cursor = None;
        self.cursor_index = 0;
        self.go(tree, dir, now_ms, badge)
    }

    /// `h`: the parent directory, with the cursor on the directory just left. `None` at the top.
    pub fn leave(
        &mut self,
        tree: &ExploreTree,
        now_ms: u64,
        badge: &dyn Fn(NodeId) -> Badge,
    ) -> Option<ListingRequest> {
        let here = self.dir?;
        let parent = tree.parent(here)?;
        self.note_key(now_ms);
        self.cursor = Some(RowId::Dir(here));
        self.cursor_index = 0;
        Some(self.go(tree, parent, now_ms, badge))
    }

    fn go(
        &mut self,
        tree: &ExploreTree,
        dir: NodeId,
        now_ms: u64,
        badge: &dyn Fn(NodeId) -> Badge,
    ) -> ListingRequest {
        if let Some(old) = self.listing.take() {
            old.cancel.store(true, Ordering::SeqCst);
        }
        self.generation = self.generation.wrapping_add(1);
        let cancel = Arc::new(AtomicBool::new(false));
        self.listing = Some(Listing {
            generation: self.generation,
            dir,
            cancel: Arc::clone(&cancel),
        });
        self.dir = Some(dir);
        self.filter = Filter::All;
        self.text.clear();
        self.listed = None;
        self.arrived = None;
        self.rebuild(tree, now_ms, badge);
        ListingRequest {
            dir,
            generation: self.generation,
            cancel,
        }
    }

    /// The listing of `dir` is done. Returns false for a stale answer (another directory, or an older
    /// generation), which changes nothing. The rows merge at the next re-sort.
    pub fn accept_files(
        &mut self,
        generation: u32,
        dir: NodeId,
        rows: FileRows,
        exists: &dyn Fn(&OsStr) -> bool,
    ) -> bool {
        match &self.listing {
            Some(l) if l.generation == generation && l.dir == dir => {}
            _ => return false,
        }
        self.listing = None;
        self.arrived = Some(ListedFiles::from_listing(rows, exists));
        true
    }

    /// The files are still being listed.
    pub fn listing_pending(&self) -> bool {
        self.listing.is_some()
    }

    /// A key that changes the order or the set of rows applies at once: the owner asked for it.
    pub fn press_sort(
        &mut self,
        key: SortKey,
        tree: &ExploreTree,
        now_ms: u64,
        badge: &dyn Fn(NodeId) -> Badge,
    ) {
        self.note_key(now_ms);
        self.sort = self.sort.pressed(key);
        self.rebuild(tree, now_ms, badge);
    }

    pub fn cycle_filter(
        &mut self,
        tree: &ExploreTree,
        now_ms: u64,
        badge: &dyn Fn(NodeId) -> Badge,
    ) {
        self.note_key(now_ms);
        self.filter = self.filter.next();
        self.rebuild(tree, now_ms, badge);
    }

    /// Whether `/` text or `f` narrows the rows (what Esc clears first).
    pub fn filter_active(&self) -> bool {
        self.filter != Filter::All || !self.text.is_empty()
    }

    /// Esc: back to every row. Marks are not touched.
    pub fn clear_filters(
        &mut self,
        tree: &ExploreTree,
        now_ms: u64,
        badge: &dyn Fn(NodeId) -> Badge,
    ) {
        self.note_key(now_ms);
        self.filter = Filter::All;
        self.text.clear();
        self.rebuild(tree, now_ms, badge);
    }

    pub fn set_text_filter(
        &mut self,
        text: &str,
        tree: &ExploreTree,
        now_ms: u64,
        badge: &dyn Fn(NodeId) -> Badge,
    ) {
        self.note_key(now_ms);
        self.text = text.to_string();
        self.rebuild(tree, now_ms, badge);
    }

    /// Moves the cursor `delta` rows (clamped) and counts as a key press.
    pub fn move_cursor(&mut self, delta: isize, now_ms: u64) {
        self.note_key(now_ms);
        let last = self.list.rows.len().saturating_sub(1);
        let to = self.cursor_index.saturating_add_signed(delta).min(last);
        self.set_index(to);
    }

    pub fn move_to(&mut self, index: usize, now_ms: u64) {
        self.note_key(now_ms);
        self.set_index(index.min(self.list.rows.len().saturating_sub(1)));
    }

    fn set_index(&mut self, i: usize) {
        self.cursor_index = i;
        self.cursor = self.list.rows.get(i).map(|r| r.id.clone());
    }

    /// The throttled re-sort. Returns whether the order was recomputed. Inside the quiet window sizes of the
    /// rows already shown update in place and new rows are appended; rows that disappeared stay until the
    /// next re-sort.
    pub fn order_snapshot(
        &mut self,
        tree: &ExploreTree,
        now_ms: u64,
        badge: &dyn Fn(NodeId) -> Badge,
    ) -> bool {
        if self.dir.is_none() {
            return false;
        }
        let quiet = self
            .last_key_ms
            .is_none_or(|k| now_ms.saturating_sub(k) >= QUIET_AFTER_KEY_MS);
        let spaced = self
            .last_sort_ms
            .is_none_or(|t| now_ms.saturating_sub(t) >= RESORT_EVERY_MS);
        if quiet && spaced {
            self.rebuild(tree, now_ms, badge);
            true
        } else {
            self.update_in_place(tree, badge);
            false
        }
    }

    fn fresh(&self, tree: &ExploreTree, badge: &dyn Fn(NodeId) -> Badge) -> Option<RowList> {
        let dir = self.dir?;
        let src = RowSource {
            tree,
            dir,
            files: self.listed.as_ref(),
            badge,
        };
        Some(build_rows(&src, self.sort, self.filter, &self.text))
    }

    /// A full re-sort: pending files merge, the order is recomputed, the cursor keeps its identity.
    fn rebuild(&mut self, tree: &ExploreTree, now_ms: u64, badge: &dyn Fn(NodeId) -> Badge) {
        if let Some(a) = self.arrived.take() {
            self.listed = Some(a);
        }
        if let Some(list) = self.fresh(tree, badge) {
            self.list = list;
        }
        self.last_sort_ms = Some(now_ms);
        self.relocate();
    }

    fn update_in_place(&mut self, tree: &ExploreTree, badge: &dyn Fn(NodeId) -> Badge) {
        let Some(fresh) = self.fresh(tree, badge) else {
            return;
        };
        let mut by_id: std::collections::HashMap<RowId, Row> =
            fresh.rows.into_iter().map(|r| (r.id.clone(), r)).collect();
        let order: Vec<RowId> = {
            // Fresh order of the rows that are new, so appended rows keep a stable, sorted tail.
            let mut v: Vec<&Row> = by_id.values().collect();
            v.sort_by(|a, b| super::rows::order(a, b, self.sort));
            v.into_iter().map(|r| r.id.clone()).collect()
        };
        let mut rows: Vec<Row> = Vec::with_capacity(self.list.rows.len());
        for old in std::mem::take(&mut self.list.rows) {
            rows.push(by_id.remove(&old.id).unwrap_or(old));
        }
        for id in order {
            if let Some(r) = by_id.remove(&id) {
                rows.push(r);
            }
        }
        // The `(+N smaller)` and empty-directory rows stay last.
        let (mut body, tail): (Vec<Row>, Vec<Row>) =
            rows.into_iter().partition(|r| !r.pinned_last());
        body.extend(tail);
        self.list = RowList {
            rows: body,
            folded: fresh.folded,
            hard_links: fresh.hard_links,
            files_unlisted: fresh.files_unlisted,
        };
        self.relocate();
    }

    /// Finds the cursor's identity in the rows. Only when that row is gone does the cursor take the row now
    /// at its old index.
    fn relocate(&mut self) {
        let rows = &self.list.rows;
        if rows.is_empty() {
            self.cursor = None;
            self.cursor_index = 0;
            return;
        }
        if let Some(id) = &self.cursor {
            if let Some(i) = rows.iter().position(|r| &r.id == id) {
                self.cursor_index = i;
                return;
            }
        }
        let i = self.cursor_index.min(rows.len() - 1);
        self.set_index(i);
    }

    /// The path-line segments after the location: the sort in words, the filters, and `listing files...`
    /// while the lister has not answered.
    pub fn path_segments(&self) -> Vec<String> {
        let mut v = vec![format!("sort: {}", self.sort.words())];
        let mut filter = self.filter.words().to_string();
        if !self.text.is_empty() {
            filter = if self.filter == Filter::All {
                format!("/{}", self.text)
            } else {
                format!("{filter} /{}", self.text)
            };
        }
        v.push(format!("filter: {filter}"));
        if self.listing_pending() {
            v.push("listing files...".to_string());
        }
        v
    }

    fn column(&self) -> ColumnKind {
        match self.sort.key {
            SortKey::Usage => ColumnKind::Usage,
            SortKey::Apparent => ColumnKind::Apparent,
            SortKey::Meta => ColumnKind::Meta,
            SortKey::Items => ColumnKind::Items,
            SortKey::Name => ColumnKind::Name,
        }
    }

    fn column_shown(&self, layout: &TierLayout) -> bool {
        let c = self.column();
        c == ColumnKind::Name || layout.columns.iter().any(|col| col.kind == c)
    }

    /// The header's `v` or `^`; `None` when the sorted column is hidden at this tier.
    pub fn sort_mark(&self, layout: &TierLayout) -> Option<SortMark> {
        self.column_shown(layout).then_some(SortMark {
            column: self.column(),
            descending: self.sort.descending,
        })
    }

    /// For the title when the sort column is not drawn (a narrow terminal still sorts by it).
    pub fn hidden_column_note(&self, layout: &TierLayout) -> Option<String> {
        (!self.column_shown(layout)).then(|| {
            format!(
                "sorted by {} (column hidden at this width)",
                self.column().label()
            )
        })
    }

    /// The footer sentence the rows call for (the name-sort cap, hard links), if any.
    pub fn footer_note(&self) -> Option<String> {
        self.list.footer_note(self.sort)
    }

    /// Whether `path` names the directory the view shows (for callers matching a lister answer).
    pub fn is_viewing(&self, tree: &ExploreTree, path: &Path) -> bool {
        self.dir.is_some_and(|d| tree.path_of(d) == path)
    }
}
