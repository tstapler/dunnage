//! `docker-images`: unused tagged images behind an age filter (Story 4.1.3). Guarded, never in the default
//! set. An image Docker says a container uses, one built on this machine (no registry digest: pulling cannot
//! bring it back) and one younger than `--min-age` (default 30d) are skipped.
//!
//! An image with several tags is removed tag by tag (`docker image rm repo:tag`), because `image rm <id>`
//! refuses an id that several repositories point at; the last untag deletes the image.

use std::collections::HashMap;
use std::sync::Mutex;

use dunnage_core::docker::{parse_docker_time, DockerContainer, DockerImage};
use dunnage_core::ports::Env;
use dunnage_core::selector::{default_min_age_secs, Selection};
use dunnage_core::target::{Guarded, KindGate, Target};
use dunnage_core::targets::docker_image_guard::decide_unused_image;
use dunnage_core::{
    AllocatedBytes, ApplyToken, Candidate, CandidateId, DeleteFailure, DockerFacts, DockerId,
    Freed, KindFacts, Locator, Outcome, SelectorId, SkipReason, Status,
};

use crate::targets::docker_common::{
    freed_of, fresh, image_facts, lock, read, remove, skip_of, GateMemo, DOCKER_IDENT,
};
use crate::targets::docker_images::DAEMON_SIDE_NOTE;

const FALLBACK_MIN_AGE_SECS: u64 = 30 * 86_400;

#[derive(Default)]
struct State {
    verdicts: HashMap<CandidateId, Option<SkipReason>>,
    list_only: Option<SkipReason>,
    min_age_secs: Option<u64>,
}

pub struct DockerUnusedImagesTarget {
    docker_host: Option<String>,
    memo: GateMemo,
    state: Mutex<State>,
}

impl DockerUnusedImagesTarget {
    pub fn new(docker_host: Option<String>) -> DockerUnusedImagesTarget {
        DockerUnusedImagesTarget {
            docker_host,
            memo: GateMemo::default(),
            state: Mutex::new(State::default()),
        }
    }

    fn min_age(&self) -> u64 {
        lock(&self.state)
            .min_age_secs
            .unwrap_or(FALLBACK_MIN_AGE_SECS)
    }

    fn verdict(
        &self,
        id: &str,
        images: &[DockerImage],
        containers: &[DockerContainer],
        now: i64,
    ) -> Option<SkipReason> {
        let rows: Vec<&DockerImage> = images
            .iter()
            .filter(|i| i.id == id && !i.is_dangling())
            .collect();
        let created = rows.first().and_then(|r| parse_docker_time(&r.created_at));
        if rows.is_empty() {
            return Some(SkipReason::Gone);
        }
        decide_unused_image(
            &image_facts(&rows, containers, created),
            self.min_age(),
            now,
        )
    }

    fn fresh_verdict(&self, env: &Env, c: &Candidate) -> Guarded {
        if let Some(r) = lock(&self.state).list_only {
            return skip_of(r);
        }
        let KindFacts::Docker(DockerFacts::UnusedImage { image_id, .. }) = c.facts() else {
            return skip_of(SkipReason::UnrecognisedOutput);
        };
        let inv = match fresh(env, self.docker_host.as_deref()) {
            Ok(i) => i,
            Err(None) => return Guarded::Allow,
            Err(Some(r)) => return skip_of(r),
        };
        self.verdict(
            image_id,
            &inv.images,
            &inv.containers,
            env.clock.now_epoch(),
        )
        .map_or(Guarded::Allow, skip_of)
    }
}

impl Target for DockerUnusedImagesTarget {
    fn name(&self) -> &'static str {
        "docker-images"
    }

    fn selectors(&self) -> &'static [SelectorId] {
        &[SelectorId::DockerImages]
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
        {
            let mut st = lock(&self.state);
            st.verdicts.clear();
            st.min_age_secs = selection
                .effective_min_age(SelectorId::DockerImages)
                .or_else(|| default_min_age_secs(SelectorId::DockerImages));
            st.list_only = match r.gate {
                KindGate::ListOnly(reason) => Some(reason),
                _ => None,
            };
        }
        let Some(inv) = r.inventory else {
            return Vec::new();
        };
        let now = env.clock.now_epoch();
        let mut out = Vec::new();
        let mut seen: Vec<&str> = Vec::new();
        for img in inv.images.iter().filter(|i| !i.is_dangling()) {
            if seen.contains(&img.id.as_str()) {
                continue;
            }
            seen.push(&img.id);
            let rows: Vec<&DockerImage> = inv.images.iter().filter(|i| i.id == img.id).collect();
            let tags: Vec<String> = rows
                .iter()
                .filter_map(|r| match (&r.repository, &r.tag) {
                    (Some(repo), Some(tag)) => Some(format!("{repo}:{tag}")),
                    (Some(repo), None) => Some(repo.clone()),
                    _ => None,
                })
                .collect();
            let c = Candidate::new(
                Locator::Docker(DockerId(img.id.clone())),
                KindFacts::Docker(DockerFacts::UnusedImage {
                    image_id: img.id.clone(),
                    tags,
                }),
                AllocatedBytes(img.size_bytes),
                parse_docker_time(&img.created_at),
                DOCKER_IDENT,
            );
            let v = self.verdict(&img.id, &inv.images, &inv.containers, now);
            lock(&self.state).verdicts.insert(c.id(), v);
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
        let KindFacts::Docker(DockerFacts::UnusedImage { image_id, tags }) = c.facts() else {
            return Err(crate::targets::docker_common::failure(
                dunnage_core::DeleteError::Refused,
            ));
        };
        // Tag by tag when every row has a `repo:tag`; otherwise by id, which Docker refuses when several
        // references share it (that item is then an error, never a forced removal).
        let by_tag = !tags.is_empty()
            && tags
                .iter()
                .all(|t| t.rsplit('/').next().is_some_and(|l| l.contains(':')));
        if by_tag {
            for t in tags {
                remove(env, proof, &["image", "rm", t])?;
            }
        } else {
            remove(env, proof, &["image", "rm", image_id])?;
        }
        Ok(freed_of(c.est_bytes().0))
    }

    fn detail(&self, c: &Candidate) -> Option<String> {
        match c.facts() {
            KindFacts::Docker(DockerFacts::UnusedImage { tags, .. }) => tags.first().cloned(),
            _ => None,
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
