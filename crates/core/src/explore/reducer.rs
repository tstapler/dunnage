//! The reducer: folds batched, id-keyed events into an [`ExploreTree`] in any arrival order (Task 6.2.1b).
//!
//! An event whose parent has not arrived is parked (at most `park_max`; the rest are counted in
//! `orphans_dropped`) and placed when the parent's event arrives. A directory's totals grow by its own
//! stats at every ancestor, so a fresh scan only grows. A node is `complete` when its own event and every
//! child's subtree completion have arrived (`pending` counts the children still open); `Boundary`,
//! `Unreadable` and `Gone` children complete at once. At the node limit a refused directory is folded into
//! the nearest kept ancestor, which stays incomplete until the folded descendants' events have arrived.

use std::ffi::OsStr;

use dunnage_scan::estimate::Stats;
use dunnage_scan::index::DirInfo;

use super::msg::{NodeEvent, ScanMode, NO_PARENT, PAINT_ID_BASE};
use super::names::Entry;
use super::tree::{flag, stats_add, ExploreTree, Parked, ReduceError, NONE};

/// What the reducer needs of a `Dir` event.
pub(super) struct DirData<'a> {
    pub name: &'a OsStr,
    pub info: &'a DirInfo,
    pub subdirs: u32,
    pub from_cache: bool,
}

#[derive(Clone, Copy)]
enum Parent {
    Root,
    Node(u32),
    Folded(u32),
}

impl ExploreTree {
    /// Folds one batch. Batches of another generation (an old scan, before a rescan) change nothing.
    /// Returns the first error after applying every event it could.
    pub fn apply_batch(
        &mut self,
        generation: u32,
        mode: ScanMode,
        events: Vec<NodeEvent>,
    ) -> Result<(), ReduceError> {
        if generation != self.generation || self.finished {
            return Ok(());
        }
        if mode == ScanMode::Revalidate && !self.paint_map.is_empty() {
            self.paint_map.clear();
        }
        let mut first_err = None;
        for ev in events {
            if let Err(e) = self.apply_event(mode, ev) {
                first_err.get_or_insert(e);
            }
        }
        first_err.map_or(Ok(()), Err)
    }

    fn space_key(mode: ScanMode, id: u64) -> Option<u64> {
        if mode == ScanMode::Paint {
            id.checked_sub(PAINT_ID_BASE)
        } else {
            Some(id)
        }
    }

    fn map_get(&self, mode: ScanMode, key: u64) -> Option<Entry> {
        if mode == ScanMode::Paint {
            self.paint_map.get(key)
        } else {
            self.live_map.get(key)
        }
    }

    pub(super) fn map_set(&mut self, mode: ScanMode, key: u64, e: Entry) {
        if mode == ScanMode::Paint {
            self.paint_map.set(key, e);
        } else {
            self.live_map.set(key, e);
        }
    }

    fn park(&mut self, mode: ScanMode, parent_id: u64, event: NodeEvent) {
        if self.parked_total >= self.park_max {
            self.orphans_dropped += 1;
            return;
        }
        self.parked_total += 1;
        self.parked
            .entry(parent_id)
            .or_default()
            .push(Parked { mode, event });
    }

    fn apply_event(&mut self, mode: ScanMode, ev: NodeEvent) -> Result<(), ReduceError> {
        let mut err = None;
        let mut work = vec![(mode, ev)];
        while let Some((m, ev)) = work.pop() {
            let id = ev.id();
            match self.apply_one(m, ev) {
                Ok(()) => {
                    if let Some(list) = self.parked.remove(&id) {
                        self.parked_total -= list.len();
                        work.extend(list.into_iter().map(|p| (p.mode, p.event)));
                    }
                }
                Err(e) => err = err.or(Some(e)),
            }
        }
        err.map_or(Ok(()), Err)
    }

    fn apply_one(&mut self, mode: ScanMode, ev: NodeEvent) -> Result<(), ReduceError> {
        let id = ev.id();
        let parent = ev.parent();
        let Some(key) = Self::space_key(mode, id) else {
            self.orphans_dropped += 1;
            return Ok(());
        };
        if key >= u64::from(u32::MAX) {
            self.tree_full = true;
            return Err(ReduceError::TreeFull);
        }
        if self.map_get(mode, key).is_some() {
            return Ok(());
        }
        let resolved = if parent == NO_PARENT {
            Parent::Root
        } else {
            let Some(pkey) = Self::space_key(mode, parent) else {
                self.orphans_dropped += 1;
                return Ok(());
            };
            match self.map_get(mode, pkey) {
                Some(Entry::Node(n)) => Parent::Node(n),
                Some(Entry::Folded(t)) => Parent::Folded(t),
                None => {
                    self.park(mode, parent, ev);
                    return Ok(());
                }
            }
        };
        self.place(mode, key, resolved, ev);
        Ok(())
    }

    fn place(&mut self, mode: ScanMode, key: u64, parent: Parent, ev: NodeEvent) {
        match ev {
            NodeEvent::Dir {
                name,
                info,
                subdirs,
                from_cache,
                ..
            } => {
                let d = DirData {
                    name: &name,
                    info: &info,
                    subdirs,
                    from_cache,
                };
                let entry = self.place_dir(mode, parent, &d);
                self.map_set(mode, key, entry);
            }
            NodeEvent::Unreadable { name, .. } => {
                let e = self.place_terminal(mode, parent, &name, flag::UNREADABLE);
                self.map_set(mode, key, e);
            }
            NodeEvent::Boundary { name, .. } => {
                let e = self.place_terminal(mode, parent, &name, flag::BOUNDARY);
                self.map_set(mode, key, e);
            }
            NodeEvent::Gone { name, .. } => self.place_gone(mode, parent, &name),
        }
    }

    fn place_dir(&mut self, mode: ScanMode, parent: Parent, d: &DirData<'_>) -> Entry {
        match parent {
            Parent::Root => {
                if mode != ScanMode::Paint {
                    if let Some(t) = self.rescan_target.take() {
                        self.assign_dir(t, d);
                        return Entry::Node(t);
                    }
                }
                if mode == ScanMode::Revalidate {
                    if let Some(r) = self.find_painted_root(d.name) {
                        self.confirm_dir(r, d);
                        return Entry::Node(r);
                    }
                }
                Entry::Node(self.create_dir(None, mode, d))
            }
            Parent::Node(p) => {
                if mode == ScanMode::Revalidate {
                    if let Some(c) = self.find_unconfirmed(p, d.name) {
                        self.confirm_dir(c, d);
                        return Entry::Node(c);
                    }
                }
                if self.alive >= self.node_max {
                    self.fold_dir(p, d);
                    return Entry::Folded(p);
                }
                Entry::Node(self.create_dir(Some(p), mode, d))
            }
            Parent::Folded(t) => {
                self.fold_dir(t, d);
                Entry::Folded(t)
            }
        }
    }

    fn place_terminal(&mut self, mode: ScanMode, parent: Parent, name: &OsStr, kind: u16) -> Entry {
        match parent {
            Parent::Root => {
                if mode != ScanMode::Paint {
                    if let Some(t) = self.rescan_target.take() {
                        self.convert_existing(t, kind);
                        return Entry::Node(t);
                    }
                }
                Entry::Node(self.create_terminal(None, name, kind))
            }
            Parent::Node(p) => {
                if mode == ScanMode::Revalidate {
                    if let Some(c) = self.find_unconfirmed(p, name) {
                        self.convert_painted(c, kind);
                        return Entry::Node(c);
                    }
                }
                if self.alive >= self.node_max {
                    self.fold_terminal(p, kind);
                    return Entry::Folded(p);
                }
                Entry::Node(self.create_terminal(Some(p), name, kind))
            }
            Parent::Folded(t) => {
                self.fold_terminal(t, kind);
                Entry::Folded(t)
            }
        }
    }

    fn place_gone(&mut self, mode: ScanMode, parent: Parent, name: &OsStr) {
        match parent {
            Parent::Root => {}
            Parent::Node(p) => {
                if mode == ScanMode::Revalidate {
                    if let Some(c) = self.find_unconfirmed(p, name) {
                        self.remove_subtree(c);
                    }
                }
                self.child_terminal(p);
            }
            Parent::Folded(t) => self.child_terminal(t),
        }
    }

    // ----- adding -----

    /// Adds `stats` to `start` and every ancestor.
    pub(super) fn propagate_add(&mut self, start: Option<u32>, stats: &Stats, mtime: i64) {
        let mut cur = start.unwrap_or(NONE);
        while cur != NONE {
            let n = self.node_mut(cur);
            stats_add(&mut n.total, stats);
            n.mtime = n.mtime.max(mtime);
            cur = n.parent;
        }
    }

    fn propagate_count(&mut self, start: Option<u32>, unreadable: u32, not_listed: u32) {
        let mut cur = start.unwrap_or(NONE);
        while cur != NONE {
            let n = self.node_mut(cur);
            n.unreadable_below += unreadable;
            n.not_listed += not_listed;
            cur = n.parent;
        }
    }

    fn dir_flags(d: &DirData<'_>, mode: ScanMode) -> u16 {
        let mut f = flag::OWN;
        if d.info.has_package_json {
            f |= flag::HAS_PKG;
        }
        if d.info.has_cargo_toml {
            f |= flag::HAS_CARGO;
        }
        if d.info.git_file {
            f |= flag::GIT_FILE;
        }
        if d.from_cache || mode == ScanMode::Paint {
            f |= flag::FROM_CACHE;
        }
        if mode == ScanMode::Paint {
            f |= flag::PAINTED;
        }
        f
    }

    fn create_dir(&mut self, parent: Option<u32>, mode: ScanMode, d: &DirData<'_>) -> u32 {
        let id = self.alloc(parent, d.name);
        let n = self.node_mut(id);
        n.total = d.info.stats;
        n.mtime = d.info.mtime;
        n.pending = d.subdirs;
        n.flags = Self::dir_flags(d, mode);
        self.propagate_add(parent, &d.info.stats, d.info.mtime);
        if mode == ScanMode::Paint {
            self.unconfirmed += 1;
            self.index_insert(parent, id);
        }
        self.settle(id);
        id
    }

    /// The first live event of a rescan lands on the node that was reset.
    fn assign_dir(&mut self, id: u32, d: &DirData<'_>) {
        let parent = {
            let n = self.node_mut(id);
            n.total = d.info.stats;
            n.mtime = d.info.mtime;
            n.pending = d.subdirs;
            n.flags = Self::dir_flags(d, ScanMode::Fresh);
            let p = n.parent;
            (p != NONE).then_some(p)
        };
        self.propagate_add(parent, &d.info.stats, d.info.mtime);
        self.settle(id);
    }

    fn create_terminal(&mut self, parent: Option<u32>, name: &OsStr, kind: u16) -> u32 {
        let id = self.alloc(parent, name);
        self.node_mut(id).flags = flag::OWN | kind;
        if kind == flag::UNREADABLE {
            self.propagate_count(parent, 1, 0);
        }
        self.settle(id);
        id
    }

    fn convert_existing(&mut self, id: u32, kind: u16) {
        let parent = {
            let n = self.node_mut(id);
            n.flags = flag::OWN | kind;
            n.pending = 0;
            let p = n.parent;
            (p != NONE).then_some(p)
        };
        if kind == flag::UNREADABLE {
            self.propagate_count(parent, 1, 0);
        }
        self.settle(id);
    }

    // ----- node limit -----

    fn fold_dir(&mut self, target: u32, d: &DirData<'_>) {
        self.propagate_add(Some(target), &d.info.stats, d.info.mtime);
        self.propagate_count(Some(target), 0, 1);
        self.folded_dirs += 1;
        let n = self.node_mut(target);
        n.pending = n.pending.saturating_add(d.subdirs).saturating_sub(1);
        self.settle(target);
    }

    fn fold_terminal(&mut self, target: u32, kind: u16) {
        self.folded_dirs += 1;
        self.propagate_count(Some(target), u32::from(kind == flag::UNREADABLE), 1);
        self.child_terminal(target);
    }

    // ----- completion -----

    /// One child of `p` is done without a node of its own.
    pub(super) fn child_terminal(&mut self, p: u32) {
        let n = self.node_mut(p);
        n.pending = n.pending.saturating_sub(1);
        self.settle(p);
    }

    /// Marks `id` complete when it can be and lets its parent count it.
    pub(super) fn settle(&mut self, id: u32) {
        let mut cur = id;
        loop {
            let n = self.node(cur);
            if !n.has(flag::OWN) || n.pending != 0 || n.has(flag::COMPLETE) {
                return;
            }
            let confirmed = n.has(flag::CONFIRMED);
            let parent = n.parent;
            self.node_mut(cur).flags |= flag::COMPLETE;
            if confirmed && self.unconfirmed > 0 {
                self.sweep_unconfirmed(cur);
            }
            if parent == NONE {
                return;
            }
            let p = self.node_mut(parent);
            p.pending = p.pending.saturating_sub(1);
            cur = parent;
        }
    }

    // ----- end of a scan -----

    /// The source sent `Finished`. A truncated scan keeps what it has (incomplete nodes read `at least`
    /// and unconfirmed painted nodes stay); a whole one drops unconfirmed painted nodes and anything
    /// still parked, then releases the walker-id maps and the name index.
    pub fn finish(&mut self, truncated: bool) {
        self.finished = true;
        self.truncated = truncated;
        self.orphans_dropped += self.parked_total as u64;
        self.parked.clear();
        self.parked_total = 0;
        if !truncated && self.unconfirmed > 0 {
            self.purge_unconfirmed();
        }
        self.index.clear();
        self.live_map.clear();
        self.paint_map.clear();
        self.nodes.shrink_to_fit();
        self.names.shrink();
        self.free.shrink_to_fit();
    }
}
