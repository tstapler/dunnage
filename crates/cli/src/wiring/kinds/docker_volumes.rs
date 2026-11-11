//! Registry slot for `docker-volumes` (unreferenced volumes): listed and apply-capable from Gate D (Story 4.1.4).

use disk_cleanup_adapters::targets::docker_volumes::DockerVolumesTarget;

use super::docker::docker_host;
use super::{KindCtx, Registrar};

pub fn register(reg: &mut Registrar, _ctx: &KindCtx<'_>) {
    reg.add('D', 'D', Box::new(DockerVolumesTarget::new(docker_host())));
}
