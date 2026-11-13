//! The host facts that need no process table: `euid`, `platform`, and the mount table.

use std::io;

use dunnage_core::{MountEntry, Platform};

pub fn euid() -> u32 {
    rustix::process::geteuid().as_raw()
}

pub fn platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::Macos
    } else {
        Platform::Linux
    }
}

/// Linux reads `/proc/self/mountinfo`, macOS asks `getfsstat`. Elsewhere there is no such table:
/// `Unsupported`, so a guard that needs it fails closed.
pub fn mounts() -> io::Result<Vec<MountEntry>> {
    if cfg!(target_os = "linux") {
        let text = std::fs::read_to_string("/proc/self/mountinfo")?;
        Ok(parse_mountinfo(&text))
    } else {
        #[cfg(target_os = "macos")]
        return macos_mounts();
        #[cfg(not(target_os = "macos"))]
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

/// The mounted filesystems through `getfsstat(2)` (thread-safe, unlike `getmntinfo`). `dev` (and `mnt_id`, which
/// macOS does not have) is the `st_dev` of the mount point; a mount point that cannot be stat'ed gets `dev` 0.
#[cfg(target_os = "macos")]
fn macos_mounts() -> io::Result<Vec<MountEntry>> {
    use std::ffi::{CStr, OsStr};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    // SAFETY: a null buffer asks only for the number of mounted filesystems.
    let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    let count = usize::try_from(count).map_err(|_| io::Error::last_os_error())?;
    // a filesystem mounted between the two calls is missed or fits in the slack
    let mut rows: Vec<libc::statfs> = Vec::with_capacity(count + 8);
    let bytes = libc::c_int::try_from(rows.capacity() * std::mem::size_of::<libc::statfs>())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: `rows` has room for `bytes` bytes; getfsstat initialises that many rows' worth at most and returns
    // the number of rows it wrote, and `statfs` is plain old data with no invalid bit patterns
    let filled = unsafe { libc::getfsstat(rows.as_mut_ptr(), bytes, libc::MNT_NOWAIT) };
    let filled = usize::try_from(filled).map_err(|_| io::Error::last_os_error())?;
    // SAFETY: the first `filled` rows were written by getfsstat, and `filled` is at most the capacity it was given
    unsafe { rows.set_len(filled.min(rows.capacity())) };
    Ok(rows
        .iter()
        .map(|r| {
            // SAFETY: `f_mntonname` is a NUL-terminated C string inside the struct
            let on = unsafe { CStr::from_ptr(r.f_mntonname.as_ptr()) };
            let mount_point = std::path::PathBuf::from(OsStr::from_bytes(on.to_bytes()));
            let dev = std::fs::metadata(&mount_point).map_or(0, |m| m.dev());
            // macOS has no mount ids; the device of the mount point tells two mounts apart
            MountEntry {
                mount_point,
                dev,
                mnt_id: dev,
            }
        })
        .collect())
}

/// Kernel `new_encode_dev`, so `dev` has the layout of an `st_dev`. It is still never compared with one: the
/// device field is `0:26` for every subvolume of one btrfs filesystem (spike 0.2.1).
fn encode_dev(major: u64, minor: u64) -> u64 {
    (minor & 0xff) | (major << 8) | ((minor & !0xff) << 12)
}

/// `\040`-style escapes of a mountinfo path field.
fn unescape(field: &str) -> String {
    let b = field.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let oct = b
            .get(i + 1..i + 4)
            .filter(|d| d.iter().all(|c| (b'0'..=b'7').contains(c)));
        match (b[i], oct) {
            (b'\\', Some(d)) => {
                let v = d.iter().fold(0u32, |a, c| a * 8 + u32::from(c - b'0'));
                out.push(u8::try_from(v).unwrap_or(b'?'));
                i += 4;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One row per well-formed line: `mount_id parent_id major:minor root mount_point ...`. A malformed line is
/// skipped, not guessed.
pub fn parse_mountinfo(text: &str) -> Vec<MountEntry> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.split(' ');
            let mnt_id: u64 = f.next()?.parse().ok()?;
            let _parent = f.next()?;
            let (major, minor) = f.next()?.split_once(':')?;
            let _root = f.next()?;
            let mount_point = f.next()?;
            Some(MountEntry {
                mount_point: unescape(mount_point).into(),
                dev: encode_dev(major.parse().ok()?, minor.parse().ok()?),
                mnt_id,
            })
        })
        .collect()
}

/// Mount point and source (`/dev/sdb1`, `tmpfs`) of every well-formed `mountinfo` line, for the explorer's
/// mount-point notes. The source is the field after the ` - ` separator and its filesystem type.
pub fn parse_mount_sources(text: &str) -> Vec<(std::path::PathBuf, String)> {
    text.lines()
        .filter_map(|line| {
            let (head, tail) = line.split_once(" - ")?;
            let mount_point = head.split(' ').nth(4)?;
            let source = tail.split(' ').nth(1)?;
            Some((unescape(mount_point).into(), unescape(source)))
        })
        .collect()
}

/// [`parse_mount_sources`] over `/proc/self/mountinfo`.
pub fn mount_sources() -> io::Result<Vec<(std::path::PathBuf, String)>> {
    let text = std::fs::read_to_string("/proc/self/mountinfo")?;
    Ok(parse_mount_sources(&text))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const FIXTURE: &str = "\
22 28 0:21 / /sys rw,nosuid,nodev,noexec,relatime shared:7 - sysfs sysfs rw
26 1 0:26 /@ / rw,relatime shared:1 - btrfs /dev/nvme0n1p2 rw,ssd,subvolid=256,subvol=/@
31 26 0:26 /@var_log /var/log rw,relatime shared:2 - btrfs /dev/nvme0n1p2 rw,subvol=/@var_log
77 26 259:3 /x /mnt/my\\040disk rw,relatime shared:9 - ext4 /dev/nvme0n1p3 rw
garbage line
88 26 x:y /x /bad rw - ext4 a b
";

    #[test]
    fn parse_mount_sources_should_return_point_and_source_and_unescape_both() {
        let got = parse_mount_sources(FIXTURE);
        assert_eq!(got[0], ("/sys".into(), "sysfs".to_string()));
        assert_eq!(got[1], ("/".into(), "/dev/nvme0n1p2".to_string()));
        assert!(got.contains(&("/mnt/my disk".into(), "/dev/nvme0n1p3".to_string())));
        assert!(!got.iter().any(|(p, _)| p.to_string_lossy() == "garbage"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mounts_should_list_the_root_filesystem_When_run_on_macos() {
        let m = mounts().unwrap();
        assert!(
            m.iter().any(|e| e.mount_point == std::path::Path::new("/")),
            "{m:?}"
        );
    }

    #[test]
    fn parse_mountinfo_should_return_point_device_and_mount_id_and_skip_malformed_lines() {
        let rows = parse_mountinfo(FIXTURE);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[1].mount_point, std::path::Path::new("/"));
        assert_eq!(rows[1].mnt_id, 26);
        assert_eq!(rows[1].dev, 26);
        assert_eq!(rows[2].mount_point, std::path::Path::new("/var/log"));
        assert_eq!(
            rows[2].dev, rows[1].dev,
            "subvolumes share the device field"
        );
        assert_eq!(rows[3].mount_point, std::path::Path::new("/mnt/my disk"));
        assert_eq!(rows[3].dev, (259 << 8) | 3);
    }

    #[test]
    fn euid_should_match_the_owner_of_proc_self_and_platform_should_match_the_target() {
        assert_eq!(euid(), proc_self_uid());
        assert_eq!(platform() == Platform::Macos, cfg!(target_os = "macos"));
    }

    fn proc_self_uid() -> u32 {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata("/proc/self").map_or_else(|_| euid(), |m| m.uid())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mounts_should_list_the_root_mount_on_linux() {
        let m = mounts().unwrap();
        assert!(m.iter().any(|e| e.mount_point == std::path::Path::new("/")));
    }
}
