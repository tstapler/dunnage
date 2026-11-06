//! Rule F: which paths `delete_tree` may ever remove. Pure and filesystem-free.

use std::ffi::OsStr;

use crate::delete_types::{DeleteError, DeletePolicy};

impl DeletePolicy {
    /// Requires `target`, `home`, every scan root and the allowed root to be absolute and lexically clean,
    /// then applies the floor rule. Body arrives with Task 2.1.2a.
    #[allow(unused_variables)]
    pub fn validate(&self, name: &OsStr) -> Result<(), DeleteError> {
        todo!("Task 2.1.2a")
    }
}
