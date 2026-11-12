//! Registry slot for `docker` (dangling images and builder cache): listed and apply-capable from Gate D
//! (Story 4.1.2). A build that has not reached Gate D does not have the kind at all.

use dunnage_adapters::targets::docker_images::DockerImagesTarget;

use super::{KindCtx, Registrar};

/// `DOCKER_HOST` decides whether a named context is local (`docker context inspect` is not allowlisted).
pub fn docker_host() -> Option<String> {
    std::env::var("DOCKER_HOST").ok()
}

pub fn register(reg: &mut Registrar, _ctx: &KindCtx<'_>) {
    reg.add('D', 'D', Box::new(DockerImagesTarget::new(docker_host())));
}
