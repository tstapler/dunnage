//! Capability proving `--apply` was passed. Mint sites are policed by `scripts/check-token-sites.sh`.

/// Passed by reference into every mutating call. Derives nothing on purpose (no `Clone`, no `Default`).
pub struct ApplyToken(());

impl ApplyToken {
    /// Called from exactly one place, `cli/src/wiring/token.rs`.
    pub fn grant() -> ApplyToken {
        ApplyToken(())
    }

    /// Test builds only; a release `dunnage` binary must not contain it.
    #[cfg(feature = "test-support")]
    pub fn for_tests() -> ApplyToken {
        ApplyToken(())
    }
}

/// Without the `test-support` feature `for_tests` does not exist. `cargo test -p dunnage-core`
/// enables the feature through the self dev-dependency, so this only runs when the feature is off.
#[cfg(not(feature = "test-support"))]
/// ```compile_fail
/// let _ = dunnage_core::ApplyToken::for_tests();
/// ```
pub struct NoForTestsWithoutFeature;
