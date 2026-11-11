//! What the three Docker targets share: the gate (probe plus a shape-checked inventory), the one mutating
//! door and the image facts. Nothing here mutates except [`remove`], which needs the `ApplyToken` and an
//! allowlisted row (`image rm`, `volume rm`, `builder prune`).

use std::sync::Mutex;

use disk_cleanup_core::docker::{
    DockerClient, DockerContainer, DockerImage, DockerInventory, DockerProbe,
};
use disk_cleanup_core::ports::{Cmd, Env};
use disk_cleanup_core::target::KindGate;
use disk_cleanup_core::targets::docker_image_guard::ImageFacts;
use disk_cleanup_core::{
    AllocatedBytes, ApparentBytes, ApplyToken, DeleteError, DeleteFailure, Freed, Identity,
    SkipReason,
};

use crate::docker_cli::DockerCli;

/// The identity of a Docker object: there is no inode, so the slot is zero.
pub const DOCKER_IDENT: Identity = Identity { dev: 0, ino: 0 };

pub fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What one pass over the daemon found.
pub struct Read {
    pub gate: KindGate,
    pub detail: Option<String>,
    pub probe: DockerProbe,
    /// Present when the daemon answered and every listing parsed and agreed.
    pub inventory: Option<DockerInventory>,
}

/// The per-target state of the gate: `unavailable_detail` is asked after `gate` and must not probe again.
#[derive(Default)]
pub struct GateMemo(Mutex<Option<(KindGate, Option<String>)>>);

impl GateMemo {
    pub fn remember(&self, gate: KindGate, detail: Option<String>) {
        *lock(&self.0) = Some((gate, detail));
    }

    pub fn detail(&self) -> Option<String> {
        lock(&self.0).as_ref().and_then(|(_, d)| d.clone())
    }
}

/// Probe the daemon and, when it answers, read the whole inventory: a changed shape or a count mismatch makes
/// the kind `Unavailable(UnrecognisedOutput)` and no list is returned (pre-mortem P1.3).
pub fn read(env: &Env, docker_host: Option<&str>) -> Read {
    let cli = DockerCli::new(env.runner, docker_host.map(str::to_string));
    let probe = cli.probe();
    if probe.gate.is_unavailable() {
        return Read {
            gate: probe.gate,
            detail: probe.detail.clone(),
            probe,
            inventory: None,
        };
    }
    match cli.inventory() {
        Ok(inv) => Read {
            gate: probe.gate,
            detail: probe.detail.clone(),
            probe,
            inventory: Some(inv),
        },
        Err(e) => Read {
            gate: e.gate(),
            detail: Some(e.to_string()),
            probe,
            inventory: None,
        },
    }
}

pub fn failure(error: DeleteError) -> DeleteFailure {
    DeleteFailure {
        error,
        freed: Freed::default(),
    }
}

/// The daemon-side figure a removal reports: Docker bytes never enter the filesystem attribution.
pub fn freed_of(bytes: u64) -> Freed {
    Freed {
        attributed_bytes: AllocatedBytes(bytes),
        apparent_bytes: ApparentBytes(bytes),
        entries_removed: 1,
        inodes_freed: 0,
    }
}

/// Runs one allowlisted mutating `docker` command through the token door.
pub fn remove(env: &Env, proof: &ApplyToken, args: &[&str]) -> Result<(), DeleteFailure> {
    let cmd = Cmd::new("docker", args.iter().copied(), true);
    env.runner
        .run_mutating(&cmd, proof)
        .map(|_| ())
        .map_err(failure)
}

/// The unreferenced-from-the-listing check shared by both image targets: `rows` are the `docker images`
/// rows of one id.
pub fn image_facts(
    rows: &[&DockerImage],
    containers: &[DockerContainer],
    created_epoch: Option<i64>,
) -> ImageFacts {
    let counts: Option<Vec<u32>> = rows.iter().map(|r| r.containers).collect();
    ImageFacts {
        containers: counts.map(|c| c.into_iter().sum()),
        named_by_container: containers.iter().any(|c| container_uses(c, rows)),
        digest: rows.iter().find_map(|r| r.digest.clone()),
        created_epoch,
    }
}

/// A `docker ps -a` row names the image by id, by `repo:tag` or by bare repository (any tag: fail closed).
fn container_uses(c: &DockerContainer, rows: &[&DockerImage]) -> bool {
    let name = c.image.trim();
    if name.is_empty() {
        return false;
    }
    let bare = name.strip_prefix("sha256:").unwrap_or(name);
    rows.iter().any(|r| {
        let by_id = bare.len() >= 12
            && (r.id.starts_with(&bare[..12.min(bare.len())]) || bare.starts_with(&r.id));
        let by_repo = r.repository.as_deref().is_some_and(|repo| {
            name == repo
                || r.tag
                    .as_deref()
                    .is_some_and(|t| name == format!("{repo}:{t}"))
        });
        by_id || by_repo
    })
}

pub fn skip_of(r: SkipReason) -> disk_cleanup_core::target::Guarded {
    disk_cleanup_core::target::Guarded::Skip(r)
}

/// Re-reads the inventory for a guard asked right before a removal. A daemon that stopped answering is not
/// a reason to skip: Docker itself refuses to remove a referenced image or volume, so the removal fails as
/// an error of that item. A shape that no longer parses is.
pub fn fresh(env: &Env, docker_host: Option<&str>) -> Result<DockerInventory, Option<SkipReason>> {
    let cli = DockerCli::new(env.runner, docker_host.map(str::to_string));
    match cli.inventory() {
        Ok(i) => Ok(i),
        Err(disk_cleanup_core::docker::DockerError::Daemon(_)) => Err(None),
        Err(_) => Err(Some(SkipReason::UnrecognisedOutput)),
    }
}
