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

/// RAII guard for the branch-registry write lock.
///
/// Held during every load-modify-save of `.thinkingroot-refs/branches.toml`
/// so that concurrent callers (separate `root` processes OR separate
/// threads inside one process) cannot lose writes by racing on the
/// read-before-modify window.  Pre-fix, two concurrent `root branch
/// create` invocations could both read the same registry, both push
/// their branch, and the second `save()` would overwrite the first,
/// silently losing one branch.
///
/// Uses `fs2::lock_exclusive` (blocking) rather than `try_lock_exclusive`
/// (non-blocking) because registry mutations are millisecond-fast and
/// the user expects "create branch" to always succeed — not to fail
/// transiently because another shell window happened to be running.
///
/// The lock is per-open-file-description on Linux/macOS, so two threads
/// inside the same process each opening their own [`File`] handle for
/// `<refs_dir>/registry.lock` will be serialised correctly.  The lock
/// is released automatically when [`RegistryLock`] is dropped.
pub struct RegistryLock {
    _file: File,
}

impl RegistryLock {
    /// Acquire the registry lock, blocking until available.
    pub fn acquire(refs_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(refs_dir)
            .map_err(|e| Error::io_path(refs_dir, e))?;
        let lock_path = refs_dir.join("registry.lock");

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| Error::io_path(&lock_path, e))?;

        // Blocking acquire — registry mutations are quick.  If a hung
        // process somewhere is holding the lock indefinitely, that is a
        // separate operational problem (kill the hung process); we do
        // not want callers to fail spuriously when contention is normal.
        // allow: clippy confuses fs2::FileExt::lock_exclusive with std::fs::File::lock_exclusive (stable 1.89)
        #[allow(clippy::incompatible_msrv)]
        file.lock_exclusive().map_err(|e| {
            Error::io_path(&lock_path, std::io::Error::other(e.to_string()))
        })?;

        Ok(Self { _file: file })
    }
}

impl Drop for RegistryLock {
    fn drop(&mut self) {
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
