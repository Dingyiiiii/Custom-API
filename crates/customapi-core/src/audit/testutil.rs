//! Test-only filesystem helpers.
//!
//! The crate does not depend on `tempfile`, so the audit and store test suites
//! share this minimal self-cleaning temporary directory. Names are unique per
//! process and per call, which keeps the tests independent under `cargo test`'s
//! parallel harness.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// A temporary directory that removes itself when dropped.
#[derive(Debug)]
pub(crate) struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Creates a fresh directory under the system temporary directory.
    pub(crate) fn new() -> TempDir {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir();
        for _ in 0..1024 {
            let name = format!(
                "customapi-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            );
            let path = base.join(name);
            match std::fs::create_dir(&path) {
                Ok(()) => return TempDir { path },
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create temp dir {}: {e}", path.display()),
            }
        }
        panic!("could not create a unique temporary directory");
    }

    /// A path inside the directory.
    pub(crate) fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
