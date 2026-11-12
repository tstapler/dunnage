//! `DockerCli`: the Docker listings through `docker ... --format` over a `CommandRunner` (Story 4.1.1,
//! ADR-001). Every wire struct requires its fields (no `#[serde(default)]`, no `Option` for a field that
//! must be present) and ignores unknown ones, so a changed output shape is an error and never a shorter
//! list (pre-mortem P1.3). Only read-only allowlist rows are used; this module never mutates.

use std::collections::BTreeMap;

use dunnage_core::docker::{
    is_anonymous, is_local_context, parse_label_string, parse_si_size, version_gate,
    DockerBuildCacheEntry, DockerClient, DockerContainer, DockerError, DockerImage,
    DockerInventory, DockerProbe, DockerVolume,
};
use dunnage_core::ports::{Cmd, CommandRunner};
use dunnage_core::target::KindGate;
use dunnage_core::{DeleteError, SkipReason};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};

const NONE: &str = "<none>";
const NA: &str = "N/A";

/// Present-but-null is allowed (Go prints a nil slice as `null`); an absent key is still an error because
/// `deserialize_with` fields are required.
fn null_or_vec<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(d)?.unwrap_or_default())
}

fn null_or_map<'de, D>(d: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<BTreeMap<String, String>>::deserialize(d)?.unwrap_or_default())
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ImageWire {
    #[serde(rename = "ID")]
    id: String,
    repository: String,
    tag: String,
    digest: String,
    size: String,
    created_at: String,
    containers: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PsWire {
    #[serde(rename = "ID")]
    id: String,
    names: String,
    mounts: String,
    state: String,
    /// Not required: an extra reference signal for images, never the only one.
    #[serde(default)]
    image: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct VolumeLsWire {
    name: String,
    driver: String,
    labels: String,
    mountpoint: String,
    #[allow(dead_code)]
    scope: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct VolumeInspectWire {
    name: String,
    driver: String,
    mountpoint: String,
    created_at: String,
    #[serde(deserialize_with = "null_or_map")]
    labels: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DfImageWire {
    #[serde(rename = "ID")]
    #[allow(dead_code)]
    id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DfContainerWire {
    #[serde(rename = "ID")]
    #[allow(dead_code)]
    id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DfVolumeWire {
    name: String,
    labels: String,
    links: String,
    mountpoint: String,
    size: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DfBuildWire {
    #[serde(rename = "ID")]
    id: String,
    in_use: String,
    size: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DfWire {
    #[serde(deserialize_with = "null_or_vec")]
    #[allow(dead_code)]
    images: Vec<DfImageWire>,
    #[serde(deserialize_with = "null_or_vec")]
    containers: Vec<DfContainerWire>,
    #[serde(deserialize_with = "null_or_vec")]
    volumes: Vec<DfVolumeWire>,
    #[serde(deserialize_with = "null_or_vec")]
    build_cache: Vec<DfBuildWire>,
}

/// The parsed `system df -v --format json`: volume facts by name, the build cache, and the container and
/// image counts the cross-check uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DfListing {
    pub container_count: usize,
    pub image_count: usize,
    pub volumes: Vec<DfVolume>,
    pub build_cache: Vec<DockerBuildCacheEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DfVolume {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub mountpoint: String,
    /// `None` when Docker printed `N/A`.
    pub approx_size_bytes: Option<u64>,
    pub links: Option<u32>,
}

/// The bare message, for the `<reason>` of `docker: SKIPPED (daemon not reachable: <reason>)`.
fn reason_of(e: DockerError) -> String {
    match e {
        DockerError::Daemon(m) | DockerError::Unrecognised(m) => m,
    }
}

fn bad(what: &str, e: impl std::fmt::Display) -> DockerError {
    DockerError::Unrecognised(format!("{what}: {e}"))
}

fn lines_of<T: DeserializeOwned>(what: &str, raw: &str) -> Result<Vec<T>, DockerError> {
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, l)| {
            serde_json::from_str(l).map_err(|e| bad(&format!("{what} line {}", i + 1), e))
        })
        .collect()
}

fn none_if(s: &str) -> Option<String> {
    (s != NONE && !s.is_empty()).then(|| s.to_string())
}

fn count_of(what: &str, s: &str) -> Result<Option<u32>, DockerError> {
    if s == NA || s.is_empty() {
        return Ok(None);
    }
    s.parse().map(Some).map_err(|e| bad(what, e))
}

fn size_of(what: &str, s: &str) -> Result<u64, DockerError> {
    parse_si_size(s).ok_or_else(|| bad(what, format!("unreadable size {s:?}")))
}

/// `docker images --format '{{json .}}'`: one object per line; an empty output is an empty list (the
/// cross-check against `system df -v` decides whether that is believable).
pub fn parse_images(raw: &str) -> Result<Vec<DockerImage>, DockerError> {
    let wires: Vec<ImageWire> = lines_of("docker images", raw)?;
    wires
        .into_iter()
        .map(|w| {
            Ok(DockerImage {
                size_bytes: size_of("docker images Size", &w.size)?,
                containers: count_of("docker images Containers", &w.containers)?,
                id: w.id,
                repository: none_if(&w.repository),
                tag: none_if(&w.tag),
                digest: none_if(&w.digest),
                created_at: w.created_at,
            })
        })
        .collect()
}

/// `docker ps -a --format '{{json .}}'`.
pub fn parse_ps(raw: &str) -> Result<Vec<DockerContainer>, DockerError> {
    let wires: Vec<PsWire> = lines_of("docker ps -a", raw)?;
    Ok(wires
        .into_iter()
        .map(|w| DockerContainer {
            id: w.id,
            names: w.names,
            mounts: w.mounts,
            state: w.state,
            image: w.image,
        })
        .collect())
}

/// `docker volume ls --format '{{json .}}'`: `Labels` is a comma-joined string; no size, no creation time.
pub fn parse_volume_ls(raw: &str) -> Result<Vec<DockerVolume>, DockerError> {
    let wires: Vec<VolumeLsWire> = lines_of("docker volume ls", raw)?;
    Ok(wires
        .into_iter()
        .map(|w| {
            let labels = parse_label_string(&w.labels);
            DockerVolume {
                anonymous: is_anonymous(&labels),
                name: w.name,
                driver: w.driver,
                mountpoint: w.mountpoint,
                labels,
                approx_size_bytes: None,
                links: None,
                created_at: None,
            }
        })
        .collect())
}

/// `docker volume inspect <name>`: an array of exactly one object whose `Labels` is an object or `null`.
/// There is no `UsageData` on 29.8.2 and none is read.
pub fn parse_volume_inspect(raw: &str) -> Result<DockerVolume, DockerError> {
    let mut v: Vec<VolumeInspectWire> =
        serde_json::from_str(raw.trim()).map_err(|e| bad("docker volume inspect", e))?;
    if v.len() != 1 {
        return Err(bad(
            "docker volume inspect",
            format!("expected one volume, got {}", v.len()),
        ));
    }
    let w = v.remove(0);
    Ok(DockerVolume {
        anonymous: is_anonymous(&w.labels),
        name: w.name,
        driver: w.driver,
        mountpoint: w.mountpoint,
        labels: w.labels,
        approx_size_bytes: None,
        links: None,
        created_at: Some(w.created_at),
    })
}

/// `docker system df -v --format json`: ONE JSON object (not JSONL, no trailing newline). Volume sizes are
/// decimal-SI strings rounded to four digits, so they are approximate.
pub fn parse_system_df(raw: &str) -> Result<DfListing, DockerError> {
    let w: DfWire = serde_json::from_str(raw.trim()).map_err(|e| bad("docker system df -v", e))?;
    let volumes = w
        .volumes
        .into_iter()
        .map(|v| {
            let approx_size_bytes = if v.size == NA {
                None
            } else {
                Some(size_of("system df Volumes.Size", &v.size)?)
            };
            Ok(DfVolume {
                labels: parse_label_string(&v.labels),
                links: count_of("system df Volumes.Links", &v.links)?,
                name: v.name,
                mountpoint: v.mountpoint,
                approx_size_bytes,
            })
        })
        .collect::<Result<Vec<_>, DockerError>>()?;
    let build_cache = w
        .build_cache
        .into_iter()
        .map(|b| {
            Ok(DockerBuildCacheEntry {
                size_bytes: size_of("system df BuildCache.Size", &b.size)?,
                in_use: match b.in_use.as_str() {
                    "true" => true,
                    "false" => false,
                    other => return Err(bad("system df BuildCache.InUse", other)),
                },
                id: b.id,
            })
        })
        .collect::<Result<Vec<_>, DockerError>>()?;
    Ok(DfListing {
        container_count: w.containers.len(),
        image_count: w.images.len(),
        volumes,
        build_cache,
    })
}

/// Joins the plain volume list with the df facts; a volume df does not list keeps `None` size and links.
fn join_volumes(
    mut ls: Vec<DockerVolume>,
    df: &DfListing,
) -> Result<Vec<DockerVolume>, DockerError> {
    if ls.is_empty() && !df.volumes.is_empty() {
        return Err(bad(
            "docker volume ls",
            "empty while system df -v lists volumes",
        ));
    }
    for v in &mut ls {
        if let Some(d) = df.volumes.iter().find(|d| d.name == v.name) {
            v.approx_size_bytes = d.approx_size_bytes;
            v.links = d.links;
        }
    }
    Ok(ls)
}

/// The adapter. `docker_host` is the value of `DOCKER_HOST` (the caller reads the environment).
pub struct DockerCli<'a> {
    runner: &'a dyn CommandRunner,
    docker_host: Option<String>,
}

impl<'a> DockerCli<'a> {
    pub fn new(runner: &'a dyn CommandRunner, docker_host: Option<String>) -> DockerCli<'a> {
        DockerCli {
            runner,
            docker_host,
        }
    }

    /// Runs one read-only docker command. The runner owns the deadline (`RealCommandRunner::with_timeout`),
    /// so a hung daemon becomes `Daemon("timed out")` instead of a hang.
    fn docker(&self, args: &[&str]) -> Result<String, DockerError> {
        let cmd = Cmd::new("docker", args.iter().copied(), false);
        match self.runner.run(&cmd) {
            Ok(o) => Ok(o.stdout_lossy()),
            Err(DeleteError::CommandFailed { message, .. }) => Err(DockerError::Daemon(message)),
            Err(DeleteError::Timeout) => Err(DockerError::Daemon("timed out".to_string())),
            Err(e) => Err(DockerError::Daemon(format!("{e:?}"))),
        }
    }

    fn df(&self) -> Result<DfListing, DockerError> {
        parse_system_df(&self.docker(&["system", "df", "-v", "--format", "json"])?)
    }

    fn ps(&self) -> Result<Vec<DockerContainer>, DockerError> {
        parse_ps(&self.docker(&["ps", "-a", "--format", "{{json .}}"])?)
    }

    fn check_containers(ps: &[DockerContainer], df: &DfListing) -> Result<(), DockerError> {
        if ps.len() == df.container_count {
            return Ok(());
        }
        Err(bad(
            "docker ps -a",
            format!(
                "{} containers listed but system df -v counts {}",
                ps.len(),
                df.container_count
            ),
        ))
    }

    fn image_list(&self, df: &DfListing) -> Result<Vec<DockerImage>, DockerError> {
        let images = parse_images(&self.docker(&["images", "--format", "{{json .}}"])?)?;
        if images.is_empty() && df.image_count > 0 {
            return Err(bad(
                "docker images",
                "empty while system df -v lists images",
            ));
        }
        Ok(images)
    }

    fn volume_list(&self, df: &DfListing) -> Result<Vec<DockerVolume>, DockerError> {
        let ls = parse_volume_ls(&self.docker(&["volume", "ls", "--format", "{{json .}}"])?)?;
        join_volumes(ls, df)
    }
}

impl DockerClient for DockerCli<'_> {
    fn probe(&self) -> DockerProbe {
        if !self.runner.exists("docker") {
            return DockerProbe::unavailable(SkipReason::ToolMissing, "docker is not installed");
        }
        match self.docker(&["context", "show"]) {
            Ok(ctx) => {
                let ctx = ctx.trim();
                if !is_local_context(ctx, self.docker_host.as_deref()) {
                    return DockerProbe::unavailable(
                        SkipReason::NonLocalDockerContext,
                        format!("non-local docker context '{ctx}'"),
                    );
                }
            }
            Err(e) => return DockerProbe::unavailable(SkipReason::DaemonUnavailable, reason_of(e)),
        }
        if let Err(e) = self.docker(&["info"]) {
            return DockerProbe::unavailable(SkipReason::DaemonUnavailable, reason_of(e));
        }
        let version = match self.docker(&["version", "--format", "{{.Server.Version}}"]) {
            Ok(v) => v.trim().to_string(),
            Err(e) => return DockerProbe::unavailable(SkipReason::DaemonUnavailable, reason_of(e)),
        };
        match version_gate(&version) {
            KindGate::Available => DockerProbe::available(version),
            KindGate::ListOnly(r) => DockerProbe {
                gate: KindGate::ListOnly(r),
                detail: Some(format!(
                    "docker server {version} is outside the tested range"
                )),
                server_version: Some(version),
            },
            KindGate::Unavailable(r) => {
                DockerProbe::unavailable(r, format!("unreadable docker server version {version:?}"))
            }
        }
    }

    fn containers(&self) -> Result<Vec<DockerContainer>, DockerError> {
        let ps = self.ps()?;
        Self::check_containers(&ps, &self.df()?)?;
        Ok(ps)
    }

    fn images(&self) -> Result<Vec<DockerImage>, DockerError> {
        let df = self.df()?;
        self.image_list(&df)
    }

    fn volumes(&self) -> Result<Vec<DockerVolume>, DockerError> {
        let df = self.df()?;
        self.volume_list(&df)
    }

    fn build_cache(&self) -> Result<Vec<DockerBuildCacheEntry>, DockerError> {
        Ok(self.df()?.build_cache)
    }

    fn volume_inspect(&self, name: &str) -> Result<DockerVolume, DockerError> {
        let mut v = parse_volume_inspect(&self.docker(&["volume", "inspect", name])?)?;
        if v.name != name {
            return Err(bad(
                "docker volume inspect",
                format!("asked for {name}, got {}", v.name),
            ));
        }
        let df = self.df()?;
        if let Some(d) = df.volumes.iter().find(|d| d.name == name) {
            v.approx_size_bytes = d.approx_size_bytes;
            v.links = d.links;
        }
        Ok(v)
    }

    fn inventory(&self) -> Result<DockerInventory, DockerError> {
        let df = self.df()?;
        let containers = self.ps()?;
        Self::check_containers(&containers, &df)?;
        Ok(DockerInventory {
            images: self.image_list(&df)?,
            volumes: self.volume_list(&df)?,
            build_cache: df.build_cache,
            containers,
        })
    }
}
