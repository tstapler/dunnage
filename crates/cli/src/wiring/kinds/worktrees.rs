//! Registry slot for `worktrees` (first registered at Gate C). Empty until the owning story fills it; the story that
//! does calls `reg.add(...)` with its target and its gate, so the kind exists only in a build that has
//! reached that gate.

use super::{KindCtx, Registrar};

pub fn register(_reg: &mut Registrar, _ctx: &KindCtx<'_>) {}
