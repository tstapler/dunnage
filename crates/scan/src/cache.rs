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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

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

const CHUNK: usize = 1 << 20;
const FREE_MARGIN: u64 = 1 << 20;
const STALE_TEMP_AGE: Duration = Duration::from_secs(3600);
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Why a cache save failed.
#[derive(Debug)]
pub enum SaveError {
    Io(io::Error),
    /// The abort flag was set while writing.
    Aborted,
    /// Less than the encoded size plus a 1 MiB margin is free.
    StorageFull,
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => e.fmt(f),
            Self::Aborted => f.write_str("save aborted"),
            Self::StorageFull => f.write_str("not enough free space for the cache"),
        }
    }
}

impl std::error::Error for SaveError {}

impl From<io::Error> for SaveError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<SaveError> for io::Error {
    fn from(e: SaveError) -> Self {
        match e {
            SaveError::Io(e) => e,
            SaveError::Aborted => io::Error::new(io::ErrorKind::Interrupted, "save aborted"),
            SaveError::StorageFull => io::Error::new(
                io::ErrorKind::StorageFull,
                "not enough free space for the cache",
            ),
        }
    }
}

/// An open temp file as the saver sees it.
pub trait TempFile {
    fn write_chunk(&mut self, chunk: &[u8]) -> io::Result<()>;
    fn sync(&mut self) -> io::Result<()>;
}

/// The filesystem operations of a save, injectable so tests can fail any step.
pub trait SaveIo {
    fn create(&self, path: &Path) -> io::Result<Box<dyn TempFile>>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn free_bytes(&self, dir: &Path) -> io::Result<u64>;
}

struct RealIo;

struct RealFile(std::fs::File);

impl TempFile for RealFile {
    fn write_chunk(&mut self, chunk: &[u8]) -> io::Result<()> {
        self.0.write_all(chunk)
    }
    fn sync(&mut self) -> io::Result<()> {
        self.0.sync_all()
    }
}

impl SaveIo for RealIo {
    fn create(&self, path: &Path) -> io::Result<Box<dyn TempFile>> {
        let f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        Ok(Box::new(RealFile(f)))
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }
    fn free_bytes(&self, dir: &Path) -> io::Result<u64> {
        let v = rustix::fs::statvfs(dir)?;
        Ok(v.f_bavail.saturating_mul(v.f_frsize))
    }
}

/// The one place the scan crate drops a file: our own abandoned cache temp (marker read by
/// `scripts/check-scan-readonly.sh`).
fn remove_stale_temp(path: &Path) {
    let _ = std::fs::remove_file(path); // readonly-ok(cache-temp)
}

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// `<stem>.<digits>.<digits>.tmp` (a writer's private name) or the legacy `<stem>.tmp`.
fn is_temp_name(name: &str, stem: &str) -> bool {
    let Some(rest) = name.strip_prefix(stem).and_then(|r| r.strip_prefix('.')) else {
        return false;
    };
    let Some(mid) = rest.strip_suffix("tmp") else {
        return false;
    };
    if mid.is_empty() {
        return true;
    }
    let Some(mid) = mid.strip_suffix('.') else {
        return false;
    };
    matches!(mid.split_once('.'), Some((a, b)) if is_digits(a) && is_digits(b))
}

/// Removes temp files of killed writers that are older than `max_age`.
fn sweep_stale_temps(dir: &Path, stem: &str, now: SystemTime, max_age: Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_temp_name(name, stem) {
            continue;
        }
        let old = e
            .metadata()
            .ok()
            .filter(|m| m.file_type().is_file())
            .and_then(|m| m.modified().ok())
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > max_age);
        if old {
            remove_stale_temp(&e.path());
        }
    }
}

fn write_temp(
    io: &dyn SaveIo,
    tmp: &Path,
    bytes: &[u8],
    abort: &AtomicBool,
) -> Result<(), SaveError> {
    let mut f = io.create(tmp)?;
    for chunk in bytes.chunks(CHUNK) {
        if abort.load(Ordering::Relaxed) {
            return Err(SaveError::Aborted);
        }
        f.write_chunk(chunk)?;
    }
    f.sync()?;
    Ok(())
}

/// Writes already-encoded cache bytes: refuses up front when they cannot fit, sweeps dead temps,
/// writes under a private temp name in chunks (checking `abort` before each), then renames over
/// `path`. On any failure the temp file is gone and the old cache is untouched.
pub fn write_atomic(
    path: &Path,
    bytes: &[u8],
    io: &dyn SaveIo,
    abort: &AtomicBool,
) -> Result<(), SaveError> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    std::fs::create_dir_all(dir)?;
    // An unreadable free-space figure does not block the save; the write itself will fail cleanly.
    if let Ok(free) = io.free_bytes(dir) {
        if free < (bytes.len() as u64).saturating_add(FREE_MARGIN) {
            return Err(SaveError::StorageFull);
        }
    }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("cache");
    sweep_stale_temps(dir, stem, SystemTime::now(), STALE_TEMP_AGE);
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!("{stem}.{}.{seq}.tmp", std::process::id()));
    let result = write_temp(io, &tmp, bytes, abort).and_then(|()| {
        io.rename(&tmp, path)?;
        Ok(())
    });
    if result.is_err() {
        remove_stale_temp(&tmp);
    }
    result
}

/// Atomic replace with the failure guarantees of [`write_atomic`].
pub fn save_atomic(
    path: &Path,
    map: &HashMap<PathBuf, Rec>,
    io: &dyn SaveIo,
    abort: &AtomicBool,
) -> Result<(), SaveError> {
    write_atomic(path, &encode(map), io, abort)
}

/// Atomic replace (temp file + rename) so an interrupted run never leaves a half-written cache.
pub fn save(path: &Path, map: &HashMap<PathBuf, Rec>) -> io::Result<()> {
    save_atomic(path, map, &RealIo, &AtomicBool::new(false)).map_err(Into::into)
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

    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Copy, PartialEq)]
    enum Fail {
        Never,
        Create,
        WriteAfter1KiB,
        Sync,
        Rename,
    }

    /// Real filesystem underneath, one injected failure, records temp names and chunk count.
    struct FaultIo {
        fail: Fail,
        free: Option<u64>,
        names: Mutex<Vec<PathBuf>>,
        chunks: Arc<AtomicUsize>,
        after_chunks: Option<(usize, Arc<AtomicBool>)>,
    }

    impl FaultIo {
        fn new(fail: Fail) -> Self {
            Self {
                fail,
                free: None,
                names: Mutex::new(Vec::new()),
                chunks: Arc::new(AtomicUsize::new(0)),
                after_chunks: None,
            }
        }
    }

    struct FaultFile {
        inner: std::fs::File,
        fail: Fail,
        written: usize,
        chunks: Arc<AtomicUsize>,
        after_chunks: Option<(usize, Arc<AtomicBool>)>,
    }

    impl TempFile for FaultFile {
        fn write_chunk(&mut self, chunk: &[u8]) -> io::Result<()> {
            if self.fail == Fail::WriteAfter1KiB {
                self.inner.write_all(&chunk[..1024.min(chunk.len())])?;
                self.written += 1024;
                return Err(io::Error::from_raw_os_error(28));
            }
            self.inner.write_all(chunk)?;
            let n = self.chunks.fetch_add(1, Ordering::SeqCst) + 1;
            if let Some((at, flag)) = &self.after_chunks {
                if n == *at {
                    flag.store(true, Ordering::SeqCst);
                }
            }
            Ok(())
        }
        fn sync(&mut self) -> io::Result<()> {
            if self.fail == Fail::Sync {
                return Err(io::Error::from_raw_os_error(5));
            }
            self.inner.sync_all()
        }
    }

    impl SaveIo for FaultIo {
        fn create(&self, path: &Path) -> io::Result<Box<dyn TempFile>> {
            self.names.lock().unwrap().push(path.to_path_buf());
            if self.fail == Fail::Create {
                return Err(io::Error::from_raw_os_error(28));
            }
            Ok(Box::new(FaultFile {
                inner: std::fs::File::create(path)?,
                fail: self.fail,
                written: 0,
                chunks: self.chunks.clone(),
                after_chunks: self.after_chunks.clone(),
            }))
        }
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            if self.fail == Fail::Rename {
                return Err(io::Error::from_raw_os_error(28));
            }
            std::fs::rename(from, to)
        }
        fn free_bytes(&self, dir: &Path) -> io::Result<u64> {
            match self.free {
                Some(f) => Ok(f),
                None => RealIo.free_bytes(dir),
            }
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dc-cache-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        v.sort();
        v
    }

    fn map_of(ino: u64) -> HashMap<PathBuf, Rec> {
        HashMap::from([(PathBuf::from("/q"), rec(ino))])
    }

    fn old_cache(dir: &Path) -> (PathBuf, Vec<u8>) {
        let file = dir.join("walker.cache");
        save(&file, &map_of(1)).unwrap();
        let bytes = std::fs::read(&file).unwrap();
        (file, bytes)
    }

    #[test]
    fn a_failed_write_leaves_only_the_old_cache() {
        for fail in [Fail::Create, Fail::WriteAfter1KiB, Fail::Sync, Fail::Rename] {
            let dir = scratch("fail");
            let (file, before) = old_cache(&dir);
            let io = FaultIo::new(fail);
            let r = save_atomic(&file, &map_of(2), &io, &AtomicBool::new(false));
            assert!(matches!(r, Err(SaveError::Io(_))));
            assert_eq!(names_in(&dir), ["walker.cache"]);
            assert_eq!(std::fs::read(&file).unwrap(), before);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn an_abort_stops_after_at_most_one_more_chunk() {
        let dir = scratch("abort");
        let (file, before) = old_cache(&dir);
        let abort = Arc::new(AtomicBool::new(false));
        let mut io = FaultIo::new(Fail::Never);
        io.after_chunks = Some((3, abort.clone()));
        let big = vec![7u8; 100 << 20];
        let r = write_atomic(&file, &big, &io, &abort);
        assert!(matches!(r, Err(SaveError::Aborted)));
        assert!(io.chunks.load(Ordering::SeqCst) <= 4);
        assert_eq!(names_in(&dir), ["walker.cache"]);
        assert_eq!(std::fs::read(&file).unwrap(), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_threads_use_different_temp_names_and_one_map_wins() {
        let dir = scratch("threads");
        let file = dir.join("walker.cache");
        let (a, b) = (map_of(10), map_of(20));
        let io = FaultIo::new(Fail::Never);
        std::thread::scope(|s| {
            s.spawn(|| save_atomic(&file, &a, &io, &AtomicBool::new(false)).unwrap());
            s.spawn(|| save_atomic(&file, &b, &io, &AtomicBool::new(false)).unwrap());
        });
        let names = io.names.lock().unwrap().clone();
        assert_eq!(names.len(), 2);
        assert_ne!(names[0], names[1]);
        let got = load(&file);
        assert!(got == a || got == b);
        assert_eq!(names_in(&dir), ["walker.cache"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Run by `two_processes_...` as a child; does nothing when started by the normal harness.
    #[test]
    fn child_saver() {
        let Ok(path) = std::env::var("DC_CACHE_CHILD_PATH") else {
            return;
        };
        let ino: u64 = std::env::var("DC_CACHE_CHILD_INO")
            .unwrap()
            .parse()
            .unwrap();
        for _ in 0..50 {
            save(Path::new(&path), &map_of(ino)).unwrap();
        }
    }

    #[test]
    fn two_processes_saving_one_path_leave_one_whole_map() {
        let dir = scratch("procs");
        let file = dir.join("walker.cache");
        let exe = std::env::current_exe().unwrap();
        let spawn = |ino: &str| {
            std::process::Command::new(&exe)
                .args(["--exact", "cache::tests::child_saver", "--test-threads=1"])
                .env("DC_CACHE_CHILD_PATH", &file)
                .env("DC_CACHE_CHILD_INO", ino)
                .output()
                .unwrap()
        };
        let (o1, o2) = std::thread::scope(|s| {
            let h1 = s.spawn(|| spawn("11"));
            let h2 = s.spawn(|| spawn("22"));
            (h1.join().unwrap(), h2.join().unwrap())
        });
        assert!(o1.status.success() && o2.status.success());
        let got = load(&file);
        assert!(got == map_of(11) || got == map_of(22));
        assert_eq!(names_in(&dir), ["walker.cache"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_write_that_cannot_fit_is_refused_before_any_file_exists() {
        let dir = scratch("full");
        let (file, before) = old_cache(&dir);
        let need = encode(&map_of(2)).len() as u64 + FREE_MARGIN;
        let mut io = FaultIo::new(Fail::Never);
        io.free = Some(need - 1);
        let r = save_atomic(&file, &map_of(2), &io, &AtomicBool::new(false));
        assert!(matches!(r, Err(SaveError::StorageFull)));
        assert!(io.names.lock().unwrap().is_empty());
        assert_eq!(names_in(&dir), ["walker.cache"]);
        assert_eq!(std::fs::read(&file).unwrap(), before);
        io.free = Some(need);
        save_atomic(&file, &map_of(2), &io, &AtomicBool::new(false)).unwrap();
        assert_eq!(load(&file), map_of(2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_old_temp_files_of_the_exact_pattern_are_swept() {
        let dir = scratch("sweep");
        let hour_plus = SystemTime::now() - Duration::from_secs(3601);
        for (name, old) in [
            ("walker.123.9.tmp", true),
            ("walker.123.10.tmp", false),
            ("walker.tmp", true),
            ("walker.cache.bak", true),
            ("other.tmp", true),
        ] {
            let f = std::fs::File::create(dir.join(name)).unwrap();
            if old {
                f.set_modified(hour_plus).unwrap();
            }
        }
        let file = dir.join("walker.cache");
        save(&file, &map_of(1)).unwrap();
        assert_eq!(
            names_in(&dir),
            [
                "other.tmp",
                "walker.123.10.tmp",
                "walker.cache",
                "walker.cache.bak"
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn temp_name_pattern_is_exact() {
        assert!(is_temp_name("walker.1.2.tmp", "walker"));
        assert!(is_temp_name("walker.tmp", "walker"));
        assert!(!is_temp_name("walker.a.2.tmp", "walker"));
        assert!(!is_temp_name("walker.1.tmp", "walker"));
        assert!(!is_temp_name("walker.1.2.3.tmp", "walker"));
        assert!(!is_temp_name("walkerx.1.2.tmp", "walker"));
    }
}
