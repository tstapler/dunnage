//! Privileged backend: stream a subvolume's filesystem tree with `BTRFS_IOC_TREE_SEARCH_V2`.
//!
//! Needs `CAP_SYS_ADMIN`. Read-only: the ioctl only searches. Items arrive sorted by
//! `(objectid, type, offset)`, so one inode's items are contiguous and ordered
//! INODE_ITEM(1) < INODE_REF(12) < INODE_EXTREF(13) < XATTR(24) < DIR_ITEM(84) < DIR_INDEX(96)
//! < EXTENT_DATA(108). That order is what makes a single pass sufficient: by the time an
//! inode's extents are read, its mode and parent are already known.
//!
//! Counts that are exact here but estimated by the walker: extents, inline data, allocated
//! bytes (`nbytes`), and hard-link counts. On-disk layouts follow `linux/btrfs_tree.h`;
//! the offsets below are the ones decoded in `parse_*` and covered by synthetic-buffer tests.

use crate::btrfs::{btrfs_mount_for_path, Mount};
use crate::estimate::{EntryInfo, Kind};
use crate::index::{Backend, CollectRequest, Collected, DirInfo};
use crate::{Error, Result};
use std::collections::HashMap;
use std::ffi::OsString;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

const ROOT_DIR_OBJECTID: u64 = 256;
const INODE_ITEM_KEY: u8 = 1;
const INODE_REF_KEY: u8 = 12;
const INODE_EXTREF_KEY: u8 = 13;
const EXTENT_DATA_KEY: u8 = 108;

const SEARCH_KEY_SIZE: usize = 104;
const ARGS_HEADER_SIZE: usize = SEARCH_KEY_SIZE + 8;
const ITEM_HEADER_SIZE: usize = 32;
const INITIAL_BUF: usize = 4 << 20;
const MAX_BUF: usize = 16 << 20;

/// `_IOWR(0x94, 17, struct btrfs_ioctl_search_args_v2)`; the struct is 112 bytes.
const IOC_TREE_SEARCH_V2: libc::c_ulong =
    (3 << 30) | ((ARGS_HEADER_SIZE as libc::c_ulong) << 16) | (0x94 << 8) | 17;

const S_IFMT: u32 = 0o170_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFREG: u32 = 0o100_000;
const S_IFLNK: u32 = 0o120_000;

fn le_u64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}
fn le_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}
fn le_u16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemKey {
    pub objectid: u64,
    pub typ: u8,
    pub offset: u64,
}

pub struct Item<'a> {
    pub key: ItemKey,
    pub data: &'a [u8],
}

/// Splits a search result buffer into items. Stops at the first truncated header or body.
pub fn parse_items(buf: &[u8], count: u32) -> Vec<Item<'_>> {
    let mut items = Vec::with_capacity(count as usize);
    let mut pos = 0;
    for _ in 0..count {
        let (Some(objectid), Some(offset), Some(typ), Some(len)) = (
            le_u64(buf, pos + 8),
            le_u64(buf, pos + 16),
            le_u32(buf, pos + 24),
            le_u32(buf, pos + 28),
        ) else {
            break;
        };
        let start = pos + ITEM_HEADER_SIZE;
        let Some(data) = buf.get(start..start + len as usize) else {
            break;
        };
        items.push(Item {
            key: ItemKey {
                objectid,
                typ: typ as u8,
                offset,
            },
            data,
        });
        pos = start + len as usize;
    }
    items
}

/// Fields of `struct btrfs_inode_item` we use.
struct InodeItem {
    size: u64,
    nbytes: u64,
    mode: u32,
    mtime_sec: u64,
}

fn parse_inode_item(d: &[u8]) -> Option<InodeItem> {
    Some(InodeItem {
        size: le_u64(d, 16)?,
        nbytes: le_u64(d, 24)?,
        mode: le_u32(d, 52)?,
        mtime_sec: le_u64(d, 136)?,
    })
}

/// `INODE_REF` item: repeated `{index u64, name_len u16, name}`; the key offset is the parent.
fn parse_inode_refs(parent: u64, d: &[u8]) -> Vec<(u64, Vec<u8>)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(n) = le_u16(d, pos + 8) {
        let start = pos + 10;
        let Some(name) = d.get(start..start + n as usize) else {
            break;
        };
        out.push((parent, name.to_vec()));
        pos = start + n as usize;
    }
    out
}

/// `INODE_EXTREF` item: repeated `{parent u64, index u64, name_len u16, name}`.
fn parse_inode_extrefs(d: &[u8]) -> Vec<(u64, Vec<u8>)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let (Some(parent), Some(n)) = (le_u64(d, pos), le_u16(d, pos + 16)) {
        let start = pos + 18;
        let Some(name) = d.get(start..start + n as usize) else {
            break;
        };
        out.push((parent, name.to_vec()));
        pos = start + n as usize;
    }
    out
}

#[derive(Default)]
struct Cur {
    ino: u64,
    inode: Option<InodeItem>,
    refs: Vec<(u64, Vec<u8>)>,
    extents: u64,
    inline_len: Option<u64>,
}

/// Single-pass aggregator over tree items in key order.
#[derive(Default)]
pub struct Aggregator {
    cur: Cur,
    /// Directory inode -> its counters.
    acc: HashMap<u64, DirInfo>,
    /// Directory inode -> (parent inode, name), for path reconstruction.
    names: HashMap<u64, (u64, Vec<u8>)>,
    pub items_seen: u64,
}

impl Aggregator {
    pub fn feed(&mut self, item: &Item<'_>) {
        self.items_seen += 1;
        let k = item.key;
        if k.objectid < ROOT_DIR_OBJECTID {
            return;
        }
        if k.objectid != self.cur.ino {
            self.flush();
            self.cur.ino = k.objectid;
        }
        match k.typ {
            INODE_ITEM_KEY => self.cur.inode = parse_inode_item(item.data),
            INODE_REF_KEY => self.cur.refs.extend(parse_inode_refs(k.offset, item.data)),
            INODE_EXTREF_KEY => self.cur.refs.extend(parse_inode_extrefs(item.data)),
            EXTENT_DATA_KEY => self.feed_extent(item.data),
            _ => {}
        }
    }

    /// `btrfs_file_extent_item`: type byte at 20 (0 inline, 1 regular, 2 prealloc); regular
    /// extents carry `disk_bytenr` at 21, zero for a hole.
    fn feed_extent(&mut self, d: &[u8]) {
        match d.get(20) {
            Some(0) => {
                let n = (d.len() as u64).saturating_sub(21);
                self.cur.inline_len = Some(self.cur.inline_len.unwrap_or(0) + n);
            }
            Some(1 | 2) if le_u64(d, 21).is_some_and(|b| b != 0) => self.cur.extents += 1,
            _ => {}
        }
    }

    fn flush(&mut self) {
        let cur = std::mem::take(&mut self.cur);
        let Some(inode) = cur.inode else { return };
        let kind = match inode.mode & S_IFMT {
            S_IFDIR => Kind::Dir,
            S_IFREG => Kind::File,
            S_IFLNK => Kind::Symlink,
            _ => Kind::Other,
        };
        let is_root = cur.ino == ROOT_DIR_OBJECTID;
        // The root directory refers to itself; treat it as having no parent entry.
        let refs: Vec<&(u64, Vec<u8>)> = if is_root {
            Vec::new()
        } else {
            cur.refs.iter().collect()
        };
        let inline = cur.inline_len.unwrap_or(0);
        let make = |name_len: usize, first: bool, counts: bool| EntryInfo {
            kind,
            size: inode.size,
            allocated: inode.nbytes.saturating_sub(inline),
            name_len: name_len as u64,
            first_link: first,
            counts_dir_entry: counts,
        };

        if kind == Kind::Dir {
            let info = self.acc.entry(cur.ino).or_default();
            info.mtime = inode.mtime_sec as i64;
            let (name_len, counts) = refs.first().map_or((0, false), |r| (r.1.len(), true));
            info.stats
                .add_entry_exact(&make(name_len, true, counts), 0, None);
            if let Some((parent, name)) = refs.first() {
                self.names.insert(cur.ino, (*parent, name.clone()));
            }
            return;
        }
        for (i, (parent, name)) in refs.into_iter().enumerate() {
            let info = self.acc.entry(*parent).or_default();
            info.note_file_name(name, kind == Kind::File);
            let inline_opt = (i == 0).then_some(cur.inline_len).flatten();
            let extents = if i == 0 { cur.extents } else { 0 };
            info.stats
                .add_entry_exact(&make(name.len(), i == 0, true), extents, inline_opt);
        }
    }

    /// Resolves directory paths under `mount_point` and returns the per-directory map.
    pub fn finish(mut self, mount_point: &Path) -> Vec<(PathBuf, DirInfo)> {
        self.flush();
        let mut memo: HashMap<u64, Option<PathBuf>> = HashMap::new();
        memo.insert(ROOT_DIR_OBJECTID, Some(mount_point.to_path_buf()));
        let inos: Vec<u64> = self.acc.keys().copied().collect();
        let mut out = Vec::with_capacity(inos.len());
        for ino in inos {
            if let Some(path) = resolve(ino, &self.names, &mut memo) {
                if let Some(info) = self.acc.remove(&ino) {
                    out.push((path, info));
                }
            }
        }
        out
    }
}

/// Walks parent links up to the subvolume root, memoising every directory on the way.
/// Returns `None` for orphans and cycles.
fn resolve(
    ino: u64,
    names: &HashMap<u64, (u64, Vec<u8>)>,
    memo: &mut HashMap<u64, Option<PathBuf>>,
) -> Option<PathBuf> {
    let mut chain = Vec::new();
    let mut cur = ino;
    let base = loop {
        if let Some(p) = memo.get(&cur) {
            break p.clone();
        }
        if chain.len() > 4096 {
            break None;
        }
        let Some((parent, _)) = names.get(&cur) else {
            break None;
        };
        chain.push(cur);
        cur = *parent;
    };
    let mut path = base;
    for &i in chain.iter().rev() {
        path = path.map(|p| p.join(OsString::from_vec(names[&i].1.clone())));
        memo.insert(i, path.clone());
    }
    path
}

#[repr(C)]
struct SearchKey {
    tree_id: u64,
    min_objectid: u64,
    max_objectid: u64,
    min_offset: u64,
    max_offset: u64,
    min_transid: u64,
    max_transid: u64,
    min_type: u32,
    max_type: u32,
    nr_items: u32,
    unused: u32,
    unused1: [u64; 4],
}

fn write_header(buf: &mut [u8], key: &SearchKey, buf_size: u64) {
    // SAFETY: SearchKey is repr(C) plain data of exactly SEARCH_KEY_SIZE bytes.
    let raw = unsafe {
        std::slice::from_raw_parts((key as *const SearchKey).cast::<u8>(), SEARCH_KEY_SIZE)
    };
    buf[..SEARCH_KEY_SIZE].copy_from_slice(raw);
    buf[SEARCH_KEY_SIZE..ARGS_HEADER_SIZE].copy_from_slice(&buf_size.to_ne_bytes());
}

fn io_err(path: &Path, source: std::io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Smallest key strictly after `k`, or `None` at the end of the key space.
fn next_key(k: ItemKey) -> Option<ItemKey> {
    if k.offset < u64::MAX {
        return Some(ItemKey {
            offset: k.offset + 1,
            ..k
        });
    }
    if k.typ < u8::MAX {
        return Some(ItemKey {
            typ: k.typ + 1,
            offset: 0,
            ..k
        });
    }
    k.objectid.checked_add(1).map(|o| ItemKey {
        objectid: o,
        typ: 0,
        offset: 0,
    })
}

/// Streams every item of `tree_id` reachable from `fd`, calling `sink` per search batch.
fn search_tree(
    fd: &impl AsRawFd,
    tree_id: u64,
    mount: &Path,
    deadline: Option<std::time::Instant>,
    mut sink: impl FnMut(&[Item<'_>]),
) -> Result<bool> {
    let mut buf_len = INITIAL_BUF;
    let mut buf = vec![0u8; ARGS_HEADER_SIZE + buf_len];
    let mut start = ItemKey {
        objectid: ROOT_DIR_OBJECTID,
        typ: 0,
        offset: 0,
    };
    loop {
        if deadline.is_some_and(|d| std::time::Instant::now() > d) {
            return Ok(true);
        }
        let key = SearchKey {
            tree_id,
            min_objectid: start.objectid,
            max_objectid: u64::MAX,
            min_offset: start.offset,
            max_offset: u64::MAX,
            min_transid: 0,
            max_transid: u64::MAX,
            min_type: u32::from(start.typ),
            max_type: u32::from(u8::MAX),
            nr_items: u32::MAX,
            unused: 0,
            unused1: [0; 4],
        };
        // Modern kernels compare whole keys against (min, max), so resuming at `start` is exact.
        write_header(&mut buf, &key, buf_len as u64);
        // SAFETY: buf is ARGS_HEADER_SIZE + buf_len bytes, matching the buf_size we declared.
        let rc = unsafe { libc::ioctl(fd.as_raw_fd(), IOC_TREE_SEARCH_V2, buf.as_mut_ptr()) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EOVERFLOW) && buf_len < MAX_BUF {
                buf_len *= 2;
                buf.resize(ARGS_HEADER_SIZE + buf_len, 0);
                continue;
            }
            if err.raw_os_error() == Some(libc::EPERM) {
                return Err(Error::Other(format!(
                    "BTRFS_IOC_TREE_SEARCH_V2 on {} was refused (EPERM): it needs CAP_SYS_ADMIN, run as root or use --backend walker",
                    mount.display()
                )));
            }
            return Err(io_err(mount, err));
        }
        let count = u32::from_ne_bytes([buf[72], buf[73], buf[74], buf[75]]);
        if count == 0 {
            return Ok(false);
        }
        let items = parse_items(&buf[ARGS_HEADER_SIZE..], count);
        let Some(last) = items.last().map(|i| i.key) else {
            return Ok(false);
        };
        sink(&items);
        match next_key(last) {
            Some(n) => start = n,
            None => return Ok(false),
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct BtrfsTreeBackend;

impl Backend for BtrfsTreeBackend {
    fn name(&self) -> &'static str {
        "btrfs-tree"
    }

    fn extents_exact(&self) -> bool {
        true
    }

    fn collect(&self, req: &CollectRequest<'_>) -> Result<Collected> {
        let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
            .map_err(|e| io_err(Path::new("/proc/self/mountinfo"), e))?;
        let mut by_mount: HashMap<PathBuf, (Mount, Vec<&PathBuf>)> = HashMap::new();
        for root in req.roots {
            let m = btrfs_mount_for_path(&mountinfo, root).ok_or_else(|| {
                Error::Other(format!("{} is not on a btrfs mount", root.display()))
            })?;
            by_mount
                .entry(m.mount_point.clone())
                .or_insert((m, Vec::new()))
                .1
                .push(root);
        }
        let mut collected = Collected::default();
        for (mount, roots) in by_mount.values() {
            let tree_id = mount.subvolid.ok_or_else(|| {
                Error::Other(format!(
                    "no subvolid= option for {}",
                    mount.mount_point.display()
                ))
            })?;
            let fd = std::fs::File::open(&mount.mount_point)
                .map_err(|e| io_err(&mount.mount_point, e))?;
            let mut agg = Aggregator::default();
            let truncated = search_tree(&fd, tree_id, &mount.mount_point, req.deadline, |items| {
                for it in items {
                    agg.feed(it);
                }
            })?;
            collected.truncated |= truncated;
            collected.notes.push(format!(
                "tree {tree_id} of {}: {} items",
                mount.mount_point.display(),
                agg.items_seen
            ));
            let dirs = agg.finish(&mount.mount_point);
            collected.dirs_listed += dirs.len() as u64;
            collected.dirs.extend(
                dirs.into_iter()
                    .filter(|(p, _)| roots.iter().any(|r| p.starts_with(r))),
            );
        }
        Ok(collected)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn inode(mode: u32, size: u64, nbytes: u64) -> Vec<u8> {
        let mut d = vec![0u8; 160];
        d[16..24].copy_from_slice(&size.to_le_bytes());
        d[24..32].copy_from_slice(&nbytes.to_le_bytes());
        d[52..56].copy_from_slice(&mode.to_le_bytes());
        d[136..144].copy_from_slice(&1_700_000_000u64.to_le_bytes());
        d
    }

    fn inode_ref(name: &str) -> Vec<u8> {
        let mut d = vec![0u8; 8];
        d.extend_from_slice(&(name.len() as u16).to_le_bytes());
        d.extend_from_slice(name.as_bytes());
        d
    }

    fn extent(typ: u8, disk_bytenr: u64, inline_len: usize) -> Vec<u8> {
        let mut d = vec![0u8; 21];
        d[20] = typ;
        if typ == 0 {
            d.extend(vec![b'x'; inline_len]);
        } else {
            d.extend_from_slice(&disk_bytenr.to_le_bytes());
            d.extend_from_slice(&[0u8; 24]);
        }
        d
    }

    fn encode(items: &[(u64, u8, u64, Vec<u8>)]) -> Vec<u8> {
        let mut buf = Vec::new();
        for (objectid, typ, offset, data) in items {
            buf.extend_from_slice(&1u64.to_le_bytes());
            buf.extend_from_slice(&objectid.to_le_bytes());
            buf.extend_from_slice(&offset.to_le_bytes());
            buf.extend_from_slice(&u32::from(*typ).to_le_bytes());
            buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
            buf.extend_from_slice(data);
        }
        buf
    }

    /// /repo (257) { package.json (258, 1 inline extent), node_modules (259) { a.js (260, 2 extents) },
    /// link (261, hard link also named in node_modules) }
    fn sample() -> Vec<u8> {
        encode(&[
            (256, 1, 0, inode(0o040_755, 0, 0)),
            (256, 12, 256, inode_ref("..")),
            (257, 1, 0, inode(0o040_755, 0, 0)),
            (257, 12, 256, inode_ref("repo")),
            (258, 1, 0, inode(0o100_644, 2, 2)),
            (258, 12, 257, inode_ref("package.json")),
            (258, 108, 0, extent(0, 0, 2)),
            (259, 1, 0, inode(0o040_755, 0, 0)),
            (259, 12, 257, inode_ref("node_modules")),
            (260, 1, 0, inode(0o100_644, 8192, 8192)),
            (260, 12, 259, inode_ref("a.js")),
            (260, 108, 0, extent(1, 4096, 0)),
            (260, 108, 4096, extent(1, 8192, 0)),
            (260, 108, 8192, extent(1, 0, 0)), // hole: not an extent
            (261, 1, 0, inode(0o100_644, 0, 0)),
            (261, 12, 257, inode_ref("l1")),
            (261, 12, 259, inode_ref("l2")),
        ])
    }

    #[test]
    fn parses_item_framing() {
        let buf = sample();
        let items = parse_items(&buf, 17);
        assert_eq!(items.len(), 17);
        assert_eq!(
            items[3].key,
            ItemKey {
                objectid: 257,
                typ: 12,
                offset: 256
            }
        );
        // A truncated buffer yields only whole items.
        assert_eq!(parse_items(&buf[..buf.len() - 1], 17).len(), 16);
    }

    #[test]
    fn aggregates_exact_counts_and_paths() {
        let buf = sample();
        let mut agg = Aggregator::default();
        for it in parse_items(&buf, 17) {
            agg.feed(&it);
        }
        let dirs: HashMap<PathBuf, DirInfo> = agg.finish(Path::new("/mnt")).into_iter().collect();
        let repo = &dirs[Path::new("/mnt/repo")];
        assert!(repo.has_package_json);
        // repo dir itself + package.json + l1 (first link of inode 261)
        assert_eq!(repo.stats.inodes, 3);
        assert_eq!(repo.stats.inline_bytes, 2);
        let nm = &dirs[Path::new("/mnt/repo/node_modules")];
        // node_modules dir + a.js; l2 adds a dir entry but no inode
        assert_eq!(nm.stats.inodes, 2);
        assert_eq!(nm.stats.extents, 2);
        assert_eq!(nm.stats.dir_entries, 3);
        assert_eq!(dirs[Path::new("/mnt")].stats.dir_entries, 0);
        assert_eq!(nm.mtime, 1_700_000_000);
    }

    #[test]
    fn orphans_are_dropped() {
        let buf = encode(&[
            (256, 1, 0, inode(0o040_755, 0, 0)),
            (300, 1, 0, inode(0o040_755, 0, 0)),
            (300, 12, 999, inode_ref("lost")),
        ]);
        let mut agg = Aggregator::default();
        for it in parse_items(&buf, 3) {
            agg.feed(&it);
        }
        let dirs = agg.finish(Path::new("/m"));
        assert_eq!(dirs.len(), 1);
    }

    #[test]
    fn ioctl_number_matches_kernel_header() {
        // _IOWR(0x94, 17, 112-byte struct)
        assert_eq!(IOC_TREE_SEARCH_V2, 0xC070_9411);
    }

    #[test]
    fn key_successor_carries() {
        let k = ItemKey {
            objectid: 5,
            typ: 108,
            offset: u64::MAX,
        };
        assert_eq!(
            next_key(k),
            Some(ItemKey {
                objectid: 5,
                typ: 109,
                offset: 0
            })
        );
        let last = ItemKey {
            objectid: 5,
            typ: u8::MAX,
            offset: u64::MAX,
        };
        assert_eq!(
            next_key(last),
            Some(ItemKey {
                objectid: 6,
                typ: 0,
                offset: 0
            })
        );
        let end = ItemKey {
            objectid: u64::MAX,
            typ: u8::MAX,
            offset: u64::MAX,
        };
        assert_eq!(next_key(end), None);
    }
}
