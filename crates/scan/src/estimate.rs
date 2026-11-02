//! Per-directory btrfs metadata-cost model. See docs/design.md for derivation and limits.
//!
//! EXACT (from lstat): inode, file, dir, symlink and directory-entry counts, name bytes,
//! apparent size, allocated bytes. ESTIMATED: extent count (lower bound, no FIEMAP yet),
//! and every byte figure derived from the item sizes below.

use serde::Serialize;

/// Every btrfs tree item costs a 25-byte leaf item header (17-byte key + offset + size).
const ITEM_HEADER: u64 = 25;
const INODE_ITEM: u64 = 160;
/// `btrfs_inode_ref` fixed part; name follows.
const INODE_REF_FIXED: u64 = 10;
/// `btrfs_dir_item` fixed part; name follows. A link has one DIR_ITEM and one DIR_INDEX.
const DIR_ITEM_FIXED: u64 = 30;
/// Regular `btrfs_file_extent_item` in the fs tree.
const FILE_EXTENT_ITEM: u64 = 53;
/// Inline file extent header; file bytes follow inside the leaf.
const FILE_EXTENT_INLINE_HEADER: u64 = 21;
/// Extent-tree item: `btrfs_extent_item` (24) + one inline data-ref backref (29).
const EXTENT_TREE_ITEM: u64 = 24 + 29;
/// Largest uncompressed extent btrfs writes.
pub const MAX_EXTENT_BYTES: u64 = 128 << 20;
/// crc32c: 4 bytes per 4 KiB block.
const CSUM_BYTES_PER_KIB: u64 = 1; // 4 B / 4 KiB = 1 B per KiB

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

/// What one directory entry contributes; built from lstat by the scanner.
#[derive(Debug, Clone, Copy)]
pub struct EntryInfo {
    pub kind: Kind,
    pub size: u64,
    /// `st_blocks * 512`. Zero with nonzero size means btrfs inlined the data.
    pub allocated: u64,
    pub name_len: u64,
    /// False for second and later hard links to an already-counted inode.
    pub first_link: bool,
    /// False for the scan root, which has no parent entry inside the scan.
    pub counts_dir_entry: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Stats {
    pub inodes: u64,
    pub files: u64,
    pub dirs: u64,
    pub symlinks: u64,
    pub dir_entries: u64,
    pub name_bytes: u64,
    pub size_bytes: u64,
    pub allocated_bytes: u64,
    /// Estimated (lower bound): ceil(size / 128 MiB) per non-inline file.
    pub extents_est: u64,
    pub inline_files: u64,
    pub inline_bytes: u64,
}

/// Cost split so callers can show which tree the bytes land in.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct MetadataCost {
    /// Inode items, inode refs, dir items/indexes, file extent items, inline data.
    pub fs_tree_bytes: u64,
    /// Extent-tree items for non-inline extents.
    pub extent_tree_bytes: u64,
    /// Checksum-tree items (~size/1024). Separate tree, but still metadata space.
    pub csum_tree_bytes: u64,
}

impl MetadataCost {
    pub fn total(&self) -> u64 {
        self.fs_tree_bytes + self.extent_tree_bytes + self.csum_tree_bytes
    }
}

impl Stats {
    pub fn add_entry(&mut self, e: &EntryInfo) {
        if e.counts_dir_entry {
            self.dir_entries += 1;
            self.name_bytes += e.name_len;
        }
        if !e.first_link {
            return;
        }
        self.inodes += 1;
        match e.kind {
            Kind::Dir => self.dirs += 1,
            Kind::Symlink => {
                self.symlinks += 1;
                // Symlink targets are stored as inline extents; lstat size is the target length.
                self.inline_files += 1;
                self.inline_bytes += e.size;
            }
            Kind::File => {
                self.files += 1;
                if e.size > 0 && e.allocated == 0 {
                    self.inline_files += 1;
                    self.inline_bytes += e.size;
                } else if e.size > 0 {
                    self.extents_est += e.size.div_ceil(MAX_EXTENT_BYTES);
                }
            }
            Kind::Other => {}
        }
        if e.kind == Kind::File {
            self.size_bytes += e.size;
            self.allocated_bytes += e.allocated;
        }
    }

    pub fn merge(&mut self, o: &Stats) {
        self.inodes += o.inodes;
        self.files += o.files;
        self.dirs += o.dirs;
        self.symlinks += o.symlinks;
        self.dir_entries += o.dir_entries;
        self.name_bytes += o.name_bytes;
        self.size_bytes += o.size_bytes;
        self.allocated_bytes += o.allocated_bytes;
        self.extents_est += o.extents_est;
        self.inline_files += o.inline_files;
        self.inline_bytes += o.inline_bytes;
    }

    /// Logical metadata bytes; on a DUP metadata profile the on-disk cost is double.
    /// Excludes xattrs/ACLs and leaf-fill slack (nodes are not packed 100%).
    pub fn metadata_cost(&self) -> MetadataCost {
        let inode_items = self.inodes * (ITEM_HEADER + INODE_ITEM);
        // Per link: INODE_REF + DIR_ITEM + DIR_INDEX (three items; the name is stored in each).
        let link_items = self.dir_entries
            * (3 * ITEM_HEADER + INODE_REF_FIXED + 2 * DIR_ITEM_FIXED)
            + 3 * self.name_bytes;
        let regular_extents = self.extents_est * (ITEM_HEADER + FILE_EXTENT_ITEM);
        let inline =
            self.inline_files * (ITEM_HEADER + FILE_EXTENT_INLINE_HEADER) + self.inline_bytes;
        MetadataCost {
            fs_tree_bytes: inode_items + link_items + regular_extents + inline,
            extent_tree_bytes: self.extents_est * (ITEM_HEADER + EXTENT_TREE_ITEM),
            csum_tree_bytes: self.allocated_bytes / 1024 * CSUM_BYTES_PER_KIB,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: Kind, size: u64, allocated: u64) -> EntryInfo {
        EntryInfo {
            kind,
            size,
            allocated,
            name_len: 4,
            first_link: true,
            counts_dir_entry: true,
        }
    }

    #[test]
    fn regular_file_counts_one_extent_and_one_entry() {
        let mut s = Stats::default();
        s.add_entry(&entry(Kind::File, 10_000, 12_288));
        assert_eq!(
            (s.inodes, s.files, s.extents_est, s.dir_entries),
            (1, 1, 1, 1)
        );
        assert_eq!(s.inline_files, 0);
    }

    #[test]
    fn huge_file_needs_multiple_extents() {
        let mut s = Stats::default();
        s.add_entry(&entry(
            Kind::File,
            3 * MAX_EXTENT_BYTES + 1,
            3 * MAX_EXTENT_BYTES + 4096,
        ));
        assert_eq!(s.extents_est, 4);
    }

    #[test]
    fn zero_allocated_nonempty_file_is_inline() {
        let mut s = Stats::default();
        s.add_entry(&entry(Kind::File, 300, 0));
        assert_eq!((s.inline_files, s.inline_bytes, s.extents_est), (1, 300, 0));
    }

    #[test]
    fn empty_file_has_no_extent() {
        let mut s = Stats::default();
        s.add_entry(&entry(Kind::File, 0, 0));
        assert_eq!((s.inodes, s.extents_est, s.inline_files), (1, 0, 0));
    }

    #[test]
    fn extra_hard_link_adds_only_a_directory_entry() {
        let mut s = Stats::default();
        s.add_entry(&entry(Kind::File, 10_000, 12_288));
        let mut link = entry(Kind::File, 10_000, 12_288);
        link.first_link = false;
        s.add_entry(&link);
        assert_eq!((s.inodes, s.dir_entries, s.extents_est), (1, 2, 1));
        assert_eq!(s.size_bytes, 10_000);
    }

    #[test]
    fn scan_root_has_no_parent_entry() {
        let mut s = Stats::default();
        let mut root = entry(Kind::Dir, 4096, 0);
        root.counts_dir_entry = false;
        s.add_entry(&root);
        assert_eq!(
            (s.inodes, s.dirs, s.dir_entries, s.name_bytes),
            (1, 1, 0, 0)
        );
    }

    #[test]
    fn symlink_is_inode_plus_inline_target() {
        let mut s = Stats::default();
        s.add_entry(&entry(Kind::Symlink, 12, 0));
        assert_eq!((s.inodes, s.symlinks, s.inline_bytes), (1, 1, 12));
    }

    #[test]
    fn cost_of_one_small_file_matches_hand_calculation() {
        let mut s = Stats::default();
        s.add_entry(&entry(Kind::File, 4096, 4096));
        let c = s.metadata_cost();
        // inode 185 + links (75+10+60+12=157) + file extent 78 = 420
        assert_eq!(c.fs_tree_bytes, 185 + 157 + 78);
        assert_eq!(c.extent_tree_bytes, 78);
        assert_eq!(c.csum_tree_bytes, 4);
        assert_eq!(c.total(), 420 + 78 + 4);
    }

    #[test]
    fn merge_is_additive() {
        let mut a = Stats::default();
        a.add_entry(&entry(Kind::File, 100, 4096));
        let mut b = Stats::default();
        b.add_entry(&entry(Kind::Dir, 4096, 0));
        let mut m = a;
        m.merge(&b);
        assert_eq!((m.inodes, m.files, m.dirs), (2, 1, 1));
    }
}
