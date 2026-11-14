//! Registry slot for `cargo-home`: listed and apply-capable from Gate C, with `build-dirs`, whose cargo
//! parts it complements (`ci/safety/kinds-by-gate.txt`).

use dunnage_adapters::targets::cargo_home::CargoHomeTarget;

use super::{KindCtx, Registrar};

pub fn register(reg: &mut Registrar, ctx: &KindCtx<'_>) {
    reg.add(
        'C',
        'C',
        Box::new(CargoHomeTarget::new(ctx.roots.clone(), (ctx.ops)())),
    );
}
