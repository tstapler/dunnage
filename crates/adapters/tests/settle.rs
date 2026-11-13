#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 1.2.2 settle, against a fixture sysfs directory (T-024, T-025).

use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use dunnage_adapters::settle::settle;
use dunnage_adapters::space_meter::SysfsAllocSource;
use dunnage_adapters::RealSpaceMeter;
use dunnage_core::ports::SpaceMeter;
use dunnage_core::Platform;

const GIB: u64 = 1 << 30;

fn write(p: &Path, v: u64) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, format!("{v}\n")).unwrap();
}

/// A sysfs directory laid out like `/sys/fs/btrfs/<uuid>` plus a mount table that maps `root` onto it.
struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    mountinfo: PathBuf,
    sysfs: PathBuf,
}

fn fixture(fstype: &str, data_used: u64) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = base.join("mnt");
    fs::create_dir_all(&root).unwrap();
    let fs_dir = base.join("sysfs/uuid-1");
    for (class, total, used, disk_total) in [
        ("data", 10 * GIB, data_used, 10 * GIB),
        ("metadata", GIB, GIB / 2, 2 * GIB),
        ("system", 8 << 20, 16 << 10, 16 << 20),
    ] {
        let d = fs_dir.join("allocation").join(class);
        write(&d.join("total_bytes"), total);
        write(&d.join("bytes_used"), used);
        write(&d.join("disk_total"), disk_total);
        write(&d.join("disk_used"), used);
    }
    write(&fs_dir.join("allocation/global_rsv_size"), 1 << 20);
    write(&fs_dir.join("allocation/global_rsv_reserved"), 0);
    write(&fs_dir.join("devices/sda2/size"), 100 * GIB / 512);
    let mountinfo = base.join("mountinfo");
    fs::write(
        &mountinfo,
        format!(
            "36 1 0:26 / {} rw - {fstype} /dev/sda2 rw,subvolid=256\n",
            root.display()
        ),
    )
    .unwrap();
    Fixture {
        root,
        mountinfo,
        sysfs: base.join("sysfs"),
        _dir: dir,
    }
}

fn meter(f: &Fixture) -> RealSpaceMeter {
    RealSpaceMeter::with_source(
        Platform::Linux,
        Box::new(SysfsAllocSource::new(&f.mountinfo, &f.sysfs)),
    )
}

#[test]
fn settle_should_return_without_waiting_for_timeout_When_two_samples_equal() {
    let f = fixture("btrfs", 100);
    let st = meter(&f).settle(&f.root, Duration::from_secs(30));
    // `waited_ms` starts after the filesystem sync, whose duration on a shared runner is not this test's business
    assert!(st.waited_ms < 5_000, "polled for {} ms", st.waited_ms);
    assert!(st.stable);
    assert_eq!(st.samples, 2);
    assert_eq!(st.data_bytes_used, Some(100));

    let syncs = Cell::new(0);
    let reads = Cell::new(0);
    let st = settle(
        || syncs.set(syncs.get() + 1),
        || {
            reads.set(reads.get() + 1);
            Some(100)
        },
        Duration::from_secs(30),
        Duration::from_millis(1),
    );
    assert_eq!(
        (syncs.get(), reads.get()),
        (1, 2),
        "one syncfs, two samples"
    );
    assert!(st.stable);
}

#[test]
fn settle_should_return_last_value_and_note_did_not_stabilise_When_counter_keeps_falling() {
    // samples 1000000, 999990, ... never two equal (room for the 80 samples a 400 ms window takes)
    let next = Cell::new(1_000_000_u64);
    let st = settle(
        || {},
        || {
            next.set(next.get() - 10);
            Some(next.get() + 10)
        },
        Duration::from_millis(400), // wide enough that a loaded runner's coarse sleeps still give 3 samples
        Duration::from_millis(5),
    );
    assert!(!st.stable, "the summary labels this provisional");
    assert!(st.samples >= 3);
    assert_eq!(st.data_bytes_used, Some(next.get() + 10));

    // the real meter over a sysfs file that keeps falling for longer than the 2 s timeout
    let f = fixture("btrfs", 1_000_000);
    let file = f.sysfs.join("uuid-1/allocation/data/bytes_used");
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (file, stop) = (file.clone(), stop.clone());
        thread::spawn(move || {
            let tmp = file.with_extension("tmp");
            let mut v = 1_000_000_u64;
            while !stop.load(Ordering::SeqCst) {
                v -= 1000;
                fs::write(&tmp, format!("{v}\n")).unwrap();
                fs::rename(&tmp, &file).unwrap();
                thread::sleep(Duration::from_millis(20));
            }
        })
    };
    let st = meter(&f).settle(&f.root, Duration::from_secs(2));
    stop.store(true, Ordering::SeqCst);
    writer.join().unwrap();
    assert!(!st.stable);
    assert!(st.data_bytes_used.unwrap() < 1_000_000);
    assert!(st.waited_ms >= 2000, "waited {} ms", st.waited_ms);
}
