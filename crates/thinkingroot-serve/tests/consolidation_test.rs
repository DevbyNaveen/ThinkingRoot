//! #2 — Promotion consolidation, end-to-end against a real engine.
//!
//! Proves the full pipeline on real CozoDB-backed per-user workspaces:
//!   - quorum'd patterns (≥ min_users distinct users) promote; sub-quorum and
//!     single-user facts do NOT;
//!   - the promoted statement is de-identified (emails scrubbed → `<email>`),
//!     and per-user secrets never leak into the shared brain;
//!   - promotion only lands through the verify-before-merge gate (health_score
//!     check passes → merged), and a reviewer requirement blocks auto-merge.

use std::path::PathBuf;

use tempfile::tempdir;
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_serve::consolidation::ConsolidationSpec;
use thinkingroot_serve::engine::{AgentClaim, Principal, QueryEngine};
use thinkingroot_serve::intelligence::session::SessionStore;

fn mem(stmt: &str) -> AgentClaim {
    AgentClaim {
        statement: stmt.to_string(),
        claim_type: "memory".into(),
        confidence: Some(0.85),
        entities: vec![],
    }
}

/// Mount a per-user workspace and seed its (own, isolated) brain with claims.
async fn seed_user(
    engine: &mut QueryEngine,
    sessions: &SessionStore,
    user: &str,
    claims: Vec<AgentClaim>,
) {
    engine.get_or_mount_user_ws(user).await.unwrap();
    engine
        .contribute_bulk(
            user,
            &format!("sdk:{user}"),
            None, // the user's own main brain
            claims,
            sessions,
            Principal::Connector {
                connector_id: "sdk".into(),
                install_id: user.into(),
            },
            &format!("seed-{user}"),
            false,
        )
        .await
        .unwrap();
}

/// Stand up a shared brain + three per-user brains with one shared pattern
/// (each phrased with the user's OWN email, so scrubbing must collapse them)
/// plus per-user secrets.
async fn setup() -> (tempfile::TempDir, PathBuf, QueryEngine, SessionStore) {
    let dir = tempdir().unwrap();
    let root: PathBuf = dir.path().join("shared");
    let graph_dir = root.join(".thinkingroot").join("graph");
    std::fs::create_dir_all(&graph_dir).unwrap();
    GraphStore::init(&graph_dir).unwrap();

    let mut engine = QueryEngine::new();
    // First non-`u_` mount becomes the shared/primary brain.
    engine.mount("shared".to_string(), root.clone()).await.unwrap();
    let sessions = SessionStore::default();

    seed_user(
        &mut engine,
        &sessions,
        "u_alice",
        vec![
            mem("Reach support at alice@acme.com for onboarding help"),
            mem("Alice's secret project codename is Falcon"),
        ],
    )
    .await;
    seed_user(
        &mut engine,
        &sessions,
        "u_bob",
        vec![
            mem("Reach support at bob@acme.com for onboarding help"),
            mem("Bob keeps his prod token in a sticky note"),
        ],
    )
    .await;
    seed_user(
        &mut engine,
        &sessions,
        "u_carol",
        vec![mem("Reach support at carol@acme.com for onboarding help.")],
    )
    .await;

    (dir, root, engine, sessions)
}

#[tokio::test]
async fn promotes_quorum_deidentified_and_merges() {
    let (_dir, root, engine, sessions) = setup().await;

    let report = engine
        .consolidate_to_shared(ConsolidationSpec::default(), &sessions)
        .await
        .unwrap();

    println!("report: {}", serde_json::to_string_pretty(&report).unwrap());

    assert_eq!(report.users_scanned, 3, "all three user brains scanned");
    assert_eq!(
        report.patterns_promoted.len(),
        1,
        "only the 3-user onboarding pattern clears quorum"
    );
    let p = &report.patterns_promoted[0];
    assert_eq!(p.distinct_users, 3, "quorum across three distinct users");

    // De-identified: the shared statement carries no raw email, just the
    // placeholder the scrubber inserts.
    assert!(
        p.statement.contains("<email>") && !p.statement.contains("@acme.com"),
        "promoted statement must be de-identified, got: {}",
        p.statement
    );

    // Per-user secrets never reached quorum, so they never appear.
    let promoted_text = report
        .patterns_promoted
        .iter()
        .map(|p| p.statement.as_str())
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(!promoted_text.contains("Falcon"), "single-user secret leaked");
    assert!(!promoted_text.contains("sticky note"), "single-user secret leaked");

    // The verify-before-merge gate ran and passed.
    assert!(
        report.checks.iter().any(|(n, passed, _)| n == "health_score" && *passed),
        "health_score check must have passed: {:?}",
        report.checks
    );
    assert!(report.merged, "approved + checks-passed promotion must merge");
    assert_eq!(report.proposal_status.as_deref(), Some("merged"));

    // Disk truth: re-open the SHARED brain graph and confirm the promoted,
    // de-identified statement physically landed there — and no user secret did.
    drop(engine);
    let shared_graph = GraphStore::init(&root.join(".thinkingroot").join("graph")).unwrap();
    let rows = shared_graph.get_all_claims_with_sources().unwrap();
    let shared_text = rows
        .iter()
        .map(|(_, stmt, ..)| stmt.as_str())
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(
        shared_text.contains("onboarding") && shared_text.contains("<email>"),
        "promoted pattern must be in the shared brain on disk, got: {shared_text}"
    );
    assert!(!shared_text.contains("@acme.com"), "raw email leaked into shared brain");
    assert!(!shared_text.contains("Falcon"), "user secret leaked into shared brain");
}

#[tokio::test]
async fn quorum_floor_blocks_promotion() {
    let (_dir, _root, engine, sessions) = setup().await;

    // Require 4 distinct users — only 3 exist, so nothing can clear quorum.
    let spec = ConsolidationSpec {
        min_users: 4,
        ..ConsolidationSpec::default()
    };
    let report = engine.consolidate_to_shared(spec, &sessions).await.unwrap();

    assert_eq!(report.users_scanned, 3);
    assert!(report.patterns_promoted.is_empty(), "nothing clears a 4-user floor");
    assert!(report.staging_branch.is_none(), "no branch created for an empty pass");
    assert!(report.proposal_id.is_none());
    assert!(!report.merged);
}

#[tokio::test]
async fn reviewer_requirement_gates_auto_merge() {
    let (_dir, _root, engine, sessions) = setup().await;

    // A human reviewer is required: even with checks passing, the proposal
    // stays Open (no reviewer recorded) and must NOT auto-merge.
    let spec = ConsolidationSpec {
        min_reviewers: 1,
        ..ConsolidationSpec::default()
    };
    let report = engine.consolidate_to_shared(spec, &sessions).await.unwrap();

    assert_eq!(report.patterns_promoted.len(), 1, "the pattern still clears quorum");
    assert!(report.proposal_id.is_some(), "a proposal is staged for review");
    assert!(
        !report.merged,
        "must NOT merge without the required reviewer: {}",
        report.note
    );
    assert_eq!(
        report.proposal_status.as_deref(),
        Some("open"),
        "proposal stays Open pending review"
    );
}
