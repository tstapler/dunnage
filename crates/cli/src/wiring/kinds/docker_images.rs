//! Registry slot for `docker-images` (unused tagged images): listed and apply-capable from Gate D (Story 4.1.3).

use dunnage_adapters::targets::docker_unused_images::DockerUnusedImagesTarget;

use super::docker::docker_host;
use super::{KindCtx, Registrar};

pub fn register(reg: &mut Registrar, _ctx: &KindCtx<'_>) {
    reg.add(
        'D',
        'D',
        Box::new(DockerUnusedImagesTarget::new(docker_host())),
    );
}
