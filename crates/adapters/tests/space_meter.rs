#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dunnage_adapters::space_meter::{estimate_is_lower_bound, AllocSource, SysfsAllocSource};
use dunnage_adapters::RealSpaceMeter;
use dunnage_core::ports::SpaceMeter;
use dunnage_core::testing::FakeProbe;
use dunnage_core::{AllocSnapshot, Platform};

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
fn snapshot_should_map_data_metadata_system_unallocated_When_root_is_btrfs() {
    let f = fixture("btrfs", 6 * GIB);
    let s = meter(&f).snapshot(&f.root);
    assert_eq!(s.fs_type, "btrfs");
    let a = s.btrfs.expect("btrfs allocation");
    assert_eq!((a.data.bytes_used, a.data.total_bytes), (6 * GIB, 10 * GIB));
    assert_eq!(
        (a.metadata.bytes_used, a.metadata.total_bytes),
        (GIB / 2, GIB)
    );
    assert_eq!(
        (a.system.bytes_used, a.system.total_bytes),
        (16 << 10, 8 << 20)
    );
    assert_eq!(a.unallocated, 100 * GIB - (10 * GIB + 2 * GIB + (16 << 20)));
    assert!(s.total_bytes > 0 && s.avail_bytes <= s.total_bytes);
}

#[test]
fn snapshot_should_map_allocation_When_the_real_root_is_btrfs() {
    let real = SysfsAllocSource::default();
    let m = RealSpaceMeter::new(Platform::Linux);
    let s = m.snapshot(Path::new("/"));
    if real.fs_type(Path::new("/")).as_deref() != Some("btrfs") {
        eprintln!("skipped: / is {}, not btrfs", s.fs_type);
        return;
    }
    assert_eq!(s.fs_type, "btrfs");
    let a = s.btrfs.expect("allocation read from the real sysfs");
    assert!(a.data.total_bytes >= a.data.bytes_used && a.data.total_bytes > 0);
    assert!(s.total_bytes > 0);
}

#[test]
fn snapshot_should_return_none_btrfs_and_print_n_a_with_fstype_When_tmpfs() {
    let f = fixture("tmpfs", 0);
    let s = meter(&f).snapshot(&f.root);
    assert_eq!(s.fs_type, "tmpfs");
    assert_eq!(s.btrfs, None);
    assert!(s.total_bytes > 0);
    assert_eq!(
        format!("btrfs: n/a (filesystem is {})", s.fs_type),
        "btrfs: n/a (filesystem is tmpfs)"
    );
}

#[test]
fn snapshot_should_return_none_btrfs_When_the_sysfs_read_is_denied() {
    let f = fixture("btrfs", 6 * GIB);
    fs::remove_dir_all(f.sysfs.join("uuid-1/allocation")).unwrap();
    let s = meter(&f).snapshot(&f.root);
    assert_eq!(s.fs_type, "btrfs");
    assert_eq!(s.btrfs, None);
    assert!(s.total_bytes > 0);
}

struct CountingSource(Arc<AtomicUsize>);

impl AllocSource for CountingSource {
    fn fs_type(&self, _: &Path) -> Option<String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Some("btrfs".into())
    }
    fn read_allocation(&self, _: &Path) -> Result<AllocSnapshot, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err("must not be called".into())
    }
}

#[test]
fn snapshot_should_not_read_allocation_When_platform_is_macos() {
    let calls = Arc::new(AtomicUsize::new(0));
    let probe = FakeProbe::new().with_platform(Platform::Macos);
    let m = RealSpaceMeter::with_source(
        dunnage_core::ports::SystemProbe::platform(&probe),
        Box::new(CountingSource(calls.clone())),
    );
    let dir = tempfile::tempdir().unwrap();
    let s = m.snapshot(dir.path());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(s.btrfs, None);
    assert!(!s.fs_type.is_empty());
}

#[test]
fn settle_should_report_stable_When_two_samples_agree() {
    let f = fixture("btrfs", 6 * GIB);
    let st = meter(&f).settle(&f.root, Duration::from_secs(2));
    assert!(st.stable);
    assert_eq!(st.data_bytes_used, Some(6 * GIB));
    assert_eq!(st.samples, 2);
}

#[test]
fn rootless_snapshot_should_flag_lower_bound_When_euid_not_root() {
    assert!(estimate_is_lower_bound(1000));
    assert!(!estimate_is_lower_bound(0));
}
