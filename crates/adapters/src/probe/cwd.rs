//! Skeleton: Story 3.1.2 replaces the body. Until then nothing is known to be unused.

use std::path::Path;

use disk_cleanup_core::Tri;

pub(super) fn cwd_in_use(_path: &Path) -> (Tri, Option<(String, u32)>) {
    (Tri::Unknown, None)
}
