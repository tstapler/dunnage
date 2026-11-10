//! On-disk incremental cache for the walker backend.
//!
//! A directory's record is valid while its inode number and mtime are unchanged: creating,
//! removing or renaming an entry bumps the directory mtime, so counts, names and marker flags stay
//! exact. Editing a file in place does not, so cached `size`/`allocated`/`extents` can go stale;
//! records older than `max_age` are re-listed to bound that drift.

use crate::estimate::Stats;
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{self, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"MSCACHE1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rec {
    pub ino: u64,
    pub mtime_sec: i64,
    pub mtime_nsec: u32,
    /// Unix seconds when the directory was last listed.
    pub listed_at: u64,
    pub files: Stats,
    pub has_package_json: bool,
    pub has_cargo_toml: bool,
    pub git_file: bool,
    pub subdirs: Vec<Vec<u8>>,
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn varint(&mut self) -> Option<u64> {
        let (mut v, mut shift) = (0u64, 0);
        loop {
            let (&b, rest) = self.0.split_first()?;
            self.0 = rest;
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
            shift += 7;
            if shift > 63 {
                return None;
            }
        }
    }

    fn bytes(&mut self) -> Option<&[u8]> {
        let n = usize::try_from(self.varint()?).ok()?;
        if n > self.0.len() {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }
}

pub fn encode(map: &HashMap<PathBuf, Rec>) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    put_varint(&mut out, map.len() as u64);
    for (path, r) in map {
        put_bytes(&mut out, path.as_os_str().as_bytes());
        put_varint(&mut out, r.ino);
        put_varint(&mut out, r.mtime_sec as u64);
        put_varint(&mut out, u64::from(r.mtime_nsec));
        put_varint(&mut out, r.listed_at);
        for v in r.files.to_array() {
            put_varint(&mut out, v);
        }
        out.push(
            u8::from(r.has_package_json)
                | u8::from(r.has_cargo_toml) << 1
                | u8::from(r.git_file) << 2,
        );
        put_varint(&mut out, r.subdirs.len() as u64);
        for s in &r.subdirs {
            put_bytes(&mut out, s);
        }
    }
    out
}

/// Returns `None` for any corruption or version mismatch; the caller then rescans from scratch.
pub fn decode(bytes: &[u8]) -> Option<HashMap<PathBuf, Rec>> {
    let mut r = Reader(bytes.strip_prefix(MAGIC.as_slice())?);
    let n = usize::try_from(r.varint()?).ok()?;
    let mut map = HashMap::with_capacity(n.min(1 << 22));
    for _ in 0..n {
        let path = PathBuf::from(OsString::from_vec(r.bytes()?.to_vec()));
        let ino = r.varint()?;
        let mtime_sec = r.varint()? as i64;
        let mtime_nsec = u32::try_from(r.varint()?).ok()?;
        let listed_at = r.varint()?;
        let mut arr = [0u64; 11];
        for a in &mut arr {
            *a = r.varint()?;
        }
        let (&flags, rest) = r.0.split_first()?;
        r.0 = rest;
        let count = usize::try_from(r.varint()?).ok()?;
        let mut subdirs = Vec::with_capacity(count.min(1 << 16));
        for _ in 0..count {
            subdirs.push(r.bytes()?.to_vec());
        }
        map.insert(
            path,
            Rec {
                ino,
                mtime_sec,
                mtime_nsec,
                listed_at,
                files: Stats::from_array(arr),
                has_package_json: flags & 1 != 0,
                has_cargo_toml: flags & 2 != 0,
                git_file: flags & 4 != 0,
                subdirs,
            },
        );
    }
    Some(map)
}

pub fn load(path: &Path) -> HashMap<PathBuf, Rec> {
    std::fs::read(path)
        .ok()
        .and_then(|b| decode(&b))
        .unwrap_or_default()
}

/// Atomic replace (temp file + rename) so an interrupted run never leaves a half-written cache.
pub fn save(path: &Path, map: &HashMap<PathBuf, Rec>) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(&encode(map))?;
    f.sync_all()?;
    std::fs::rename(tmp, path)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn rec(ino: u64) -> Rec {
        let files = Stats {
            inodes: 3,
            size_bytes: 1 << 40,
            ..Stats::default()
        };
        Rec {
            ino,
            mtime_sec: -5,
            mtime_nsec: 999,
            listed_at: 1_800_000_000,
            files,
            has_package_json: true,
            has_cargo_toml: false,
            git_file: true,
            subdirs: vec![b"a".to_vec(), vec![0xff, 0x00, b'z']],
        }
    }

    #[test]
    fn roundtrip() {
        let mut m = HashMap::new();
        m.insert(PathBuf::from("/x/y"), rec(7));
        m.insert(PathBuf::from("/x/z w"), rec(u64::MAX));
        assert_eq!(decode(&encode(&m)).unwrap(), m);
    }

    #[test]
    fn rejects_corruption() {
        let mut m = HashMap::new();
        m.insert(PathBuf::from("/x"), rec(1));
        let bytes = encode(&m);
        assert!(decode(&bytes[..bytes.len() - 3]).is_none());
        assert!(decode(b"garbage").is_none());
        assert!(decode(b"").is_none());
    }

    #[test]
    fn save_and_load_through_a_file() {
        let dir =
            std::env::temp_dir().join(format!("disk-cleanup-scan-cache-{}", std::process::id()));
        let file = dir.join("c.bin");
        let mut m = HashMap::new();
        m.insert(PathBuf::from("/q"), rec(2));
        save(&file, &m).unwrap();
        assert_eq!(load(&file), m);
        assert!(load(&dir.join("missing")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
