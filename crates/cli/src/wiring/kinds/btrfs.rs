//! Registry slot for `btrfs` (Story 4.3.2): the usage-filtered balance, listed and apply-capable from Gate D,
//! so the shipped binary (no `gate-*` feature) does not register it until the owner's Gate D review.

use disk_cleanup_adapters::targets::btrfs_balance::BtrfsBalanceTarget;

use super::{KindCtx, Registrar};

pub fn register(reg: &mut Registrar, _ctx: &KindCtx<'_>) {
    reg.add('D', 'D', Box::new(BtrfsBalanceTarget::new()));
}
