//! Registry slot for `steam`: listed and apply-capable from Gate D (Story 4.2.2), so a build without the
//! `gate-d` feature does not register it.

use dunnage_adapters::steam::target::SteamTarget;

use super::{KindCtx, Registrar};

pub fn register(reg: &mut Registrar, ctx: &KindCtx<'_>) {
    reg.add(
        'D',
        'D',
        Box::new(SteamTarget::new(ctx.roots.clone(), (ctx.ops)())),
    );
}
