//! Integration tests for the ThinkingRoot compilation pipeline.
//!
//! These tests verify the non-LLM stages work correctly end-to-end:
//! parsing, graph storage, compilation, verification, and entity resolution.
//!
//! LLM extraction is NOT tested here (requires API credentials).

use std::path::PathBuf;

/// Get the path to test fixtures (relative to workspace root).
fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/sample-repo")
        .canonicalize()
        .expect("fixture directory not found — run from workspace root")
}

// ─── Parsing ─────────────────────────────────────────────────

#[test]
fn parse_fixture_files() {
    let config = thinkingroot_core::config::ParserConfig::default();
    let docs = thinkingroot_parse::parse_directory(&fixtures_dir(), &config).unwrap();

    // Should find at least the markdown and rust files.
    assert!(
        docs.len() >= 2,
        "expected at least 2 documents, got {}",
        docs.len()
    );

    for doc in &docs {
        assert!(!doc.uri.is_empty());
        assert!(!doc.chunks.is_empty(), "document {} has no chunks", doc.uri);
    }
}

#[test]
fn parse_markdown_produces_chunks() {
    let md_path = fixtures_dir().join("docs").join("architecture.md");
    let doc = thinkingroot_parse::parse_file(&md_path).unwrap();

    assert!(
        doc.chunks.len() >= 2,
        "expected multiple chunks from markdown, got {}",
        doc.chunks.len()
    );
    assert!(!doc.content_hash.0.is_empty(), "content hash should be set");
}

#[test]
fn parse_rust_produces_chunks() {
    let rs_path = fixtures_dir().join("src").join("auth.rs");
    let doc = thinkingroot_parse::parse_file(&rs_path).unwrap();

    assert!(!doc.chunks.is_empty(), "rust parser should produce chunks");

    // Should find type definitions (struct AuthHandler, impl AuthHandler, enum AuthError).
    let has_typedef = doc
        .chunks
        .iter()
        .any(|c| matches!(c.chunk_type, thinkingroot_core::ir::ChunkType::TypeDef));
    assert!(has_typedef, "should find type definitions in auth.rs");
}

// ─── Graph Store ─────────────────────────────────────────────

fn temp_graph_store() -> (tempfile::TempDir, thinkingroot_graph::graph::GraphStore) {
    let tmp = tempfile::tempdir().unwrap();
    let store = thinkingroot_graph::graph::GraphStore::init(tmp.path()).unwrap();
    (tmp, store)
}

#[test]
fn graph_store_roundtrip() {
    let (_tmp, store) = temp_graph_store();

    // Insert an entity.
    let entity = thinkingroot_core::Entity::new(
        "TestService",
        thinkingroot_core::types::EntityType::Service,
    );
    store.insert_entity(&entity).unwrap();

    // Query it back.
    let entities = store.get_all_entities().unwrap();
    assert_eq!(entities.len(), 1);
    assert_eq!(entities[0].1, "TestService");

    // Insert a claim.
    let claim = thinkingroot_core::Claim::new(
        "TestService uses PostgreSQL",
        thinkingroot_core::types::ClaimType::Fact,
        thinkingroot_core::types::SourceId::new(),
        thinkingroot_core::types::WorkspaceId::new(),
    );
    store.insert_claim(&claim).unwrap();

    // Link claim to entity.
    store
        .link_claim_to_entity(&claim.id.to_string(), &entity.id.to_string())
        .unwrap();

    // Query claims for entity.
    let claims = store.get_claims_for_entity(&entity.id.to_string()).unwrap();
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].1, "TestService uses PostgreSQL");

    // Counts.
    let (_, c, e) = store.get_counts().unwrap();
    assert_eq!(c, 1);
    assert_eq!(e, 1);
}

#[test]
fn contradiction_detection_in_graph() {
    let (_tmp, store) = temp_graph_store();

    store
        .insert_contradiction("c1", "claim_a", "claim_b", "test contradiction")
        .unwrap();

    let contradictions = store.get_contradictions().unwrap();
    assert_eq!(contradictions.len(), 1);
    assert_eq!(contradictions[0].3, "test contradiction");
    assert_eq!(contradictions[0].4, "Detected");
}

// ─── Entity Resolution ───────────────────────────────────────

#[test]
fn entity_resolution_merges_duplicates() {
    use thinkingroot_core::types::{Entity, EntityType};
    use thinkingroot_link::resolution;

    let existing = vec![Entity::new("PostgreSQL", EntityType::Database)];

    // Exact match.
    let dup = Entity::new("PostgreSQL", EntityType::Database);
    let resolved = resolution::resolve_entity(&dup, &existing);
    assert!(resolved.is_some(), "exact match should resolve");

    // Fuzzy match (high similarity, above 0.85 threshold).
    let similar = Entity::new("Postgresql", EntityType::Database);
    let resolved = resolution::resolve_entity(&similar, &existing);
    assert!(resolved.is_some(), "fuzzy match should resolve");

    // Different type should still match by exact name (case-insensitive).
    let different_type = Entity::new("PostgreSQL", EntityType::Person);
    let resolved = resolution::resolve_entity(&different_type, &existing);
    assert!(
        resolved.is_some(),
        "exact name match should resolve regardless of type"
    );

    // Completely different name should NOT match.
    let different_name = Entity::new("Redis", EntityType::Database);
    let resolved = resolution::resolve_entity(&different_name, &existing);
    assert!(resolved.is_none(), "different name should not resolve");

    // Test merge.
    let mut base = Entity::new("PostgreSQL", EntityType::Database);
    let addon = Entity::new("Postgresql", EntityType::Database);
    resolution::merge_entities(&mut base, &addon);
    assert!(
        base.aliases.iter().any(|a| a == "Postgresql"),
        "merge should add alias"
    );
}

// ─── Incremental Compilation ─────────────────────────────────

#[test]
fn incremental_hash_detection() {
    let (_tmp, store) = temp_graph_store();

    // No hash exists yet.
    assert!(!store.source_hash_exists("abc123").unwrap());

    // Insert a source with that hash.
    let source = thinkingroot_core::Source::new(
        "test://file.md".into(),
        thinkingroot_core::types::SourceType::File,
    )
    .with_hash(thinkingroot_core::types::ContentHash("abc123".into()));
    store.insert_source(&source).unwrap();

    // Now it should exist.
    assert!(store.source_hash_exists("abc123").unwrap());

    // Different hash should not exist.
    assert!(!store.source_hash_exists("xyz789").unwrap());
}

// ─── Compiler ────────────────────────────────────────────────

#[test]
fn compiler_produces_all_artifact_types() {
    let (_tmp, store) = temp_graph_store();

    // Insert minimal data for compilation.
    let entity =
        thinkingroot_core::Entity::new("TestAPI", thinkingroot_core::types::EntityType::Api);
    store.insert_entity(&entity).unwrap();

    let source = thinkingroot_core::Source::new(
        "test://doc.md".into(),
        thinkingroot_core::types::SourceType::File,
    );
    store.insert_source(&source).unwrap();

    let claim = thinkingroot_core::Claim::new(
        "TestAPI handles requests",
        thinkingroot_core::types::ClaimType::Fact,
        source.id,
        thinkingroot_core::types::WorkspaceId::new(),
    );
    store.insert_claim(&claim).unwrap();
    store
        .link_claim_to_source(&claim.id.to_string(), &source.id.to_string())
        .unwrap();
    store
        .link_claim_to_entity(&claim.id.to_string(), &entity.id.to_string())
        .unwrap();

    // Compile to a temp dir.
    let compile_dir = tempfile::tempdir().unwrap();
    let config = thinkingroot_core::Config::default();
    let compiler = thinkingroot_compile::Compiler::new(&config).unwrap();
    let artifacts = compiler.compile_all(&store, compile_dir.path()).unwrap();

    // Should produce: 1 entity page + architecture map + contradiction report +
    // decision log + task pack + agent brief + runbook + health report = 8
    assert!(
        artifacts.len() >= 8,
        "expected at least 8 artifacts, got {}",
        artifacts.len()
    );

    // Check files on disk.
    let artifact_dir = compile_dir.path().join("artifacts");
    assert!(artifact_dir.join("entities").exists());
    assert!(artifact_dir.join("architecture-map.md").exists());
    assert!(artifact_dir.join("contradiction-report.md").exists());
    assert!(artifact_dir.join("decision-log.md").exists());
    assert!(artifact_dir.join("task-pack.md").exists());
    assert!(artifact_dir.join("agent-brief.md").exists());
    assert!(artifact_dir.join("runbook.md").exists());
    assert!(artifact_dir.join("health-report.md").exists());
}

// ─── Health Score ────────────────────────────────────────────

#[test]
fn health_score_computation() {
    let score = thinkingroot_core::types::HealthScore::compute(1.0, 1.0, 1.0, 1.0);
    assert_eq!(score.as_percentage(), 100);

    let score = thinkingroot_core::types::HealthScore::compute(0.5, 0.5, 0.5, 0.5);
    assert_eq!(score.as_percentage(), 50);

    let score = thinkingroot_core::types::HealthScore::compute(0.0, 0.0, 0.0, 0.0);
    assert_eq!(score.as_percentage(), 0);
}

// ─── Temporal Ordering ───────────────────────────────────────

#[test]
fn temporal_supersession_tracking() {
    let (_tmp, store) = temp_graph_store();

    // Insert two claims.
    let source_id = thinkingroot_core::types::SourceId::new();
    let ws_id = thinkingroot_core::types::WorkspaceId::new();

    let old_claim = thinkingroot_core::Claim::new(
        "PostgreSQL 14 is the latest",
        thinkingroot_core::types::ClaimType::Fact,
        source_id,
        ws_id,
    );
    let new_claim = thinkingroot_core::Claim::new(
        "PostgreSQL 16 is the latest",
        thinkingroot_core::types::ClaimType::Fact,
        source_id,
        ws_id,
    );
    store.insert_claim(&old_claim).unwrap();
    store.insert_claim(&new_claim).unwrap();

    // No superseded claims yet.
    assert_eq!(store.count_superseded_claims().unwrap(), 0);

    // Supersede the old claim.
    store
        .supersede_claim(&old_claim.id.to_string(), &new_claim.id.to_string())
        .unwrap();

    // Now there should be 1 superseded claim.
    assert_eq!(store.count_superseded_claims().unwrap(), 1);
}

// ─── Orphan Detection ────────────────────────────────────────

#[test]
fn orphan_claim_detection() {
    let (_tmp, store) = temp_graph_store();

    // Insert a source and a properly linked claim.
    let source = thinkingroot_core::Source::new(
        "test://exists.md".into(),
        thinkingroot_core::types::SourceType::File,
    );
    store.insert_source(&source).unwrap();

    let linked_claim = thinkingroot_core::Claim::new(
        "This claim has a source",
        thinkingroot_core::types::ClaimType::Fact,
        source.id,
        thinkingroot_core::types::WorkspaceId::new(),
    );
    store.insert_claim(&linked_claim).unwrap();

    // Insert a claim whose source_id points to a non-existent source.
    let orphan_claim = thinkingroot_core::Claim::new(
        "This claim is orphaned",
        thinkingroot_core::types::ClaimType::Fact,
        thinkingroot_core::types::SourceId::new(), // non-existent source
        thinkingroot_core::types::WorkspaceId::new(),
    );
    store.insert_claim(&orphan_claim).unwrap();

    // Should detect 1 orphaned claim.
    assert_eq!(store.count_orphaned_claims().unwrap(), 1);
}

// ─── Search ──────────────────────────────────────────────────

#[test]
fn keyword_search_in_graph() {
    let (_tmp, store) = temp_graph_store();

    // Insert source + claim + edge for keyword search (needs claim_source_edges).
    let source = thinkingroot_core::Source::new(
        "test://search.md".into(),
        thinkingroot_core::types::SourceType::File,
    );
    store.insert_source(&source).unwrap();

    let claim = thinkingroot_core::Claim::new(
        "PostgreSQL stores transaction records",
        thinkingroot_core::types::ClaimType::Fact,
        source.id,
        thinkingroot_core::types::WorkspaceId::new(),
    );
    store.insert_claim(&claim).unwrap();
    store
        .link_claim_to_source(&claim.id.to_string(), &source.id.to_string())
        .unwrap();

    let results = store.search_claims("postgresql").unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].1.contains("PostgreSQL"));

    // No match for unrelated keyword.
    let empty = store.search_claims("redis").unwrap();
    assert!(empty.is_empty());

    // Entity search.
    let entity = thinkingroot_core::Entity::new(
        "PostgreSQL",
        thinkingroot_core::types::EntityType::Database,
    );
    store.insert_entity(&entity).unwrap();

    let entity_results = store.search_entities("postgres").unwrap();
    assert_eq!(entity_results.len(), 1);
}
