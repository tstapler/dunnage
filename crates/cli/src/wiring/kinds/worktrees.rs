//! Registry slot for `worktrees` (Story 3.1.3): listed and apply-capable from Gate C, so the shipped binary
//! (no `gate-*` feature) does not register it until the owner's Gate C review.

use disk_cleanup_adapters::targets::worktree::WorktreeTarget;

use super::{KindCtx, Registrar};

pub fn register(reg: &mut Registrar, ctx: &KindCtx<'_>) {
    reg.add(
        'C',
        'C',
        Box::new(WorktreeTarget::new(ctx.roots.clone(), (ctx.ops)())),
    );
}
