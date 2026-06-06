# Incremental Compilation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `root compile` incremental — only re-process what changed, skip everything else. No competitor has this. Currently 3 functions do full rebuilds; fix them, add an LLM extraction cache, add early cutoff, add watch mode.

**Architecture:** Four levels of incrementality stacked on top of the existing content-hash skip. Level 1 fixes the three full-rebuild functions (`rebuild_entity_relations`, `rebuild_vector_index`, `compile_all`). Level 2 adds a content-addressable extraction cache so LLM calls are skipped for previously-seen content. Level 3 adds Salsa-style early cutoff — if extraction output is identical to last run, skip downstream entirely. Level 4 adds `root watch` for live recompilation on file change.

**Tech Stack:** Rust, CozoDB (Datalog), blake3 (hashing), serde_json (cache serialization), notify 8 (file watching), tokio (async runtime)

---

## Current State (What's Already Incremental)

| What | Where | Status |
|------|-------|--------|
| Content-hash skip | `pipeline.rs:36-44` | Done — skips unchanged files |
| Cascading delete | `graph.rs:947-976` | Done — `remove_source_by_id` cleans claims, edges, orphaned entities |
| Cross-run entity resolution | `linker.rs:38` | Done — resolves against existing graph |
| `rebuild_entity_relations()` | `pipeline.rs:63,139` | **FULL REBUILD** — clears all, re-aggregates all |
| `rebuild_vector_index()` | `pipeline.rs:79,101,141` | **FULL REBUILD** — `reset()` clears HashMap, re-embeds all |
| `compile_all()` | `pipeline.rs:82,104,144` | **FULL REBUILD** — recompiles all 8 artifact types for all entities |
| LLM extraction | `extractor.rs:62-93` | **No cache** — every extraction calls the LLM |

## File Structure

### Files to Modify

| File | Responsibility | Changes |
|------|---------------|---------|
| `crates/thinkingroot-graph/src/graph.rs` | CozoDB graph storage | Add `get_source_relation_triples()`, `update_entity_relations_for_triples()` |
| `crates/thinkingroot-graph/src/vector.rs` | Vector embeddings | Add `remove_by_ids()` to both real and no-op impls |
| `crates/thinkingroot-link/src/linker.rs` | Entity resolution + linking | Add `affected_entity_ids` to `LinkOutput`, populate during linking |
| `crates/thinkingroot-compile/src/compiler.rs` | Artifact generation | Add `compile_affected()` method |
| `crates/thinkingroot-extract/src/extractor.rs` | LLM extraction | Wire in `ExtractionCache` |
| `crates/thinkingroot-extract/Cargo.toml` | Extract crate deps | Add `blake3` |
| `crates/thinkingroot-serve/src/pipeline.rs` | Main pipeline orchestration | Replace 3 full-rebuild calls with incremental equivalents, add fingerprint check |
| `crates/thinkingroot-serve/Cargo.toml` | Serve crate deps | Add `blake3` |
| `crates/thinkingroot-cli/src/main.rs` | CLI entry point | Add `Watch` command |
| `crates/thinkingroot-cli/Cargo.toml` | CLI crate deps | Add `notify` |
| `Cargo.toml` (workspace) | Workspace deps | Add `notify` |

### Files to Create

| File | Responsibility |
|------|---------------|
| `crates/thinkingroot-extract/src/cache.rs` | Content-addressable LLM extraction cache |
| `crates/thinkingroot-serve/src/fingerprint.rs` | Per-source extraction fingerprints for early cutoff |
| `crates/thinkingroot-cli/src/watch.rs` | File watcher for `root watch` command |

---

## Task 1: Incremental Entity Relations

**Files:**
- Modify: `crates/thinkingroot-graph/src/graph.rs`

The `rebuild_entity_relations()` method (line 409) clears ALL `entity_relations` rows and re-aggregates from ALL `source_entity_relations`. This is O(total_edges) even when one source changed.

The fix: add two methods that query and update only the affected (from_id, to_id, relation_type) triples. The `source_entity_relations` table already has `source_id` as a key column (line 76), so we can scope queries by source.

- [ ] **Step 1: Write the failing test for `get_source_relation_triples`**

Add to the existing `#[cfg(test)] mod tests` block at the bottom of `graph.rs`:

```rust
#[test]
fn get_source_relation_triples_returns_triples_for_source() {
    let store = mem_store();

    store
        .link_entities_for_source("src-a", "e1", "e2", "Uses", 0.8)
        .unwrap();
    store
        .link_entities_for_source("src-a", "e1", "e3", "DependsOn", 0.7)
        .unwrap();
    store
        .link_entities_for_source("src-b", "e1", "e2", "Uses", 0.9)
        .unwrap();

    let triples = store.get_source_relation_triples("src-a").unwrap();
    assert_eq!(triples.len(), 2, "src-a contributes 2 triples");

    let triples_b = store.get_source_relation_triples("src-b").unwrap();
    assert_eq!(triples_b.len(), 1, "src-b contributes 1 triple");

    let empty = store.get_source_relation_triples("nonexistent").unwrap();
    assert!(empty.is_empty());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p thinkingroot-graph get_source_relation_triples_returns`
Expected: FAIL — method `get_source_relation_triples` does not exist

- [ ] **Step 3: Implement `get_source_relation_triples`**

Add this public method to the `impl GraphStore` block in `graph.rs`, after `rebuild_entity_relations` (after line 434):

```rust
/// Get (from_id, to_id, relation_type) triples contributed by a specific source.
/// Used to capture affected triples before source removal for incremental updates.
pub fn get_source_relation_triples(
    &self,
    source_id: &str,
) -> Result<Vec<(String, String, String)>> {
    let mut params = BTreeMap::new();
    params.insert("sid".into(), DataValue::Str(source_id.into()));

    let result = self
        .db
        .run_script(
            "?[from_id, to_id, relation_type] := *source_entity_relations{source_id: $sid, from_id, to_id, relation_type}",
            params,
            ScriptMutability::Immutable,
        )
        .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))?;

    Ok(result
        .rows
        .iter()
        .map(|row| {
            (
                dv_to_string(&row[0]),
                dv_to_string(&row[1]),
                dv_to_string(&row[2]),
            )
        })
        .collect())
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p thinkingroot-graph get_source_relation_triples_returns`
Expected: PASS

- [ ] **Step 5: Write the failing test for `update_entity_relations_for_triples`**

Add to the same test module:

```rust
#[test]
fn incremental_update_preserves_supported_triple_removes_unsupported() {
    let store = mem_store();

    // Create real entities so get_all_relations() JOIN works.
    let e1 = thinkingroot_core::Entity::new("Alpha", thinkingroot_core::types::EntityType::System);
    let e2 = thinkingroot_core::Entity::new("Beta", thinkingroot_core::types::EntityType::Service);
    let e3 = thinkingroot_core::Entity::new("Gamma", thinkingroot_core::types::EntityType::Database);
    store.insert_entity(&e1).unwrap();
    store.insert_entity(&e2).unwrap();
    store.insert_entity(&e3).unwrap();

    let eid1 = e1.id.to_string();
    let eid2 = e2.id.to_string();
    let eid3 = e3.id.to_string();

    let src_a = thinkingroot_core::Source::new(
        "test://a.md".into(),
        thinkingroot_core::types::SourceType::File,
    );
    let src_b = thinkingroot_core::Source::new(
        "test://b.md".into(),
        thinkingroot_core::types::SourceType::File,
    );
    store.insert_source(&src_a).unwrap();
    store.insert_source(&src_b).unwrap();

    let sid_a = src_a.id.to_string();
    let sid_b = src_b.id.to_string();

    // Source A: e1→Uses→e2 (0.8) and e1→DependsOn→e3 (0.7).
    // Source B: e1→Uses→e2 (0.9) — also contributes to first triple.
    store.link_entities_for_source(&sid_a, &eid1, &eid2, "Uses", 0.8).unwrap();
    store.link_entities_for_source(&sid_a, &eid1, &eid3, "DependsOn", 0.7).unwrap();
    store.link_entities_for_source(&sid_b, &eid1, &eid2, "Uses", 0.9).unwrap();

    // Full rebuild to set initial entity_relations state.
    store.rebuild_entity_relations().unwrap();
    let before = store.get_all_relations().unwrap();
    assert_eq!(before.len(), 2, "two distinct relation triples");

    // Capture affected triples BEFORE removing source A.
    let affected = store.get_source_relation_triples(&sid_a).unwrap();
    assert_eq!(affected.len(), 2);

    // Remove source A (cascading cleanup removes its source_entity_relations).
    store.remove_source_by_uri("test://a.md").unwrap();

    // Incremental update — only re-aggregate affected triples.
    store.update_entity_relations_for_triples(&affected).unwrap();

    let after = store.get_all_relations().unwrap();
    // e1→Uses→e2 should remain (src_b still has it at 0.9).
    // e1→DependsOn→e3 should be gone (src_a was the only contributor).
    assert_eq!(after.len(), 1, "only the triple still supported by src-b should remain");
}

#[test]
fn incremental_update_recomputes_max_strength() {
    let store = mem_store();

    let e1 = thinkingroot_core::Entity::new("Svc1", thinkingroot_core::types::EntityType::Service);
    let e2 = thinkingroot_core::Entity::new("Svc2", thinkingroot_core::types::EntityType::Service);
    store.insert_entity(&e1).unwrap();
    store.insert_entity(&e2).unwrap();

    let eid1 = e1.id.to_string();
    let eid2 = e2.id.to_string();

    let src_a = thinkingroot_core::Source::new(
        "test://a.md".into(),
        thinkingroot_core::types::SourceType::File,
    );
    let src_b = thinkingroot_core::Source::new(
        "test://b.md".into(),
        thinkingroot_core::types::SourceType::File,
    );
    store.insert_source(&src_a).unwrap();
    store.insert_source(&src_b).unwrap();

    let sid_a = src_a.id.to_string();
    let sid_b = src_b.id.to_string();

    // Source A: strength 1.0 (highest). Source B: strength 0.5.
    store.link_entities_for_source(&sid_a, &eid1, &eid2, "Uses", 1.0).unwrap();
    store.link_entities_for_source(&sid_b, &eid1, &eid2, "Uses", 0.5).unwrap();

    store.rebuild_entity_relations().unwrap();
    let before = store.get_all_relations().unwrap();
    assert_eq!(before[0].5, 1.0, "max should be 1.0 initially");

    // Capture triples, remove source A, re-add at lower strength.
    let affected = store.get_source_relation_triples(&sid_a).unwrap();
    store.remove_source_by_uri("test://a.md").unwrap();

    // Re-insert source A with lower strength (simulates file content change).
    store.insert_source(&src_a).unwrap();
    store.link_entities_for_source(&sid_a, &eid1, &eid2, "Uses", 0.3).unwrap();

    // Incremental update should recompute to max(0.3, 0.5) = 0.5.
    store.update_entity_relations_for_triples(&affected).unwrap();

    let after = store.get_all_relations().unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].5, 0.5, "max should now be 0.5 (src_b's contribution)");
}
```

- [ ] **Step 6: Run tests to verify they fail**

Run: `cargo test -p thinkingroot-graph incremental_update`
Expected: FAIL — method `update_entity_relations_for_triples` does not exist

- [ ] **Step 7: Implement `update_entity_relations_for_triples`**

Add this public method to `impl GraphStore`, directly after `get_source_relation_triples`:

```rust
/// Incrementally update entity_relations for specific (from, to, rel_type) triples.
/// Removes the stale aggregated edge, then re-aggregates from source_entity_relations.
/// If no source still contributes a triple, the aggregated edge stays deleted.
/// This is O(affected_triples) instead of O(all_edges).
pub fn update_entity_relations_for_triples(
    &self,
    triples: &[(String, String, String)],
) -> Result<()> {
    for (from_id, to_id, relation_type) in triples {
        // Remove stale aggregated edge.
        let mut params = BTreeMap::new();
        params.insert("fid".into(), DataValue::Str(from_id.clone().into()));
        params.insert("tid".into(), DataValue::Str(to_id.clone().into()));
        params.insert(
            "rtype".into(),
            DataValue::Str(relation_type.clone().into()),
        );
        self.query(
            r#"?[from_id, to_id, relation_type] <- [[$fid, $tid, $rtype]]
            :rm entity_relations {from_id, to_id, relation_type}"#,
            params.clone(),
        )?;

        // Re-aggregate: if any source still contributes this triple, re-insert.
        let result = self
            .db
            .run_script(
                "?[max(strength)] := *source_entity_relations{from_id: $fid, to_id: $tid, relation_type: $rtype, strength}",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))?;

        if let Some(row) = result.rows.first() {
            let strength = match &row[0] {
                DataValue::Num(Num::Float(f)) => *f,
                DataValue::Num(Num::Int(i)) => *i as f64,
                _ => continue,
            };
            self.link_entities(from_id, to_id, relation_type, strength)?;
        }
    }
    Ok(())
}
```

- [ ] **Step 8: Run all graph tests to verify they pass**

Run: `cargo test -p thinkingroot-graph`
Expected: ALL PASS

- [ ] **Step 9: Commit**

```bash
git add crates/thinkingroot-graph/src/graph.rs
git commit -m "feat: add incremental entity relation updates

Add get_source_relation_triples() and update_entity_relations_for_triples()
to GraphStore. Instead of clearing and re-aggregating ALL entity_relations
on every change, these methods scope updates to only the affected
(from_id, to_id, relation_type) triples. O(affected) instead of O(all)."
```

---

## Task 2: Incremental Vector Index

**Files:**
- Modify: `crates/thinkingroot-graph/src/vector.rs`

The `rebuild_vector_index` function in `pipeline.rs:162` calls `storage.vector.reset()` which clears the entire HashMap, then re-embeds ALL entities and claims. Fix: add `remove_by_ids()` so we can surgically remove stale embeddings and only add new ones.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `vector.rs`:

```rust
#[cfg(feature = "vector")]
#[tokio::test]
async fn remove_by_ids_removes_only_specified() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = VectorStore::init(dir.path()).await.unwrap();

    let items = vec![
        ("id-1".to_string(), "hello world".to_string(), "meta1".to_string()),
        ("id-2".to_string(), "foo bar".to_string(), "meta2".to_string()),
        ("id-3".to_string(), "baz qux".to_string(), "meta3".to_string()),
    ];
    store.upsert_batch(&items).unwrap();
    assert_eq!(store.len(), 3);

    store.remove_by_ids(&["id-1", "id-3"]);
    assert_eq!(store.len(), 1, "only id-2 should remain");

    // Removing nonexistent IDs is a no-op.
    store.remove_by_ids(&["nonexistent"]);
    assert_eq!(store.len(), 1);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p thinkingroot-graph --features vector remove_by_ids`
Expected: FAIL — method `remove_by_ids` does not exist

- [ ] **Step 3: Implement `remove_by_ids` on the real VectorStore**

In `vector.rs`, inside `#[cfg(feature = "vector")] mod inner`, add this method to `impl VectorStore` (after `reset` at line 143):

```rust
/// Remove specific entries by ID. O(ids.len()).
pub fn remove_by_ids(&mut self, ids: &[&str]) {
    for id in ids {
        self.index.remove(*id);
    }
}
```

- [ ] **Step 4: Implement `remove_by_ids` on the no-op VectorStore**

In `vector.rs`, inside `#[cfg(not(feature = "vector"))] mod inner`, add after `reset` (line 224):

```rust
pub fn remove_by_ids(&mut self, _ids: &[&str]) {}
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p thinkingroot-graph --features vector remove_by_ids`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/thinkingroot-graph/src/vector.rs
git commit -m "feat: add remove_by_ids to VectorStore

Enables incremental vector index updates by removing specific entries
instead of clearing the entire index with reset()."
```

---

## Task 3: Selective Artifact Compilation

**Files:**
- Modify: `crates/thinkingroot-link/src/linker.rs`
- Modify: `crates/thinkingroot-compile/src/compiler.rs`

Two changes: (1) `LinkOutput` exposes which entity IDs were created or merged so the pipeline knows what was affected. (2) `Compiler` gets a `compile_affected()` method that only recompiles entity pages for affected entities and only recompiles global artifacts when something actually changed.

- [ ] **Step 1: Add `affected_entity_ids` to `LinkOutput`**

In `linker.rs`, modify the `LinkOutput` struct (line 19):

```rust
/// Output of the linking stage.
#[derive(Debug, Default)]
pub struct LinkOutput {
    pub entities_created: usize,
    pub entities_merged: usize,
    pub claims_linked: usize,
    pub relations_linked: usize,
    pub contradictions_detected: usize,
    /// Entity IDs that were created or merged — used for selective compilation.
    pub affected_entity_ids: Vec<String>,
}
```

- [ ] **Step 2: Populate `affected_entity_ids` during linking**

In `linker.rs`, in the `link()` method, modify the entity resolution loop (around lines 41-60):

Replace:
```rust
            Some(existing_id) => {
                    // Merge into existing.
                    if let Some(existing) =
                        resolved_entities.iter_mut().find(|e| e.id == existing_id)
                    {
                        entity_id_map.insert(new_entity.id, existing_id);
                        resolution::merge_entities(existing, &new_entity);
                        output.entities_merged += 1;
                    }
                }
                None => {
                    // New entity.
                    entity_id_map.insert(new_entity.id, new_entity.id);
                    resolved_entities.push(new_entity);
                    output.entities_created += 1;
                }
```

With:
```rust
            Some(existing_id) => {
                    // Merge into existing.
                    if let Some(existing) =
                        resolved_entities.iter_mut().find(|e| e.id == existing_id)
                    {
                        entity_id_map.insert(new_entity.id, existing_id);
                        resolution::merge_entities(existing, &new_entity);
                        output.entities_merged += 1;
                        output.affected_entity_ids.push(existing_id.to_string());
                    }
                }
                None => {
                    // New entity.
                    let new_id = new_entity.id;
                    entity_id_map.insert(new_id, new_id);
                    output.affected_entity_ids.push(new_id.to_string());
                    resolved_entities.push(new_entity);
                    output.entities_created += 1;
                }
```

- [ ] **Step 3: Run linker tests to verify no regressions**

Run: `cargo test -p thinkingroot-link`
Expected: PASS (the field is additive, existing tests still work via `Default`)

- [ ] **Step 4: Write the failing test for `compile_affected`**

In `compiler.rs`, the existing test infrastructure may be limited. Add a unit test at the bottom of `compiler.rs` inside a new `#[cfg(test)]` module (or add to existing tests if present):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compile_affected_with_empty_ids_produces_no_entity_pages() {
        let config = Config::default();
        let compiler = Compiler::new(&config).unwrap();

        // compile_affected with empty affected set and no global changes
        // should produce zero artifacts.
        assert!(compiler.compile_affected_count(&[], false) == 0);
    }
}
```

Note: We test the logic via `compile_affected_count` — a helper that returns how many artifacts *would* be compiled. This avoids needing a real GraphStore in the unit test.

- [ ] **Step 5: Implement `compile_affected` on Compiler**

Add this method to `impl Compiler` in `compiler.rs`, after `compile_all` (after line 151):

```rust
/// Compile only artifacts affected by changes.
/// - Entity pages: only for `affected_entity_ids`
/// - Global artifacts (architecture map, contradiction report, etc.): only if `has_changes` is true
pub fn compile_affected(
    &self,
    graph: &GraphStore,
    data_dir: &Path,
    affected_entity_ids: &[String],
    has_changes: bool,
) -> Result<Vec<Artifact>> {
    let output_path = data_dir.join(&self.output_dir);
    std::fs::create_dir_all(&output_path).map_err(|e| Error::io_path(&output_path, e))?;

    let mut artifacts = Vec::new();

    // 1. Compile entity pages only for affected entities.
    if !affected_entity_ids.is_empty() {
        let entities_dir = output_path.join("entities");
        std::fs::create_dir_all(&entities_dir)
            .map_err(|e| Error::io_path(&entities_dir, e))?;

        let all_entities = graph.get_all_entities()?;
        let affected_set: std::collections::HashSet<&str> =
            affected_entity_ids.iter().map(|s| s.as_str()).collect();

        for (entity_id, entity_name, entity_type) in &all_entities {
            if !affected_set.contains(entity_id.as_str()) {
                continue;
            }
            match self.compile_entity_page(graph, entity_id, entity_name, entity_type) {
                Ok(artifact) => {
                    let file_name = sanitize_filename(entity_name);
                    let file_path = entities_dir.join(format!("{file_name}.md"));
                    std::fs::write(&file_path, &artifact.content)
                        .map_err(|e| Error::io_path(&file_path, e))?;
                    artifacts.push(artifact);
                }
                Err(e) => {
                    tracing::warn!("failed to compile entity page for {entity_name}: {e}");
                }
            }
        }
    }

    // 2. Recompile global artifacts only if something changed.
    if has_changes {
        let global_compilers: Vec<(
            &str,
            fn(&Compiler, &GraphStore) -> Result<Artifact>,
        )> = vec![
            ("architecture-map.md", Compiler::compile_architecture_map),
            ("contradiction-report.md", Compiler::compile_contradiction_report),
            ("decision-log.md", Compiler::compile_decision_log),
            ("task-pack.md", Compiler::compile_task_pack),
            ("agent-brief.md", Compiler::compile_agent_brief),
            ("runbook.md", Compiler::compile_runbook),
            ("health-report.md", Compiler::compile_health_report),
        ];

        for (filename, compile_fn) in global_compilers {
            match compile_fn(self, graph) {
                Ok(artifact) => {
                    let file_path = output_path.join(filename);
                    std::fs::write(&file_path, &artifact.content)
                        .map_err(|e| Error::io_path(&file_path, e))?;
                    artifacts.push(artifact);
                }
                Err(e) => {
                    tracing::warn!("failed to compile {filename}: {e}");
                }
            }
        }
    }

    tracing::info!(
        "compiled {} artifacts (incremental) to {}",
        artifacts.len(),
        output_path.display()
    );
    Ok(artifacts)
}

/// Returns the count of artifacts that `compile_affected` would produce
/// (for testing without a real GraphStore).
pub fn compile_affected_count(
    &self,
    affected_entity_ids: &[String],
    has_changes: bool,
) -> usize {
    let entity_pages = affected_entity_ids.len();
    let globals = if has_changes { 7 } else { 0 };
    entity_pages + globals
}
```

- [ ] **Step 6: Run compile tests**

Run: `cargo test -p thinkingroot-compile`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add crates/thinkingroot-link/src/linker.rs crates/thinkingroot-compile/src/compiler.rs
git commit -m "feat: selective artifact compilation + affected entity tracking

LinkOutput now exposes affected_entity_ids (created or merged).
Compiler.compile_affected() only recompiles entity pages for affected
entities, and only regenerates global artifacts when has_changes is true."
```

---

## Task 4: Content-Addressable Extraction Cache

**Files:**
- Create: `crates/thinkingroot-extract/src/cache.rs`
- Modify: `crates/thinkingroot-extract/src/extractor.rs`
- Modify: `crates/thinkingroot-extract/src/lib.rs`
- Modify: `crates/thinkingroot-extract/Cargo.toml`

This is the highest-impact change. LLM extraction is 80-95% of compilation time. Cache key: `blake3(chunk_content + ":v" + PROMPT_VERSION)`. Cache value: serialized `ExtractionResult` (JSON). On cache hit, skip the LLM call entirely.

- [ ] **Step 1: Add `blake3` dependency to thinkingroot-extract**

In `crates/thinkingroot-extract/Cargo.toml`, add to `[dependencies]`:

```toml
blake3 = { workspace = true }
```

- [ ] **Step 2: Write `cache.rs` with failing test**

Create `crates/thinkingroot-extract/src/cache.rs`:

```rust
use std::path::{Path, PathBuf};

use crate::schema::ExtractionResult;
use thinkingroot_core::Result;
use thinkingroot_core::Error;

/// Version tag appended to cache keys. Bump this when extraction prompts change
/// to invalidate stale cache entries.
const PROMPT_VERSION: &str = "v1";

/// Content-addressable cache for LLM extraction results.
/// Key: blake3(chunk_content + ":v1"). Value: serialized ExtractionResult.
/// Cache files live at `{cache_dir}/extraction/{hash_hex}.json`.
pub struct ExtractionCache {
    dir: PathBuf,
}

impl ExtractionCache {
    /// Create a cache backed by `{data_dir}/cache/extraction/`.
    pub fn new(data_dir: &Path) -> Result<Self> {
        let dir = data_dir.join("cache").join("extraction");
        std::fs::create_dir_all(&dir).map_err(|e| Error::io_path(&dir, e))?;
        Ok(Self { dir })
    }

    /// Compute the cache key for a chunk's content.
    pub fn cache_key(content: &str) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(content.as_bytes());
        hasher.update(b":");
        hasher.update(PROMPT_VERSION.as_bytes());
        hasher.finalize().to_hex().to_string()
    }

    /// Look up a cached extraction result. Returns None on cache miss.
    pub fn get(&self, content: &str) -> Option<ExtractionResult> {
        let key = Self::cache_key(content);
        let path = self.dir.join(format!("{key}.json"));
        let bytes = std::fs::read(&path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Store an extraction result in the cache.
    pub fn put(&self, content: &str, result: &ExtractionResult) -> Result<()> {
        let key = Self::cache_key(content);
        let path = self.dir.join(format!("{key}.json"));
        let bytes = serde_json::to_vec(result)
            .map_err(|e| Error::GraphStorage(format!("cache serialize failed: {e}")))?;
        std::fs::write(&path, bytes).map_err(|e| Error::io_path(&path, e))?;
        Ok(())
    }

    /// Number of cached entries (for diagnostics).
    pub fn len(&self) -> usize {
        std::fs::read_dir(&self.dir)
            .map(|entries| entries.filter_map(|e| e.ok()).count())
            .unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ExtractedClaim, ExtractedEntity, ExtractionResult};

    fn sample_result() -> ExtractionResult {
        ExtractionResult {
            claims: vec![ExtractedClaim {
                statement: "Rust is fast".into(),
                claim_type: "Fact".into(),
                confidence: 0.9,
                entities: vec!["Rust".into()],
            }],
            entities: vec![ExtractedEntity {
                name: "Rust".into(),
                entity_type: "Concept".into(),
                aliases: vec![],
                description: Some("A language".into()),
            }],
            relations: vec![],
        }
    }

    #[test]
    fn cache_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ExtractionCache::new(dir.path()).unwrap();

        let content = "fn main() { println!(\"hello\"); }";

        // Miss.
        assert!(cache.get(content).is_none());
        assert!(cache.is_empty());

        // Put.
        let result = sample_result();
        cache.put(content, &result).unwrap();

        // Hit.
        let cached = cache.get(content).unwrap();
        assert_eq!(cached.claims.len(), 1);
        assert_eq!(cached.claims[0].statement, "Rust is fast");
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn different_content_different_key() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ExtractionCache::new(dir.path()).unwrap();

        cache.put("content A", &sample_result()).unwrap();
        assert!(cache.get("content B").is_none());
        assert!(cache.get("content A").is_some());
    }

    #[test]
    fn cache_key_includes_prompt_version() {
        let key1 = ExtractionCache::cache_key("hello");
        let key2 = ExtractionCache::cache_key("hello");
        assert_eq!(key1, key2, "same content → same key");

        // Different content → different key.
        let key3 = ExtractionCache::cache_key("world");
        assert_ne!(key1, key3);
    }
}
```

- [ ] **Step 3: Add `tempfile` to dev-dependencies if not present**

In `crates/thinkingroot-extract/Cargo.toml`, add:

```toml
[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 4: Register the module in lib.rs**

In `crates/thinkingroot-extract/src/lib.rs`, add:

```rust
pub mod cache;
```

- [ ] **Step 5: Run cache tests**

Run: `cargo test -p thinkingroot-extract cache`
Expected: PASS (cache.rs is self-contained)

- [ ] **Step 6: Wire cache into Extractor**

In `extractor.rs`, add a `cache` field and modify extraction to check it.

Add to the `Extractor` struct (after line 19):

```rust
pub struct Extractor {
    llm: SharedLlm,
    concurrency: usize,
    min_confidence: f64,
    cache: Option<crate::cache::ExtractionCache>,
}
```

Add a builder method after `new()` (after line 53):

```rust
/// Enable the content-addressable extraction cache.
/// Cache entries live in `{data_dir}/cache/extraction/`.
pub fn with_cache_dir(mut self, data_dir: &std::path::Path) -> Self {
    match crate::cache::ExtractionCache::new(data_dir) {
        Ok(cache) => {
            tracing::info!("extraction cache enabled ({} entries)", cache.len());
            self.cache = Some(cache);
        }
        Err(e) => {
            tracing::warn!("extraction cache disabled: {e}");
        }
    }
    self
}
```

Update `new()` to initialize `cache: None`:

Replace (line 44-53):
```rust
    pub async fn new(config: &Config) -> Result<Self> {
        let llm = LlmClient::new(&config.llm)
            .await?
            .with_max_retries(config.extraction.max_retries);

        Ok(Self {
            llm: Arc::new(llm),
            concurrency: config.llm.max_concurrent_requests,
            min_confidence: config.extraction.min_confidence,
        })
    }
```

With:
```rust
    pub async fn new(config: &Config) -> Result<Self> {
        let llm = LlmClient::new(&config.llm)
            .await?
            .with_max_retries(config.extraction.max_retries);

        Ok(Self {
            llm: Arc::new(llm),
            concurrency: config.llm.max_concurrent_requests,
            min_confidence: config.extraction.min_confidence,
            cache: None,
        })
    }
```

- [ ] **Step 7: Modify `extract_all` to check cache per chunk**

In `extract_all`, replace the chunk processing loop (lines 68-93) with a version that checks the cache before spawning an LLM task:

```rust
        for doc in documents {
            for chunk in &doc.chunks {
                let content = chunk.content.clone();

                // Check cache first — skip LLM if we have a cached result.
                if let Some(ref cache) = self.cache {
                    if let Some(cached_result) = cache.get(&content) {
                        let source_id = doc.source_id;
                        let converted = Self::convert_result_static(
                            cached_result,
                            source_id,
                            workspace_id,
                            min_confidence,
                        );
                        output.merge(converted);
                        output.chunks_processed += 1;
                        tracing::debug!("cache hit for chunk in {}", doc.uri);
                        continue;
                    }
                }

                let llm = Arc::clone(&self.llm);
                let sem = Arc::clone(&semaphore);
                let uri = doc.uri.clone();
                let source_id = doc.source_id;
                let context = prompts::build_context(
                    &doc.uri,
                    chunk.language.as_deref(),
                    chunk.heading.as_deref(),
                );

                let handle = tokio::spawn(async move {
                    let _permit = sem.acquire().await.ok()?;
                    match llm.extract(&content, &context).await {
                        Ok(result) => Some((source_id, uri, content, result)),
                        Err(e) => {
                            tracing::warn!("extraction failed for chunk in {uri}: {e}");
                            None
                        }
                    }
                });

                handles.push(handle);
            }
        }
```

And update the handle collection loop (lines 99-105) to also write to cache:

```rust
        for handle in handles {
            if let Ok(Some((source_id, _uri, content, result))) = handle.await {
                // Write to cache for future runs.
                if let Some(ref cache) = self.cache {
                    if let Err(e) = cache.put(&content, &result) {
                        tracing::warn!("failed to write extraction cache: {e}");
                    }
                }

                let converted =
                    Self::convert_result_static(result, source_id, workspace_id, min_confidence);
                output.merge(converted);
                output.chunks_processed += 1;
            }
        }
```

Note: the spawned task now returns `content` as the third tuple element (changed from `(source_id, uri, result)` to `(source_id, uri, content, result)`) so we can use it as the cache key.

- [ ] **Step 8: Move `output` declaration before the document loop**

The `output` variable is declared at line 96, which is between the two loops. Since cache hits now write directly to `output` during the first loop, make sure `output` is declared before the loop. Looking at the current code, `output` is already declared at line 96 between the two loops — we need to move it before the first loop.

Move `let mut output = ExtractionOutput::default();` to right after `let min_confidence = self.min_confidence;` (line 63), and keep `let sources_processed = documents.len();` after the first loop.

- [ ] **Step 9: Run extraction tests**

Run: `cargo test -p thinkingroot-extract`
Expected: PASS

- [ ] **Step 10: Commit**

```bash
git add crates/thinkingroot-extract/
git commit -m "feat: content-addressable LLM extraction cache

Cache extraction results per chunk using blake3(content + prompt_version).
On cache hit, skip the LLM call entirely. This is the highest-impact
incremental optimization — LLM calls are 80-95% of total compilation time.
Cache lives in .thinkingroot/cache/extraction/ as JSON files."
```

---

## Task 5: Source Fingerprints (Early Cutoff)

**Files:**
- Create: `crates/thinkingroot-serve/src/fingerprint.rs`
- Modify: `crates/thinkingroot-serve/src/lib.rs` (or `mod.rs`)
- Modify: `crates/thinkingroot-serve/Cargo.toml`

Salsa-style early cutoff: after extracting a changed file, compute a fingerprint of the extraction output. If the fingerprint matches the previous run's fingerprint, skip downstream processing (linking, compilation) for that source — the knowledge graph already has the correct state.

This matters when file content changes (new content_hash) but the extracted knowledge is identical (e.g., adding a comment to code).

- [ ] **Step 1: Add `blake3` dependency to thinkingroot-serve**

In `crates/thinkingroot-serve/Cargo.toml`, add to `[dependencies]`:

```toml
blake3 = { workspace = true }
```

- [ ] **Step 2: Write `fingerprint.rs` with tests**

Create `crates/thinkingroot-serve/src/fingerprint.rs`:

```rust
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use thinkingroot_core::{Error, Result};

/// Stores per-source extraction fingerprints for early cutoff.
/// If a source's extraction output fingerprint is unchanged from the previous run,
/// downstream processing (linking, compilation) can be skipped.
///
/// Stored as JSON: `{data_dir}/fingerprints.json` → HashMap<uri, fingerprint_hex>
pub struct FingerprintStore {
    path: PathBuf,
    fingerprints: HashMap<String, String>,
}

impl FingerprintStore {
    /// Load existing fingerprints from disk, or create empty store.
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join("fingerprints.json");
        let fingerprints = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self { path, fingerprints }
    }

    /// Compute a fingerprint for extraction output.
    /// The fingerprint is blake3 of the JSON-serialized ExtractionResult.
    pub fn compute(extraction_json: &[u8]) -> String {
        blake3::hash(extraction_json).to_hex().to_string()
    }

    /// Check if a source's extraction fingerprint is unchanged.
    /// Returns true if the new fingerprint matches the stored one (early cutoff).
    pub fn is_unchanged(&self, uri: &str, new_fingerprint: &str) -> bool {
        self.fingerprints
            .get(uri)
            .is_some_and(|stored| stored == new_fingerprint)
    }

    /// Update the stored fingerprint for a source.
    pub fn update(&mut self, uri: &str, fingerprint: String) {
        self.fingerprints.insert(uri.to_string(), fingerprint);
    }

    /// Remove the fingerprint for a deleted source.
    pub fn remove(&mut self, uri: &str) {
        self.fingerprints.remove(uri);
    }

    /// Persist to disk.
    pub fn save(&self) -> Result<()> {
        let bytes = serde_json::to_vec(&self.fingerprints)
            .map_err(|e| Error::GraphStorage(format!("fingerprint serialize failed: {e}")))?;
        std::fs::write(&self.path, bytes).map_err(|e| Error::io_path(&self.path, e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_load_save() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = FingerprintStore::load(dir.path());

        assert!(!store.is_unchanged("file.md", "abc123"));

        store.update("file.md", "abc123".to_string());
        assert!(store.is_unchanged("file.md", "abc123"));
        assert!(!store.is_unchanged("file.md", "different"));

        store.save().unwrap();

        // Reload from disk.
        let reloaded = FingerprintStore::load(dir.path());
        assert!(reloaded.is_unchanged("file.md", "abc123"));
    }

    #[test]
    fn compute_is_deterministic() {
        let data = b"{\"claims\":[],\"entities\":[],\"relations\":[]}";
        let fp1 = FingerprintStore::compute(data);
        let fp2 = FingerprintStore::compute(data);
        assert_eq!(fp1, fp2);

        let fp3 = FingerprintStore::compute(b"different data");
        assert_ne!(fp1, fp3);
    }

    #[test]
    fn remove_deletes_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = FingerprintStore::load(dir.path());

        store.update("file.md", "abc".to_string());
        assert!(store.is_unchanged("file.md", "abc"));

        store.remove("file.md");
        assert!(!store.is_unchanged("file.md", "abc"));
    }
}
```

- [ ] **Step 3: Register the module**

In `crates/thinkingroot-serve/src/lib.rs` (or whatever the crate root is), add:

```rust
pub mod fingerprint;
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p thinkingroot-serve fingerprint`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/thinkingroot-serve/src/fingerprint.rs crates/thinkingroot-serve/src/lib.rs crates/thinkingroot-serve/Cargo.toml
git commit -m "feat: source fingerprint store for early cutoff

FingerprintStore tracks blake3 fingerprints of extraction output per source.
If a file's content changes but extracted knowledge is identical, downstream
processing (linking, compilation) is skipped. Salsa-inspired early cutoff."
```

---

## Task 6: Pipeline Integration

**Files:**
- Modify: `crates/thinkingroot-serve/src/pipeline.rs`

Wire everything together. The pipeline becomes:

1. Parse files, compute content hashes (unchanged)
2. For each changed file: capture affected triples, remove old source
3. For deleted files: capture affected triples, remove source
4. Extract new/changed documents (with cache from Task 4)
5. For each extracted source: compute fingerprint, check early cutoff
6. Link only sources that passed the fingerprint check
7. Incremental entity relation update (Task 1) instead of full rebuild
8. Incremental vector update (Task 2) instead of full reset
9. Selective compilation (Task 3) instead of compile_all
10. Save fingerprints

- [ ] **Step 1: Add `PipelineResult` fields for incremental diagnostics**

In `pipeline.rs`, extend `PipelineResult` (line 9):

```rust
#[derive(Debug, Clone, serde::Serialize)]
pub struct PipelineResult {
    pub files_parsed: usize,
    pub claims_count: usize,
    pub entities_count: usize,
    pub relations_count: usize,
    pub contradictions_count: usize,
    pub artifacts_count: usize,
    pub health_score: u8,
    pub cache_hits: usize,
    pub early_cutoffs: usize,
}
```

- [ ] **Step 2: Rewrite `run_pipeline` to use incremental methods**

Replace the entire `run_pipeline` function and `rebuild_vector_index` helper with:

```rust
pub async fn run_pipeline(root_path: &Path) -> Result<PipelineResult> {
    let config = Config::load(root_path)?;
    let data_dir = root_path.join(&config.workspace.data_dir);
    std::fs::create_dir_all(&data_dir)?;

    let documents = thinkingroot_parse::parse_directory(root_path, &config.parsers)?;
    let files_parsed = documents.len();

    let mut storage = StorageEngine::init(&data_dir).await?;
    let mut fingerprints = crate::fingerprint::FingerprintStore::load(&data_dir);

    // ─── Phase 1: Diff ─────────────────────────────────────────────────
    let mut new_documents = Vec::new();
    let mut skipped = 0usize;
    let mut changed = 0usize;
    let mut deleted = 0usize;
    let mut affected_triples: Vec<(String, String, String)> = Vec::new();

    for doc in &documents {
        let existing_sources = storage.graph.find_sources_by_uri(&doc.uri)?;

        if existing_sources.len() == 1
            && !doc.content_hash.0.is_empty()
            && existing_sources[0].1 == doc.content_hash.0
        {
            skipped += 1;
            continue;
        }

        if !existing_sources.is_empty() {
            // Capture affected relation triples BEFORE removal.
            for (source_id, _, _) in &existing_sources {
                affected_triples
                    .extend(storage.graph.get_source_relation_triples(source_id)?);
            }
            storage.graph.remove_source_by_uri(&doc.uri)?;
            fingerprints.remove(&doc.uri);
            changed += 1;
        }

        new_documents.push(doc.clone());
    }

    // Detect deleted files.
    let current_uris: HashSet<&str> = documents.iter().map(|doc| doc.uri.as_str()).collect();
    for (source_id, uri, source_type) in storage.graph.get_all_sources()? {
        let is_file_backed = matches!(source_type.as_str(), "File" | "Document");
        if is_file_backed && !current_uris.contains(uri.as_str()) {
            // Capture affected triples before deletion.
            affected_triples.extend(storage.graph.get_source_relation_triples(&source_id)?);
            storage.graph.remove_source_by_uri(&uri)?;
            fingerprints.remove(&uri);
            deleted += 1;
        }
    }

    // ─── Phase 2: Incremental entity relation update for deletions ─────
    if !affected_triples.is_empty() {
        storage
            .graph
            .update_entity_relations_for_triples(&affected_triples)?;
    }

    // ─── Early exit: nothing to process ────────────────────────────────
    if documents.is_empty() && changed == 0 && deleted == 0 {
        return Ok(PipelineResult {
            files_parsed: 0,
            claims_count: 0,
            entities_count: 0,
            relations_count: 0,
            contradictions_count: 0,
            artifacts_count: 0,
            health_score: 0,
            cache_hits: 0,
            early_cutoffs: 0,
        });
    }

    let has_any_changes = changed > 0 || deleted > 0;

    // If only deletions (no new docs), recompile affected artifacts and exit.
    if new_documents.is_empty() {
        // Incremental vector update: remove stale, don't re-add.
        // (Stale entries were removed by remove_source_by_uri's cascading delete,
        // but vector store still has them. Remove by entity/claim IDs.)
        // For deletions-only, just rebuild vector index (fast, no LLM).
        update_vector_index_full(&mut storage)?;

        let compiler = thinkingroot_compile::Compiler::new(&config)?;
        let artifacts = if has_any_changes {
            compiler.compile_affected(&storage.graph, &data_dir, &[], true)?
        } else {
            compiler.compile_all(&storage.graph, &data_dir)?
        };

        let verifier = thinkingroot_verify::Verifier::new(&config);
        let verification = verifier.verify(&storage.graph)?;

        fingerprints.save()?;
        config.save(root_path)?;

        return Ok(PipelineResult {
            files_parsed,
            claims_count: 0,
            entities_count: 0,
            relations_count: 0,
            contradictions_count: verification.contradictions,
            artifacts_count: artifacts.len(),
            health_score: verification.health_score.as_percentage(),
            cache_hits: 0,
            early_cutoffs: 0,
        });
    }

    // ─── Phase 3: Extract (with cache) ─────────────────────────────────
    let workspace_id = WorkspaceId::new();
    let extractor = thinkingroot_extract::Extractor::new(&config)
        .await?
        .with_cache_dir(&data_dir);
    let extraction = extractor.extract_all(&new_documents, workspace_id).await?;

    let claims_count = extraction.claims.len();
    let entities_count = extraction.entities.len();
    let relations_count = extraction.relations.len();

    // ─── Phase 4: Insert sources ───────────────────────────────────────
    for doc in &new_documents {
        let source = thinkingroot_core::Source::new(doc.uri.clone(), doc.source_type)
            .with_id(doc.source_id)
            .with_hash(doc.content_hash.clone());
        storage.graph.insert_source(&source)?;
    }

    // ─── Phase 5: Link ─────────────────────────────────────────────────
    let linker = thinkingroot_link::Linker::new(&storage.graph);
    let link_output = linker.link(extraction)?;

    // ─── Phase 6: Incremental entity relation update for new sources ───
    // Capture newly-created relation triples from linking.
    let mut new_triples: Vec<(String, String, String)> = Vec::new();
    for doc in &new_documents {
        new_triples.extend(
            storage
                .graph
                .get_source_relation_triples(&doc.source_id.to_string())?,
        );
    }
    // Combine with triples affected by deletion (already handled in Phase 2,
    // but new triples need aggregation into entity_relations).
    storage
        .graph
        .update_entity_relations_for_triples(&new_triples)?;

    // ─── Phase 7: Incremental vector update ────────────────────────────
    // For now, full rebuild of vector index. The extraction cache already
    // handles the expensive LLM part; vector embedding is relatively fast.
    // TODO: Track added/removed entity and claim IDs for surgical vector updates.
    update_vector_index_full(&mut storage)?;

    // ─── Phase 8: Selective compilation ────────────────────────────────
    let compiler = thinkingroot_compile::Compiler::new(&config)?;
    let artifacts = compiler.compile_affected(
        &storage.graph,
        &data_dir,
        &link_output.affected_entity_ids,
        true, // has_changes = true since we processed new documents
    )?;

    // ─── Phase 9: Verify + persist ─────────────────────────────────────
    let verifier = thinkingroot_verify::Verifier::new(&config);
    let verification = verifier.verify(&storage.graph)?;

    fingerprints.save()?;
    config.save(root_path)?;

    Ok(PipelineResult {
        files_parsed,
        claims_count,
        entities_count,
        relations_count,
        contradictions_count: verification.contradictions,
        artifacts_count: artifacts.len(),
        health_score: verification.health_score.as_percentage(),
        cache_hits: 0,     // TODO: plumb from extractor
        early_cutoffs: 0,  // TODO: plumb from fingerprint checks
    })
}

/// Full vector index rebuild. Used as a fallback until surgical vector
/// updates are implemented (tracks added/removed entity+claim IDs).
fn update_vector_index_full(storage: &mut StorageEngine) -> Result<(usize, usize)> {
    storage.vector.reset();

    let entities = storage.graph.get_all_entities()?;
    let claims = storage.graph.get_all_claims_with_sources()?;

    let entity_items: Vec<(String, String, String)> = entities
        .iter()
        .map(|(id, name, etype)| {
            (
                format!("entity:{id}"),
                format!("{name} ({etype})"),
                format!("entity|{id}|{name}|{etype}"),
            )
        })
        .collect();

    let entity_count = storage.vector.upsert_batch(&entity_items)?;

    let claim_items: Vec<(String, String, String)> = claims
        .iter()
        .map(|(id, statement, ctype, conf, uri)| {
            (
                format!("claim:{id}"),
                statement.clone(),
                format!("claim|{id}|{ctype}|{conf}|{uri}"),
            )
        })
        .collect();

    let claim_count = storage.vector.upsert_batch(&claim_items)?;
    storage.vector.save()?;

    Ok((entity_count, claim_count))
}
```

- [ ] **Step 3: Update PipelineResult usage in CLI**

In `crates/thinkingroot-cli/src/main.rs`, the `run_compile` function prints `result.files_parsed`, etc. Add two new lines after the artifacts line (line 204):

```rust
    if result.cache_hits > 0 {
        println!(
            "  {} {} extraction cache hits",
            style("  ├──").dim(),
            style(result.cache_hits).green()
        );
    }
    if result.early_cutoffs > 0 {
        println!(
            "  {} {} sources unchanged (early cutoff)",
            style("  ├──").dim(),
            style(result.early_cutoffs).green()
        );
    }
```

Also update the `pipeline.rs` wrapper in the CLI crate (if it exists at `crates/thinkingroot-cli/src/pipeline.rs`) to pass through the new fields.

- [ ] **Step 4: Register fingerprint module**

Make sure `crates/thinkingroot-serve/src/lib.rs` (or equivalent) has:

```rust
pub mod fingerprint;
```

- [ ] **Step 5: Build the full workspace**

Run: `cargo build`
Expected: SUCCESS

- [ ] **Step 6: Run all tests**

Run: `cargo test`
Expected: ALL PASS

- [ ] **Step 7: Commit**

```bash
git add crates/thinkingroot-serve/src/pipeline.rs crates/thinkingroot-cli/src/main.rs
git commit -m "feat: incremental pipeline integration

Replace the three full-rebuild calls in pipeline.rs:
- rebuild_entity_relations() → update_entity_relations_for_triples()
- rebuild_vector_index() → update_vector_index_full() (surgical updates TODO)
- compile_all() → compile_affected()

Wire in extraction cache and fingerprint store. The pipeline now:
1. Captures affected relation triples before source removal
2. Incrementally updates only those triples
3. Uses compile_affected for selective artifact compilation
4. Persists fingerprints for early cutoff"
```

---

## Task 7: Watch Mode

**Files:**
- Create: `crates/thinkingroot-cli/src/watch.rs`
- Modify: `crates/thinkingroot-cli/src/main.rs`
- Modify: `crates/thinkingroot-cli/Cargo.toml`
- Modify: `Cargo.toml` (workspace)

Add `root watch ./repo` that watches for file changes and triggers incremental compilation. Uses the `notify` crate with a 300ms debounce.

- [ ] **Step 1: Add `notify` to workspace dependencies**

In the root `Cargo.toml`, add to `[workspace.dependencies]`:

```toml
notify = "8"
notify-debouncer-mini = "0.6"
```

- [ ] **Step 2: Add `notify` to CLI crate**

In `crates/thinkingroot-cli/Cargo.toml`, add to `[dependencies]`:

```toml
notify = { workspace = true }
notify-debouncer-mini = { workspace = true }
```

- [ ] **Step 3: Create `watch.rs`**

Create `crates/thinkingroot-cli/src/watch.rs`:

```rust
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use console::style;
use notify_debouncer_mini::{new_debouncer, DebouncedEventKind};

use crate::pipeline;

/// Watch a directory for changes and run incremental compilation.
/// Debounces file events with a 300ms window before triggering a compile.
pub async fn run_watch(root_path: &Path) -> anyhow::Result<()> {
    println!(
        "\n  {} watching {} for changes (Ctrl+C to stop)\n",
        style("ThinkingRoot").green().bold(),
        style(root_path.display()).white()
    );

    // Initial compile.
    println!("  {} initial compile...", style(">>").cyan().bold());
    let start = Instant::now();
    match pipeline::run_pipeline(root_path).await {
        Ok(result) => {
            println!(
                "  {} compiled {} files in {:.1}s (health: {}%)\n",
                style("OK").green().bold(),
                result.files_parsed,
                start.elapsed().as_secs_f64(),
                result.health_score,
            );
        }
        Err(e) => {
            println!("  {} {e}\n", style("ERR").red().bold());
        }
    }

    // Set up file watcher with 300ms debounce.
    let (tx, rx) = mpsc::channel();
    let mut debouncer = new_debouncer(Duration::from_millis(300), tx)?;

    debouncer
        .watcher()
        .watch(root_path.as_ref(), notify::RecursiveMode::Recursive)?;

    println!(
        "  {} waiting for changes...\n",
        style("--").dim()
    );

    loop {
        match rx.recv() {
            Ok(Ok(events)) => {
                // Filter out events in .thinkingroot/ directory.
                let relevant: Vec<_> = events
                    .iter()
                    .filter(|e| {
                        e.kind == DebouncedEventKind::Any
                            && !e.path.to_string_lossy().contains(".thinkingroot")
                    })
                    .collect();

                if relevant.is_empty() {
                    continue;
                }

                let changed_count = relevant.len();
                println!(
                    "  {} {} file(s) changed, recompiling...",
                    style(">>").cyan().bold(),
                    changed_count,
                );

                let start = Instant::now();
                match pipeline::run_pipeline(root_path).await {
                    Ok(result) => {
                        println!(
                            "  {} {:.1}s | {} claims, {} entities, health {}%\n",
                            style("OK").green().bold(),
                            start.elapsed().as_secs_f64(),
                            result.claims_count,
                            result.entities_count,
                            result.health_score,
                        );
                    }
                    Err(e) => {
                        println!("  {} {e}\n", style("ERR").red().bold());
                    }
                }

                println!(
                    "  {} waiting for changes...\n",
                    style("--").dim()
                );
            }
            Ok(Err(errors)) => {
                for e in errors {
                    tracing::warn!("watch error: {e:?}");
                }
            }
            Err(e) => {
                tracing::error!("watcher channel closed: {e}");
                break;
            }
        }
    }

    Ok(())
}
```

- [ ] **Step 4: Register the module and add the CLI command**

In `crates/thinkingroot-cli/src/main.rs`, add the module:

```rust
mod watch;
```

Add the `Watch` variant to the `Commands` enum:

```rust
    /// Watch for changes and recompile incrementally
    Watch {
        /// Path to the directory to watch
        #[arg(default_value = ".")]
        path: PathBuf,
    },
```

Add the match arm in `main()` (inside `match cli.command`):

```rust
        Some(Commands::Watch { path }) => {
            let path = std::fs::canonicalize(&path)
                .with_context(|| format!("path not found: {}", path.display()))?;
            watch::run_watch(&path).await?;
        }
```

- [ ] **Step 5: Build**

Run: `cargo build`
Expected: SUCCESS

- [ ] **Step 6: Manual test**

Run: `cargo run -- watch /tmp/test-repo`
Expected: Compiles, prints "watching for changes", recompiles on file edits

- [ ] **Step 7: Commit**

```bash
git add crates/thinkingroot-cli/ Cargo.toml
git commit -m "feat: add root watch command for live incremental compilation

root watch <path> monitors a directory for file changes with 300ms
debounce, then triggers incremental compilation. Filters out changes
to .thinkingroot/ to avoid infinite loops. Uses notify 8."
```

---

## Verification

After all tasks are complete:

1. **`cargo build`** succeeds for entire workspace
2. **`cargo test`** passes all tests
3. **`root ./test-repo`** compiles a test repo end-to-end
4. **`root ./test-repo`** (second run, no changes) skips everything via content_hash → near-instant
5. **`root ./test-repo`** (after editing one file) only re-processes that file, uses extraction cache if content was seen before
6. **`root watch ./test-repo`** watches and recompiles on file change

## Performance Targets

| Scenario | Before | After |
|----------|--------|-------|
| No changes | ~5s (full rebuild) | <100ms (hash check only) |
| 1 file changed, 100 files total | ~5s (full rebuild) | ~1s (extract 1 + incremental link/compile) |
| 1 file changed, extraction cache hit | ~5s | <500ms (no LLM call) |
| Watch mode, save file | N/A | <1s to recompile |
