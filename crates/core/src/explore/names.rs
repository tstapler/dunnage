//! The name arena and the walker-id map of the explorer's tree (Task 6.2.1b).

/// Every directory name lives here, back to back; a node holds an offset and a length, never a `PathBuf`.
#[derive(Debug, Default)]
pub struct NameArena {
    bytes: Vec<u8>,
}

impl NameArena {
    /// Copies `name` in (cut at `u16::MAX` bytes) and returns its offset and length. Grows by a quarter, so
    /// the reserved capacity stays within a quarter of what is used.
    pub fn push(&mut self, name: &[u8]) -> (u32, u16) {
        let name = &name[..name.len().min(usize::from(u16::MAX))];
        if self.bytes.capacity() - self.bytes.len() < name.len() {
            let extra = (self.bytes.len() / 4).max(4096).max(name.len());
            self.bytes.reserve_exact(extra);
        }
        let off = u32::try_from(self.bytes.len()).unwrap_or(u32::MAX);
        self.bytes.extend_from_slice(name);
        (off, name.len() as u16)
    }

    pub fn get(&self, off: u32, len: u16) -> &[u8] {
        let start = off as usize;
        self.bytes
            .get(start..start + usize::from(len))
            .unwrap_or(&[])
    }

    /// Reserved bytes, not used ones.
    pub fn capacity(&self) -> usize {
        self.bytes.capacity()
    }

    pub fn shrink(&mut self) {
        self.bytes.shrink_to_fit();
    }
}

/// What a walker id stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    /// A kept node.
    Node(u32),
    /// A directory refused at the node limit; its bytes and its children's go to this kept ancestor.
    Folded(u32),
}

const PAGE_BITS: u32 = 12;
const PAGE: usize = 1 << PAGE_BITS;
const FOLD_BIT: u32 = 1 << 31;

/// Walker id to node, four bytes per directory. Pages are allocated on first touch, so a shuffled arrival
/// of dense ids costs what the ids need and a hostile id of 4 billion costs one page.
#[derive(Debug, Default)]
pub struct IdMap {
    pages: Vec<Option<Box<[u32]>>>,
    allocated: usize,
}

impl IdMap {
    pub fn get(&self, id: u64) -> Option<Entry> {
        let page = self.pages.get((id >> PAGE_BITS) as usize)?.as_ref()?;
        match page[(id as usize) & (PAGE - 1)] {
            0 => None,
            v if v & FOLD_BIT != 0 => Some(Entry::Folded((v & !FOLD_BIT) - 1)),
            v => Some(Entry::Node(v - 1)),
        }
    }

    pub fn set(&mut self, id: u64, e: Entry) {
        let pi = (id >> PAGE_BITS) as usize;
        if self.pages.len() <= pi {
            let extra = (pi + 1 - self.pages.len()).max(self.pages.len() / 8);
            self.pages.reserve_exact(extra);
            self.pages.resize_with(pi + 1, || None);
        }
        if self.pages[pi].is_none() {
            self.pages[pi] = Some(vec![0u32; PAGE].into_boxed_slice());
            self.allocated += 1;
        }
        let v = match e {
            Entry::Node(n) => n + 1,
            Entry::Folded(n) => (n + 1) | FOLD_BIT,
        };
        if let Some(page) = self.pages[pi].as_mut() {
            page[(id as usize) & (PAGE - 1)] = v;
        }
    }

    pub fn clear(&mut self) {
        *self = IdMap::default();
    }

    pub fn is_empty(&self) -> bool {
        self.allocated == 0
    }

    pub fn heap_bytes(&self) -> usize {
        self.allocated * PAGE * 4
            + self.pages.capacity() * std::mem::size_of::<Option<Box<[u32]>>>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_map_round_trips_and_is_sparse() {
        let mut m = IdMap::default();
        assert_eq!(m.get(7), None);
        m.set(7, Entry::Node(3));
        m.set(4_000_000_000, Entry::Folded(9));
        assert_eq!(m.get(7), Some(Entry::Node(3)));
        assert_eq!(m.get(4_000_000_000), Some(Entry::Folded(9)));
        assert_eq!(m.get(8), None);
        assert!(m.heap_bytes() < 64 * 1024 * 1024);
    }

    #[test]
    fn arena_returns_what_it_stored() {
        let mut a = NameArena::default();
        let (o1, l1) = a.push(b"abc");
        let (o2, l2) = a.push(b"de");
        assert_eq!(a.get(o1, l1), b"abc");
        assert_eq!(a.get(o2, l2), b"de");
    }
}
