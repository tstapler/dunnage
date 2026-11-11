//! `docker-volumes`: unreferenced volumes behind guards (Story 4.1.4, ADR-003). Destructive: needs the
//! reviewed plan-id, `--docker-volumes anonymous|named` for the class and an age of 30 days. A volume the
//! invoking user cannot read is `ContentsUnknown` (the normal case on a rootful daemon), so without root only
//! `--volume <name>` makes one removable.
//!
//! Reference evidence: `system df -v` Links, the `Mounts` of `docker ps -a` and, right before the removal, a
//! fresh `docker ps -a --filter volume=<name>`. Docker's own `volume is in use` refusal is the last line and
//! maps to `Skipped(VolumeReferenced)` (`DeleteError::skip_reason`).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Mutex;

use disk_cleanup_core::docker::{DockerContainer, DockerError, DockerInventory, DockerVolume};
use disk_cleanup_core::ports::{Cmd, Env};
use disk_cleanup_core::selector::{default_min_age_secs, DockerVolumes, Selection};
use disk_cleanup_core::target::{Guarded, KindGate, Target};
use disk_cleanup_core::targets::docker_volume_guard::{
    decide_volume, evidence_text, Contents, VolumeFacts, VolumeOpts,
};
use disk_cleanup_core::{
    AllocatedBytes, ApplyToken, Candidate, CandidateId, DeleteError, DeleteFailure, DockerFacts,
    DockerId, Freed, KindFacts, Locator, Outcome, SelectorId, SkipReason, Status,
};

use crate::docker_cli::{parse_ps, parse_volume_inspect};
use crate::targets::docker_common::{
    failure, freed_of, lock, read, remove, skip_of, GateMemo, DOCKER_IDENT,
};
use crate::targets::docker_images::DAEMON_SIDE_NOTE;

const COMPOSE_PROJECT: &str = "com.docker.compose.project";
const COMPOSE_WORKING_DIR: &str = "com.docker.compose.project.working_dir";
const FALLBACK_MIN_AGE_SECS: u64 = 30 * 86_400;

/// What `discover` fixed for the rest of the run.
#[derive(Clone, Default)]
struct Run {
    class: Option<DockerVolumes>,
    named: Vec<String>,
    min_age_secs: Option<u64>,
}

#[derive(Default)]
struct State {
    run: Run,
    verdicts: HashMap<CandidateId, (Option<SkipReason>, Option<String>)>,
    evidence: HashMap<CandidateId, String>,
    /// The listed size per candidate: `None` is unknown, which `Candidate::est_bytes` cannot say.
    sizes: HashMap<CandidateId, Option<u64>>,
    list_only: Option<SkipReason>,
}

pub struct DockerVolumesTarget {
    docker_host: Option<String>,
    memo: GateMemo,
    state: Mutex<State>,
}

fn docker_read(env: &Env, args: &[&str]) -> Result<String, DockerError> {
    let cmd = Cmd::new("docker", args.iter().copied(), false);
    match env.runner.run(&cmd) {
        Ok(o) => Ok(o.stdout_lossy()),
        Err(DeleteError::CommandFailed { message, .. }) => Err(DockerError::Daemon(message)),
        Err(DeleteError::Timeout) => Err(DockerError::Daemon("timed out".to_string())),
        Err(e) => Err(DockerError::Daemon(format!("{e:?}"))),
    }
}

/// The first container that mounts the volume: by the `Mounts` column (an anonymous name is cut with `...`
/// there) or, when `ps` names none, the reference count of `system df -v`.
fn used_by(v: &DockerVolume, containers: &[DockerContainer]) -> Option<String> {
    let by_mount = containers.iter().find(|c| {
        c.mounts.split(',').map(str::trim).any(|m| {
            let m = m.trim_end_matches(['\u{2026}', '.']);
            !m.is_empty() && (m == v.name || (v.name.starts_with(m) && m.len() >= 12))
        })
    });
    match (by_mount, v.links) {
        (Some(c), _) => Some(c.names.clone()),
        (None, Some(0)) => None,
        (None, Some(_)) => Some("a container".to_string()),
        // Fail closed: df did not count this volume's containers.
        (None, None) => Some("a container (reference count unknown)".to_string()),
    }
}

fn contents_of(env: &Env, mountpoint: &str) -> Contents {
    match env.fs.read_dir(Path::new(mountpoint)) {
        Ok(entries) => Contents::Names(
            entries
                .into_iter()
                .map(|e| e.name.to_string_lossy().into_owned())
                .collect(),
        ),
        Err(_) => Contents::Unreadable,
    }
}

fn compose_present(env: &Env, labels: &BTreeMap<String, String>) -> bool {
    labels.contains_key(COMPOSE_PROJECT)
        && labels
            .get(COMPOSE_WORKING_DIR)
            .is_some_and(|d| env.fs.lstat(Path::new(d)).is_ok())
}

impl DockerVolumesTarget {
    pub fn new(docker_host: Option<String>) -> DockerVolumesTarget {
        DockerVolumesTarget {
            docker_host,
            memo: GateMemo::default(),
            state: Mutex::new(State::default()),
        }
    }

    /// The facts of one volume from fresh reads of `inspect` (creation time, true labels) plus the listing's
    /// size and the container evidence passed in.
    fn facts(
        env: &Env,
        listed: &DockerVolume,
        inspected: &Result<DockerVolume, DockerError>,
        used_by: Option<String>,
    ) -> (VolumeFacts, String) {
        let (labels, created_at) = match inspected {
            Ok(i) => {
                let mut l = listed.labels.clone();
                l.extend(i.labels.clone());
                (l, i.created_at.clone())
            }
            Err(_) => (listed.labels.clone(), None),
        };
        let facts = VolumeFacts {
            name: listed.name.clone(),
            anonymous: listed.anonymous,
            used_by,
            compose_project_present: compose_present(env, &labels),
            approx_size_bytes: listed.approx_size_bytes,
            created_epoch: created_at
                .as_deref()
                .and_then(disk_cleanup_core::docker::parse_docker_time),
            contents: contents_of(env, &listed.mountpoint),
        };
        let evidence = evidence_text(
            listed.approx_size_bytes,
            created_at.as_deref(),
            &listed.mountpoint,
            listed.anonymous,
        );
        (facts, evidence)
    }

    fn opts(run: &Run, now: i64) -> VolumeOpts<'_> {
        VolumeOpts {
            class: run.class,
            named: &run.named,
            min_age_secs: run.min_age_secs.unwrap_or(FALLBACK_MIN_AGE_SECS),
            now,
        }
    }

    /// The evidence every listed row shows (T-404): approximate size, `CreatedAt`, mount point and class.
    pub fn evidence(&self, c: &Candidate) -> Option<String> {
        lock(&self.state).evidence.get(&c.id()).cloned()
    }

    fn inspect(env: &Env, name: &str) -> Result<DockerVolume, DockerError> {
        parse_volume_inspect(&docker_read(env, &["volume", "inspect", name])?)
    }

    fn fresh_verdict(&self, env: &Env, c: &Candidate) -> Guarded {
        let (run, list_only) = {
            let st = lock(&self.state);
            (st.run.clone(), st.list_only)
        };
        if let Some(r) = list_only {
            return skip_of(r);
        }
        let KindFacts::Docker(DockerFacts::Volume { name, anonymous }) = c.facts() else {
            return skip_of(SkipReason::UnrecognisedOutput);
        };
        // A daemon that stopped answering cannot show that the volume is unreferenced: skip, never guess.
        let filter = format!("volume={name}");
        let ps = match docker_read(
            env,
            &["ps", "-a", "--filter", &filter, "--format", "{{json .}}"],
        )
        .and_then(|raw| parse_ps(&raw))
        {
            Ok(p) => p,
            Err(DockerError::Daemon(_)) => return skip_of(SkipReason::DaemonUnavailable),
            Err(DockerError::Unrecognised(_)) => return skip_of(SkipReason::UnrecognisedOutput),
        };
        let inspected = match Self::inspect(env, name) {
            Ok(i) => i,
            Err(DockerError::Daemon(m)) if m.contains("No such volume") => {
                return skip_of(SkipReason::Gone)
            }
            Err(DockerError::Daemon(_)) => return skip_of(SkipReason::DaemonUnavailable),
            Err(DockerError::Unrecognised(_)) => return skip_of(SkipReason::UnrecognisedOutput),
        };
        let plan_size = lock(&self.state).sizes.get(&c.id()).copied().flatten();
        let listed = DockerVolume {
            approx_size_bytes: plan_size,
            anonymous: *anonymous,
            ..inspected.clone()
        };
        let used = ps.first().map(|p| p.names.clone());
        let (facts, _) = Self::facts(env, &listed, &Ok(inspected), used);
        decide_volume(&facts, &Self::opts(&run, env.clock.now_epoch()))
            .reason
            .map_or(Guarded::Allow, skip_of)
    }
}

impl Target for DockerVolumesTarget {
    fn name(&self) -> &'static str {
        "docker-volumes"
    }

    fn selectors(&self) -> &'static [SelectorId] {
        &[SelectorId::DockerVolumes]
    }

    fn gate(&self, env: &Env) -> KindGate {
        let r = read(env, self.docker_host.as_deref());
        self.memo.remember(r.gate, r.detail);
        r.gate
    }

    fn unavailable_detail(&self, env: &Env) -> Option<String> {
        if self.memo.detail().is_none() {
            let _ = self.gate(env);
        }
        self.memo.detail()
    }

    fn discover(&self, env: &Env, selection: &Selection) -> Vec<Candidate> {
        let r = read(env, self.docker_host.as_deref());
        let run = Run {
            class: selection.docker_volumes,
            named: selection.volumes.clone(),
            min_age_secs: selection
                .effective_min_age(SelectorId::DockerVolumes)
                .or_else(|| default_min_age_secs(SelectorId::DockerVolumes)),
        };
        {
            let mut st = lock(&self.state);
            st.run = run.clone();
            st.verdicts.clear();
            st.evidence.clear();
            st.sizes.clear();
            st.list_only = match r.gate {
                KindGate::ListOnly(reason) => Some(reason),
                _ => None,
            };
        }
        let Some(DockerInventory {
            volumes,
            containers,
            ..
        }) = r.inventory
        else {
            return Vec::new();
        };
        let now = env.clock.now_epoch();
        let mut out = Vec::new();
        for v in &volumes {
            let inspected = Self::inspect(env, &v.name);
            let (facts, evidence) = Self::facts(env, v, &inspected, used_by(v, &containers));
            let verdict = decide_volume(&facts, &Self::opts(&run, now));
            let c = Candidate::new(
                Locator::Docker(DockerId(v.name.clone())),
                KindFacts::Docker(DockerFacts::Volume {
                    name: v.name.clone(),
                    anonymous: v.anonymous,
                }),
                AllocatedBytes(v.approx_size_bytes.unwrap_or(0)),
                facts.created_epoch,
                DOCKER_IDENT,
            );
            let mut st = lock(&self.state);
            st.verdicts.insert(c.id(), (verdict.reason, verdict.detail));
            st.evidence.insert(c.id(), evidence);
            st.sizes.insert(c.id(), v.approx_size_bytes);
            drop(st);
            out.push(c);
        }
        out
    }

    fn guard(&self, c: &Candidate, env: &Env) -> Guarded {
        {
            let st = lock(&self.state);
            if let Some(r) = st.list_only {
                return skip_of(r);
            }
            if let Some((reason, _)) = st.verdicts.get(&c.id()) {
                return reason.map_or(Guarded::Allow, skip_of);
            }
        }
        self.fresh_verdict(env, c)
    }

    fn guard_before_delete(&self, c: &Candidate, env: &Env) -> Guarded {
        self.fresh_verdict(env, c)
    }

    fn execute(
        &self,
        c: &Candidate,
        env: &Env,
        proof: &ApplyToken,
    ) -> Result<Freed, DeleteFailure> {
        let KindFacts::Docker(DockerFacts::Volume { name, .. }) = c.facts() else {
            return Err(failure(DeleteError::Refused));
        };
        remove(env, proof, &["volume", "rm", name])?;
        Ok(freed_of(c.est_bytes().0))
    }

    fn detail(&self, c: &Candidate) -> Option<String> {
        lock(&self.state)
            .verdicts
            .get(&c.id())
            .and_then(|(_, d)| d.clone())
    }

    fn finish(&self, outcomes: &[Outcome], _env: &Env) -> Vec<String> {
        if outcomes.iter().any(|o| o.status == Status::Removed) {
            vec![DAEMON_SIDE_NOTE.to_string()]
        } else {
            Vec::new()
        }
    }
}
