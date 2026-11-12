#![allow(clippy::unwrap_used)]
//! Review C9: the real read-only `Fs` reports the mount id the engine compares (statx), not `None`.

use dunnage_cli::wiring::env::RealFs;
use dunnage_core::ports::Fs;

#[test]
fn real_fs_lstat_should_report_a_mount_id_for_a_directory_and_a_file_on_linux() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("f"), b"x").unwrap();
    for p in [tmp.path().to_path_buf(), tmp.path().join("f")] {
        let st = RealFs.lstat(&p).unwrap();
        assert_eq!(st.mnt_id.is_some(), cfg!(target_os = "linux"), "{p:?}");
    }
    assert_eq!(
        RealFs.lstat(tmp.path()).unwrap().mnt_id,
        RealFs.lstat(&tmp.path().join("f")).unwrap().mnt_id
    );
}

#[test]
fn real_fs_fs_type_should_name_proc_as_the_proc_magic_on_linux() {
    if cfg!(target_os = "linux") && std::path::Path::new("/proc/self").exists() {
        assert_eq!(RealFs.fs_type(std::path::Path::new("/proc")), Some(0x9fa0));
    }
}
