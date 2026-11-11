//! `docker`: dangling images and builder cache (Story 4.1.2, ADR-001). Removal is by id
//! (`docker image rm <id>`) and by `docker builder prune --force --filter until=24h`; `system prune`,
//! `image prune -a` and `volume prune` are not allowlist rows and never appear here.
//!
//! Bytes are the daemon's, not the filesystem's: an image's is its listed (rounded) size, the build cache's is
//! the `system df -v` total before minus after. The run reports them on their own line and keeps them out of
//! the btrfs attribution (`summary::is_docker`).

use std::collections::HashMap;
use std::sync::Mutex;

use disk_cleanup_core::docker::{parse_docker_time, DockerClient, DockerContainer, DockerImage};
use disk_cleanup_core::ports::Env;
use disk_cleanup_core::selector::Selection;
use disk_cleanup_core::target::{Guarded, KindGate, Target};
use disk_cleanup_core::targets::docker_image_guard::decide_dangling_image;
use disk_cleanup_core::{
    AllocatedBytes, ApplyToken, Candidate, CandidateId, DeleteFailure, DockerFacts, DockerId,
    Freed, KindFacts, Locator, Outcome, SelectorId, SkipReason, Status,
};

use crate::docker_cli::DockerCli;
use crate::targets::docker_common::{
    freed_of, fresh, image_facts, lock, read, remove, skip_of, GateMemo, DOCKER_IDENT,
};

/// The locator of the single build-cache candidate.
pub const BUILD_CACHE_ID: &str = "build-cache";

/// The cache older than this is pruned; younger entries stay for the next build.
pub const BUILD_CACHE_UNTIL: &str = "until=24h";

/// Said once per run when Docker bytes were freed (triad iteration 1, gap p).
pub const DAEMON_SIDE_NOTE: &str =
    "docker: freed bytes are daemon-side figures and may differ from the btrfs delta";

#[derive(Default)]
struct State {
    verdicts: HashMap<CandidateId, Option<SkipReason>>,
    list_only: Option<SkipReason>,
}

pub struct DockerImagesTarget {
    docker_host: Option<String>,
    memo: GateMemo,
    state: Mutex<State>,
}

impl DockerImagesTarget {
    /// `docker_host` is the value of `DOCKER_HOST`; the caller reads the environment.
    pub fn new(docker_host: Option<String>) -> DockerImagesTarget {
        DockerImagesTarget {
            docker_host,
            memo: GateMemo::default(),
            state: Mutex::new(State::default()),
        }
    }

    fn dangling_verdict(
        id: &str,
        images: &[DockerImage],
        containers: &[DockerContainer],
    ) -> Option<SkipReason> {
        let rows: Vec<&DockerImage> = images.iter().filter(|i| i.id == id).collect();
        if rows.is_empty() {
            return Some(SkipReason::Gone);
        }
        decide_dangling_image(&image_facts(&rows, containers, None))
    }

    fn fresh_verdict(&self, env: &Env, c: &Candidate) -> Guarded {
        if let Some(r) = lock(&self.state).list_only {
            return skip_of(r);
        }
        let inv = match fresh(env, self.docker_host.as_deref()) {
            Ok(i) => i,
            Err(None) => return Guarded::Allow,
            Err(Some(r)) => return skip_of(r),
        };
        match c.facts() {
            KindFacts::Docker(DockerFacts::DanglingImage { image_id }) => {
                Self::dangling_verdict(image_id, &inv.images, &inv.containers)
                    .map_or(Guarded::Allow, skip_of)
            }
            KindFacts::Docker(DockerFacts::BuildCache) => {
                if inv
                    .build_cache
                    .iter()
                    .any(|b| !b.in_use && b.size_bytes > 0)
                {
                    Guarded::Allow
                } else {
                    skip_of(SkipReason::Gone)
                }
            }
            _ => skip_of(SkipReason::UnrecognisedOutput),
        }
    }
}

impl Target for DockerImagesTarget {
    fn name(&self) -> &'static str {
        "docker"
    }

    fn selectors(&self) -> &'static [SelectorId] {
        &[SelectorId::Docker]
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

    fn discover(&self, env: &Env, _selection: &Selection) -> Vec<Candidate> {
        let r = read(env, self.docker_host.as_deref());
        let mut st = lock(&self.state);
        st.verdicts.clear();
        st.list_only = match r.gate {
            KindGate::ListOnly(reason) => Some(reason),
            _ => None,
        };
        let Some(inv) = r.inventory else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut seen: Vec<&str> = Vec::new();
        for img in inv.images.iter().filter(|i| i.is_dangling()) {
            if seen.contains(&img.id.as_str()) {
                continue;
            }
            seen.push(&img.id);
            let c = Candidate::new(
                Locator::Docker(DockerId(img.id.clone())),
                KindFacts::Docker(DockerFacts::DanglingImage {
                    image_id: img.id.clone(),
                }),
                AllocatedBytes(img.size_bytes),
                parse_docker_time(&img.created_at),
                DOCKER_IDENT,
            );
            let v = Self::dangling_verdict(&img.id, &inv.images, &inv.containers);
            st.verdicts.insert(c.id(), v);
            out.push(c);
        }
        let reclaimable: u64 = inv
            .build_cache
            .iter()
            .filter(|b| !b.in_use)
            .map(|b| b.size_bytes)
            .sum();
        if reclaimable > 0 {
            let c = Candidate::new(
                Locator::Docker(DockerId(BUILD_CACHE_ID.to_string())),
                KindFacts::Docker(DockerFacts::BuildCache),
                AllocatedBytes(reclaimable),
                None,
                DOCKER_IDENT,
            );
            st.verdicts.insert(c.id(), None);
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
            if let Some(v) = st.verdicts.get(&c.id()) {
                return v.map_or(Guarded::Allow, skip_of);
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
        match c.facts() {
            KindFacts::Docker(DockerFacts::DanglingImage { image_id }) => {
                remove(env, proof, &["image", "rm", image_id])?;
                Ok(freed_of(c.est_bytes().0))
            }
            KindFacts::Docker(DockerFacts::BuildCache) => {
                let cli = DockerCli::new(env.runner, self.docker_host.clone());
                let total = |cli: &DockerCli<'_>| {
                    cli.build_cache()
                        .ok()
                        .map(|v| v.iter().map(|b| b.size_bytes).sum::<u64>())
                };
                let before = total(&cli);
                remove(
                    env,
                    proof,
                    &["builder", "prune", "--force", "--filter", BUILD_CACHE_UNTIL],
                )?;
                let bytes = match (before, total(&cli)) {
                    (Some(b), Some(a)) => b.saturating_sub(a),
                    _ => c.est_bytes().0,
                };
                Ok(freed_of(bytes))
            }
            _ => Err(crate::targets::docker_common::failure(
                disk_cleanup_core::DeleteError::Refused,
            )),
        }
    }

    fn finish(&self, outcomes: &[Outcome], _env: &Env) -> Vec<String> {
        if outcomes.iter().any(|o| o.status == Status::Removed) {
            vec![DAEMON_SIDE_NOTE.to_string()]
        } else {
            Vec::new()
        }
    }
}
