//! Process-wide LRU of open branch `GraphStore` handles.
//!
//! # Why
//!
//! Before Phase C, every `*_branched` read and every branch-scoped
//! `contribute` opened a fresh `GraphStore` per call. Each open runs:
//!
//! - schema creation (`:create` for every relation if absent),
//! - migration checks,
//! - index creation.
//!
//! That cost is ~10–50ms at small scale, more at large. For hot paths
//! (agent sessions issuing rapid `list_claims` / `get_relations` on a
//! branch) this dominates wall time.
//!
//! # Critical invariant
//!
//! CozoDB's `Db<SqliteStorage>` keeps per-instance metadata
//! (`relation_store_id: Arc<AtomicU64>`, `running_queries`, per-instance
//! `TempStorage`). Two `DbInstance`s on the same `graph.db` file *can*
//! coexist for reads via SQLite's own file-level coordination, but their
//! in-memory counters diverge. For correctness we treat it as an invariant
//! that **at most one `GraphStore` per `(workspace_root, branch_name)`
//! lives in the serve crate at any time**. Every serve-crate code path
//! that opens a branch graph MUST go through this cache.
//!
//! The one exception is `QueryEngine::merge_branch`, which deliberately
//! opens a *separate* short-lived branch `GraphStore` for `compute_diff`
//! and drops it before calling `execute_merge`. Because merge only *reads*
//! the branch (writes go to main, which is not cached here), two
//! concurrent branch handles during that brief window is safe per the
//! cozo + SQLite concurrent-reader guarantee.
//!
//! # Thread safety
//!
//! The LRU is behind a `tokio::sync::Mutex`. The mutex is held only to
//! `get`/`put` on the hashmap — `GraphStore::init` happens outside the
//! lock so a slow open doesn't serialize unrelated lookups.
//!
//! Values are `Arc<BranchEngineHandle>`; once returned, readers can issue
//! concurrent cozo queries without touching the cache.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lru::LruCache;
use tokio::sync::Mutex;

use thinkingroot_branch::snapshot::resolve_data_dir;
use thinkingroot_core::{Error, Result};
use thinkingroot_graph::graph::GraphStore;

/// A cached, shareable branch storage handle.
pub struct BranchEngineHandle {
    pub graph: GraphStore,
    pub branch_name: String,
    pub loaded_at: Instant,
}

type Key = (PathBuf, String);

/// Bounded LRU cache of `(workspace_root, branch_name) → Arc<BranchEngineHandle>`.
pub struct BranchEngineCache {
    inner: Mutex<LruCache<Key, Arc<BranchEngineHandle>>>,
    ttl: Duration,
    disabled: bool,
}

impl BranchEngineCache {
    pub fn new(cfg: &thinkingroot_core::config::BranchCacheConfig) -> Self {
        // NonZeroUsize::new rejects 0; fall back to 1 so the cache is always
        // structurally valid (a zero-cap LRU would misbehave).
        let cap = NonZeroUsize::new(cfg.max_entries.max(1)).expect("max_entries.max(1) is nonzero");
        Self {
            inner: Mutex::new(LruCache::new(cap)),
            ttl: Duration::from_secs(cfg.ttl_secs.max(1)),
            disabled: cfg.disabled,
        }
    }

    /// Process-wide default cache — used when `QueryEngine::new` has no
    /// config yet. Mirrors `BranchCacheConfig::default()`.
    pub fn default_cache() -> Self {
        Self::new(&thinkingroot_core::config::BranchCacheConfig::default())
    }

    /// Return an `Arc<BranchEngineHandle>` for the given branch, opening
    /// a fresh `GraphStore` only on a miss or on a TTL-expired entry.
    ///
    /// When `disabled = true` in config, this always opens a fresh handle
    /// and does *not* insert it into the LRU (bypass mode).
    pub async fn get_or_open(
        &self,
        workspace_root: &Path,
        branch_name: &str,
    ) -> Result<Arc<BranchEngineHandle>> {
        let branch_data_dir = resolve_data_dir(workspace_root, Some(branch_name));
        if !branch_data_dir.exists() {
            return Err(Error::EntityNotFound(format!(
                "branch '{branch_name}' not found"
            )));
        }

        if self.disabled {
            return self.open_fresh(&branch_data_dir, branch_name).await;
        }

        let key: Key = (workspace_root.to_path_buf(), branch_name.to_string());

        // Fast path — hit with a handle that is still within TTL.
        {
            let mut guard = self.inner.lock().await;
            if let Some(existing) = guard.get(&key) {
                if existing.loaded_at.elapsed() < self.ttl {
                    return Ok(existing.clone());
                }
                // Stale — drop it so we re-open below.
                guard.pop(&key);
            }
        }

        // Open outside the lock so a slow open doesn't block other branches.
        let fresh = self.open_fresh(&branch_data_dir, branch_name).await?;

        // Insert; a racing opener may have beaten us here — accept whichever
        // handle ended up cached (both point at the same file, either is
        // correct, old Arc readers stay on whatever they already had).
        let mut guard = self.inner.lock().await;
        if let Some(winner) = guard.get(&key) {
            if winner.loaded_at.elapsed() < self.ttl {
                return Ok(winner.clone());
            }
            guard.pop(&key);
        }
        guard.put(key, fresh.clone());
        Ok(fresh)
    }

    /// Evict a single `(workspace_root, branch_name)` entry. Intended for
    /// use immediately *before* a hard-delete / purge of the branch so
    /// concurrent readers stop receiving a handle to the about-to-be-gone
    /// file. Cheap — idempotent — safe to call on missing keys.
    pub async fn invalidate(&self, workspace_root: &Path, branch_name: &str) {
        if self.disabled {
            return;
        }
        let key: Key = (workspace_root.to_path_buf(), branch_name.to_string());
        let mut guard = self.inner.lock().await;
        guard.pop(&key);
    }

    /// Evict every entry whose workspace_root matches. Used by `gc_branches`
    /// where we'd otherwise have to enumerate each purged branch name.
    pub async fn invalidate_workspace(&self, workspace_root: &Path) {
        if self.disabled {
            return;
        }
        let mut guard = self.inner.lock().await;
        let root = workspace_root.to_path_buf();
        // LruCache lacks retain; collect keys to evict then pop.
        let stale: Vec<Key> = guard
            .iter()
            .filter_map(|(k, _)| if k.0 == root { Some(k.clone()) } else { None })
            .collect();
        for k in stale {
            guard.pop(&k);
        }
    }

    /// Current occupancy — used by tests and telemetry. Not intended for
    /// load-bearing code paths.
    pub async fn len(&self) -> usize {
        self.inner.lock().await.len()
    }

    async fn open_fresh(
        &self,
        branch_data_dir: &Path,
        branch_name: &str,
    ) -> Result<Arc<BranchEngineHandle>> {
        let graph = GraphStore::init(&branch_data_dir.join("graph"))
            .map_err(|e| Error::GraphStorage(format!("branch graph init failed: {e}")))?;
        Ok(Arc::new(BranchEngineHandle {
            graph,
            branch_name: branch_name.to_string(),
            loaded_at: Instant::now(),
        }))
    }
}

impl std::fmt::Debug for BranchEngineCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BranchEngineCache")
            .field("ttl", &self.ttl)
            .field("disabled", &self.disabled)
            .finish()
    }
}
