//! `ExploreTree`: the explorer's single-owner arena of directories (Task 6.2.1b). Files are never nodes;
//! their bytes are part of a directory's `total`. The reducer (`reducer.rs`, `revalidate.rs`) is the only
//! writer; everything here is the shape and the read side.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use dunnage_scan::estimate::Stats;
use dunnage_scan::{classify_dir, ParentFacts};

use super::msg::{NodeEvent, ScanMode};
use super::names::{IdMap, NameArena};
use super::revalidate::NameIndex;

/// The shipped cap on kept directories (about 600 MB of arena).
pub const NODE_MAX: usize = 4_000_000;
/// Events held while they wait for a parent that has not arrived.
pub const PARK_MAX: usize = 100_000;

pub(super) const NONE: u32 = u32::MAX;

pub(super) mod flag {
    pub const COMPLETE: u16 = 1;
    pub const PAINTED: u16 = 1 << 1;
    pub const CONFIRMED: u16 = 1 << 2;
    pub const UNREADABLE: u16 = 1 << 3;
    pub const BOUNDARY: u16 = 1 << 4;
    pub const HAS_PKG: u16 = 1 << 5;
    pub const HAS_CARGO: u16 = 1 << 6;
    pub const GIT_FILE: u16 = 1 << 7;
    /// The directory's own event has arrived.
    pub const OWN: u16 = 1 << 8;
    pub const FROM_CACHE: u16 = 1 << 9;
    pub const DEAD: u16 = 1 << 10;
}

/// Handle of a directory in an [`ExploreTree`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub(super) u32);

impl NodeId {
    pub fn index(self) -> u32 {
        self.0
    }
}

/// 128 bytes: `total` 88, `mtime` 8, seven `u32` and two `u16`.
#[derive(Debug, Clone)]
pub(super) struct Node {
    pub total: Stats,
    pub mtime: i64,
    pub parent: u32,
    pub first_child: u32,
    pub next_sibling: u32,
    pub name_off: u32,
    /// Child directories whose subtree is not complete yet.
    pub pending: u32,
    /// Unreadable directories at or below this node, itself excluded.
    pub unreadable_below: u32,
    /// Directories folded into this node at the node limit, at or below it.
    pub not_listed: u32,
    pub name_len: u16,
    pub flags: u16,
}

impl Node {
    pub fn has(&self, f: u16) -> bool {
        self.flags & f != 0
    }
}

/// An event waiting for its parent.
#[derive(Debug)]
pub(super) struct Parked {
    pub mode: ScanMode,
    pub event: NodeEvent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReduceError {
    /// A walker id too large for the arena (`u32::MAX` or more).
    TreeFull,
}

impl std::fmt::Display for ReduceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the directory tree is full")
    }
}

impl std::error::Error for ReduceError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartialCause {
    NodeLimit,
    Orphans,
    TreeFull,
    Incomplete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanStatus {
    Scanning,
    Complete,
    /// The walk was stopped; incomplete nodes read `at least`.
    Truncated,
    Partial(PartialCause),
}

/// Whether a node sits inside a build directory or `.git` that the cleaner lists as one item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inside {
    No,
    Yes,
    /// The ancestors above the scan root are not all known.
    Unknown,
}

#[derive(Debug)]
pub struct ExploreTree {
    pub(super) nodes: Vec<Node>,
    pub(super) names: NameArena,
    pub(super) free: Vec<u32>,
    pub(super) alive: usize,
    pub(super) roots: Vec<u32>,
    pub(super) live_map: IdMap,
    pub(super) paint_map: IdMap,
    pub(super) parked: HashMap<u64, Vec<Parked>>,
    pub(super) parked_total: usize,
    pub(super) index: NameIndex,
    pub(super) unconfirmed: usize,
    pub(super) generation: u32,
    pub(super) rescan_target: Option<u32>,
    pub(super) node_max: usize,
    pub(super) park_max: usize,
    pub(super) orphans_dropped: u64,
    pub(super) folded_dirs: u64,
    pub(super) tree_full: bool,
    pub(super) finished: bool,
    pub(super) truncated: bool,
}

impl Default for ExploreTree {
    fn default() -> ExploreTree {
        ExploreTree::new()
    }
}

pub(super) fn stats_add(a: &mut Stats, b: &Stats) {
    a.inodes += b.inodes;
    a.files += b.files;
    a.dirs += b.dirs;
    a.symlinks += b.symlinks;
    a.dir_entries += b.dir_entries;
    a.name_bytes += b.name_bytes;
    a.size_bytes += b.size_bytes;
    a.allocated_bytes += b.allocated_bytes;
    a.extents += b.extents;
    a.inline_files += b.inline_files;
    a.inline_bytes += b.inline_bytes;
}

pub(super) fn stats_sub(a: &mut Stats, b: &Stats) {
    a.inodes = a.inodes.saturating_sub(b.inodes);
    a.files = a.files.saturating_sub(b.files);
    a.dirs = a.dirs.saturating_sub(b.dirs);
    a.symlinks = a.symlinks.saturating_sub(b.symlinks);
    a.dir_entries = a.dir_entries.saturating_sub(b.dir_entries);
    a.name_bytes = a.name_bytes.saturating_sub(b.name_bytes);
    a.size_bytes = a.size_bytes.saturating_sub(b.size_bytes);
    a.allocated_bytes = a.allocated_bytes.saturating_sub(b.allocated_bytes);
    a.extents = a.extents.saturating_sub(b.extents);
    a.inline_files = a.inline_files.saturating_sub(b.inline_files);
    a.inline_bytes = a.inline_bytes.saturating_sub(b.inline_bytes);
}

impl ExploreTree {
    pub fn new() -> ExploreTree {
        ExploreTree::with_limits(NODE_MAX, PARK_MAX)
    }

    /// The limits are parameters so a test can reach the node cap with a small tree.
    pub fn with_limits(node_max: usize, park_max: usize) -> ExploreTree {
        ExploreTree {
            nodes: Vec::new(),
            names: NameArena::default(),
            free: Vec::new(),
            alive: 0,
            roots: Vec::new(),
            live_map: IdMap::default(),
            paint_map: IdMap::default(),
            parked: HashMap::new(),
            parked_total: 0,
            index: NameIndex::default(),
            unconfirmed: 0,
            generation: 0,
            rescan_target: None,
            node_max: node_max.min(NONE as usize / 2),
            park_max,
            orphans_dropped: 0,
            folded_dirs: 0,
            tree_full: false,
            finished: false,
            truncated: false,
        }
    }

    pub(super) fn node(&self, i: u32) -> &Node {
        &self.nodes[i as usize]
    }

    pub(super) fn node_mut(&mut self, i: u32) -> &mut Node {
        &mut self.nodes[i as usize]
    }

    /// Takes a slot (a freed one first), links it as the first child of `parent`, and stores the name.
    pub(super) fn alloc(&mut self, parent: Option<u32>, name: &OsStr) -> u32 {
        let (name_off, name_len) = self.names.push(name.as_bytes());
        let node = Node {
            total: Stats::default(),
            mtime: 0,
            parent: parent.unwrap_or(NONE),
            first_child: NONE,
            next_sibling: NONE,
            name_off,
            pending: 0,
            unreadable_below: 0,
            not_listed: 0,
            name_len,
            flags: 0,
        };
        let id = if let Some(slot) = self.free.pop() {
            self.nodes[slot as usize] = node;
            slot
        } else {
            if self.nodes.len() == self.nodes.capacity() {
                self.nodes.reserve_exact((self.nodes.len() / 4).max(1024));
            }
            self.nodes.push(node);
            (self.nodes.len() - 1) as u32
        };
        self.alive += 1;
        match parent {
            Some(p) => {
                let head = self.nodes[p as usize].first_child;
                self.nodes[id as usize].next_sibling = head;
                self.nodes[p as usize].first_child = id;
            }
            None => self.roots.push(id),
        }
        id
    }

    pub(super) fn name_bytes(&self, i: u32) -> &[u8] {
        let n = self.node(i);
        self.names.get(n.name_off, n.name_len)
    }

    // ----- read side -----

    /// The scan roots, in arrival order.
    pub fn roots(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.roots.iter().map(|&i| NodeId(i))
    }

    /// Every kept directory.
    pub fn iter(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| !n.has(flag::DEAD))
            .map(|(i, _)| NodeId(i as u32))
    }

    pub fn directories(&self) -> usize {
        self.alive
    }

    pub fn children(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        Children {
            tree: self,
            cur: self.node(id.0).first_child,
        }
    }

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        let p = self.node(id.0).parent;
        (p != NONE).then_some(NodeId(p))
    }

    /// The basename; for a root, the full path it was scanned as.
    pub fn name(&self, id: NodeId) -> &OsStr {
        OsStr::from_bytes(self.name_bytes(id.0))
    }

    /// Rebuilt from the parent chain; no path is stored per node.
    pub fn path_of(&self, id: NodeId) -> PathBuf {
        let mut parts: Vec<&OsStr> = Vec::new();
        let mut cur = id.0;
        loop {
            parts.push(OsStr::from_bytes(self.name_bytes(cur)));
            let p = self.node(cur).parent;
            if p == NONE {
                break;
            }
            cur = p;
        }
        let mut path = PathBuf::new();
        for part in parts.iter().rev() {
            path.push(part);
        }
        path
    }

    /// Totals of the directory and everything below it.
    pub fn total(&self, id: NodeId) -> &Stats {
        &self.node(id.0).total
    }

    pub fn newest_mtime(&self, id: NodeId) -> i64 {
        self.node(id.0).mtime
    }

    pub fn is_complete(&self, id: NodeId) -> bool {
        self.node(id.0).has(flag::COMPLETE)
    }

    pub fn is_unreadable(&self, id: NodeId) -> bool {
        self.node(id.0).has(flag::UNREADABLE)
    }

    pub fn is_boundary(&self, id: NodeId) -> bool {
        self.node(id.0).has(flag::BOUNDARY)
    }

    /// Unreadable directories below this one.
    pub fn unreadable_below(&self, id: NodeId) -> u32 {
        self.node(id.0).unreadable_below
    }

    /// Directories folded into this node at the node limit (`+N directories not listed`).
    pub fn not_listed(&self, id: NodeId) -> u32 {
        self.node(id.0).not_listed
    }

    /// Painted from the cache and not yet confirmed by a live event (or answered from the cache).
    pub fn from_cache(&self, id: NodeId) -> bool {
        self.node(id.0).has(flag::FROM_CACHE)
    }

    pub fn has_package_json(&self, id: NodeId) -> bool {
        self.node(id.0).has(flag::HAS_PKG)
    }

    pub fn has_cargo_toml(&self, id: NodeId) -> bool {
        self.node(id.0).has(flag::HAS_CARGO)
    }

    /// After a truncated scan an incomplete node's numbers are lower bounds.
    pub fn is_at_least(&self, id: NodeId) -> bool {
        self.finished && self.truncated && !self.is_complete(id)
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }

    pub fn orphans_dropped(&self) -> u64 {
        self.orphans_dropped
    }

    pub fn status(&self) -> ScanStatus {
        if !self.finished {
            return ScanStatus::Scanning;
        }
        if self.truncated {
            return ScanStatus::Truncated;
        }
        if self.tree_full {
            return ScanStatus::Partial(PartialCause::TreeFull);
        }
        if self.folded_dirs > 0 {
            return ScanStatus::Partial(PartialCause::NodeLimit);
        }
        if self.orphans_dropped > 0 {
            return ScanStatus::Partial(PartialCause::Orphans);
        }
        if self
            .roots
            .iter()
            .any(|&r| !self.node(r).has(flag::COMPLETE))
        {
            return ScanStatus::Partial(PartialCause::Incomplete);
        }
        ScanStatus::Complete
    }

    /// Reserved bytes, not lengths: the node vector's and the name arena's capacity, the walker-id maps,
    /// the name index and what is parked.
    pub fn heap_bytes(&self) -> usize {
        self.nodes.capacity() * std::mem::size_of::<Node>()
            + self.names.capacity()
            + self.free.capacity() * 4
            + self.roots.capacity() * 4
            + self.live_map.heap_bytes()
            + self.paint_map.heap_bytes()
            + self.index.heap_bytes()
            + self.parked_total * std::mem::size_of::<Parked>()
    }

    /// Whether `id` sits inside a build directory or `.git`, from the parent chain alone (no I/O).
    ///
    /// `Yes`: an ancestor is `.git`, or a build directory by `classify_dir` with its own parent's marker
    /// flags. `Unknown`: the scan root, or a path component above it, is named `node_modules` or `target`
    /// (the flags of that name's parent are not in the tree). A `.git` above the scan root is `Yes`.
    pub fn inside_special(&self, id: NodeId) -> Inside {
        let mut unknown = false;
        let mut top = id.0;
        let mut cur = self.node(id.0).parent;
        while cur != NONE {
            top = cur;
            let name = OsStr::from_bytes(self.name_bytes(cur));
            let parent = self.node(cur).parent;
            if parent == NONE {
                // The root: its own name is a full path, judged by its basename below.
                break;
            }
            if name.as_bytes() == b".git" {
                return Inside::Yes;
            }
            let pf = self.node(parent);
            let facts = ParentFacts {
                has_package_json: pf.has(flag::HAS_PKG),
                has_cargo_toml: pf.has(flag::HAS_CARGO),
            };
            if classify_dir(name, facts).is_some() {
                return Inside::Yes;
            }
            cur = parent;
        }
        let root_path = Path::new(OsStr::from_bytes(self.name_bytes(top)));
        let comps: Vec<&OsStr> = root_path
            .components()
            .filter_map(|c| match c {
                std::path::Component::Normal(s) => Some(s),
                _ => None,
            })
            .collect();
        for (i, c) in comps.iter().enumerate() {
            if i + 1 == comps.len() && top == id.0 {
                continue; // the node is the root: its own name is not an ancestor
            }
            match c.as_bytes() {
                b".git" => return Inside::Yes,
                b"node_modules" | b"target" => unknown = true,
                _ => {}
            }
        }
        if unknown {
            Inside::Unknown
        } else {
            Inside::No
        }
    }
}

struct Children<'a> {
    tree: &'a ExploreTree,
    cur: u32,
}

impl Iterator for Children<'_> {
    type Item = NodeId;
    fn next(&mut self) -> Option<NodeId> {
        if self.cur == NONE {
            return None;
        }
        let c = self.cur;
        self.cur = self.tree.node(c).next_sibling;
        Some(NodeId(c))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_node_is_at_most_136_bytes() {
        assert!(
            std::mem::size_of::<Node>() <= 136,
            "{}",
            std::mem::size_of::<Node>()
        );
    }
}
