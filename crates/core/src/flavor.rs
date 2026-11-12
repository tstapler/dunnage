//! Build flavor marker. `check-release-features.sh` greps the release binary for the test-support string.

#[cfg(feature = "test-support")]
pub fn build_flavor() -> &'static str {
    "dc-flavor=test-support;9f3ac7e1"
}

#[cfg(not(feature = "test-support"))]
pub fn build_flavor() -> &'static str {
    "dc-flavor=release"
}

/// The point named by `DC_EXPLORE_PANIC_AT` (Gate F's forced-panic journey), or `None`. Only a `test-support`
/// build reads the variable; the shipped and dist builds do not even contain its name, which
/// `check-release-features.sh` greps for.
#[cfg(feature = "test-support")]
pub fn explore_panic_at() -> Option<String> {
    std::env::var("DC_EXPLORE_PANIC_AT")
        .ok()
        .filter(|v| !v.is_empty())
}

#[cfg(not(feature = "test-support"))]
pub fn explore_panic_at() -> Option<String> {
    None
}

/// Milliseconds a planner call waits before it plans, from `DC_EXPLORE_PLAN_DELAY_MS` (the full-disk journeys
/// send a signal during a slow planner). `test-support` builds only, like [`explore_panic_at`].
#[cfg(feature = "test-support")]
pub fn explore_plan_delay_ms() -> Option<u64> {
    std::env::var("DC_EXPLORE_PLAN_DELAY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
}

#[cfg(not(feature = "test-support"))]
pub fn explore_plan_delay_ms() -> Option<u64> {
    None
}

/// Milliseconds the explorer's apply pauses after each item, from `DC_EXPLORE_DELETE_DELAY_MS` (the live-delete
/// journey sends SIGTERM while the delete runs). `test-support` builds only.
#[cfg(feature = "test-support")]
pub fn explore_delete_delay_ms() -> Option<u64> {
    std::env::var("DC_EXPLORE_DELETE_DELAY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
}

#[cfg(not(feature = "test-support"))]
pub fn explore_delete_delay_ms() -> Option<u64> {
    None
}
