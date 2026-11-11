#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 6.3.2 Task 6.3.2a: `FiemapProbe::shared_bytes` against a real btrfs fixture under `$DC_FIXTURE_ROOT`
//! (default `<workspace>/target/dc-fixtures`, never `$HOME`). Skips with a named line off btrfs;
//! `DC_REQUIRE_BTRFS=1` makes that a failure. Nothing is deleted except the test's own temp directory.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use disk_cleanup_adapters::FiemapProbe;
use disk_cleanup_core::ports::{ExtentProbe, SharedEstimate, Unavailable};

const MIB: u64 = 1 << 20;
const BTRFS_MAGIC: u64 = 0x9123_683E;

fn fixture_root() -> Option<PathBuf> {
    let root = std::env::var_os("DC_FIXTURE_ROOT").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/dc-fixtures"),
        PathBuf::from,
    );
    fs::create_dir_all(&root).ok()?;
    let magic = rustix::fs::statfs(&root).ok()?.f_type as u64 & 0xFFFF_FFFF;
    if magic == BTRFS_MAGIC {
        return Some(root);
    }
    assert!(
        std::env::var_os("DC_REQUIRE_BTRFS").is_none_or(|v| v != "1"),
        "DC_REQUIRE_BTRFS=1 but {} is not btrfs",
        root.display()
    );
    eprintln!("SKIP: not btrfs ({})", root.display());
    None
}

fn fixture_dir(root: &Path) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("explore-fiemap-")
        .tempdir_in(root)
        .unwrap()
}

/// Incompressible bytes, so `st_blocks` equals what the file occupies.
fn random_file(path: &Path, mib: u64, seed: u64) {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut f = fs::File::create(path).unwrap();
    let mut block = vec![0_u8; MIB as usize];
    for _ in 0..mib {
        for chunk in block.chunks_mut(8) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            chunk.copy_from_slice(&x.to_le_bytes()[..chunk.len()]);
        }
        f.write_all(&block).unwrap();
    }
    f.sync_all().unwrap();
}

fn run(cmd: &mut Command) {
    let st = cmd.status().unwrap();
    assert!(st.success(), "{cmd:?} failed: {st}");
}

/// Extents of freshly written, unflushed data may not carry the shared flag yet.
fn sync() {
    run(&mut Command::new("sync"));
}

#[allow(clippy::type_complexity)]
fn estimated(e: SharedEstimate) -> (u64, u64, u64, u64, u64, bool) {
    let SharedEstimate::Estimated {
        bytes,
        files_probed,
        unreadable,
        probed_usage,
        dir_usage,
        partial,
    } = e
    else {
        panic!("expected an estimate, got {e:?}");
    };
    (
        bytes,
        files_probed,
        unreadable,
        probed_usage,
        dir_usage,
        partial,
    )
}

#[test]
fn shared_bytes_should_count_both_halves_of_a_reflinked_pair_and_nothing_for_an_unshared_file() {
    let Some(root) = fixture_root() else { return };
    let pair = fixture_dir(&root);
    random_file(&pair.path().join("a"), 8, 1);
    sync();
    run(Command::new("cp")
        .arg("--reflink=always")
        .arg(pair.path().join("a"))
        .arg(pair.path().join("b")));
    sync();
    let alone = fixture_dir(&root);
    random_file(&alone.path().join("solo"), 8, 2);
    sync();

    let (bytes, files, unreadable, _, _, partial) =
        estimated(FiemapProbe.shared_bytes(pair.path(), 10, Duration::from_millis(250)));
    assert_eq!(bytes, 16 * MIB, "the extents of both files carry SHARED");
    assert_eq!((files, unreadable, partial), (2, 0, false));

    let (bytes, files, ..) =
        estimated(FiemapProbe.shared_bytes(alone.path(), 10, Duration::from_millis(250)));
    assert_eq!(
        (bytes, files),
        (0, 1),
        "an unshared file is probed and shares nothing"
    );
}

#[test]
fn shared_bytes_should_probe_only_the_top_n_largest_files() {
    let Some(root) = fixture_root() else { return };
    let dir = fixture_dir(&root);
    random_file(&dir.path().join("big"), 8, 3);
    sync();
    run(Command::new("cp")
        .arg("--reflink=always")
        .arg(dir.path().join("big"))
        .arg(dir.path().join("big2")));
    for n in 0..4 {
        random_file(&dir.path().join(format!("small{n}")), 1, 10 + n);
    }
    sync();
    let (bytes, files, _, probed, usage, _) =
        estimated(FiemapProbe.shared_bytes(dir.path(), 2, Duration::from_millis(250)));
    assert_eq!((bytes, files), (16 * MIB, 2));
    assert!(
        probed < usage,
        "the four small files are in the usage but not in the probe"
    );
}

#[test]
fn shared_bytes_should_return_inside_the_budget_and_say_partial_on_a_huge_directory() {
    let Some(root) = fixture_root() else { return };
    let dir = fixture_dir(&root);
    for n in 0..100_000_u32 {
        fs::File::create(dir.path().join(format!("f{n:06}"))).unwrap();
    }
    sync();
    let t0 = Instant::now();
    let est = FiemapProbe.shared_bytes(dir.path(), 10, Duration::from_millis(250));
    let took = t0.elapsed();
    eprintln!("100,000 files, 250 ms budget: returned in {took:?}: {est:?}");
    assert!(took < Duration::from_millis(1000), "took {took:?}");
    let (_, files, _, _, _, partial) = estimated(est);
    assert!(files <= 10);
    // Whether 100,000 entries fit in 250 ms depends on the machine (222 ms measured here), so `partial` is
    // asserted at a budget no machine meets and only reported at 250 ms.
    eprintln!("partial at 250 ms: {partial}");
    let t0 = Instant::now();
    let est = FiemapProbe.shared_bytes(dir.path(), 10, Duration::from_millis(10));
    let took = t0.elapsed();
    assert!(took < Duration::from_millis(150), "took {took:?}");
    let (_, files, _, _, _, partial) = estimated(est);
    assert!(files <= 10);
    assert!(
        partial,
        "100,000 entries do not fit in 10 ms; the result must say so"
    );
}

#[test]
fn shared_bytes_should_open_only_regular_files_and_count_the_unreadable_one() {
    let Some(root) = fixture_root() else { return };
    let dir = fixture_dir(&root);
    let p = dir.path();
    random_file(&p.join("locked"), 2, 4);
    random_file(&p.join("plain"), 1, 5);
    sync();
    fs::set_permissions(p.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
    if fs::File::open(p.join("locked")).is_ok() {
        eprintln!("SKIP unreadable-file case: permission bits do not stop this user (root?)");
        return;
    }
    run(Command::new("mkfifo").arg(p.join("pipe")));
    std::os::unix::fs::symlink("plain", p.join("link")).unwrap();
    fs::create_dir(p.join("sub")).unwrap();

    // A FIFO opened for reading would block without O_NONBLOCK; reaching the assert is the proof it was not.
    let (bytes, files, unreadable, ..) =
        estimated(FiemapProbe.shared_bytes(p, 10, Duration::from_millis(250)));
    assert_eq!(
        (bytes, files, unreadable),
        (0, 1, 1),
        "plain probed, locked unreadable"
    );
    fs::set_permissions(p.join("locked"), fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn shared_bytes_should_be_unavailable_off_btrfs_before_opening_anything() {
    // /proc is never btrfs.
    let est = FiemapProbe.shared_bytes(Path::new("/proc/self"), 10, Duration::from_millis(250));
    assert_eq!(est, SharedEstimate::Unavailable(Unavailable::NotBtrfs));
}
