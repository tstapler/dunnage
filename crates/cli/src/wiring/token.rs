//! The one production mint site of the `--apply` capability (`scripts/check-token-sites.sh`).

use dunnage_core::ApplyToken;

/// Called once per `--apply` run, only after the arguments and `confine_mode` accepted the run.
pub fn grant_apply() -> ApplyToken {
    ApplyToken::grant()
}
