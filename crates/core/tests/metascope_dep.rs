//! Smoke test: the metascope-core dependency resolves (Story 0.1.2).

use metascope_core::btrfs::read_allocation;
use metascope_core::scan::ScanOptions;

#[test]
fn metascope_core_scan_options_and_read_allocation_should_resolve_when_path_dep_builds() {
    let read: fn(&std::path::Path) -> _ = read_allocation;
    assert!(read(std::path::Path::new("/nonexistent-dc-fixture")).is_err());
    assert!(std::mem::size_of::<ScanOptions>() > 0);
}
