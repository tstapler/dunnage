//! Skeleton: Story 3.2.2 replaces the body. Until then a build directory's state is unknown (fail closed).

use std::path::Path;

use disk_cleanup_core::ports::RunState;

pub(super) fn run_state(_path: &Path) -> RunState {
    RunState::Unknown
}
