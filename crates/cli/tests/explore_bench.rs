#![cfg(feature = "explore")]
#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 6.5.6, Task 6.5.6b: the measurements behind `scripts/bench-explore.sh` (Gate F, "it stays responsive
//! on the largest tree"). The real binary runs on a pty over (1) a synthetic explorer cache of 676,296
//! directories shaped like the one measured in SA s3 (8-ary heap, 47 MB on disk), laid out as a real tree of
//! the same directories below a temp directory, and is run cold (no cache), against the synthetic cache (a stale
//! cache: every directory is listed again) and warm (the cache its own cold run saved). Each measurement prints a `BENCH name=value` line; the script asserts the bounds and
//! writes them to `research/spikes.md`.
//!
//! Without `DC_BENCH_EXPLORE=1` this is a smoke test over 2,000 directories that asserts nothing about time:
//! it only keeps the harness compiling and working. With it, `DC_BENCH_EXPLORE_DIRS` (directories, default
//! 676,296) and `DC_BENCH_BIN` (the binary; default the test build) pick the size and the binary. `DC_BENCH_REAL_HOME=1` (the owner's
//! machine) scans the real `$HOME` as a third, cold run.
//!
//! The "frame" is one `ratatui` draw as it reaches the pty: it ends with the default-colour resets
//! `ESC[39m ESC[49m ESC[59m ESC[0m`. The longest frame time is the longest wait from a key press to the next
//! frame while the scan runs (the loop's own time per frame is not visible from outside); the draw rate is
//! the frames per second of a scan with no key.

mod common;
mod explore_pty_rig;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use disk_cleanup_adapters::explore::cache_paint::{
    cache_config, explorer_cache_path, CacheMode, DEFAULT_MAX_AGE,
};
use disk_cleanup_adapters::explore::scan_source::WalkerScanSource;
use disk_cleanup_core::explore::fmt::fmt_size_cell;
use disk_cleanup_core::explore::msg::{ScanMode, ScanMsg};
use disk_cleanup_core::explore::ports::{ScanOpts, ScanSource};
use disk_cleanup_core::explore::tree::ExploreTree;
use disk_cleanup_core::Dirs;
use disk_cleanup_scan::cache::{self, Rec};
use disk_cleanup_scan::estimate::Stats;
use disk_cleanup_scan::walker::WalkerBackend;
use explore_pty_rig::{find, fuzzy_find, screen_text_of, Pty, ENTER_ALT};

const FRAME_END: &[u8] = b"\x1b[39m\x1b[49m\x1b[59m\x1b[0m";
const HEAP_ARITY: u64 = 8;
/// The sparse file in every 8th directory: the apparent total is hundreds of TiB, so its one-decimal title
/// figure moves by far less than one file and shows when the paint is complete.
const FILE_BYTES: u64 = 5_000_000_000;

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn bench_on() -> bool {
    std::env::var_os("DC_BENCH_EXPLORE").is_some_and(|v| v == "1")
}

fn bin() -> PathBuf {
    std::env::var_os("DC_BENCH_BIN").map_or_else(common::bin_path, PathBuf::from)
}

fn report(name: &str, value: impl std::fmt::Display) {
    println!("BENCH {name}={value}");
}

struct Env {
    _dir: tempfile::TempDir,
    home: PathBuf,
    xdg: PathBuf,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let xdg = dir.path().join("xdg");
        for d in [
            home.join("code"),
            xdg.join("state"),
            xdg.join("cache"),
            xdg.join("config"),
            dir.path().join("pty"),
        ] {
            fs::create_dir_all(&d).unwrap();
        }
        Env {
            _dir: dir,
            home,
            xdg,
        }
    }

    fn pty_dir(&self) -> PathBuf {
        self._dir.path().join("pty")
    }

    fn dirs(&self) -> Dirs {
        Dirs {
            home: self.home.clone(),
            state: self.xdg.join("state"),
            cache: self.xdg.join("cache"),
            config: self.xdg.join("config"),
        }
    }

    fn command(&self, home: &Path) -> std::process::Command {
        let mut c = std::process::Command::new("sh");
        c.env_clear();
        c.env("PATH", std::env::var_os("PATH").unwrap_or_default());
        c.env("HOME", home);
        c.env("XDG_STATE_HOME", self.xdg.join("state"));
        c.env("XDG_CACHE_HOME", self.xdg.join("cache"));
        c.env("XDG_CONFIG_HOME", self.xdg.join("config"));
        c.env("LANG", "C");
        c
    }
}

/// The records of an 8-ary heap of `total` directories below `root` (`d{n}` is node n; node 0 is `root`).
fn heap_cache(root: &Path, total: u64) -> HashMap<PathBuf, Rec> {
    let path_of = |n: u64| -> PathBuf {
        let mut chain = Vec::new();
        let mut cur = n;
        while cur != 0 {
            chain.push(cur);
            cur = (cur - 1) / HEAP_ARITY;
        }
        let mut p = root.to_path_buf();
        for c in chain.iter().rev() {
            p.push(format!("d{c}"));
        }
        p
    };
    (0..total)
        .map(|n| {
            let first = n * HEAP_ARITY + 1;
            let kids = total.saturating_sub(first).min(HEAP_ARITY);
            let has_file = n != 0 && n % 8 == 0;
            let rec = Rec {
                ino: n + 100,
                mtime_sec: 1_000_000,
                mtime_nsec: 0,
                listed_at: 1,
                files: Stats {
                    inodes: 1 + u64::from(has_file),
                    dirs: 1,
                    files: u64::from(has_file),
                    size_bytes: if has_file { FILE_BYTES } else { 0 },
                    ..Stats::default()
                },
                has_package_json: false,
                has_cargo_toml: false,
                git_file: false,
                subdirs: (0..kids)
                    .map(|i| format!("d{}", first + i).into_bytes())
                    .collect(),
            };
            (path_of(n), rec)
        })
        .collect()
}

/// Loads the cache the way the explorer's paint does, then lets the live walk finish, and returns the arena's
/// bytes per directory when the paint is complete (revalidating: the bound is 200) and at the end (fresh: 160),
/// with the seconds the load and paint took in this (debug) test build.
fn heap_per_directory(env: &Env, root: &Path, total: u64) -> (f64, f64, f64) {
    let backend = WalkerBackend {
        cache: cache_config(&env.dirs(), CacheMode::Use, DEFAULT_MAX_AGE),
    };
    let start = Instant::now();
    let h = WalkerScanSource::new(backend).start(
        &[root.to_path_buf()],
        &ScanOpts {
            paint_from_cache: true,
            ..ScanOpts::default()
        },
    );
    let mut tree = ExploreTree::new();
    let (mut painted, mut painted_secs, mut painted_heap) = (0u64, 0.0, 0.0);
    let per = |t: &ExploreTree| t.heap_bytes() as f64 / t.directories().max(1) as f64;
    loop {
        match h.rx.recv_timeout(Duration::from_secs(900)).unwrap() {
            ScanMsg::Batch {
                generation,
                mode,
                events,
            } => {
                let paint = mode == ScanMode::Paint;
                if paint {
                    painted += events.len() as u64;
                }
                tree.apply_batch(generation, mode, events).unwrap();
                if paint && painted == total {
                    painted_secs = start.elapsed().as_secs_f64();
                    painted_heap = per(&tree);
                }
            }
            ScanMsg::Finished(_) => break,
            ScanMsg::SourceFailed(t) => panic!("{t}"),
            _ => {}
        }
    }
    h.detach();
    (painted_heap, per(&tree), painted_secs)
}

fn frames_in(bytes: &[u8]) -> usize {
    explore_pty_rig::count(bytes, FRAME_END)
}

fn vm(pid: u32, field: &str) -> Option<u64> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = status.lines().find(|l| l.starts_with(field))?;
    line.split_whitespace().nth(1)?.parse::<u64>().ok()
}

struct Timeline {
    alt: Option<Duration>,
    first_frame: Option<Duration>,
}

/// Reads the pty until `done(screen)` holds, noting when the alternate screen and the first frame arrived.
fn watch(
    p: &mut Pty,
    t0: Instant,
    limit: Duration,
    done: &dyn Fn(&str) -> bool,
) -> (Timeline, Duration) {
    let mut tl = Timeline {
        alt: None,
        first_frame: None,
    };
    loop {
        p.pump();
        let now = t0.elapsed();
        if tl.alt.is_none() && find(&p.seen, ENTER_ALT).is_some() {
            tl.alt = Some(now);
        }
        if tl.first_frame.is_none() && frames_in(&p.seen) > 0 {
            tl.first_frame = Some(now);
        }
        if done(&screen_text_of(&p.seen)) {
            return (tl, now);
        }
        assert!(
            now < limit,
            "the screen never reached the wanted state in {limit:?}: {:?}",
            {
                let t = screen_text_of(&p.seen);
                t[t.len().saturating_sub(1800)..].to_string()
            }
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Presses `j` every `every` while the scan runs and returns the longest wait for the next frame.
fn key_latency_during(p: &mut Pty, until: &dyn Fn(&str) -> bool, every: Duration) -> Duration {
    let mut longest = Duration::ZERO;
    let end = Instant::now() + Duration::from_secs(300);
    while Instant::now() < end && !until(&screen_text_of(&p.seen)) {
        p.pump();
        let before = frames_in(&p.seen);
        let sent = Instant::now();
        p.send(b"j");
        loop {
            p.pump();
            if frames_in(&p.seen) > before {
                longest = longest.max(sent.elapsed());
                break;
            }
            if sent.elapsed() > Duration::from_secs(5) || until(&screen_text_of(&p.seen)) {
                break;
            }
            std::thread::sleep(Duration::from_micros(500));
        }
        std::thread::sleep(every);
    }
    longest
}

fn quit(p: &mut Pty) {
    p.send(b"q");
    let _ = p.exit_code();
}

struct Run {
    first_frame_ms: u128,
    painted_ms: Option<u128>,
    scan_s: f64,
    draws_per_s: f64,
    peak_rss_mib: u64,
}

/// One session over `root`: spawn, wait for the first frame, optionally for the painted cache (`want` on the
/// title), then for the end of the scan, counting frames; `keys` presses `j` all along and returns the longest
/// key-to-frame wait instead of the frame rate. `q` ends it.
fn session(env: &Env, root: &Path, want: Option<&str>, keys: bool) -> (Run, Duration) {
    let t0 = Instant::now();
    let mut p = Pty::spawn_with(
        env.command(&env.home),
        &bin(),
        &[root],
        &env.pty_dir(),
        30,
        100,
    );
    let limit = Duration::from_secs(900);
    let (tl, _) = watch(&mut p, t0, limit, &|s| {
        s.contains("SCANNING") || s.contains("SCAN D")
    });
    let (window, started) = (p.seen.len(), Instant::now());
    let painted_ms = want.map(|w| {
        watch(&mut p, t0, limit, &|s| {
            s.contains(w) || fuzzy_find(s, w).is_some()
        })
        .1
        .as_millis()
    });
    let pid = p.binary_pid();
    let mut latency = Duration::ZERO;
    if keys {
        latency = key_latency_during(&mut p, &|s| s.contains("SCAN D"), Duration::from_millis(30));
    }
    watch(&mut p, t0, limit, &|s| s.contains("SCAN D"));
    let scan_s = t0.elapsed().as_secs_f64();
    let frames = frames_in(&p.seen[window..]) as f64;
    let draws_per_s = frames / started.elapsed().as_secs_f64().max(0.001);
    let rss = vm(pid, "VmHWM:").unwrap_or(0) / 1024;
    // Let the cache save finish before the next run reads it.
    quit(&mut p);
    (
        Run {
            first_frame_ms: tl.first_frame.map_or(0, |d| d.as_millis()),
            painted_ms,
            scan_s,
            draws_per_s,
            peak_rss_mib: rss,
        },
        latency,
    )
}

fn print_run(label: &str, r: &Run) {
    report(&format!("{label}_first_frame_ms"), r.first_frame_ms);
    if let Some(ms) = r.painted_ms {
        report(&format!("{label}_painted_ms"), ms);
    }
    report(&format!("{label}_scan_done_s"), format!("{:.2}", r.scan_s));
    report(
        &format!("{label}_draws_per_s"),
        format!("{:.2}", r.draws_per_s),
    );
    report(&format!("{label}_peak_rss_mib"), r.peak_rss_mib);
}

/// `d{n}` below `root` for n in 1..total (8-ary heap order); every 8th directory holds a sparse file of
/// [`FILE_BYTES`], which makes the apparent total large without using space.
fn make_tree(root: &Path, total: u64) -> u64 {
    let mut files = 0u64;
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    for n in 1..total {
        let parent = (n - 1) / HEAP_ARITY;
        let mut p = stack[parent as usize].clone();
        p.push(format!("d{n}"));
        fs::create_dir(&p).unwrap();
        if n % 8 == 0 {
            fs::File::create(p.join("f"))
                .unwrap()
                .set_len(FILE_BYTES)
                .unwrap();
            files += 1;
        }
        stack.push(p);
    }
    files
}

#[test]
fn the_bench_harness_should_measure_the_cold_stale_and_warm_runs_on_the_large_tree() {
    let on = bench_on();
    let total = if on {
        env_u64("DC_BENCH_EXPLORE_DIRS", 676_296)
    } else {
        2_000
    };
    let env = Env::new();
    let root = fs::canonicalize(env.home.join("code")).unwrap();
    let t = Instant::now();
    let files = make_tree(&root, total);
    report("tree_dirs", total);
    report("tree_build_s", format!("{:.1}", t.elapsed().as_secs_f64()));
    // The scan counts the root as a directory too; the apparent total is the files' alone.
    let want = format!("{} apparent", fmt_size_cell(files * FILE_BYTES));
    let file = explorer_cache_path(&env.dirs());
    fs::create_dir_all(file.parent().unwrap()).unwrap();

    // The synthetic cache: the same paths, other mtimes, so the live walk lists every directory again.
    cache::save(&file, &heap_cache(&root, total)).unwrap();
    report("cache_file_bytes", fs::metadata(&file).unwrap().len());
    let pristine = file.with_extension("pristine");
    fs::copy(&file, &pristine).unwrap();
    let (painted_heap, fresh_heap, inproc_secs) = heap_per_directory(&env, &root, total);
    report("heap_bytes_per_dir_painted", format!("{painted_heap:.1}"));
    report("heap_bytes_per_dir_fresh", format!("{fresh_heap:.1}"));
    report(
        "cache_load_and_paint_inprocess_debug_s",
        format!("{inproc_secs:.2}"),
    );
    fs::copy(&pristine, &file).unwrap();
    let (stale, latency) = session(&env, &root, Some(&want), true);
    print_run("stale", &stale);
    report("stale_key_to_frame_max_ms", latency.as_millis());

    let _ = fs::remove_file(&file);
    let (cold, _) = session(&env, &root, None, false);
    print_run("cold", &cold);
    assert!(file.exists(), "the cold run saved no cache");
    let (warm, _) = session(&env, &root, Some(&want), false);
    print_run("warm", &warm);

    let _ = fs::remove_file(&file);
    let (_, latency) = session(&env, &root, None, true);
    report("cold_key_to_frame_max_ms", latency.as_millis());

    if on && std::env::var_os("DC_BENCH_REAL_HOME").is_some_and(|v| v == "1") {
        let real = std::env::var_os("DC_BENCH_REAL_HOME_PATH").map_or_else(
            || PathBuf::from(std::env::var_os("HOME").unwrap()),
            PathBuf::from,
        );
        let real_env = Env::new();
        let _ = fs::remove_file(explorer_cache_path(&real_env.dirs()));
        let (cold, _) = session(&real_env, &real, None, false);
        print_run("real_home_cold", &cold);
        let (warm, _) = session(&real_env, &real, None, false);
        print_run("real_home_warm", &warm);
    }
}
