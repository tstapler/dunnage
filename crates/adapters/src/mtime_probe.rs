//! The lazy newest-mtime probe and its sidecar (Story 1.3.1, spike 0.2.6).
//!
//! `NewestMtime` of a build directory is the newest mtime of anything inside, because a directory's own mtime
//! only moves when a direct entry is added or removed. The probe walks through [`treestat::measure`], the one
//! tree-walk rule, and ends at the first entry newer than the floor.
//!
//! The sidecar `<walker cache>.newest` stores `(Identity, own mtime, newest mtime)` per directory. A stored
//! value is a lower bound of recency: it may end a probe early when it is already younger than the floor, and
//! is never taken as proof that nothing newer exists (a file written in place does not move the directory's
//! own mtime).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dunnage_core::{DeleteError, Identity, RootDev};

use crate::dirops::{open_verified_root, DirOps};
use crate::treestat::{measure, MeasureOpts, SkipWhy, TreeStats};

const HEADER: &str = "dcnewest1";

pub fn to_system_time(secs: i64, nsec: u32) -> SystemTime {
    if secs >= 0 {
        UNIX_EPOCH + Duration::new(secs.unsigned_abs(), nsec)
    } else {
        UNIX_EPOCH - Duration::new(secs.unsigned_abs(), 0) + Duration::new(0, nsec)
    }
}

pub fn unix_secs(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_secs()).unwrap_or(i64::MAX),
    }
}

/// A directory's own `(mtime secs, nsec)` as `lstat` reported it.
pub type OwnMtime = (i64, u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stored {
    own: OwnMtime,
    newest: (i64, u32),
}

/// The sidecar beside the walker cache. Unreadable or malformed content reads as empty: a missing sidecar
/// only costs a full walk.
#[derive(Debug, Default)]
pub struct NewestStore {
    path: Option<PathBuf>,
    map: HashMap<(u64, u64), Stored>,
    dirty: bool,
}

impl NewestStore {
    /// `walker.cache` becomes `walker.cache.newest`.
    pub fn sidecar_path(walker_cache: &Path) -> PathBuf {
        let mut s = walker_cache.as_os_str().to_owned();
        s.push(".newest");
        PathBuf::from(s)
    }

    /// An empty store that saves nowhere.
    pub fn in_memory() -> NewestStore {
        NewestStore::default()
    }

    /// Reads the sidecar at `path`.
    pub fn open(path: PathBuf) -> NewestStore {
        let map = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| parse(&t))
            .unwrap_or_default();
        NewestStore {
            path: Some(path),
            map,
            dirty: false,
        }
    }

    /// `--rescan`: nothing stored is read or trusted, the sidecar at `path` is still rewritten.
    pub fn open_ignoring_contents(path: PathBuf) -> NewestStore {
        NewestStore {
            path: Some(path),
            map: HashMap::new(),
            dirty: true,
        }
    }

    /// The stored newest mtime, only while the directory's identity and own mtime are both unchanged.
    pub fn get(&self, ident: Identity, own: OwnMtime) -> Option<SystemTime> {
        let s = self.map.get(&(ident.dev, ident.ino))?;
        (s.own == own).then(|| to_system_time(s.newest.0, s.newest.1))
    }

    pub fn record(&mut self, ident: Identity, own: OwnMtime, newest: SystemTime) {
        let d = newest.duration_since(UNIX_EPOCH).unwrap_or_default();
        let value = Stored {
            own,
            newest: (
                i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
                d.subsec_nanos(),
            ),
        };
        if self.map.insert((ident.dev, ident.ino), value) != Some(value) {
            self.dirty = true;
        }
    }

    /// Takes over the entries `other` recorded (parallel probes each fill their own store).
    pub fn absorb(&mut self, other: NewestStore) {
        self.dirty |= other.dirty;
        self.map.extend(other.map);
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Atomic write (temp file, then rename); a no-op when nothing changed or the store has no path.
    pub fn save(&mut self) -> io::Result<()> {
        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        if !self.dirty {
            return Ok(());
        }
        let mut rows: Vec<_> = self.map.iter().collect();
        rows.sort_by_key(|(k, _)| **k);
        let mut text = String::from(HEADER);
        text.push('\n');
        for ((dev, ino), s) in rows {
            let _ = writeln!(
                text,
                "{dev} {ino} {} {} {} {}",
                s.own.0, s.own.1, s.newest.0, s.newest.1
            );
        }
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(format!(".tmp{}", std::process::id()));
        let tmp = PathBuf::from(tmp);
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        // A failed rename leaves the temp file behind; nothing here deletes (check-no-stray-unlink.sh).
        std::fs::rename(&tmp, &path)?;
        self.dirty = false;
        Ok(())
    }
}

fn parse(text: &str) -> Option<HashMap<(u64, u64), Stored>> {
    let mut lines = text.lines();
    if lines.next()? != HEADER {
        return None;
    }
    let mut map = HashMap::new();
    for line in lines {
        let mut f = line.split(' ');
        let dev: u64 = f.next()?.parse().ok()?;
        let ino: u64 = f.next()?.parse().ok()?;
        let os: i64 = f.next()?.parse().ok()?;
        let on: u32 = f.next()?.parse().ok()?;
        let ns: i64 = f.next()?.parse().ok()?;
        let nn: u32 = f.next()?.parse().ok()?;
        if f.next().is_some() {
            return None;
        }
        map.insert(
            (dev, ino),
            Stored {
                own: (os, on),
                newest: (ns, nn),
            },
        );
    }
    Some(map)
}

/// What the probe could establish about one directory against the floor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recency {
    /// Something inside is newer than the floor.
    Young {
        newest: SystemTime,
        from_store: bool,
    },
    /// The whole tree was read and nothing is newer than the floor (`None`: the tree holds no entries).
    Old { newest: Option<SystemTime> },
    /// The tree could not be read completely and nothing newer was seen, so "old" is not proven.
    Unknown { why: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub recency: Recency,
    /// The walk's totals; `None` when the sidecar ended the probe without touching the directory.
    pub stats: Option<TreeStats>,
}

/// The directory a probe looks at.
#[derive(Debug, Clone, Copy)]
pub struct ProbeTarget<'a> {
    pub path: &'a Path,
    pub ident: Identity,
    pub own: OwnMtime,
    pub root_dev: RootDev,
}

/// Probes `target` against `floor`. With a store whose entry for the directory is unchanged and already
/// younger than the floor, no syscall is made under the directory. Otherwise the tree is walked and its
/// newest mtime is written back to the store.
pub fn probe_newest_mtime<D: DirOps>(
    ops: &D,
    target: ProbeTarget<'_>,
    floor: SystemTime,
    store: &mut NewestStore,
) -> Result<Probe, DeleteError> {
    let ProbeTarget {
        path,
        ident,
        own,
        root_dev,
    } = target;
    if let Some(newest) = store.get(ident, own).filter(|n| *n > floor) {
        return Ok(Probe {
            recency: Recency::Young {
                newest,
                from_store: true,
            },
            stats: None,
        });
    }
    let root = open_verified_root(ops, path, ident)?;
    let stats = measure(
        ops,
        root.dir(),
        root_dev,
        MeasureOpts {
            stop_if_newer_than: Some(floor),
        },
    );
    if let Some(newest) = stats.newest_mtime {
        store.record(ident, own, newest);
    }
    let recency = classify(&stats, floor);
    Ok(Probe {
        recency,
        stats: Some(stats),
    })
}

fn classify(stats: &TreeStats, floor: SystemTime) -> Recency {
    match stats.newest_mtime {
        Some(newest) if stats.stopped_early || newest > floor => Recency::Young {
            newest,
            from_store: false,
        },
        newest => {
            let unreadable = stats
                .skipped
                .iter()
                .any(|s| matches!(s.why, SkipWhy::Unreadable(_) | SkipWhy::TooDeep));
            if unreadable {
                Recency::Unknown {
                    why: "part of the tree could not be read".into(),
                }
            } else {
                Recency::Old { newest }
            }
        }
    }
}
