//! Registry slot for `build-dirs`: listed and apply-capable from Gate C (Story 3.2.4). A build that has not
//! reached Gate C does not have the kind at all (`ci/safety/kinds-by-gate.txt`).

use disk_cleanup_adapters::targets::build_dir::BuildDirTarget;

use super::{KindCtx, Registrar};

pub fn register(reg: &mut Registrar, ctx: &KindCtx<'_>) {
    reg.add(
        'C',
        'C',
        Box::new(BuildDirTarget::new(ctx.roots.clone(), (ctx.ops)())),
    );
}
