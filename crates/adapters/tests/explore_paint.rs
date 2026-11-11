#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 6.3.1 Task 6.3.1b: cache paint through the walker's own map, the explorer cache file and its flags,
//! the free-space policy of the save, and the cache under a full disk or a concurrent writer.

#[path = "support/fullfs.rs"]
mod fullfs;
#[path = "../../scan/tests/stream_common/mod.rs"]
mod stream_common;

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use disk_cleanup_adapters::explore::cache_paint::{
    cache_config, explorer_cache_path, paints, save_guard, save_room, CacheMode, DEFAULT_MAX_AGE,
    NEARLY_FULL, STATE_UNREADABLE,
};
use disk_cleanup_adapters::explore::scan_source::WalkerScanSource;
use disk_cleanup_adapters::RealSpaceMeter;
use disk_cleanup_core::explore::msg::{ScanMode, ScanMsg, ScanSummary};
use disk_cleanup_core::explore::ports::{ScanHandle, ScanOpts, ScanSource};
use disk_cleanup_core::explore::tree::ExploreTree;
use disk_cleanup_core::ports::SpaceMeter;
use disk_cleanup_core::snapshot_types::{AllocSnapshot, SpaceUse};
use disk_cleanup_core::testing::FakeSpaceMeter;
use disk_cleanup_core::{Dirs, FsSnapshot, Platform};
use disk_cleanup_scan::cache::{self, Rec};
use disk_cleanup_scan::index::{
    Backend, CollectRequest, Collected, DirEvent, StreamRequest, StreamSummary,
};
use disk_cleanup_scan::walker::{CacheConfig, DirStat, WalkHooks, WalkerBackend};
use stream_common::{FakeFs, FAKE_MTIME, ROOT};

type Sink<'a> = &'a (dyn Fn(Vec<DirEvent>) + Sync);

/// The walker over a fake filesystem whose cache is read from a real file, counting the loads.
struct FileHooks {
    fs: FakeFs,
    loads: Arc<AtomicUsize>,
}

impl WalkHooks for FileHooks {
    fn load_cache(&self, path: &Path) -> HashMap<PathBuf, Rec> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        cache::load(path)
    }
    fn stat_override(&self, p: &Path) -> Option<Result<DirStat, i32>> {
        self.fs.stat_override(p)
    }
    fn list_override(&self, p: &Path, st: &DirStat) -> Option<Result<Rec, i32>> {
        self.fs.list_override(p, st)
    }
}

struct Hooked {
    walker: WalkerBackend,
    hooks: FileHooks,
}

impl Backend for Hooked {
    fn name(&self) -> &'static str {
        "hooked"
    }
    fn extents_exact(&self) -> bool {
        false
    }
    fn collect(&self, _r: &CollectRequest<'_>) -> disk_cleanup_scan::Result<Collected> {
        Ok(Collected::default())
    }
    fn collect_streaming(
        &self,
        req: &StreamRequest<'_>,
        sink: Sink<'_>,
    ) -> io::Result<StreamSummary> {
        self.walker.stream(req, sink, &self.hooks)
    }
}

fn full_map(fake: &FakeFs) -> HashMap<PathBuf, Rec> {
    (0..fake.heap.total)
        .map(|n| (fake.path_of(n), fake.rec(n, FAKE_MTIME, 1)))
        .collect()
}

fn dirs_in(tmp: &Path) -> Dirs {
    Dirs {
        home: tmp.to_path_buf(),
        state: tmp.join("state"),
        cache: tmp.join("cache"),
        config: tmp.join("config"),
    }
}

fn roots() -> Vec<PathBuf> {
    vec![PathBuf::from(ROOT)]
}

fn finish(h: &ScanHandle) -> (Vec<ScanMsg>, ScanSummary) {
    let mut seen = Vec::new();
    loop {
        match h.rx.recv_timeout(Duration::from_secs(120)).unwrap() {
            ScanMsg::Finished(s) => return (seen, s),
            ScanMsg::SourceFailed(t) => panic!("scan failed: {t}"),
            m => seen.push(m),
        }
    }
}

#[test]
fn explorer_cache_should_be_separate_from_the_cleaners_walker_cache() {
    let dirs = Dirs {
        home: "/h".into(),
        state: "/h/.local/state".into(),
        cache: "/h/.cache".into(),
        config: "/h/.config".into(),
    };
    let p = explorer_cache_path(&dirs);
    assert_eq!(p, PathBuf::from("/h/.cache/disk-cleanup/explorer.cache"));
    assert_ne!(p.file_name(), Some("walker.cache".as_ref()));
}

#[test]
fn cache_flags_should_map_to_the_walkers_config() {
    let dirs = dirs_in(Path::new("/h"));
    assert!(cache_config(&dirs, CacheMode::Off, DEFAULT_MAX_AGE).is_none());
    let use_ = cache_config(&dirs, CacheMode::Use, DEFAULT_MAX_AGE).unwrap();
    let rescan = cache_config(&dirs, CacheMode::Rescan, DEFAULT_MAX_AGE).unwrap();
    assert!(!use_.rescan && rescan.rescan);
    assert_eq!(use_.path, explorer_cache_path(&dirs));
    assert!(paints(CacheMode::Use) && !paints(CacheMode::Off) && !paints(CacheMode::Rescan));
}

fn run_mode(mode: CacheMode, seeded: bool) -> (usize, bool, PathBuf, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let dirs = dirs_in(tmp.path());
    let path = explorer_cache_path(&dirs);
    let fake = FakeFs::new(50, 4);
    if seeded {
        cache::save(&path, &full_map(&fake)).unwrap();
    }
    let loads = Arc::new(AtomicUsize::new(0));
    let backend = Hooked {
        walker: WalkerBackend {
            cache: cache_config(&dirs, mode, DEFAULT_MAX_AGE),
        },
        hooks: FileHooks {
            fs: fake,
            loads: Arc::clone(&loads),
        },
    };
    let opts = ScanOpts {
        paint_from_cache: paints(mode),
        ..ScanOpts::default()
    };
    let h = WalkerScanSource::new(backend).start(&roots(), &opts);
    let (batches, _) = finish(&h);
    let painted = batches.iter().any(|m| {
        matches!(
            m,
            ScanMsg::Batch {
                mode: ScanMode::Paint,
                ..
            }
        )
    });
    assert_eq!(painted, mode == CacheMode::Use && seeded);
    let rewritten = path.exists() && cache::load(&path).len() == 50;
    (loads.load(Ordering::SeqCst), rewritten, path, tmp)
}

#[test]
fn no_cache_should_read_nothing_and_write_nothing() {
    let (loads, _, path, _tmp) = run_mode(CacheMode::Off, true);
    assert_eq!(loads, 0);
    // The seeded file is untouched: still 50 records and no sibling.
    assert_eq!(cache::load(&path).len(), 50);
    let names: Vec<_> = fs::read_dir(path.parent().unwrap()).unwrap().collect();
    assert_eq!(names.len(), 1);
}

#[test]
fn rescan_should_read_nothing_and_rewrite_the_cache() {
    let (loads, rewritten, _, _tmp) = run_mode(CacheMode::Rescan, true);
    assert_eq!(loads, 0);
    assert!(rewritten);
}

#[test]
fn default_mode_should_load_once_paint_and_rewrite() {
    let (loads, rewritten, _, _tmp) = run_mode(CacheMode::Use, true);
    assert_eq!(loads, 1);
    assert!(rewritten);
}

#[test]
fn painted_events_should_fold_into_a_tree_with_incomplete_nodes_where_a_child_has_no_record() {
    let tmp = tempfile::tempdir().unwrap();
    let dirs = dirs_in(tmp.path());
    let path = explorer_cache_path(&dirs);
    let fake = FakeFs::new(10, 2);
    let mut map = full_map(&fake);
    // Node 3 has no record: its parent (node 1) names it but gets no event for it.
    map.remove(&fake.path_of(3));
    cache::save(&path, &map).unwrap();
    // Paint only: the live pass is cancelled before it can confirm anything.
    let backend = Hooked {
        walker: WalkerBackend {
            cache: cache_config(&dirs, CacheMode::Use, DEFAULT_MAX_AGE),
        },
        hooks: FileHooks {
            fs: fake,
            loads: Arc::new(AtomicUsize::new(0)),
        },
    };
    let h = WalkerScanSource::new(backend).start(
        &roots(),
        &ScanOpts {
            paint_from_cache: true,
            ..ScanOpts::default()
        },
    );
    let mut tree = ExploreTree::new();
    let mut painted = 0;
    // Nodes 3, 7 and 8 (the missing record and its children) have no event.
    while painted < 7 {
        match h.rx.recv_timeout(Duration::from_secs(30)).unwrap() {
            ScanMsg::Batch {
                generation,
                mode: ScanMode::Paint,
                events,
            } => {
                painted += events.len();
                tree.apply_batch(generation, ScanMode::Paint, events)
                    .unwrap();
            }
            ScanMsg::Batch { .. } => {}
            other => panic!("{other:?}"),
        }
    }
    h.detach();
    let find = |name: &str| tree.iter().find(|n| tree.name(*n) == name).unwrap();
    assert!(!tree.is_complete(find("d1")), "d1 waits for its child d3");
    assert!(tree.is_complete(find("d2")) || tree.children(find("d2")).count() > 0);
}

#[test]
fn paint_of_676296_records_should_load_once_come_in_batches_of_256_and_fit_the_bound() {
    let tmp = tempfile::tempdir().unwrap();
    let dirs = dirs_in(tmp.path());
    let path = explorer_cache_path(&dirs);
    let total = 676_296u64;
    let fake = FakeFs::new(total, 8);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    cache::save(&path, &full_map(&fake)).unwrap();
    eprintln!(
        "synthetic cache: {} bytes",
        fs::metadata(&path).unwrap().len()
    );

    let loads = Arc::new(AtomicUsize::new(0));
    let backend = Hooked {
        walker: WalkerBackend {
            cache: cache_config(&dirs, CacheMode::Use, DEFAULT_MAX_AGE),
        },
        hooks: FileHooks {
            fs: fake,
            loads: Arc::clone(&loads),
        },
    };
    let t = Instant::now();
    let h = WalkerScanSource::new(backend).start(
        &roots(),
        &ScanOpts {
            paint_from_cache: true,
            ..ScanOpts::default()
        },
    );
    let mut tree = ExploreTree::new();
    let (mut painted, mut first_len, mut max_len) = (0u64, None, 0usize);
    let mut arrived = None;
    while painted < total {
        let ScanMsg::Batch {
            generation,
            mode,
            events,
        } = h.rx.recv_timeout(Duration::from_secs(120)).unwrap()
        else {
            panic!("unexpected message before the paint ended")
        };
        assert_eq!(
            mode,
            ScanMode::Paint,
            "no live event before the paint is out"
        );
        first_len.get_or_insert(events.len());
        max_len = max_len.max(events.len());
        painted += events.len() as u64;
        tree.apply_batch(generation, mode, events).unwrap();
        if painted == total {
            arrived = Some(t.elapsed());
        }
    }
    let folded = t.elapsed();
    h.detach();
    let arrived = arrived.unwrap();
    eprintln!("load + paint events delivered in {arrived:?}; folded into the tree by {folded:?}");
    let bound: f64 = std::env::var("DC_PAINT_BOUND_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10.0);
    assert!(
        arrived.as_secs_f64() <= bound,
        "load plus paint took {arrived:?}, bound {bound} s"
    );
    assert_eq!(
        loads.load(Ordering::SeqCst),
        1,
        "cache::load ran once for the whole run"
    );
    assert_eq!(first_len, Some(256));
    assert_eq!(max_len, 256);
    assert_eq!(tree.directories() as u64, total);
    assert!(tree.roots().all(|r| tree.is_complete(r)));
}

// ----- the free-space policy -----

fn snap(avail: u64, btrfs: Option<AllocSnapshot>, fs_type: &str) -> FsSnapshot {
    FsSnapshot {
        path: "/".into(),
        taken_at: 0,
        total_bytes: 1 << 40,
        used_bytes: (1 << 40) - avail,
        avail_bytes: avail,
        fs_type: fs_type.into(),
        btrfs,
    }
}

fn alloc(unallocated: u64, meta_free: u64) -> AllocSnapshot {
    let used = |free: u64| SpaceUse {
        bytes_used: 1 << 30,
        total_bytes: (1 << 30) + free,
    };
    AllocSnapshot {
        data: used(10 << 30),
        metadata: used(meta_free),
        system: used(1 << 20),
        unallocated,
    }
}

const GIB: u64 = 1 << 30;

#[test]
fn save_room_should_refuse_the_cases_of_the_policy_and_allow_the_rest() {
    let ok = |s: FsSnapshot, prev: u64| save_room(&Ok(s), prev);
    // Plenty of room, plain and btrfs.
    assert_eq!(ok(snap(50 * GIB, None, "ext4"), 0), Ok(()));
    assert_eq!(
        ok(
            snap(50 * GIB, Some(alloc(50 * GIB, 5 * GIB)), "btrfs"),
            100 << 20
        ),
        Ok(())
    );
    // 4 x previous + 1 GiB, exactly and one byte under.
    let prev = 100 << 20;
    assert_eq!(ok(snap(4 * prev + GIB, None, "ext4"), prev), Ok(()));
    assert_eq!(
        ok(snap(4 * prev + GIB - 1, None, "ext4"), prev),
        Err(NEARLY_FULL)
    );
    // No previous cache: the 1 GiB floor.
    assert_eq!(ok(snap(GIB - 1, None, "ext4"), 0), Err(NEARLY_FULL));
    // CRIT verdict (unallocated under 3 GiB) and the btrfs floor.
    assert_eq!(
        ok(snap(50 * GIB, Some(alloc(2 * GIB, 5 * GIB)), "btrfs"), 0),
        Err(NEARLY_FULL)
    );
    assert_eq!(
        ok(snap(50 * GIB, Some(alloc(GIB / 2, 5 * GIB)), "btrfs"), 0),
        Err(NEARLY_FULL)
    );
    // Unknown or unreadable fails closed.
    assert_eq!(
        save_room(&Err(io::Error::from_raw_os_error(5)), 0),
        Err(STATE_UNREADABLE)
    );
    assert_eq!(
        ok(snap(50 * GIB, None, "unknown"), 0),
        Err(STATE_UNREADABLE)
    );
}

/// A scan of 50 fake directories writing to `cache`, guarded by `meter`.
fn guarded_scan(cache: &Path, meter: FakeSpaceMeter) -> (ScanSummary, Arc<FakeSpaceMeter>) {
    let meter = Arc::new(meter);
    let backend = Hooked {
        walker: WalkerBackend {
            cache: Some(CacheConfig {
                path: cache.to_path_buf(),
                rescan: true,
                max_age: DEFAULT_MAX_AGE,
            }),
        },
        hooks: FileHooks {
            fs: FakeFs::new(50, 4),
            loads: Arc::new(AtomicUsize::new(0)),
        },
    };
    let dynamic: Arc<dyn SpaceMeter> = meter.clone();
    let src = WalkerScanSource::new(backend).with_save_guard(save_guard(dynamic));
    let h = src.start(&roots(), &ScanOpts::default());
    let (_, summary) = finish(&h);
    (summary, meter)
}

fn no_tmp(dir: &Path) {
    let leftovers: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n.to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
}

#[test]
fn a_scan_should_not_write_the_cache_When_the_verdict_is_crit_or_room_is_short_or_state_is_unknown()
{
    let cases: Vec<(&str, FakeSpaceMeter, &str)> = vec![
        (
            "CRIT",
            FakeSpaceMeter::new(
                vec![snap(50 * GIB, Some(alloc(GIB, GIB)), "btrfs")],
                FakeSpaceMeter::stable(),
            ),
            "cache not saved: disk nearly full",
        ),
        (
            "avail short",
            FakeSpaceMeter::new(vec![snap(GIB / 2, None, "ext4")], FakeSpaceMeter::stable()),
            "cache not saved: disk nearly full",
        ),
        (
            "unallocated floor",
            FakeSpaceMeter::new(
                vec![snap(50 * GIB, Some(alloc(GIB / 2, 5 * GIB)), "btrfs")],
                FakeSpaceMeter::stable(),
            ),
            "cache not saved: disk nearly full",
        ),
        (
            "unreadable",
            FakeSpaceMeter::new(vec![snap(50 * GIB, None, "ext4")], FakeSpaceMeter::stable())
                .failing(5),
            "cache not saved: filesystem state unreadable",
        ),
    ];
    for (name, meter, note) in cases {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("c/explorer.cache");
        let (summary, meter) = guarded_scan(&cache, meter);
        assert!(
            summary.notes.contains(&note.to_string()),
            "{name}: {:?}",
            summary.notes
        );
        assert!(!cache.exists(), "{name}: cache written");
        if let Some(dir) = cache.parent().filter(|d| d.exists()) {
            no_tmp(dir);
        }
        // Read once, at the save: not at the start of the scan (a failing read records no call).
        if name != "unreadable" {
            assert_eq!(meter.calls().len(), 1, "{name}");
        }
    }
}

#[test]
fn a_scan_should_write_the_cache_When_the_verdict_is_ok_and_room_remains() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("c/explorer.cache");
    let meter = FakeSpaceMeter::new(vec![snap(50 * GIB, None, "ext4")], FakeSpaceMeter::stable());
    let (summary, _) = guarded_scan(&cache, meter);
    assert!(summary.notes.is_empty(), "{:?}", summary.notes);
    assert_eq!(cache::load(&cache).len(), 50);
    no_tmp(cache.parent().unwrap());
}

#[test]
fn the_previous_cache_should_count_fourfold_When_the_policy_reads_the_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("explorer.cache");
    let fake = FakeFs::new(50, 4);
    cache::save(&cache, &full_map(&fake)).unwrap();
    let len = fs::metadata(&cache).unwrap().len();
    let before = fs::read(&cache).unwrap();
    let short = FakeSpaceMeter::new(
        vec![snap(4 * len + GIB - 1, None, "ext4")],
        FakeSpaceMeter::stable(),
    );
    let (summary, _) = guarded_scan(&cache, short);
    assert!(summary
        .notes
        .contains(&"cache not saved: disk nearly full".to_string()));
    assert_eq!(fs::read(&cache).unwrap(), before, "the old cache is intact");
}

#[test]
fn q_during_the_write_should_abort_the_save_remove_the_temp_and_leave_the_old_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("explorer.cache");
    cache::save(&cache, &full_map(&FakeFs::new(10, 2))).unwrap();
    let before = fs::read(&cache).unwrap();
    // 300,000 records take long enough to encode that the gate, cleared as the write begins (what `q`'s
    // `detach()` does), is seen before the first chunk goes out.
    let big = FakeFs::new(300_000, 8);
    let backend = Hooked {
        walker: WalkerBackend {
            cache: Some(CacheConfig {
                path: cache.clone(),
                rescan: true,
                max_age: DEFAULT_MAX_AGE,
            }),
        },
        hooks: FileHooks {
            fs: big,
            loads: Arc::new(AtomicUsize::new(0)),
        },
    };
    let at_save = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let a2 = Arc::clone(&at_save);
    // The guard runs just before the write; it holds the walk there until `q` has been pressed.
    let guard: disk_cleanup_scan::index::SaveGuard = Arc::new(move |_p| {
        a2.store(true, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(300));
        Ok(())
    });
    let src = WalkerScanSource::new(backend).with_save_guard(guard);
    let h = src.start(&roots(), &ScanOpts::default());
    let t = Instant::now();
    let summary = loop {
        match h.rx.recv_timeout(Duration::from_millis(10)) {
            Ok(ScanMsg::Finished(s)) => break s,
            Ok(ScanMsg::SourceFailed(t)) => panic!("{t}"),
            Ok(_) => {}
            Err(_) => {
                if at_save.load(Ordering::SeqCst) {
                    h.detach();
                }
            }
        }
    };
    assert!(t.elapsed() < Duration::from_secs(10));
    let want = format!("cache not saved to {}: save aborted", cache.display());
    assert!(summary.notes.contains(&want), "{:?}", summary.notes);
    assert_eq!(fs::read(&cache).unwrap(), before, "the old cache is intact");
    no_tmp(tmp.path());
}

// ----- the cache under a full disk and a concurrent writer -----

const FULL_CHILD: &str = "full_tmpfs_child";

fn fixture(base: &Path) -> PathBuf {
    let root = base.join("tree");
    for i in 0..20 {
        fs::create_dir_all(root.join(format!("d{i}/e"))).unwrap();
        fs::write(root.join(format!("d{i}/e/f")), b"x").unwrap();
    }
    root
}

#[test]
fn full_tmpfs_child() {
    let Some(dir) = fullfs::child_dir() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let root = fixture(scratch.path());
    fullfs::fill(&dir);
    let cache_path = dir.join("cache/explorer.cache");
    fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
    let cfg = CacheConfig {
        path: cache_path.clone(),
        rescan: false,
        max_age: DEFAULT_MAX_AGE,
    };

    // The explorer: the policy refuses first (its own note), and the walker's own refusal is the backstop.
    let meter: Arc<dyn SpaceMeter> = Arc::new(RealSpaceMeter::new(Platform::Linux));
    let guarded = WalkerScanSource::new(WalkerBackend {
        cache: Some(cfg.clone()),
    })
    .with_save_guard(save_guard(meter));
    let (_, s) = finish(&guarded.start(&[root.clone()], &ScanOpts::default()));
    assert!(
        s.notes
            .contains(&"cache not saved: disk nearly full".to_string()),
        "{:?}",
        s.notes
    );
    let bare = WalkerScanSource::new(WalkerBackend {
        cache: Some(cfg.clone()),
    });
    let (_, s) = finish(&bare.start(&[root.clone()], &ScanOpts::default()));
    let want = format!("cache not saved to {}: ", cache_path.display());
    assert!(
        s.notes.iter().any(|n| n.starts_with(&want)),
        "{:?}",
        s.notes
    );

    // The cleaner's collect keeps its note text.
    let roots = [root];
    let collected = WalkerBackend { cache: Some(cfg) }
        .collect(&CollectRequest {
            roots: &roots,
            cross_fs: false,
            deadline: None,
            threads: 2,
        })
        .unwrap();
    assert!(
        collected.notes.iter().any(|n| n.starts_with(&want)),
        "{:?}",
        collected.notes
    );

    no_tmp(cache_path.parent().unwrap());
    assert!(!cache_path.exists());
    fullfs::ok(FULL_CHILD);
}

#[test]
fn explore_and_cleaner_should_finish_with_a_note_and_no_temp_file_When_the_cache_disk_is_full() {
    fullfs::run_child("full_tmpfs_child", 1024);
}

#[test]
fn explore_scan_and_cleaner_collect_should_leave_decodable_caches_When_they_run_at_once() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fixture(tmp.path());
    let cdir = tmp.path().join("caches");
    fs::create_dir_all(&cdir).unwrap();
    for same_file in [false, true] {
        let explorer = cdir.join("explorer.cache");
        let walker = if same_file {
            explorer.clone()
        } else {
            cdir.join("walker.cache")
        };
        let cfg = |p: &Path| CacheConfig {
            path: p.to_path_buf(),
            rescan: true,
            max_age: DEFAULT_MAX_AGE,
        };
        std::thread::scope(|s| {
            for _ in 0..3 {
                s.spawn(|| {
                    let src = WalkerScanSource::new(WalkerBackend {
                        cache: Some(cfg(&explorer)),
                    });
                    let (_, sum) = finish(&src.start(&[root.clone()], &ScanOpts::default()));
                    assert!(sum.notes.is_empty(), "{:?}", sum.notes);
                });
                s.spawn(|| {
                    let roots = [root.clone()];
                    let got = WalkerBackend {
                        cache: Some(cfg(&walker)),
                    }
                    .collect(&CollectRequest {
                        roots: &roots,
                        cross_fs: false,
                        deadline: None,
                        threads: 2,
                    })
                    .unwrap();
                    assert!(got.notes.is_empty(), "{:?}", got.notes);
                });
            }
        });
        for p in [&explorer, &walker] {
            assert!(
                cache::decode(&fs::read(p).unwrap()).is_some_and(|m| m.len() == 41),
                "{p:?}"
            );
        }
        no_tmp(&cdir);
    }
}
