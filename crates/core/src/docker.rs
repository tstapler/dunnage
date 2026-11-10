//! Docker domain logic (Story 4.1.1): the typed listings, the `DockerClient` port and the pure gate
//! decisions. The CLI adapter (`disk-cleanup-adapters::docker_cli`) fills these from `docker ... --format`
//! output; nothing here runs a process (ADR-001: docker through the CLI, not a socket client).

use std::collections::BTreeMap;

use crate::outcome::SkipReason;
use crate::target::KindGate;

/// Major versions of the Docker server whose output shapes were recorded (spike 0.2.2: 29.8.2). Any other
/// major is listed with `DockerVersionUntested` and nothing in it is removable.
pub const TESTED_SERVER_MAJORS: &[u32] = &[29];

/// The label Docker puts on a volume it created for a `VOLUME` or `-v /path` with no name.
pub const ANONYMOUS_LABEL: &str = "com.docker.volume.anonymous";

/// `docker context show` names that always mean the local daemon. The CLI endpoint is not readable under the
/// command allowlist (`docker context inspect` is not a row), so any other context is treated as remote.
pub const LOCAL_CONTEXTS: &[&str] = &["default", "desktop-linux", "colima", "orbstack"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerImage {
    /// The short id printed by `docker images` (12 hex digits).
    pub id: String,
    /// `None` for a dangling image (`<none>`).
    pub repository: Option<String>,
    pub tag: Option<String>,
    /// `None` for a locally built image (`<none>`); a `sha256:` value for a pulled one.
    pub digest: Option<String>,
    /// Decimal-SI size rounded by Docker to three or four digits: approximate.
    pub size_bytes: u64,
    /// As printed, for example `2026-11-01 18:36:31 -0800 PST`.
    pub created_at: String,
    /// Containers using the image when Docker knows (`N/A` is `None`).
    pub containers: Option<u32>,
}

impl DockerImage {
    pub fn is_dangling(&self) -> bool {
        self.repository.is_none() && self.tag.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerContainer {
    /// Short id from `docker ps -a`.
    pub id: String,
    pub names: String,
    /// Volume names and bind sources, comma separated as printed (anonymous names are truncated).
    pub mounts: String,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerVolume {
    pub name: String,
    pub driver: String,
    pub mountpoint: String,
    pub labels: BTreeMap<String, String>,
    pub anonymous: bool,
    /// From `system df -v` only, decimal SI rounded to four digits: approximate, never an exact-bytes input.
    /// `None` when the volume is absent from the df listing.
    pub approx_size_bytes: Option<u64>,
    /// Containers referencing the volume, from `system df -v` (`Links`).
    pub links: Option<u32>,
    /// `volume inspect` only; the plain listing has none.
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerBuildCacheEntry {
    pub id: String,
    pub in_use: bool,
    pub size_bytes: u64,
}

/// Everything one discovery pass reads, already cross-checked.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DockerInventory {
    pub images: Vec<DockerImage>,
    pub containers: Vec<DockerContainer>,
    pub volumes: Vec<DockerVolume>,
    pub build_cache: Vec<DockerBuildCacheEntry>,
}

/// What the probe decided and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerProbe {
    pub gate: KindGate,
    /// The `<reason>` of the `SKIPPED` line or the untested-version note.
    pub detail: Option<String>,
    /// The server version when it was read.
    pub server_version: Option<String>,
}

impl DockerProbe {
    pub fn available(version: String) -> DockerProbe {
        DockerProbe {
            gate: KindGate::Available,
            detail: None,
            server_version: Some(version),
        }
    }

    pub fn unavailable(reason: SkipReason, detail: impl Into<String>) -> DockerProbe {
        DockerProbe {
            gate: KindGate::Unavailable(reason),
            detail: Some(detail.into()),
            server_version: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DockerError {
    /// The command ran and failed, or timed out: the daemon is not answering.
    #[error("docker daemon not reachable: {0}")]
    Daemon(String),
    /// A shape changed or two listings disagree: nothing may be judged from them.
    #[error("unrecognised docker output: {0}")]
    Unrecognised(String),
}

impl DockerError {
    pub fn gate(&self) -> KindGate {
        match self {
            DockerError::Daemon(_) => KindGate::Unavailable(SkipReason::DaemonUnavailable),
            DockerError::Unrecognised(_) => KindGate::Unavailable(SkipReason::UnrecognisedOutput),
        }
    }
}

/// The port the Docker kinds read through. All methods are read-only.
pub trait DockerClient: Send + Sync {
    /// Context, daemon and version checks, in that order; stops at the first failure.
    fn probe(&self) -> DockerProbe;
    fn containers(&self) -> Result<Vec<DockerContainer>, DockerError>;
    fn images(&self) -> Result<Vec<DockerImage>, DockerError>;
    /// `volume ls` joined with the size and reference count of `system df -v`.
    fn volumes(&self) -> Result<Vec<DockerVolume>, DockerError>;
    fn build_cache(&self) -> Result<Vec<DockerBuildCacheEntry>, DockerError>;
    /// `volume inspect <name>`: the volume with `created_at` filled.
    fn volume_inspect(&self, name: &str) -> Result<DockerVolume, DockerError>;
    /// One pass over every listing; fails closed when `ps -a` and `system df -v` disagree on the number of
    /// containers (an empty `ps -a` must never read as "nothing references this volume").
    fn inventory(&self) -> Result<DockerInventory, DockerError>;
}

/// The gate for a server version string: tested majors are `Available`, any other is `ListOnly`.
pub fn version_gate(version: &str) -> KindGate {
    match major_of(version) {
        Some(m) if TESTED_SERVER_MAJORS.contains(&m) => KindGate::Available,
        Some(_) => KindGate::ListOnly(SkipReason::DockerVersionUntested),
        None => KindGate::Unavailable(SkipReason::UnrecognisedOutput),
    }
}

fn major_of(version: &str) -> Option<u32> {
    version.trim().split('.').next()?.parse().ok()
}

/// True when `context` (the output of `docker context show`) is a known-local context and `docker_host`
/// (the `DOCKER_HOST` variable) is unset or a unix socket.
pub fn is_local_context(context: &str, docker_host: Option<&str>) -> bool {
    let host_ok = docker_host.is_none_or(|h| h.is_empty() || h.starts_with("unix://"));
    host_ok && LOCAL_CONTEXTS.contains(&context.trim())
}

/// Parses a size as `docker` prints it (`1.049MB`, `0B`, `117.5MB`, `1.5kB`) in decimal SI units.
/// `N/A` and anything else is `None`.
pub fn parse_si_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let (num, unit) = s.split_at(split);
    let mult: f64 = match unit {
        "B" => 1.0,
        "kB" | "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        "PB" => 1e15,
        _ => return None,
    };
    let n: f64 = num.parse().ok()?;
    if !n.is_finite() || n < 0.0 {
        return None;
    }
    Some((n * mult).round() as u64)
}

/// `k=v,k=v` as `volume ls` and `system df -v` print labels; an empty string is no labels. A value that
/// holds a comma is split wrongly (the format is ambiguous), which can only add keys, never hide
/// the anonymous marker.
pub fn parse_label_string(s: &str) -> BTreeMap<String, String> {
    s.split(',')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => (p.to_string(), String::new()),
        })
        .collect()
}

pub fn is_anonymous(labels: &BTreeMap<String, String>) -> bool {
    labels.contains_key(ANONYMOUS_LABEL)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn si_sizes() {
        assert_eq!(parse_si_size("1.049MB"), Some(1_049_000));
        assert_eq!(parse_si_size("0B"), Some(0));
        assert_eq!(parse_si_size("117.5MB"), Some(117_500_000));
        assert_eq!(parse_si_size("1.5kB"), Some(1_500));
        assert_eq!(parse_si_size("N/A"), None);
        assert_eq!(parse_si_size("12"), None);
        assert_eq!(parse_si_size("1.2MiB"), None);
    }

    #[test]
    fn versions() {
        assert_eq!(version_gate("29.8.2"), KindGate::Available);
        assert_eq!(
            version_gate("30.0.1"),
            KindGate::ListOnly(SkipReason::DockerVersionUntested)
        );
        assert_eq!(
            version_gate("28.5.1"),
            KindGate::ListOnly(SkipReason::DockerVersionUntested)
        );
        assert!(version_gate("").is_unavailable());
    }

    #[test]
    fn labels() {
        let l = parse_label_string("com.docker.volume.anonymous=");
        assert!(is_anonymous(&l));
        assert!(parse_label_string("").is_empty());
    }

    #[test]
    fn contexts() {
        assert!(is_local_context("default\n", None));
        assert!(is_local_context(
            "default",
            Some("unix:///var/run/docker.sock")
        ));
        assert!(!is_local_context("default", Some("tcp://10.0.0.1:2375")));
        assert!(!is_local_context("remote-ci", None));
    }
}
