//! Scratch folders for tests, under the system's temp folder, removed on drop.

use std::path::{Path, PathBuf};

pub(crate) struct TestDir(PathBuf);

impl TestDir {
    pub(crate) fn new() -> Self {
        let path = std::env::temp_dir().join(format!("push-bridge-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create the test folder");
        Self(path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    /// Creates `relative` (and its parents) inside the test folder.
    pub(crate) fn mkdir(&self, relative: &str) -> PathBuf {
        let path = self.0.join(relative);
        std::fs::create_dir_all(&path).expect("create a folder");
        path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        // Removes links, not their targets: remove_dir_all does not follow symlinks or junctions.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
