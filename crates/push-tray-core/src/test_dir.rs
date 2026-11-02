//! Scratch folders for tests, under the system's temp folder, removed on drop.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) struct TestDir(PathBuf);

impl TestDir {
    pub(crate) fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        let name = format!(
            "push-tray-core-test-{}-{nanos}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(name);
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

    /// Creates the bridge folder `relative` with a `config.json` in it.
    pub(crate) fn bridge(&self, relative: &str) -> PathBuf {
        let path = self.mkdir(relative);
        std::fs::write(path.join("config.json"), "{}").expect("write config.json");
        path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
