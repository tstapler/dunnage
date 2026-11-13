//! Newtypes that keep device ids, inode numbers and byte counts from being mixed up (Story 0.1.3, C1/C2/C6).

use std::os::unix::fs::MetadataExt;

/// `(st_dev, st_ino)` of a filesystem object.
///
/// Comparing it with a bare integer does not compile; go through [`Identity::on_dev`]:
///
/// ```compile_fail
/// use dunnage_core::Identity;
/// let id = Identity { dev: 28, ino: 5 };
/// assert!(28u64 == id);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Identity {
    pub dev: u64,
    pub ino: u64,
}

impl Identity {
    pub fn of(m: &std::fs::Metadata) -> Identity {
        Identity {
            dev: m.dev(),
            ino: m.ino(),
        }
    }

    pub fn on_dev(&self, d: RootDev) -> bool {
        self.dev == d.0
    }
}

/// Birth time (`statx` `stx_btime`; `Metadata::created` elsewhere) of a filesystem object.
///
/// A directory removed and recreated under one name can get the same inode number back (ext4 does); it cannot get
/// the same birth time to the nanosecond, so this tells the two apart where the inode number does not. It is
/// `None` where the filesystem or platform reports no birth time, and then only `(dev, ino)` binds the object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BirthTime {
    pub secs: i64,
    pub nsec: u32,
}

impl BirthTime {
    /// `None` when the platform or filesystem does not report a creation time.
    pub fn of(m: &std::fs::Metadata) -> Option<BirthTime> {
        BirthTime::from_system_time(m.created().ok()?)
    }

    fn from_system_time(t: std::time::SystemTime) -> Option<BirthTime> {
        use std::time::UNIX_EPOCH;
        match t.duration_since(UNIX_EPOCH) {
            Ok(d) => Some(BirthTime {
                secs: i64::try_from(d.as_secs()).ok()?,
                nsec: d.subsec_nanos(),
            }),
            Err(_) => None,
        }
    }
}

/// `st_dev` of the scan root; every descent is compared against it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RootDev(pub u64);

/// Bytes actually allocated on disk (`st_blocks * 512`).
///
/// Allocated and apparent sizes cannot be added to each other:
///
/// ```compile_fail
/// use dunnage_core::{AllocatedBytes, ApparentBytes};
/// let _ = AllocatedBytes(1) + ApparentBytes(2);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct AllocatedBytes(pub u64);

/// File length as `st_size` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct ApparentBytes(pub u64);

/// Steam app id. `>= 2^31` marks a non-Steam shortcut, which is never an orphan.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct AppId(pub u64);

impl AppId {
    pub fn is_non_steam_shortcut(self) -> bool {
        self.0 >= 1 << 31
    }
}

/// Docker object id (image id, volume name, ...). Opaque to everything but the Docker adapter.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct DockerId(pub String);

/// Stable hash of (kind, canonical path or docker id). Printed and stored as 16 lowercase hex digits.
///
/// Not a raw string: a plain `&str` cannot be used where an id is wanted.
///
/// ```compile_fail
/// use dunnage_core::CandidateId;
/// let _: CandidateId = "0123456789abcdef";
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CandidateId(u64);

impl CandidateId {
    /// FNV-1a over the kind id, a separator and the locator; stable across Rust versions (unlike
    /// `DefaultHasher`), which matters because ids are written to the audit log and saved plans.
    pub fn new(kind: crate::kind::Kind, locator: &crate::candidate::Locator) -> CandidateId {
        use crate::candidate::Locator;
        use std::os::unix::ffi::OsStrExt;
        let mut h = Fnv::new();
        h.write(kind.id().as_bytes());
        h.write(&[0]);
        match locator {
            Locator::Path(p) => {
                h.write(b"path\0");
                h.write(p.as_os_str().as_bytes());
            }
            Locator::Docker(d) => {
                h.write(b"docker\0");
                h.write(d.0.as_bytes());
            }
            Locator::Steam(app, p) => {
                h.write(b"steam\0");
                h.write(&app.0.to_le_bytes());
                h.write(p.as_os_str().as_bytes());
            }
        }
        CandidateId(h.0)
    }

    pub fn as_hex(&self) -> String {
        format!("{:016x}", self.0)
    }

    pub fn from_hex(s: &str) -> Option<CandidateId> {
        if s.len() != 16
            || !s
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return None;
        }
        u64::from_str_radix(s, 16).ok().map(CandidateId)
    }
}

impl std::fmt::Display for CandidateId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

impl serde::Serialize for CandidateId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.as_hex())
    }
}

impl<'de> serde::Deserialize<'de> for CandidateId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        CandidateId::from_hex(&s)
            .ok_or_else(|| serde::de::Error::custom("candidate id must be 16 lowercase hex digits"))
    }
}

struct Fnv(u64);

impl Fnv {
    fn new() -> Fnv {
        Fnv(0xcbf2_9ce4_8422_2325)
    }

    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
}
