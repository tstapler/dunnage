//! Registry slot for `btrfs` (Story 4.3.2) and `btrfs-trim` (Story 4.3.3): the usage-filtered balance and
//! `fstrim`, listed and apply-capable from Gate D,
//! so a build without the `gate-d` feature does not register it.

use dunnage_adapters::targets::btrfs_balance::BtrfsBalanceTarget;
use dunnage_adapters::targets::btrfs_trim::BtrfsTrimTarget;

use super::{KindCtx, Registrar};

pub fn register(reg: &mut Registrar, _ctx: &KindCtx<'_>) {
    reg.add('D', 'D', Box::new(BtrfsBalanceTarget::new()));
    reg.add('D', 'D', Box::new(BtrfsTrimTarget::new()));
}
