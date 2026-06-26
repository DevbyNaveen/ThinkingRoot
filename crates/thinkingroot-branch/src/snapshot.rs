// crates/thinkingroot-branch/src/snapshot.rs
use std::fs;
use std::path::{Path, PathBuf};
use thinkingroot_core::Result;

/// T2.1 — Clone a file using the fastest path the host filesystem
/// supports, falling back to a regular byte copy.
///
/// - **macOS APFS**: `libc::clonefile` does a copy-on-write reflink in
///   O(1) regardless of file size.  A 4 GB `graph.db` clones in under
///   1 ms.  Falls back to `fs::copy` if the kernel rejects the call
///   (cross-volume, non-APFS target, etc.).
/// - **Linux btrfs/xfs/zfs**: `FICLONE` ioctl gives the same
///   semantics; we attempt it first via `reflink_copy::reflink` and
///   fall back to `fs::copy` on `EINVAL`/`EOPNOTSUPP`.
/// - **Everything else**: `fs::copy` — same byte-by-byte path the
///   pre-T2.1 code used.  Performance regression is impossible
///   because we only ever upgrade.
///
/// We deliberately do NOT bring in the `reflink_copy` crate because
/// it adds a workspace-level dependency for a single call site.
/// Instead the macOS path uses a thin `libc::clonefile` FFI and the
/// Linux path uses the `FICLONE` ioctl directly.  Both are stable
/// kernel APIs older than this codebase.
pub fn clone_file_fast(src: &Path, dst: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        match clone_file_macos(src, dst) {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::debug!(
                    src = %src.display(),
                    dst = %dst.display(),
                    error = %e,
                    "clonefile fell back to fs::copy"
                );
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        match clone_file_linux(src, dst) {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::debug!(
                    src = %src.display(),
                    dst = %dst.display(),
                    error = %e,
                    "FICLONE fell back to fs::copy"
                );
            }
        }
    }
    fs::copy(src, dst).map(|_| ()).map_err(Into::into)
}

#[cfg(target_os = "macos")]
fn clone_file_macos(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let src_c = CString::new(src.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let dst_c = CString::new(dst.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

    // SAFETY: clonefile is a stable macOS syscall (since 10.12).
    // - src_c and dst_c are valid, NUL-terminated C strings on the
    //   stack for the duration of the call.
    // - flags = 0 is the documented "default" — no follow-symlink,
    //   no clone-attributes-only.
    // - clonefile returns 0 on success and -1 with errno set on
    //   failure; we map that to std::io::Error.
    unsafe extern "C" {
        fn clonefile(
            src: *const libc::c_char,
            dst: *const libc::c_char,
            flags: libc::c_int,
        ) -> libc::c_int;
    }
    let rc = unsafe { clonefile(src_c.as_ptr(), dst_c.as_ptr(), 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn clone_file_linux(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;

    // FICLONE = _IOW(0x94, 9, int) — see linux/fs.h.  Encodes as
    // 0x40049409 on every architecture Linux ships on (the type is
    // fixed at i32 sized = 4).  We hard-code rather than recompute
    // via the _IOW macro because the latter would require a
    // platform-conditional libc dependency.
    const FICLONE: libc::c_ulong = 0x40049409;

    let src_file = std::fs::File::open(src)?;
    // O_CREAT | O_WRONLY | O_TRUNC — same semantics as fs::copy's
    // destination open: replace if present, error otherwise.
    let dst_file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(dst)?;
    // SAFETY: ioctl is a stable Linux syscall.  Both fds are alive
    // for the duration of the call; FICLONE takes the source fd as
    // the int argument.
    let rc = unsafe {
        libc::ioctl(
            dst_file.as_raw_fd(),
            FICLONE,
            src_file.as_raw_fd() as libc::c_int,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        // On EINVAL / EOPNOTSUPP the target FS doesn't support the
        // ioctl; the caller (`clone_file_fast`) falls back to
        // fs::copy after this returns.  Make sure the half-written
        // dst doesn't poison the fallback by removing it first.
        let _ = std::fs::remove_file(dst);
        Err(std::io::Error::last_os_error())
    }
}

/// Convert a branch name to a filesystem-safe slug.
/// "feature/graphql" → "feature-graphql"
/// "My Branch" → "my-branch"
///
/// Inputs that contain only separator/whitespace characters (e.g.
/// `"///"`, `"   "`, `"--"`) collapse to the empty string under the
/// previous algorithm — and an empty slug joined with the data
/// directory yields `<root>/.thinkingroot/branches/`, which then
/// races every other empty-slug branch into the same directory.
/// Returns the literal `"branch"` for those degenerate inputs so the
/// resolved data dir is unambiguous.
pub fn slugify(name: &str) -> String {
    let slug = name
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        "branch".to_string()
    } else {
        slug
    }
}

/// Resolve the data directory for a given branch.
/// main (or None) → `{root}/.thinkingroot`
/// other branch   → `{root}/.thinkingroot/branches/{slug}`
pub fn resolve_data_dir(root_path: &Path, branch: Option<&str>) -> PathBuf {
    match branch {
        None | Some("main") => root_path.join(".thinkingroot"),
        Some(name) => root_path
            .join(".thinkingroot")
            .join("branches")
            .join(slugify(name)),
    }
}

/// Migrate legacy branch directories from the old sibling layout to the new
/// nested layout in a single pass.
///
/// Old: `{root}/.thinkingroot-{slug}/`
/// New: `{root}/.thinkingroot/branches/{slug}/`
///
/// Skips `.thinkingroot-refs` (the branch registry — never a data dir).
/// Skips any branch whose target already exists (idempotent).
/// Returns the number of directories successfully migrated.
pub fn migrate_legacy_layout(root_path: &Path) -> Result<usize> {
    let prefix = ".thinkingroot-";
    let branches_dir = root_path.join(".thinkingroot").join("branches");

    let entries = match fs::read_dir(root_path) {
        Ok(e) => e,
        Err(_) => return Ok(0),
    };

    let mut migrated = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if !name_str.starts_with(prefix) || !entry.path().is_dir() {
            continue;
        }
        // Skip the refs directory — it's not a branch data dir.
        if name_str == ".thinkingroot-refs" {
            continue;
        }

        let slug = &name_str[prefix.len()..];
        let target = branches_dir.join(slug);

        if target.exists() {
            continue; // already migrated or created by new code
        }

        fs::create_dir_all(&branches_dir)?;
        fs::rename(entry.path(), &target)?;
        tracing::info!(
            "migrated branch '{}' → .thinkingroot/branches/{}",
            slug,
            slug
        );
        migrated += 1;
    }

    Ok(migrated)
}

/// Subdirectory name under `{branch_data_dir}/graph/` that holds the
/// immutable T0.5 LCA snapshot.  Layout:
///
/// ```text
/// {branch_data_dir}/graph/graph.db                     ← mutable working copy
/// {branch_data_dir}/graph/parent-at-fork/graph.db      ← immutable LCA
/// ```
///
/// Putting it in its own subdirectory lets `GraphStore::init` open it
/// directly without temp-file gymnastics.
pub const PARENT_AT_FORK_DIR: &str = "parent-at-fork";

/// Resolve the directory `GraphStore::init` should be called with to
/// open the T0.5 LCA snapshot for a branch.  Returns
/// `<branch_data_dir>/graph/parent-at-fork`.
pub fn parent_at_fork_dir(branch_data_dir: &Path) -> PathBuf {
    branch_data_dir.join("graph").join(PARENT_AT_FORK_DIR)
}

/// Create the directory layout for a new branch:
/// - Copy `{parent_data_dir}/graph/graph.db` → `{branch_data_dir}/graph/graph.db`
/// - Copy that same file → `{branch_data_dir}/graph/parent-at-fork/graph.db`
///   (immutable T0.5 LCA snapshot — see [`PARENT_AT_FORK_DIR`])
/// - Symlink `{parent_data_dir}/models` → `{branch_data_dir}/models`
/// - Symlink `{parent_data_dir}/cache`  → `{branch_data_dir}/cache`
pub fn create_branch_layout(parent_data_dir: &Path, branch_data_dir: &Path) -> Result<()> {
    let branch_graph_dir = branch_data_dir.join("graph");
    fs::create_dir_all(&branch_graph_dir)?;

    // Copy graph.db (mutable working copy) AND the same bytes to a
    // separate immutable subdirectory the merge gate can open as a
    // GraphStore via the LCA.  T0.5 §"Snapshot at fork" — without
    // this, three-way merge cannot identify the lowest common
    // ancestor and would silently last-writer-win on concurrent
    // edits to the same claim.
    // graph.db: with the RocksDB backend it is a DIRECTORY held OPEN by the
    // daemon (exclusive per-dir lock), so a raw file/dir copy would be torn /
    // inconsistent. Use cozo's CONSISTENT backup instead: dump the parent via
    // its SHARED handle (GraphStore::init resolves to the daemon's already-open
    // DB through the global OPEN_DBS registry — no second open) → restore into
    // the branch working copy AND the immutable parent-at-fork LCA snapshot.
    // Backend-agnostic: works identically on SQLite. (The LCA copy is what lets
    // three-way merge identify the lowest common ancestor — T0.5 §"Snapshot at
    // fork" — instead of silently last-writer-winning concurrent edits.)
    let parent_graph_dir = parent_data_dir.join("graph");
    let src_db = parent_graph_dir.join("graph.db");
    if src_db.exists() {
        use thinkingroot_graph::graph::GraphStore;
        let staging = branch_graph_dir.join(".fork-staging.bak");
        let _ = fs::remove_file(&staging);
        {
            // Shared handle (registry hit when the daemon owns it) → consistent
            // point-in-time dump. Do NOT release it: the daemon may still own it.
            let parent = GraphStore::init(&parent_graph_dir)?;
            parent.raw_db().backup_db(&staging).map_err(|e| {
                thinkingroot_core::Error::GraphStorage(format!(
                    "branch fork: backup of parent graph failed: {e}"
                ))
            })?;
        }
        for target in [
            branch_graph_dir.clone(),
            branch_graph_dir.join(PARENT_AT_FORK_DIR),
        ] {
            fs::create_dir_all(&target)?;
            // Grow a FRESH empty DB from the parent's dump (restore_backup needs
            // an empty target — init_from_backup deliberately skips create_schema).
            let store = GraphStore::init_from_backup(&target, &staging)?;
            drop(store);
            // We opened these fresh dirs; unpin so a later mount re-opens cleanly.
            GraphStore::release(&target);
        }
        let _ = fs::remove_file(&staging);
    }

    // Copy vectors.bin — must be a physical copy, not a symlink.
    // Each branch has its own writable vector index; sharing would corrupt the parent.
    let src_vec = parent_data_dir.join("vectors.bin");
    let dst_vec = branch_data_dir.join("vectors.bin");
    if src_vec.exists() {
        clone_file_fast(&src_vec, &dst_vec)?;
    }

    // Share models/ (fastembed cache — ~300MB, never duplicate).
    // Unix: symlink. Windows: copy recursively (junctions require elevated perms).
    let parent_models = parent_data_dir.join("models");
    let branch_models = branch_data_dir.join("models");
    if parent_models.exists() && !branch_models.exists() {
        #[cfg(unix)]
        std::os::unix::fs::symlink(&parent_models, &branch_models)?;
        #[cfg(windows)]
        copy_dir_all(&parent_models, &branch_models)?;
    }

    // Share cache/ (extraction cache).
    let parent_cache = parent_data_dir.join("cache");
    let branch_cache = branch_data_dir.join("cache");
    if parent_cache.exists() && !branch_cache.exists() {
        #[cfg(unix)]
        std::os::unix::fs::symlink(&parent_cache, &branch_cache)?;
        #[cfg(windows)]
        copy_dir_all(&parent_cache, &branch_cache)?;
    }

    Ok(())
}

#[cfg(test)]
mod clone_tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn clone_file_fast_produces_byte_identical_copy() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.bin");
        let dst = dir.path().join("dst.bin");
        // 64 KB of pseudo-random-but-deterministic bytes — large
        // enough to exercise a real copy path without making the
        // test slow.
        let bytes: Vec<u8> = (0..65_536).map(|i| (i * 31 + 7) as u8).collect();
        fs::write(&src, &bytes).unwrap();

        clone_file_fast(&src, &dst).expect("clone must succeed");

        let copied = fs::read(&dst).unwrap();
        assert_eq!(
            copied, bytes,
            "clone must reproduce source bytes exactly (no path diverges from byte equality)"
        );
        assert!(
            src.exists(),
            "source must still exist after clone (clone is a copy, not a move)"
        );
    }

    #[test]
    fn clone_file_fast_falls_back_when_target_dir_missing() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.bin");
        // Destination's parent does NOT exist — both APFS clonefile
        // and FICLONE error out, and the fallback fs::copy errors
        // too.  Function MUST surface the error rather than silently
        // succeed.
        let dst = dir.path().join("nonexistent").join("dst.bin");
        fs::write(&src, b"hello").unwrap();
        let err = clone_file_fast(&src, &dst);
        assert!(
            err.is_err(),
            "missing parent dir must surface, never silently succeed"
        );
    }
}

/// Recursively copy a directory tree from `src` to `dst`.
///
/// Used on Windows as a fallback when creating branch layouts, since
/// creating symlinks there requires elevated privileges.
#[allow(dead_code)]
fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_all(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}
