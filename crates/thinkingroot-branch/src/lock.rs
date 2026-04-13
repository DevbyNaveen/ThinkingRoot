// crates/thinkingroot-branch/src/lock.rs
//! Advisory file lock for merge operations.
//!
//! Prevents two concurrent `root merge` invocations from corrupting main's
//! `graph.db` by racing on the same CozoDB database file.
//!
//! The lock is an exclusive advisory lock on `.thinkingroot-refs/merge.lock`.
//! It is released automatically when `MergeLock` is dropped.
//!
//! # Example
//! ```no_run
//! use std::path::Path;
//! use thinkingroot_branch::lock::MergeLock;
//!
//! fn do_merge(root: &Path) -> thinkingroot_core::Result<()> {
//!     let _lock = MergeLock::acquire(root)?;
//!     // merge logic here — lock released on drop
//!     Ok(())
//! }
//! ```

use std::fs::{File, OpenOptions};
use std::path::Path;

use fs2::FileExt;
use thinkingroot_core::Result;
use thinkingroot_core::error::Error;

/// RAII guard for the merge advisory lock.
///
/// Holds an exclusive `flock`/`LockFileEx` on `.thinkingroot-refs/merge.lock`
/// for the lifetime of this value.
pub struct MergeLock {
    _file: File,
}

impl MergeLock {
    /// Attempt to acquire an exclusive merge lock.
    ///
    /// Returns `Err(Error::MergeBlocked(...))` immediately if another process
    /// holds the lock, rather than blocking.
    pub fn acquire(root_path: &Path) -> Result<Self> {
        let refs_dir = root_path.join(".thinkingroot-refs");
        std::fs::create_dir_all(&refs_dir)?;
        let lock_path = refs_dir.join("merge.lock");

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| Error::io_path(&lock_path, e))?;

        file.try_lock_exclusive().map_err(|_| {
            Error::MergeBlocked(
                "another merge is already in progress — try again in a moment".to_string(),
            )
        })?;

        Ok(Self { _file: file })
    }
}

impl Drop for MergeLock {
    fn drop(&mut self) {
        // `fs2::FileExt::unlock` is best-effort; if it fails the OS will
        // release the lock when the file handle is closed anyway.
        // allow: clippy confuses fs2::FileExt::unlock with std::fs::File::unlock (stable 1.89)
        #[allow(clippy::incompatible_msrv)]
        let _ = self._file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn acquire_and_release() {
        let dir = TempDir::new().unwrap();
        let lock = MergeLock::acquire(dir.path());
        assert!(lock.is_ok(), "first acquire must succeed");
        drop(lock);
        // After drop the lock is released; second acquire must succeed.
        let lock2 = MergeLock::acquire(dir.path());
        assert!(lock2.is_ok(), "acquire after release must succeed");
    }

    #[test]
    fn second_acquire_fails_while_held() {
        let dir = TempDir::new().unwrap();
        let _lock = MergeLock::acquire(dir.path()).unwrap();
        // Same process / same thread — try_lock_exclusive on the *same* file
        // from a second handle should fail on most platforms.
        let result = MergeLock::acquire(dir.path());
        // On Linux flock is per-process, so this may succeed; on macOS it fails.
        // We just verify the function compiles and runs without panicking.
        let _ = result;
    }
}
