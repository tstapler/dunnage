//! Paint, revalidate and rescan (Task 6.2.1c). A tree painted from the cache holds provisional nodes
//! (`painted`, not `confirmed`); the live pass confirms each one in place with the correction
//! `new - old` (old recomputed as `total - sum(child totals)`), adds directories that are new, and removes
//! painted nodes no live event confirmed. A per-parent name index finds the painted node of a live event in
//! constant time; it exists only while painted nodes are unconfirmed.

use std::collections::hash_map::Entry as MapEntry;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

use disk_cleanup_scan::estimate::Stats;

use super::reducer::DirData;
use super::tree::{flag, stats_add, stats_sub, ExploreTree, NodeId, NONE};

/// `(parent, name hash)` to node, one 64-bit key per painted directory. A hit is compared by name; two
/// names with one key (never expected) fall back to scanning the parent's children.
#[derive(Debug, Default)]
pub struct NameIndex {
    map: HashMap<u64, u32>,
    collided: bool,
}

fn name_hash(name: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in name {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn key(parent: Option<u32>, name: &[u8]) -> u64 {
    name_hash(name) ^ u64::from(parent.unwrap_or(NONE)).wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

impl NameIndex {
    pub fn clear(&mut self) {
        *self = NameIndex::default();
    }

    /// Reserved bytes: buckets of a `(u64, u32)` entry plus a control byte, at 7/8 load.
    pub fn heap_bytes(&self) -> usize {
        self.map.capacity() * 17 * 8 / 7
    }
}

impl ExploreTree {
    pub(super) fn index_insert(&mut self, parent: Option<u32>, id: u32) {
        let k = key(parent, self.name_bytes(id));
        match self.index.map.entry(k) {
            MapEntry::Vacant(v) => {
                v.insert(id);
            }
            MapEntry::Occupied(_) => self.index.collided = true,
        }
    }

    fn index_remove(&mut self, id: u32) {
        let parent = self.node(id).parent;
        let k = key((parent != NONE).then_some(parent), self.name_bytes(id));
        if self.index.map.get(&k) == Some(&id) {
            self.index.map.remove(&k);
        }
    }

    fn is_unconfirmed(&self, i: u32) -> bool {
        let n = self.node(i);
        n.has(flag::PAINTED) && !n.has(flag::CONFIRMED)
    }

    /// The painted, not yet confirmed child of `p` called `name`.
    pub(super) fn find_unconfirmed(&self, p: u32, name: &OsStr) -> Option<u32> {
        if self.unconfirmed == 0 {
            return None;
        }
        let bytes = name.as_bytes();
        if let Some(&c) = self.index.map.get(&key(Some(p), bytes)) {
            if self.node(c).parent == p && self.name_bytes(c) == bytes && self.is_unconfirmed(c) {
                return Some(c);
            }
        }
        if !self.index.collided {
            return None;
        }
        self.children(NodeId(p))
            .map(|c| c.0)
            .find(|&c| self.name_bytes(c) == bytes && self.is_unconfirmed(c))
    }

    pub(super) fn find_painted_root(&self, name: &OsStr) -> Option<u32> {
        self.roots
            .iter()
            .copied()
            .find(|&r| self.name_bytes(r) == name.as_bytes() && self.is_unconfirmed(r))
    }

    /// What the directory itself holds: its total less its children's totals (saturating).
    fn own_stats(&self, id: u32) -> Stats {
        let mut own = self.node(id).total;
        let mut c = self.node(id).first_child;
        while c != NONE {
            stats_sub(&mut own, &self.node(c).total);
            c = self.node(c).next_sibling;
        }
        own
    }

    fn one_confirmed(&mut self, id: u32) {
        self.unconfirmed = self.unconfirmed.saturating_sub(1);
        self.index_remove(id);
        if self.unconfirmed == 0 {
            self.index.clear();
        }
    }

    /// A live `Dir` event for a painted node: apply `new - old` up the chain and reopen the node.
    pub(super) fn confirm_dir(&mut self, id: u32, d: &DirData<'_>) {
        let old = self.own_stats(id);
        let mut cur = id;
        while cur != NONE {
            let n = self.node_mut(cur);
            stats_add(&mut n.total, &d.info.stats);
            stats_sub(&mut n.total, &old);
            n.mtime = n.mtime.max(d.info.mtime);
            cur = n.parent;
        }
        let n = self.node_mut(id);
        n.mtime = d.info.mtime.max(n.mtime);
        n.pending = d.subdirs;
        let mut f = n.flags
            & !(flag::COMPLETE
                | flag::PAINTED
                | flag::FROM_CACHE
                | flag::HAS_PKG
                | flag::HAS_CARGO
                | flag::GIT_FILE);
        f |= flag::CONFIRMED | flag::OWN;
        if d.info.has_package_json {
            f |= flag::HAS_PKG;
        }
        if d.info.has_cargo_toml {
            f |= flag::HAS_CARGO;
        }
        if d.info.git_file {
            f |= flag::GIT_FILE;
        }
        if d.from_cache {
            f |= flag::FROM_CACHE;
        }
        n.flags = f;
        self.one_confirmed(id);
        self.settle(id);
    }

    /// A live `Unreadable` or `Boundary` event for a painted node: it keeps no sizes and no children.
    pub(super) fn convert_painted(&mut self, id: u32, kind: u16) {
        let kids: Vec<u32> = self.children(NodeId(id)).map(|c| c.0).collect();
        for k in kids {
            self.remove_subtree(k);
        }
        let own = self.node(id).total;
        let mut cur = id;
        while cur != NONE {
            let n = self.node_mut(cur);
            stats_sub(&mut n.total, &own);
            cur = n.parent;
        }
        let parent = {
            let n = self.node_mut(id);
            n.flags = flag::CONFIRMED | flag::OWN | kind;
            n.pending = 0;
            n.parent
        };
        if kind == flag::UNREADABLE && parent != NONE {
            let mut cur = parent;
            while cur != NONE {
                let n = self.node_mut(cur);
                n.unreadable_below += 1;
                cur = n.parent;
            }
        }
        self.one_confirmed(id);
        self.settle(id);
    }

    /// Takes `id` and everything below it out of the tree: its totals leave every ancestor, its slots are
    /// reused.
    pub(super) fn remove_subtree(&mut self, id: u32) {
        let (total, unreadable, not_listed, parent) = {
            let n = self.node(id);
            (
                n.total,
                n.unreadable_below + u32::from(n.has(flag::UNREADABLE)),
                n.not_listed,
                n.parent,
            )
        };
        let mut cur = parent;
        while cur != NONE {
            let n = self.node_mut(cur);
            stats_sub(&mut n.total, &total);
            n.unreadable_below = n.unreadable_below.saturating_sub(unreadable);
            n.not_listed = n.not_listed.saturating_sub(not_listed);
            cur = n.parent;
        }
        self.detach(id);
        self.free_subtree(id);
    }

    fn detach(&mut self, id: u32) {
        let (parent, next) = {
            let n = self.node(id);
            (n.parent, n.next_sibling)
        };
        if parent == NONE {
            self.roots.retain(|&r| r != id);
            return;
        }
        let mut c = self.node(parent).first_child;
        if c == id {
            self.node_mut(parent).first_child = next;
            return;
        }
        while c != NONE {
            let after = self.node(c).next_sibling;
            if after == id {
                self.node_mut(c).next_sibling = next;
                return;
            }
            c = after;
        }
    }

    fn free_subtree(&mut self, id: u32) {
        let mut stack = vec![id];
        while let Some(i) = stack.pop() {
            let mut c = self.node(i).first_child;
            while c != NONE {
                stack.push(c);
                c = self.node(c).next_sibling;
            }
            if self.is_unconfirmed(i) {
                self.index_remove(i);
                self.unconfirmed = self.unconfirmed.saturating_sub(1);
            }
            self.node_mut(i).flags = flag::DEAD;
            self.free.push(i);
            self.alive -= 1;
        }
        if self.unconfirmed == 0 {
            self.index.clear();
        }
    }

    /// Called when a confirmed node completes: its painted children that no live event named are gone.
    pub(super) fn sweep_unconfirmed(&mut self, id: u32) {
        let stale: Vec<u32> = self
            .children(NodeId(id))
            .map(|c| c.0)
            .filter(|&c| self.is_unconfirmed(c))
            .collect();
        for c in stale {
            self.remove_subtree(c);
        }
    }

    /// End of a whole scan: every painted node still unconfirmed is stale.
    pub(super) fn purge_unconfirmed(&mut self) {
        let tops: Vec<u32> = (0..self.nodes.len() as u32)
            .filter(|&i| {
                let n = self.node(i);
                if n.has(flag::DEAD) || !self.is_unconfirmed(i) {
                    return false;
                }
                n.parent == NONE || !self.is_unconfirmed(n.parent)
            })
            .collect();
        for t in tops {
            if !self.node(t).has(flag::DEAD) {
                self.remove_subtree(t);
            }
        }
    }

    /// Starts a rescan of `id`: its subtree leaves every ancestor's totals, its children are dropped and
    /// the node is incomplete again. Batches of the previous generation are ignored from here on; the new
    /// generation is returned for the source's options. The next live root event lands on `id`.
    pub fn begin_rescan(&mut self, id: NodeId) -> u32 {
        let id = id.0;
        let (total, unreadable, not_listed, parent, was_complete) = {
            let n = self.node(id);
            (
                n.total,
                n.unreadable_below + u32::from(n.has(flag::UNREADABLE)),
                n.not_listed,
                n.parent,
                n.has(flag::COMPLETE),
            )
        };
        let mut cur = parent;
        while cur != NONE {
            let n = self.node_mut(cur);
            stats_sub(&mut n.total, &total);
            n.unreadable_below = n.unreadable_below.saturating_sub(unreadable);
            n.not_listed = n.not_listed.saturating_sub(not_listed);
            cur = n.parent;
        }
        let kids: Vec<u32> = self.children(NodeId(id)).map(|c| c.0).collect();
        for k in kids {
            self.free_subtree(k);
        }
        if self.is_unconfirmed(id) {
            self.one_confirmed(id);
        }
        {
            let n = self.node_mut(id);
            n.first_child = NONE;
            n.total = Stats::default();
            n.pending = 0;
            n.unreadable_below = 0;
            n.not_listed = 0;
            n.flags = 0; // the live event restores the facts
        }
        let mut reopen = was_complete;
        let mut up = parent;
        while reopen && up != NONE {
            let p = self.node_mut(up);
            p.pending += 1;
            reopen = p.has(flag::COMPLETE);
            p.flags &= !flag::COMPLETE;
            up = p.parent;
        }
        self.generation += 1;
        self.rescan_target = Some(id);
        self.finished = false;
        self.truncated = false;
        self.live_map.clear();
        self.paint_map.clear();
        self.orphans_dropped += self.parked_total as u64;
        self.parked.clear();
        self.parked_total = 0;
        self.generation
    }
}
