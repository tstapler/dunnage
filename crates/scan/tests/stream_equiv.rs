#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 6.1.2: what `WalkerBackend::collect_streaming` delivers equals what `collect` returns, plus the
//! compatibility, batching, paint and cache-label cases.

mod stream_common;

use dunnage_scan::index::{
    Backend, CollectRequest, Collected, DirEvent, DirInfo, StreamRequest, NO_PARENT,
};
use dunnage_scan::walker::{CacheConfig, WalkerBackend};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use stream_common::{FakeFs, Recorder, FAKE_MTIME, ROOT};

fn tmp(name: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!(
        "dc-scan-stream-{name}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    fs::canonicalize(&d).unwrap()
}

fn request(roots: &[PathBuf], threads: usize) -> StreamRequest<'_> {
    StreamRequest::new(
        CollectRequest {
            roots,
            cross_fs: false,
            deadline: None,
            threads,
        },
        Arc::new(AtomicBool::new(false)),
    )
}

/// Rebuilds `(path, DirInfo)` from `Dir` events by following parent ids.
fn fold(events: &[DirEvent]) -> BTreeMap<PathBuf, DirInfo> {
    let mut by_id: HashMap<u64, (&DirEvent, PathBuf)> = HashMap::new();
    let mut pending: Vec<&DirEvent> = events.iter().filter(|e| !is_painted(e)).collect();
    while !pending.is_empty() {
        let before = pending.len();
        pending.retain(|e| {
            let path = if e.parent() == NO_PARENT {
                Some(PathBuf::from(name(e)))
            } else {
                by_id.get(&e.parent()).map(|(_, p)| p.join(name(e)))
            };
            match path {
                Some(p) => {
                    by_id.insert(e.id(), (e, p));
                    false
                }
                None => true,
            }
        });
        assert!(pending.len() < before, "events whose parent never arrived");
    }
    by_id
        .into_values()
        .filter_map(|(e, p)| match e {
            DirEvent::Dir { info, .. } => Some((p, info.clone())),
            _ => None,
        })
        .collect()
}

fn name(e: &DirEvent) -> &std::ffi::OsStr {
    match e {
        DirEvent::Dir { name, .. }
        | DirEvent::Unreadable { name, .. }
        | DirEvent::Boundary { name, .. }
        | DirEvent::Gone { name, .. } => name,
        _ => unreachable!(),
    }
}

fn is_painted(e: &DirEvent) -> bool {
    matches!(e, DirEvent::Dir { painted: true, .. })
}

struct Lcg(u64);
impl Lcg {
    fn next(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % n
    }
}

/// A random tree: up to 60 directories at depth <= 6 holding files, empty directories and symlinks.
fn random_tree(seed: u64, root: &Path) {
    let mut rng = Lcg(seed);
    let mut dirs = vec![root.to_path_buf()];
    for i in 0..(5 + rng.next(55)) {
        let parent = dirs[rng.next(dirs.len() as u64) as usize].clone();
        if parent.strip_prefix(root).unwrap().components().count() >= 6 {
            continue;
        }
        let d = parent.join(format!("d{i}"));
        fs::create_dir(&d).unwrap();
        dirs.push(d);
    }
    for (i, d) in dirs.iter().enumerate() {
        for f in 0..rng.next(4) {
            fs::write(
                d.join(format!("f{f}")),
                vec![1u8; (rng.next(9000)) as usize],
            )
            .unwrap();
        }
        match rng.next(6) {
            0 => fs::write(d.join("package.json"), "{}").unwrap(),
            1 => fs::write(d.join("Cargo.toml"), "").unwrap(),
            2 => std::os::unix::fs::symlink("/nonexistent", d.join(format!("l{i}"))).unwrap(),
            _ => {}
        }
    }
}

#[test]
fn streamed_dir_events_should_fold_to_exactly_what_collect_returns_When_random_tree_untruncated() {
    for seed in 0..40 {
        let root = tmp("equiv");
        random_tree(seed, &root);
        let roots = vec![root.clone()];
        let rec = Recorder::default();
        let summary = WalkerBackend::default()
            .collect_streaming(&request(&roots, 3), &rec.sink())
            .unwrap();
        assert!(!summary.truncated);
        let events = rec.events();

        let mut ids: Vec<u64> = events.iter().map(DirEvent::id).collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            (0..events.len() as u64).collect::<Vec<_>>(),
            "seed {seed}: ids dense from 0"
        );
        let by_id: HashMap<u64, &DirEvent> = events.iter().map(|e| (e.id(), e)).collect();
        for e in &events {
            match e.parent() {
                NO_PARENT => assert_eq!(
                    name(e),
                    root.as_os_str(),
                    "seed {seed}: root carries its full path"
                ),
                p => assert!(by_id.contains_key(&p), "seed {seed}: parent id exists"),
            }
            if let DirEvent::Dir { dev, .. } = e {
                assert!(*dev != 0);
            }
        }
        let streamed = fold(&events);
        let got: Collected = WalkerBackend::default()
            .collect(&request(&roots, 3).collect)
            .unwrap();
        let want: BTreeMap<PathBuf, String> = got
            .dirs
            .iter()
            .map(|(p, i)| (p.clone(), format!("{i:?}")))
            .collect();
        let have: BTreeMap<PathBuf, String> = streamed
            .iter()
            .map(|(p, i)| (p.clone(), format!("{i:?}")))
            .collect();
        assert_eq!(have, want, "seed {seed}");
        // each Dir event's dev is the directory's st_dev
        let want_dev = fs::metadata(&root).unwrap().dev();
        assert!(events
            .iter()
            .all(|e| !matches!(e, DirEvent::Dir { dev, .. } if *dev != want_dev)));
        let _ = fs::remove_dir_all(&root);
    }
}

#[test]
fn collect_streaming_should_build_through_dyn_backend_and_replay_collect_When_backend_has_only_collect(
) {
    struct OnlyCollect;
    impl Backend for OnlyCollect {
        fn name(&self) -> &'static str {
            "only-collect"
        }
        fn extents_exact(&self) -> bool {
            false
        }
        fn collect(&self, _req: &CollectRequest<'_>) -> dunnage_scan::Result<Collected> {
            let info = |m| DirInfo {
                mtime: m,
                ..DirInfo::default()
            };
            Ok(Collected {
                dirs: vec![
                    (PathBuf::from("/r/a/b"), info(3)),
                    (PathBuf::from("/r"), info(1)),
                    (PathBuf::from("/r/a"), info(2)),
                    (PathBuf::from("/r/c"), info(4)),
                ],
                errors: 7,
                truncated: true,
                notes: vec!["n".into()],
                ..Collected::default()
            })
        }
    }
    let backend: &dyn Backend = &OnlyCollect;
    let roots = vec![PathBuf::from("/r")];
    let rec = Recorder::default();
    let summary = backend
        .collect_streaming(&request(&roots, 1), &rec.sink())
        .unwrap();
    assert!(summary.truncated && summary.errors == 7 && summary.notes == ["n"]);
    let events = rec.events();
    assert_eq!(events.len(), 4);
    let mut seen = HashSet::new();
    for (i, e) in events.iter().enumerate() {
        assert_eq!(e.id(), i as u64, "dense from 0");
        assert!(
            e.parent() == NO_PARENT || seen.contains(&e.parent()),
            "parents before children"
        );
        seen.insert(e.id());
        let DirEvent::Dir {
            subdirs,
            from_cache,
            painted,
            name: n,
            ..
        } = e
        else {
            panic!("{e:?}")
        };
        assert!(!from_cache && !painted);
        match i {
            0 => assert_eq!((n.to_str(), *subdirs), (Some("/r"), 2)),
            1 | 2 => assert!(matches!(n.to_str(), Some("a" | "c"))),
            _ => assert_eq!(n.to_str(), Some("b")),
        }
    }
}

#[test]
fn walker_should_publish_before_the_walk_ends_and_keep_batches_within_batch_max_When_2000_dirs_4_threads(
) {
    let fake = FakeFs::new(2000, 8);
    let rec = Arc::new(Recorder::default());
    let first_seen = std::sync::atomic::AtomicU64::new(u64::MAX);
    let sink = |events: Vec<DirEvent>| {
        let _ = first_seen.compare_exchange(
            u64::MAX,
            fake.finished.load(Ordering::Relaxed),
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
        rec.batches
            .lock()
            .unwrap()
            .push((std::time::Instant::now(), events));
    };
    let roots = vec![PathBuf::from(ROOT)];
    WalkerBackend::default()
        .stream(&request(&roots, 4), &sink, &fake)
        .unwrap();
    assert_eq!(rec.terminal_count(), 2000);
    assert!(
        first_seen.load(Ordering::Relaxed) < 2000,
        "the first batch arrived before all directories were listed"
    );
    assert!(rec.max_batch() <= 256, "batch_max");
}

fn age_dirs(paths: &[&Path]) {
    use rustix::fs::{utimensat, AtFlags, Timespec, Timestamps, CWD};
    let t = Timespec {
        tv_sec: 1_600_000_000,
        tv_nsec: 0,
    };
    for p in paths {
        utimensat(
            CWD,
            *p,
            &Timestamps {
                last_access: t,
                last_modification: t,
            },
            AtFlags::empty(),
        )
        .unwrap();
    }
}

#[test]
fn cache_hits_should_be_labelled_from_cache_and_counted_When_second_run_is_unchanged() {
    let root = tmp("cacheflag");
    fs::create_dir_all(root.join("a/b")).unwrap();
    fs::create_dir_all(root.join("c")).unwrap();
    fs::write(root.join("a/b/f"), "x").unwrap();
    age_dirs(&[&root.join("a/b"), &root.join("a"), &root.join("c"), &root]);
    let cfg = |rescan| CacheConfig {
        path: tmp("cacheflag-home").join("c.bin"),
        rescan,
        max_age: Duration::from_secs(3600),
    };
    let first = cfg(false);
    let roots = vec![root.clone()];
    let run = |backend: WalkerBackend| {
        let rec = Recorder::default();
        let summary = backend
            .collect_streaming(&request(&roots, 2), &rec.sink())
            .unwrap();
        (rec.events(), summary)
    };
    let (events, s1) = run(WalkerBackend {
        cache: Some(first.clone()),
    });
    assert!(events.iter().all(|e| matches!(
        e,
        DirEvent::Dir {
            from_cache: false,
            ..
        }
    )));
    assert_eq!(s1.dirs_cached, 0);
    let (events, s2) = run(WalkerBackend {
        cache: Some(first.clone()),
    });
    let hits = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                DirEvent::Dir {
                    from_cache: true,
                    ..
                }
            )
        })
        .count();
    assert_eq!(hits as u64, s2.dirs_cached);
    assert_eq!(
        s2.dirs_cached, 4,
        "all four unchanged directories came from the cache"
    );
    let (events, s3) = run(WalkerBackend {
        cache: Some(CacheConfig {
            rescan: true,
            ..first
        }),
    });
    assert!(events.iter().all(|e| matches!(
        e,
        DirEvent::Dir {
            from_cache: false,
            ..
        }
    )));
    assert_eq!(s3.dirs_cached, 0);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn walker_should_load_the_cache_once_and_paint_before_the_live_walk_When_paint_from_cache() {
    let fake = FakeFs::new(2000, 8);
    {
        let mut c = fake.cache.lock().unwrap();
        for n in 0..2000 {
            c.insert(fake.path_of(n), fake.rec(n, FAKE_MTIME, 1));
        }
    }
    let dir = tmp("paint");
    let backend = WalkerBackend {
        cache: Some(CacheConfig {
            path: dir.join("c.bin"),
            rescan: false,
            max_age: Duration::from_secs(3600),
        }),
    };
    let roots = vec![PathBuf::from(ROOT)];
    let mut req = request(&roots, 4);
    req.paint_from_cache = true;
    req.save.store(false, Ordering::Relaxed);
    let rec = Recorder::default();
    backend.stream(&req, &rec.sink(), &fake).unwrap();
    assert_eq!(
        fake.loads.load(Ordering::Relaxed),
        1,
        "cache::load ran exactly once for the whole run"
    );
    let events = rec.events();
    let painted = events.iter().take_while(|e| is_painted(e)).count();
    assert_eq!(painted, 2000, "the cache's records come first");
    assert!(events[painted..]
        .iter()
        .all(|e| matches!(e, DirEvent::Dir { painted: false, .. })));
    assert_eq!(
        events.len(),
        4000,
        "the live walk emits the same directories again"
    );
    assert!(!dir.join("c.bin").exists(), "save gate was clear");
}

#[test]
fn collect_request_and_collected_should_still_build_by_literal_and_read_as_before() {
    let roots: Vec<PathBuf> = vec![];
    let req = CollectRequest {
        roots: &roots,
        cross_fs: false,
        deadline: None,
        threads: 1,
    };
    let _ = req.threads;
    let c = Collected {
        dirs: vec![],
        errors: 0,
        truncated: false,
        dirs_listed: 0,
        dirs_cached: 0,
        notes: vec![],
    };
    let _ = (
        c.dirs.len(),
        c.errors,
        c.truncated,
        c.dirs_listed,
        c.dirs_cached,
        c.notes.len(),
    );
}
