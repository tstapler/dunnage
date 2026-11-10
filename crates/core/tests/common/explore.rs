#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]
//! Synthetic trees, event streams and a reference fold for the explorer reducer tests.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;

use disk_cleanup_core::explore::msg::{NodeEvent, ScanMode, NO_PARENT, PAINT_ID_BASE};
use disk_cleanup_core::explore::tree::ExploreTree;
use disk_cleanup_scan::estimate::Stats;
use disk_cleanup_scan::index::DirInfo;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum K {
    Dir,
    Unreadable,
    Boundary,
    Gone,
}

#[derive(Debug, Clone)]
pub struct Spec {
    pub parent: Option<usize>,
    pub name: String,
    pub kind: K,
    pub alloc: u64,
}

pub struct Rng(pub u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

pub fn stats_of(alloc: u64) -> Stats {
    Stats {
        inodes: 1,
        dirs: 1,
        allocated_bytes: alloc,
        size_bytes: alloc,
        ..Stats::default()
    }
}

/// The comparable part of a total.
pub fn key(s: &Stats) -> (u64, u64, u64) {
    (s.allocated_bytes, s.inodes, s.dirs)
}

pub fn random_tree(seed: u64, max_depth: usize, max_fan: usize, n_max: usize) -> Vec<Spec> {
    let mut rng = Rng::new(seed);
    let mut specs = vec![Spec {
        parent: None,
        name: "/r".into(),
        kind: K::Dir,
        alloc: 4096,
    }];
    let mut depth = vec![0usize];
    let mut i = 0;
    while i < specs.len() {
        if specs[i].kind == K::Dir && depth[i] < max_depth {
            let fan = rng.below(max_fan + 1);
            for _ in 0..fan {
                if specs.len() >= n_max {
                    break;
                }
                let kind = match rng.below(20) {
                    0 => K::Unreadable,
                    1 => K::Boundary,
                    2 => K::Gone,
                    _ => K::Dir,
                };
                let n = specs.len();
                specs.push(Spec {
                    parent: Some(i),
                    name: format!("d{n}"),
                    kind,
                    alloc: 4096 * (1 + rng.below(50) as u64),
                });
                depth.push(depth[i] + 1);
            }
        }
        i += 1;
    }
    specs
}

pub fn children_count(specs: &[Spec]) -> Vec<u32> {
    let mut c = vec![0u32; specs.len()];
    for s in specs {
        if let Some(p) = s.parent {
            c[p] += 1;
        }
    }
    c
}

/// One event per spec; ids are `base + index`.
pub fn events(specs: &[Spec], base: u64, painted: bool) -> Vec<NodeEvent> {
    let kids = children_count(specs);
    specs
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let id = base + i as u64;
            let parent = s.parent.map_or(NO_PARENT, |p| base + p as u64);
            let name = OsString::from(&s.name);
            match s.kind {
                K::Dir => NodeEvent::Dir {
                    id,
                    parent,
                    name,
                    dev: 1,
                    info: DirInfo {
                        stats: stats_of(s.alloc),
                        mtime: i as i64,
                        ..DirInfo::default()
                    },
                    subdirs: kids[i],
                    from_cache: painted,
                    painted,
                },
                K::Unreadable => NodeEvent::Unreadable {
                    id,
                    parent,
                    name,
                    errno: 13,
                },
                K::Boundary => NodeEvent::Boundary {
                    id,
                    parent,
                    name,
                    dev: 2,
                },
                K::Gone => NodeEvent::Gone { id, parent, name },
            }
        })
        .collect()
}

pub fn paint_events(specs: &[Spec]) -> Vec<NodeEvent> {
    events(specs, PAINT_ID_BASE, true)
        .into_iter()
        .filter(|e| {
            !matches!(
                e,
                NodeEvent::Gone { .. } | NodeEvent::Boundary { .. } | NodeEvent::Unreadable { .. }
            )
        })
        .collect()
}

pub fn shuffle<T>(v: &mut [T], seed: u64) {
    let mut rng = Rng::new(seed);
    for i in (1..v.len()).rev() {
        let j = rng.below(i + 1);
        v.swap(i, j);
    }
}

pub fn batches<T>(v: Vec<T>, seed: u64, max: usize) -> Vec<Vec<T>> {
    let mut rng = Rng::new(seed ^ 0x00ab_cdef);
    let mut out = Vec::new();
    let mut cur = Vec::new();
    let mut limit = 1 + rng.below(max);
    for e in v {
        cur.push(e);
        if cur.len() >= limit {
            out.push(std::mem::take(&mut cur));
            limit = 1 + rng.below(max);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

pub fn path_of_spec(specs: &[Spec], mut i: usize) -> PathBuf {
    let mut parts = vec![specs[i].name.clone()];
    while let Some(p) = specs[i].parent {
        parts.push(specs[p].name.clone());
        i = p;
    }
    let mut path = PathBuf::new();
    for p in parts.iter().rev() {
        path.push(p);
    }
    path
}

/// Reference fold: total of every kept (Dir, Unreadable, Boundary) node by path.
pub fn reference(specs: &[Spec]) -> HashMap<PathBuf, (u64, u64, u64)> {
    let mut total: Vec<(u64, u64, u64)> = specs
        .iter()
        .map(|s| {
            if s.kind == K::Dir {
                key(&stats_of(s.alloc))
            } else {
                (0, 0, 0)
            }
        })
        .collect();
    for i in (1..specs.len()).rev() {
        if let Some(p) = specs[i].parent {
            let t = total[i];
            total[p].0 += t.0;
            total[p].1 += t.1;
            total[p].2 += t.2;
        }
    }
    specs
        .iter()
        .enumerate()
        .filter(|(_, s)| s.kind != K::Gone)
        .map(|(i, _)| (path_of_spec(specs, i), total[i]))
        .collect()
}

pub fn by_path(t: &ExploreTree) -> HashMap<PathBuf, (u64, u64, u64)> {
    t.iter().map(|n| (t.path_of(n), key(t.total(n)))).collect()
}

pub fn reduce(t: &mut ExploreTree, mode: ScanMode, generation: u32, bs: Vec<Vec<NodeEvent>>) {
    for b in bs {
        t.apply_batch(generation, mode, b).unwrap();
    }
}

pub fn fresh(specs: &[Spec]) -> ExploreTree {
    let mut t = ExploreTree::new();
    t.apply_batch(0, ScanMode::Fresh, events(specs, 0, false))
        .unwrap();
    t.finish(false);
    t
}

/// Shuffles inside windows only: the order a threaded breadth-first walk produces (parents mostly first).
pub fn shuffle_windowed<T>(v: &mut [T], seed: u64, window: usize) {
    for (i, w) in v.chunks_mut(window).enumerate() {
        shuffle(w, seed.wrapping_add(i as u64));
    }
}
