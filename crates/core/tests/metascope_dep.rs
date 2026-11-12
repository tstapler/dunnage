//! Smoke test: the scan crate (formerly metascope-core) resolves as a workspace member (Stories 0.1.2, 0.1.5).

use dunnage_scan::btrfs::read_allocation;
use dunnage_scan::scan::ScanOptions;

#[test]
fn metascope_core_scan_options_and_read_allocation_should_resolve_when_path_dep_builds() {
    let read: fn(&std::path::Path) -> _ = read_allocation;
    assert!(read(std::path::Path::new("/nonexistent-dc-fixture")).is_err());
    assert!(std::mem::size_of::<ScanOptions>() > 0);
}
