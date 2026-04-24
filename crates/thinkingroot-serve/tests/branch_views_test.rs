//! Phase B regression tests: the four `*_branched` methods must return the
//! branch's real data, not silently fall through to main (which they did as
//! stubs at engine.rs:1295–1328 before Phase B landed).

use std::path::PathBuf;
use tempfile::tempdir;

use thinkingroot_core::{
    Claim, ClaimType, ContentHash, Entity, EntityType, Source, SourceType, TrustLevel, WorkspaceId,
};
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_serve::engine::{ClaimFilter, QueryEngine};

fn seed_source_and_claim(
    graph: &GraphStore,
    workspace: WorkspaceId,
    statement: &str,
    uri: &str,
) -> (String, String) {
    let source = Source::new(uri.to_string(), SourceType::Document)
        .with_trust(TrustLevel::Trusted)
        .with_hash(ContentHash(format!("hash-{uri}")));
    let source_id = source.id.to_string();
    graph.insert_source(&source).expect("insert source");

    let claim = Claim::new(statement, ClaimType::Fact, source.id, workspace);
    let claim_id = claim.id.to_string();
    graph.insert_claim(&claim).expect("insert claim");
    graph
        .link_claim_to_source(&claim_id, &source_id)
        .expect("link claim to source");
    (claim_id, source_id)
}

fn seed_entity(graph: &GraphStore, canonical_name: &str) -> String {
    let entity = Entity::new(canonical_name, EntityType::Service);
    let id = entity.id.to_string();
    graph.insert_entity(&entity).expect("insert entity");
    id
}

/// Build a workspace with:
/// - main has entity `AuthService`, 1 claim linked to it
/// - branch `feature/oauth` adds 1 more claim linked to the same entity
async fn setup_ws_with_branch() -> (tempfile::TempDir, PathBuf, QueryEngine, String, String) {
    let dir = tempdir().unwrap();
    let root: PathBuf = dir.path().to_path_buf();
    let graph_dir = root.join(".thinkingroot").join("graph");
    std::fs::create_dir_all(&graph_dir).unwrap();
    let workspace = WorkspaceId::new();

    let main_entity_id = {
        let main_graph = GraphStore::init(&graph_dir).unwrap();
        let entity_id = seed_entity(&main_graph, "AuthService");
        let (claim_id, _) = seed_source_and_claim(
            &main_graph,
            workspace,
            "AuthService uses JWT tokens",
            "file:///main.md",
        );
        main_graph
            .link_claim_to_entity(&claim_id, &entity_id)
            .unwrap();
        // Seed a relation AuthService → Database so get_relations has something.
        let other = seed_entity(&main_graph, "Database");
        main_graph
            .link_entities(&entity_id, &other, "DependsOn", 0.9)
            .unwrap();
        entity_id
    };

    thinkingroot_branch::create_branch(&root, "feature/oauth", "main", None)
        .await
        .unwrap();

    let branch_data_dir = root
        .join(".thinkingroot")
        .join("branches")
        .join("feature-oauth");
    let branch_claim_id = {
        let branch_graph = GraphStore::init(&branch_data_dir.join("graph")).unwrap();
        // Link the new claim to the (inherited) AuthService entity id.
        let (claim_id, _) = seed_source_and_claim(
            &branch_graph,
            workspace,
            "AuthService also supports OAuth2",
            "file:///branch.md",
        );
        branch_graph
            .link_claim_to_entity(&claim_id, &main_entity_id)
            .unwrap();
        // Add a branch-only relation: AuthService → Vault
        let vault_id = seed_entity(&branch_graph, "Vault");
        branch_graph
            .link_entities(&main_entity_id, &vault_id, "Uses", 0.85)
            .unwrap();
        claim_id
    };

    let mut engine = QueryEngine::new();
    engine
        .mount("demo".to_string(), root.clone())
        .await
        .unwrap();

    (dir, root, engine, main_entity_id, branch_claim_id)
}

#[tokio::test]
async fn list_claims_branched_sees_branch_additions() {
    let (_dir, _root, engine, _, branch_claim_id) = setup_ws_with_branch().await;

    let main = engine
        .list_claims("demo", ClaimFilter::default())
        .await
        .unwrap();
    assert_eq!(main.len(), 1, "main should see 1 claim");

    let branch = engine
        .list_claims_branched("demo", ClaimFilter::default(), Some("feature/oauth"))
        .await
        .unwrap();
    assert_eq!(
        branch.len(),
        2,
        "branch should see inherited + added = 2 claims, got {:?}",
        branch.iter().map(|c| &c.statement).collect::<Vec<_>>()
    );
    assert!(
        branch.iter().any(|c| c.id == branch_claim_id),
        "branch-only claim must appear"
    );
}

#[tokio::test]
async fn list_claims_branched_none_delegates_to_main() {
    let (_dir, _root, engine, _, _) = setup_ws_with_branch().await;

    let a = engine
        .list_claims("demo", ClaimFilter::default())
        .await
        .unwrap();
    let b = engine
        .list_claims_branched("demo", ClaimFilter::default(), None)
        .await
        .unwrap();
    assert_eq!(
        a.len(),
        b.len(),
        "branch=None must delegate to list_claims exactly"
    );
}

#[tokio::test]
async fn list_claims_branched_entity_filter_works() {
    let (_dir, _root, engine, _, _) = setup_ws_with_branch().await;
    let filter = ClaimFilter {
        entity_name: Some("AuthService".to_string()),
        ..Default::default()
    };
    let branch = engine
        .list_claims_branched("demo", filter, Some("feature/oauth"))
        .await
        .unwrap();
    assert_eq!(
        branch.len(),
        2,
        "entity-filtered branch should see 2 claims linked to AuthService"
    );
}

#[tokio::test]
async fn get_relations_branched_sees_branch_only_relation() {
    let (_dir, _root, engine, _, _) = setup_ws_with_branch().await;
    let main = engine.get_relations("demo", "AuthService").await.unwrap();
    assert_eq!(
        main.len(),
        1,
        "main relation count = 1 (DependsOn Database)"
    );

    let branch = engine
        .get_relations_branched("demo", "AuthService", Some("feature/oauth"))
        .await
        .unwrap();
    assert_eq!(
        branch.len(),
        2,
        "branch should see 2 relations (DependsOn + Uses)"
    );
    assert!(branch.iter().any(|r| r.relation_type == "Uses"));
}

#[tokio::test]
async fn get_workspace_brief_branched_counts_match_branch() {
    let (_dir, _root, engine, _, _) = setup_ws_with_branch().await;
    let main_brief = engine.get_workspace_brief("demo").await.unwrap();
    let branch_brief = engine
        .get_workspace_brief_branched("demo", Some("feature/oauth"))
        .await
        .unwrap();
    assert!(
        branch_brief.claim_count > main_brief.claim_count,
        "branch claim_count ({}) must exceed main ({})",
        branch_brief.claim_count,
        main_brief.claim_count
    );
    assert!(
        branch_brief.entity_count >= main_brief.entity_count,
        "branch has at least as many entities as main"
    );
}

#[tokio::test]
async fn get_entity_context_branched_reads_branch_graph() {
    let (_dir, _root, engine, _, _) = setup_ws_with_branch().await;
    let ctx = engine
        .get_entity_context_branched("demo", "AuthService", Some("feature/oauth"))
        .await
        .unwrap();
    assert!(
        ctx.is_some(),
        "branch should find AuthService entity context"
    );
}

#[tokio::test]
async fn branched_methods_error_on_missing_branch() {
    let (_dir, _root, engine, _, _) = setup_ws_with_branch().await;

    let r = engine
        .list_claims_branched("demo", ClaimFilter::default(), Some("does/not/exist"))
        .await;
    assert!(r.is_err(), "missing branch must error (never silent)");

    let r = engine
        .get_relations_branched("demo", "AuthService", Some("does/not/exist"))
        .await;
    assert!(r.is_err());

    let r = engine
        .get_workspace_brief_branched("demo", Some("does/not/exist"))
        .await;
    assert!(r.is_err());

    let r = engine
        .get_entity_context_branched("demo", "AuthService", Some("does/not/exist"))
        .await;
    assert!(r.is_err());
}

#[tokio::test]
async fn branched_methods_with_main_delegate() {
    let (_dir, _root, engine, _, _) = setup_ws_with_branch().await;

    let a = engine.get_relations("demo", "AuthService").await.unwrap();
    let b = engine
        .get_relations_branched("demo", "AuthService", Some("main"))
        .await
        .unwrap();
    assert_eq!(
        a.len(),
        b.len(),
        "branch=Some(\"main\") delegates to main path"
    );
}
