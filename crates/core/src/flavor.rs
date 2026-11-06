//! Build flavor marker. `check-release-features.sh` greps the release binary for the test-support string.

#[cfg(feature = "test-support")]
pub fn build_flavor() -> &'static str {
    "dc-flavor=test-support;9f3ac7e1"
}

#[cfg(not(feature = "test-support"))]
pub fn build_flavor() -> &'static str {
    "dc-flavor=release"
}
