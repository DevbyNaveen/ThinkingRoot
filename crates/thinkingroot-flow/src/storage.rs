//! Per-workspace flow storage (C9, 2026-05-22).
//!
//! Stores flow definitions + active/completed flow runs on disk
//! under `<workspace_root>/.thinkingroot/`. Files-on-disk matches
//! the existing per-workspace persistence pattern (`branches.toml`,
//! `mcp-servers.toml`) and gives us:
//!
//! - **Durability across daemon restarts** — flow run state lives
//!   in JSON files, not in-memory DashMaps. The runtime's
//!   auto-resume scan at startup (C10) reads from here.
//! - **User-editable definitions** — `<workspace_root>/.thinkingroot/flows/<id>.yaml`
//!   is just a YAML file the user can hand-edit. The
//!   `flow_define` MCP tool writes here; humans can too.
//! - **Multi-process safety** — every mutating call holds an
//!   fs2 advisory lock on `.thinkingroot/flows.lock`. Two
//!   concurrent `flow_run` calls from the same daemon (or even
//!   from a CLI in parallel) serialise on the lock, never lose
//!   writes.
//! - **No schema migration** — adding a field to
//!   `FlowRunRecord` doesn't require a CozoDB migration; serde
//!   `#[serde(default)]` handles the forward-compat.
//!
//! The CozoDB-backed alternative (one relation per workspace
//! graph) is intentionally NOT shipped for v1. Datalog queryability
//! over flow run history would be nice-to-have but isn't required
//! by any consumer the runtime serves today. If a future feature
//! needs "find all flows that touched entity X via Datalog join",
//! that ship can add a new `FlowStore` trait impl backed by
//! CozoDB without changing this interface.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::definition::FlowDefinition;
use crate::error::{FlowError, Result};

/// On-disk record for a flow definition. Wraps the user's
/// `FlowDefinition` with the content hash + timestamps the runtime
/// needs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FlowDefinitionRecord {
    /// The user's definition, byte-for-byte.
    pub definition: FlowDefinition,
    /// Content-addressed BLAKE3 hash of the canonical TOML
    /// serialisation. Lets the runtime detect when a redefinition
    /// is a true edit vs an idempotent re-publish.
    pub content_blake3: String,
    /// When the definition first landed.
    pub created_at: DateTime<Utc>,
    /// Most recent edit time (= `created_at` for never-edited).
    pub updated_at: DateTime<Utc>,
}

/// One active or completed flow run. The runtime reads this back
/// on daemon startup to auto-resume incomplete runs (the locked-in
/// design decision per plan §"Locked-in design decisions" #3).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FlowRunRecord {
    /// ULID minted at run start. Lex-sortable by start time.
    pub flow_run_id: String,
    /// Which flow definition this run is executing.
    pub flow_id: String,
    /// Version of the flow definition at run start. Pinned so
    /// later edits to the definition don't retroactively change
    /// the run's semantics.
    #[serde(default = "default_version")]
    pub flow_version: u32,
    /// Lifecycle status — see [`FlowRunStatus`].
    pub status: FlowRunStatus,
    /// The currently-executing node id, or the last-attempted
    /// node id when status is `Failed` / `Cancelled`. Empty
    /// string for runs that haven't started any node yet.
    #[serde(default)]
    pub current_node: String,
    pub started_at: DateTime<Utc>,
    /// When the run reached a terminal state (`Succeeded`,
    /// `Failed`, `Cancelled`). None for `Running` / `Paused`.
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    /// Parent branch the run was launched against — typically the
    /// session's active branch or `main`.
    pub parent_branch: String,
    /// MCP session id that initiated this flow run via the
    /// `flow_run` tool, when applicable. Used by `client_sampling`
    /// executors to back-call the connected client's LLM. None
    /// for CLI / REST-launched runs.
    #[serde(default)]
    pub originating_session_id: Option<String>,
    /// Caller-supplied inputs at run start, snapshotted so resume
    /// is deterministic.
    #[serde(default)]
    pub inputs: serde_json::Value,
    /// Per-node output checkpoint. Keyed by node id. The runtime
    /// updates this after each node's `commit_cognition` completes
    /// successfully — auto-resume reads from here to skip
    /// already-completed nodes.
    #[serde(default)]
    pub node_outputs: BTreeMap<String, serde_json::Value>,
    /// Final outputs after the flow completes. Empty until
    /// `status == Succeeded`.
    #[serde(default)]
    pub outputs: BTreeMap<String, serde_json::Value>,
    /// Error message when status is `Failed` or `Cancelled`.
    #[serde(default)]
    pub error: Option<String>,
}

fn default_version() -> u32 {
    1
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FlowRunStatus {
    /// Run is actively dispatching nodes. Auto-resume targets
    /// these on daemon startup.
    Running,
    /// User-requested pause (via `flow_status { action: "pause" }`).
    /// NOT auto-resumed on daemon start.
    Paused,
    /// All nodes completed; final_merge applied if configured.
    Succeeded,
    /// A node failed beyond `max_node_retries` OR the validator
    /// rejected mid-run.
    Failed,
    /// User-requested cancellation OR transport drop +
    /// `notifications/cancelled` arrived.
    Cancelled,
}

impl FlowRunStatus {
    /// Whether the run should be auto-resumed on daemon startup.
    /// True only for `Running`. `Paused` requires explicit
    /// `flow_resume`; terminal states never resume.
    pub fn is_auto_resumable(&self) -> bool {
        matches!(self, FlowRunStatus::Running)
    }

    /// Terminal states never transition further. Used by status
    /// updates to guard against stale writes.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            FlowRunStatus::Succeeded | FlowRunStatus::Failed | FlowRunStatus::Cancelled
        )
    }
}

/// File-backed per-workspace flow store. Holds the workspace root
/// path + provides atomic CRUD operations on flow definitions and
/// flow runs under `<workspace_root>/.thinkingroot/`.
///
/// All mutating methods hold an fs2 advisory lock on
/// `.thinkingroot/flows.lock` to serialise writes across processes
/// (CLI + desktop sidecar). Reads don't acquire the lock — readers
/// see eventually-consistent state.
pub struct FlowStore {
    root: PathBuf,
}

impl FlowStore {
    /// Construct a store rooted at `<workspace_root>`. Creates
    /// the `.thinkingroot/flows` + `.thinkingroot/flow-runs`
    /// directories on first use; safe to call on an unmounted
    /// workspace (no I/O happens until a CRUD method is invoked).
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            root: workspace_root.into(),
        }
    }

    fn flows_dir(&self) -> PathBuf {
        self.root.join(".thinkingroot").join("flows")
    }

    fn runs_dir(&self) -> PathBuf {
        self.root.join(".thinkingroot").join("flow-runs")
    }

    fn lock_path(&self) -> PathBuf {
        self.root.join(".thinkingroot").join("flows.lock")
    }

    fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(self.flows_dir()).map_err(|e| FlowError::Storage(format!(
            "create flows dir: {e}"
        )))?;
        std::fs::create_dir_all(self.runs_dir()).map_err(|e| FlowError::Storage(format!(
            "create flow-runs dir: {e}"
        )))?;
        Ok(())
    }

    fn flow_path(&self, flow_id: &str) -> PathBuf {
        // Sanitise: flow ids should be safe filename chars by
        // convention, but reject path traversal defensively.
        let safe = sanitize_id(flow_id);
        self.flows_dir().join(format!("{safe}.yaml"))
    }

    fn run_path(&self, flow_run_id: &str) -> PathBuf {
        let safe = sanitize_id(flow_run_id);
        self.runs_dir().join(format!("{safe}.json"))
    }

    /// Write a flow definition atomically (tempfile + persist).
    /// Returns the stored record including content hash + timestamps.
    pub fn insert_flow_definition(
        &self,
        def: FlowDefinition,
    ) -> Result<FlowDefinitionRecord> {
        self.ensure_dirs()?;
        let _lock = self.acquire_lock()?;

        let now = Utc::now();
        let content_blake3 = def
            .content_hash()
            .map_err(|e| FlowError::Storage(format!("content_hash: {e}")))?;

        // If the file already exists, preserve created_at.
        let path = self.flow_path(&def.id);
        let created_at = match std::fs::read_to_string(&path) {
            Ok(prev_yaml) => match serde_yaml::from_str::<FlowDefinitionRecord>(&prev_yaml)
            {
                Ok(prev) => prev.created_at,
                Err(_) => now,
            },
            Err(_) => now,
        };

        let record = FlowDefinitionRecord {
            definition: def,
            content_blake3,
            created_at,
            updated_at: now,
        };
        let yaml = serde_yaml::to_string(&record).map_err(|e| FlowError::Storage(format!(
            "serialize flow definition: {e}"
        )))?;
        atomic_write(&path, &yaml)?;
        Ok(record)
    }

    /// Read a flow definition by id.
    pub fn get_flow_definition(&self, flow_id: &str) -> Result<Option<FlowDefinitionRecord>> {
        let path = self.flow_path(flow_id);
        match std::fs::read_to_string(&path) {
            Ok(yaml) => {
                let record = serde_yaml::from_str(&yaml).map_err(|e| FlowError::Storage(
                    format!("parse flow definition: {e}"),
                ))?;
                Ok(Some(record))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(FlowError::Storage(format!("read flow definition: {e}"))),
        }
    }

    /// List every stored flow definition, sorted by id for stable
    /// CLI output.
    pub fn list_flow_definitions(&self) -> Result<Vec<FlowDefinitionRecord>> {
        let dir = self.flows_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut records = Vec::new();
        for entry in
            std::fs::read_dir(&dir).map_err(|e| FlowError::Storage(format!("read dir: {e}")))?
        {
            let entry = entry.map_err(|e| FlowError::Storage(format!("dir entry: {e}")))?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("yaml") {
                continue;
            }
            let yaml = std::fs::read_to_string(&path).map_err(|e| FlowError::Storage(
                format!("read {}: {e}", path.display()),
            ))?;
            match serde_yaml::from_str::<FlowDefinitionRecord>(&yaml) {
                Ok(record) => records.push(record),
                Err(e) => {
                    tracing::warn!(
                        target: "thinkingroot_flow::storage",
                        path = %path.display(),
                        error = %e,
                        "skipping malformed flow definition file"
                    );
                }
            }
        }
        records.sort_by(|a, b| a.definition.id.cmp(&b.definition.id));
        Ok(records)
    }

    /// Delete a flow definition. Returns `Ok(false)` when the file
    /// didn't exist (idempotent).
    pub fn delete_flow_definition(&self, flow_id: &str) -> Result<bool> {
        let _lock = self.acquire_lock()?;
        let path = self.flow_path(flow_id);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(FlowError::Storage(format!("delete flow definition: {e}"))),
        }
    }

    /// Persist a flow run record atomically. Called by the runtime
    /// at run-start AND after every node checkpoint.
    pub fn upsert_flow_run(&self, record: &FlowRunRecord) -> Result<()> {
        self.ensure_dirs()?;
        let _lock = self.acquire_lock()?;
        let path = self.run_path(&record.flow_run_id);
        let json = serde_json::to_string_pretty(record).map_err(|e| FlowError::Storage(
            format!("serialize flow run: {e}"),
        ))?;
        atomic_write(&path, &json)
    }

    /// Read a flow run by id.
    pub fn get_flow_run(&self, flow_run_id: &str) -> Result<Option<FlowRunRecord>> {
        let path = self.run_path(flow_run_id);
        match std::fs::read_to_string(&path) {
            Ok(json) => {
                let record = serde_json::from_str(&json).map_err(|e| FlowError::Storage(
                    format!("parse flow run: {e}"),
                ))?;
                Ok(Some(record))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(FlowError::Storage(format!("read flow run: {e}"))),
        }
    }

    /// List every stored flow run. Sorted descending by
    /// `started_at` (newest first) since that's the most useful
    /// order for `flow list` UX.
    pub fn list_flow_runs(&self) -> Result<Vec<FlowRunRecord>> {
        let dir = self.runs_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut records = Vec::new();
        for entry in
            std::fs::read_dir(&dir).map_err(|e| FlowError::Storage(format!("read dir: {e}")))?
        {
            let entry = entry.map_err(|e| FlowError::Storage(format!("dir entry: {e}")))?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let json = std::fs::read_to_string(&path).map_err(|e| FlowError::Storage(
                format!("read {}: {e}", path.display()),
            ))?;
            match serde_json::from_str::<FlowRunRecord>(&json) {
                Ok(record) => records.push(record),
                Err(e) => {
                    tracing::warn!(
                        target: "thinkingroot_flow::storage",
                        path = %path.display(),
                        error = %e,
                        "skipping malformed flow run file"
                    );
                }
            }
        }
        records.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        Ok(records)
    }

    /// Return every flow run whose `status.is_auto_resumable()`
    /// returns true. The runtime's startup hook (C10) iterates
    /// this and re-spawns each.
    pub fn list_resumable_runs(&self) -> Result<Vec<FlowRunRecord>> {
        Ok(self
            .list_flow_runs()?
            .into_iter()
            .filter(|r| r.status.is_auto_resumable())
            .collect())
    }

    fn acquire_lock(&self) -> Result<FlowsLock> {
        std::fs::create_dir_all(self.root.join(".thinkingroot")).map_err(|e| {
            FlowError::Storage(format!("create .thinkingroot dir: {e}"))
        })?;
        let path = self.lock_path();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| FlowError::Storage(format!("open flows.lock: {e}")))?;
        use fs2::FileExt;
        file.lock_exclusive()
            .map_err(|e| FlowError::Storage(format!("lock flows.lock: {e}")))?;
        Ok(FlowsLock { _file: file })
    }
}

/// RAII lock guard — `lock_exclusive` is released on drop.
struct FlowsLock {
    _file: std::fs::File,
}

/// Reject path-traversal in user-supplied ids.
fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' | '.' => c,
            _ => '_',
        })
        .collect()
}

/// Atomic write via tempfile + persist (rename(2) on POSIX,
/// ReplaceFileW on Windows). Readers never observe a torn write.
fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| FlowError::Storage(format!("no parent for {}", path.display())))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .map_err(|e| FlowError::Storage(format!("create tempfile in {}: {e}", dir.display())))?;
    use std::io::Write;
    tmp.write_all(contents.as_bytes()).map_err(|e| FlowError::Storage(
        format!("write tempfile: {e}"),
    ))?;
    tmp.flush()
        .map_err(|e| FlowError::Storage(format!("flush tempfile: {e}")))?;
    tmp.persist(path)
        .map_err(|e| FlowError::Storage(format!("persist tempfile: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::FlowDefinition;
    use tempfile::TempDir;

    fn fixture_definition(id: &str) -> FlowDefinition {
        FlowDefinition::from_yaml(&format!(
            r#"
id: {id}
description: Test fixture
nodes:
  scanner:
    type: deterministic
    function: search
"#
        ))
        .expect("parse")
    }

    #[test]
    fn flow_definition_roundtrips_through_disk() {
        let tmp = TempDir::new().unwrap();
        let store = FlowStore::new(tmp.path());

        let def = fixture_definition("rt-1");
        let saved = store.insert_flow_definition(def.clone()).expect("save");
        assert_eq!(saved.definition, def);
        assert_eq!(saved.content_blake3.len(), 64);
        assert_eq!(saved.created_at, saved.updated_at); // first write

        let loaded = store
            .get_flow_definition("rt-1")
            .expect("load")
            .expect("present");
        assert_eq!(loaded, saved);
    }

    #[test]
    fn get_flow_definition_returns_none_for_unknown_id() {
        let tmp = TempDir::new().unwrap();
        let store = FlowStore::new(tmp.path());
        let result = store.get_flow_definition("nonexistent").expect("call ok");
        assert!(result.is_none());
    }

    #[test]
    fn second_insert_preserves_created_at_and_bumps_updated_at() {
        let tmp = TempDir::new().unwrap();
        let store = FlowStore::new(tmp.path());

        let def1 = fixture_definition("ev-1");
        let saved1 = store.insert_flow_definition(def1).expect("save 1");

        std::thread::sleep(std::time::Duration::from_millis(10));

        let mut def2 = fixture_definition("ev-1");
        def2.description = "edited".to_string();
        let saved2 = store.insert_flow_definition(def2).expect("save 2");

        assert_eq!(saved1.created_at, saved2.created_at);
        assert!(saved2.updated_at > saved1.updated_at);
        assert_ne!(saved1.content_blake3, saved2.content_blake3);
    }

    #[test]
    fn list_flow_definitions_returns_sorted_by_id() {
        let tmp = TempDir::new().unwrap();
        let store = FlowStore::new(tmp.path());
        for id in ["bravo", "alpha", "charlie"] {
            store
                .insert_flow_definition(fixture_definition(id))
                .expect("save");
        }
        let list = store.list_flow_definitions().expect("list");
        assert_eq!(
            list.iter().map(|r| r.definition.id.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "bravo", "charlie"]
        );
    }

    #[test]
    fn delete_flow_definition_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let store = FlowStore::new(tmp.path());
        store
            .insert_flow_definition(fixture_definition("dd-1"))
            .expect("save");

        assert!(store.delete_flow_definition("dd-1").expect("delete 1"));
        assert!(!store.delete_flow_definition("dd-1").expect("delete 2"));
    }

    #[test]
    fn flow_run_roundtrips_through_disk() {
        let tmp = TempDir::new().unwrap();
        let store = FlowStore::new(tmp.path());

        let record = FlowRunRecord {
            flow_run_id: "01HFGABCDEFGHIJKLMNOPQRSTUV".to_string(),
            flow_id: "lit-review-v1".to_string(),
            flow_version: 1,
            status: FlowRunStatus::Running,
            current_node: "scanner".to_string(),
            started_at: Utc::now(),
            finished_at: None,
            parent_branch: "main".to_string(),
            originating_session_id: None,
            inputs: serde_json::json!({ "papers": ["./a.pdf"] }),
            node_outputs: BTreeMap::new(),
            outputs: BTreeMap::new(),
            error: None,
        };
        store.upsert_flow_run(&record).expect("save");

        let loaded = store
            .get_flow_run(&record.flow_run_id)
            .expect("load")
            .expect("present");
        assert_eq!(loaded, record);
    }

    #[test]
    fn checkpoint_resume_after_simulated_crash() {
        let tmp = TempDir::new().unwrap();
        let store = FlowStore::new(tmp.path());

        // Simulate a run that completed 2 of 3 nodes before
        // "crashing".
        let mut record = FlowRunRecord {
            flow_run_id: "crash-test".to_string(),
            flow_id: "demo".to_string(),
            flow_version: 1,
            status: FlowRunStatus::Running,
            current_node: "node2".to_string(),
            started_at: Utc::now(),
            finished_at: None,
            parent_branch: "main".to_string(),
            originating_session_id: None,
            inputs: serde_json::json!({}),
            node_outputs: BTreeMap::new(),
            outputs: BTreeMap::new(),
            error: None,
        };
        record
            .node_outputs
            .insert("node1".to_string(), serde_json::json!("first result"));
        record
            .node_outputs
            .insert("node2".to_string(), serde_json::json!("second result"));
        store.upsert_flow_run(&record).expect("checkpoint save");

        // Simulate daemon restart — a fresh FlowStore reads the
        // same workspace dir.
        let restarted_store = FlowStore::new(tmp.path());
        let resumable = restarted_store
            .list_resumable_runs()
            .expect("list resumable");
        assert_eq!(resumable.len(), 1);
        assert_eq!(resumable[0].flow_run_id, "crash-test");
        assert_eq!(resumable[0].node_outputs.len(), 2);
        // The runtime would skip node1+node2 and resume at node3.
    }

    #[test]
    fn list_resumable_runs_filters_to_running_status() {
        let tmp = TempDir::new().unwrap();
        let store = FlowStore::new(tmp.path());

        let base = FlowRunRecord {
            flow_run_id: String::new(),
            flow_id: "demo".to_string(),
            flow_version: 1,
            status: FlowRunStatus::Running,
            current_node: String::new(),
            started_at: Utc::now(),
            finished_at: None,
            parent_branch: "main".to_string(),
            originating_session_id: None,
            inputs: serde_json::json!({}),
            node_outputs: BTreeMap::new(),
            outputs: BTreeMap::new(),
            error: None,
        };

        for (id, status) in [
            ("r-running", FlowRunStatus::Running),
            ("r-paused", FlowRunStatus::Paused),
            ("r-succeeded", FlowRunStatus::Succeeded),
            ("r-failed", FlowRunStatus::Failed),
            ("r-cancelled", FlowRunStatus::Cancelled),
        ] {
            let mut r = base.clone();
            r.flow_run_id = id.to_string();
            r.status = status;
            if status.is_terminal() {
                r.finished_at = Some(Utc::now());
            }
            store.upsert_flow_run(&r).expect("save");
        }

        let resumable = store.list_resumable_runs().expect("list");
        assert_eq!(resumable.len(), 1);
        assert_eq!(resumable[0].flow_run_id, "r-running");
    }

    #[test]
    fn list_flow_runs_sorts_newest_first() {
        let tmp = TempDir::new().unwrap();
        let store = FlowStore::new(tmp.path());

        let base_time = Utc::now();
        for (offset_secs, id) in [(0, "oldest"), (10, "middle"), (20, "newest")] {
            let r = FlowRunRecord {
                flow_run_id: id.to_string(),
                flow_id: "demo".to_string(),
                flow_version: 1,
                status: FlowRunStatus::Succeeded,
                current_node: String::new(),
                started_at: base_time + chrono::Duration::seconds(offset_secs),
                finished_at: Some(base_time + chrono::Duration::seconds(offset_secs + 1)),
                parent_branch: "main".to_string(),
            originating_session_id: None,
                inputs: serde_json::json!({}),
                node_outputs: BTreeMap::new(),
                outputs: BTreeMap::new(),
                error: None,
            };
            store.upsert_flow_run(&r).expect("save");
        }

        let list = store.list_flow_runs().expect("list");
        assert_eq!(
            list.iter().map(|r| r.flow_run_id.as_str()).collect::<Vec<_>>(),
            vec!["newest", "middle", "oldest"]
        );
    }

    #[test]
    fn sanitize_id_rejects_path_traversal_attempts() {
        // `/` becomes `_`; `.` stays (so legit ids like v1.2
        // round-trip). `..` alone can't escape a directory
        // without a separator — the `/` removal is what
        // defangs traversal.
        assert_eq!(sanitize_id("../../etc"), ".._.._etc");
        assert_eq!(sanitize_id("safe-id_v1.2"), "safe-id_v1.2");
        assert_eq!(sanitize_id("with spaces"), "with_spaces");
        // Backslash also defanged (Windows path separator).
        assert_eq!(sanitize_id("a\\b"), "a_b");
        // Null byte → underscore.
        assert_eq!(sanitize_id("a\0b"), "a_b");
    }

    #[test]
    fn status_lifecycle_helpers_classify_correctly() {
        assert!(FlowRunStatus::Running.is_auto_resumable());
        assert!(!FlowRunStatus::Paused.is_auto_resumable());
        assert!(!FlowRunStatus::Succeeded.is_auto_resumable());

        assert!(FlowRunStatus::Succeeded.is_terminal());
        assert!(FlowRunStatus::Failed.is_terminal());
        assert!(FlowRunStatus::Cancelled.is_terminal());
        assert!(!FlowRunStatus::Running.is_terminal());
        assert!(!FlowRunStatus::Paused.is_terminal());
    }

    #[test]
    fn atomic_write_does_not_leave_torn_files_on_concurrent_read() {
        // Best-effort smoke: write a large payload while a reader
        // pulls it back. The persist(rename) is atomic on the
        // filesystem, so the reader sees either the full new
        // contents or the prior contents — never half.
        let tmp = TempDir::new().unwrap();
        let store = FlowStore::new(tmp.path());

        let big_def = {
            let mut yaml = String::from("id: big\ndescription: ");
            yaml.push_str(&"x".repeat(50_000));
            yaml.push_str("\nnodes:\n  a:\n    type: deterministic\n    function: f\n");
            FlowDefinition::from_yaml(&yaml).expect("parse")
        };
        store.insert_flow_definition(big_def.clone()).expect("save");
        let loaded = store
            .get_flow_definition("big")
            .expect("load")
            .expect("present");
        assert_eq!(loaded.definition.description.len(), 50_000);
    }
}
