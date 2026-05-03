//! T0.7 + T2.6 integration tests.
//!
//! T0.7 — `contribute_bulk` is the connector-attributed write API
//! with idempotent replay semantics. The tests verify:
//!
//!   1. First-time call writes claims and records the
//!      `connector_ingest_log` entry.
//!   2. Replay (same `connector_id`/`install_id`/`idempotency_key`)
//!      short-circuits to the original `accepted_ids` without
//!      writing duplicates.
//!   3. Different `idempotency_key` → fresh write.
//!   4. Non-connector principals are rejected.
//!   5. Empty `idempotency_key` is rejected.
//!   6. Connector source URI (`connector://...`) lands on the
//!      synthetic source rather than the historical
//!      `mcp://agent/...` form.
//!
//! T2.6 — per-branch `RedactionPolicy` is enforced at the response
//! boundary. Verified by writing PII-shaped claims (an email, an
//! SSN-shaped string), setting a policy, and confirming both the
//! pattern rewrite and the `min_sensitivity` drop fire.

use std::path::PathBuf;
use tempfile::tempdir;

use thinkingroot_core::{
    BranchKind, BranchPermissions, MergePolicy, OutboundMode, RedactionPolicy, Sensitivity,
    WorkspaceId,
};
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_serve::engine::{AgentClaim, ClaimFilter, Principal, QueryEngine};

async fn setup_engine_with_branch() -> (tempfile::TempDir, PathBuf, QueryEngine, String) {
    let dir = tempdir().unwrap();
    let root: PathBuf = dir.path().to_path_buf();
    let graph_dir = root.join(".thinkingroot").join("graph");
    std::fs::create_dir_all(&graph_dir).unwrap();
    // Init the main graph so vector index has somewhere to land.
    {
        let _g = GraphStore::init(&graph_dir).unwrap();
    }

    // Create a feature branch we can target.
    thinkingroot_branch::create_branch_full(
        &root,
        "feature/connector",
        "main",
        Some("connector branch".into()),
        None,
        BranchPermissions::default(),
        BranchKind::Feature,
        MergePolicy::Manual,
        None,
    )
    .await
    .unwrap();

    let mut engine = QueryEngine::new();
    engine.mount("demo".to_string(), root.clone()).await.unwrap();

    let _ = WorkspaceId::new(); // silence unused-import on the workspace symbol
    (dir, root, engine, "feature/connector".into())
}

fn ac(stmt: &str) -> AgentClaim {
    AgentClaim {
        statement: stmt.to_string(),
        claim_type: "fact".into(),
        confidence: Some(0.8),
        entities: vec![],
    }
}

// ─── T0.7 ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn contribute_bulk_idempotent_replay_short_circuits() {
    let (_dir, _root, engine, branch) = setup_engine_with_branch().await;
    let sessions = thinkingroot_serve::intelligence::session::SessionStore::default();

    let principal = Principal::Connector {
        connector_id: "github".into(),
        install_id: "alice-acme".into(),
    };

    let first = engine
        .contribute_bulk(
            "demo",
            "sess",
            Some(&branch),
            vec![ac("PR #42 merged to main"), ac("issue #100 closed")],
            &sessions,
            principal.clone(),
            "delivery-12345",
            false,
        )
        .await
        .unwrap();
    assert_eq!(first.accepted_count, 2, "first call should accept 2 claims");
    assert_eq!(first.accepted_ids.len(), 2);
    assert!(
        first.source_uri.starts_with("connector://github/alice-acme/"),
        "source URI should carry the connector identity, got: {}",
        first.source_uri
    );

    // ── Replay: same triple → short-circuit, same accepted_ids, no
    // new writes. The replay must surface the "replay: existing
    // ingest" warning so callers can distinguish replay from a
    // fresh write.
    let replay = engine
        .contribute_bulk(
            "demo",
            "sess",
            Some(&branch),
            vec![ac("PR #42 merged to main"), ac("issue #100 closed")],
            &sessions,
            principal.clone(),
            "delivery-12345",
            false,
        )
        .await
        .unwrap();
    assert_eq!(
        replay.accepted_ids, first.accepted_ids,
        "replay must return identical accepted_ids"
    );
    assert!(
        replay.warnings.iter().any(|w| w.contains("replay")),
        "replay warning expected, got: {:?}",
        replay.warnings
    );

    // ── Branch must hold exactly the original claim count, not
    // double. list_claims_branched joins the branch graph + main.
    let listed = engine
        .list_claims_branched("demo", ClaimFilter::default(), Some(&branch))
        .await
        .unwrap();
    let our_statements: Vec<&str> = listed.iter().map(|c| c.statement.as_str()).collect();
    assert_eq!(
        our_statements
            .iter()
            .filter(|s| **s == "PR #42 merged to main")
            .count(),
        1,
        "PR #42 claim must appear exactly once across replays, got listing: {our_statements:?}"
    );
}

#[tokio::test]
async fn contribute_bulk_different_idempotency_key_writes_fresh() {
    let (_dir, _root, engine, branch) = setup_engine_with_branch().await;
    let sessions = thinkingroot_serve::intelligence::session::SessionStore::default();

    let principal = Principal::Connector {
        connector_id: "slack".into(),
        install_id: "alice-acme".into(),
    };

    let a = engine
        .contribute_bulk(
            "demo",
            "sess",
            Some(&branch),
            vec![ac("message m1")],
            &sessions,
            principal.clone(),
            "msg-1",
            false,
        )
        .await
        .unwrap();
    let b = engine
        .contribute_bulk(
            "demo",
            "sess",
            Some(&branch),
            vec![ac("message m2")],
            &sessions,
            principal,
            "msg-2",
            false,
        )
        .await
        .unwrap();
    assert_ne!(
        a.accepted_ids, b.accepted_ids,
        "different idempotency_key must produce different accepted_ids"
    );
    assert!(b.warnings.iter().all(|w| !w.contains("replay")));
}

#[tokio::test]
async fn contribute_bulk_rejects_non_connector_principal() {
    let (_dir, _root, engine, branch) = setup_engine_with_branch().await;
    let sessions = thinkingroot_serve::intelligence::session::SessionStore::default();

    let result = engine
        .contribute_bulk(
            "demo",
            "sess",
            Some(&branch),
            vec![ac("anything")],
            &sessions,
            Principal::Agent("claude".into()),
            "k1",
            false,
        )
        .await;
    assert!(
        result.is_err(),
        "non-connector principal must be rejected for contribute_bulk"
    );
}

#[tokio::test]
async fn contribute_bulk_rejects_empty_idempotency_key() {
    let (_dir, _root, engine, branch) = setup_engine_with_branch().await;
    let sessions = thinkingroot_serve::intelligence::session::SessionStore::default();

    let principal = Principal::Connector {
        connector_id: "notion".into(),
        install_id: "alice-acme".into(),
    };
    let result = engine
        .contribute_bulk(
            "demo",
            "sess",
            Some(&branch),
            vec![ac("p")],
            &sessions,
            principal,
            "",
            false,
        )
        .await;
    assert!(
        result.is_err(),
        "empty idempotency_key must be rejected (no scope to dedupe against)"
    );
}

// ─── T2.6 ──────────────────────────────────────────────────────────────

/// Compose a couple of claims inline without hitting the engine
/// contribute path, then exercise `RedactionPolicy::rewrite` +
/// `should_drop` at the same boundary the engine layer would. Keeps
/// the test independent of the storage layer.
#[test]
fn redaction_pattern_rewrite_drops_email() {
    let policy = RedactionPolicy {
        patterns: vec![r"\b[\w.+-]+@[\w.-]+\.[A-Za-z]{2,}\b".into()],
        replacement: "[email]".into(),
        modes: vec![OutboundMode::ListClaims, OutboundMode::Search],
        min_sensitivity: None,
        drop_above_min: true,
    };
    let claim = "ping alice@corp.com about the bob@corp.com handoff";
    let after = policy.rewrite(claim);
    assert_eq!(after, "ping [email] about the [email] handoff");
    assert!(policy.applies_to(&OutboundMode::ListClaims));
    assert!(policy.applies_to(&OutboundMode::Search));
    assert!(!policy.applies_to(&OutboundMode::Brief));
    assert!(!policy.applies_to(&OutboundMode::Probe));
}

#[test]
fn redaction_min_sensitivity_drop_above_threshold() {
    let policy = RedactionPolicy {
        patterns: vec![],
        replacement: String::new(),
        modes: vec![],
        min_sensitivity: Some(Sensitivity::Confidential),
        drop_above_min: true,
    };
    // At-or-above threshold → drop.
    assert!(policy.should_drop(Sensitivity::Confidential));
    assert!(policy.should_drop(Sensitivity::Restricted));
    // Below threshold → pass through unchanged.
    assert!(!policy.should_drop(Sensitivity::Internal));
    assert!(!policy.should_drop(Sensitivity::Public));
    // Drop mode → no substitution text.
    assert!(policy.redact_text(Sensitivity::Confidential).is_none());
}

#[test]
fn redaction_min_sensitivity_substitute_keeps_row() {
    let policy = RedactionPolicy {
        patterns: vec![],
        replacement: String::new(),
        modes: vec![],
        min_sensitivity: Some(Sensitivity::Confidential),
        drop_above_min: false,
    };
    // Substitute mode never drops; text is replaced instead.
    assert!(!policy.should_drop(Sensitivity::Confidential));
    assert!(!policy.should_drop(Sensitivity::Restricted));
    assert_eq!(
        policy.redact_text(Sensitivity::Confidential),
        Some("[redacted: Confidential]".to_string())
    );
    assert_eq!(
        policy.redact_text(Sensitivity::Restricted),
        Some("[redacted: Restricted]".to_string())
    );
    assert!(policy.redact_text(Sensitivity::Internal).is_none());
}

#[test]
fn redaction_set_branch_redaction_persists_through_registry() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join(".thinkingroot/graph")).unwrap();
    std::fs::write(root.join(".thinkingroot/graph/graph.db"), b"placeholder").unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        thinkingroot_branch::create_branch(root, "feature/redact", "main", None)
            .await
            .unwrap();
    });

    let policy = RedactionPolicy {
        patterns: vec![
            r"\b[\w.+-]+@[\w.-]+\.[A-Za-z]{2,}\b".into(),
            r"\b\d{3}-\d{2}-\d{4}\b".into(),
        ],
        replacement: "[redacted]".into(),
        modes: vec![OutboundMode::ListClaims, OutboundMode::Search],
        min_sensitivity: Some(Sensitivity::Confidential),
        drop_above_min: false,
    };

    thinkingroot_branch::set_branch_redaction(root, "feature/redact", Some(policy.clone()))
        .unwrap();

    // Round-trip via list_branches to confirm TOML persistence.
    let branches = thinkingroot_branch::list_branches(root).unwrap();
    let our = branches
        .iter()
        .find(|b| b.name == "feature/redact")
        .expect("branch must be in active list");
    assert_eq!(our.redaction.as_ref(), Some(&policy));
}

// ─── T0.7 + T2.6 — graph-level connector ingest persistence ──────────

#[test]
fn connector_ingest_log_round_trips() {
    let dir = tempdir().unwrap();
    let graph_dir = dir.path().join("graph");
    std::fs::create_dir_all(&graph_dir).unwrap();
    let g = GraphStore::init(&graph_dir).unwrap();

    let ids = vec!["claim-a".to_string(), "claim-b".to_string()];
    g.record_connector_ingest(
        "github",
        "alice-acme",
        "delivery-1",
        &ids,
        Some("feature/x"),
        "connector://github/alice-acme/delivery-1",
    )
    .unwrap();

    let hit = g
        .lookup_connector_ingest("github", "alice-acme", "delivery-1")
        .unwrap()
        .expect("ingest log entry must round-trip");
    assert_eq!(hit.claim_ids, ids);
    assert_eq!(hit.connector_id, "github");
    assert_eq!(hit.install_id, "alice-acme");
    assert_eq!(hit.idempotency_key, "delivery-1");
    assert_eq!(hit.branch.as_deref(), Some("feature/x"));
    assert!(hit.source_uri.starts_with("connector://github"));

    // Different key returns None.
    assert!(
        g.lookup_connector_ingest("github", "alice-acme", "delivery-2")
            .unwrap()
            .is_none()
    );
    // Different install_id returns None.
    assert!(
        g.lookup_connector_ingest("github", "bob-acme", "delivery-1")
            .unwrap()
            .is_none()
    );
}

// ─── T2.6 end-to-end through the engine outbound path ───────────────

#[tokio::test]
async fn list_claims_branched_redacts_via_pattern_and_sensitivity() {
    use thinkingroot_core::{
        Claim, ClaimType, ContentHash, Source, SourceType, TrustLevel,
    };

    // Set up a workspace with a branch and seed the *branch graph*
    // with three claims:
    //
    //   1. Public, contains an email pattern
    //   2. Confidential
    //   3. Restricted
    //
    // Then attach a redaction policy:
    //   - patterns: email regex
    //   - min_sensitivity: Confidential, drop_above_min: true
    //
    // Expected: only claim 1 survives, with its email replaced.
    let dir = tempdir().unwrap();
    let root: PathBuf = dir.path().to_path_buf();
    let graph_dir = root.join(".thinkingroot").join("graph");
    std::fs::create_dir_all(&graph_dir).unwrap();
    {
        let _g = GraphStore::init(&graph_dir).unwrap();
    }

    thinkingroot_branch::create_branch(&root, "feature/policy", "main", None)
        .await
        .unwrap();

    let workspace = WorkspaceId::new();
    let branch_data_dir = root
        .join(".thinkingroot")
        .join("branches")
        .join("feature-policy");
    {
        let bg = GraphStore::init(&branch_data_dir.join("graph")).unwrap();
        let s = Source::new("file:///b".into(), SourceType::Document)
            .with_trust(TrustLevel::Trusted)
            .with_hash(ContentHash("h".into()));
        bg.insert_source(&s).unwrap();
        for (stmt, sens) in [
            (
                "ping alice@corp.com about the rollout",
                Sensitivity::Public,
            ),
            ("internal partnership terms", Sensitivity::Confidential),
            ("legal hold details", Sensitivity::Restricted),
        ] {
            let mut c = Claim::new(stmt, ClaimType::Fact, s.id, workspace);
            c = c.with_sensitivity(sens);
            let cid = c.id.to_string();
            bg.insert_claim(&c).unwrap();
            bg.link_claim_to_source(&cid, &s.id.to_string()).unwrap();
        }
    }

    let policy = RedactionPolicy {
        patterns: vec![r"\b[\w.+-]+@[\w.-]+\.[A-Za-z]{2,}\b".into()],
        replacement: "[email]".into(),
        modes: vec![OutboundMode::ListClaims],
        min_sensitivity: Some(Sensitivity::Confidential),
        drop_above_min: true,
    };
    thinkingroot_branch::set_branch_redaction(&root, "feature/policy", Some(policy)).unwrap();

    let mut engine = QueryEngine::new();
    engine.mount("demo".into(), root).await.unwrap();

    let listed = engine
        .list_claims_branched(
            "demo",
            ClaimFilter::default(),
            Some("feature/policy"),
        )
        .await
        .unwrap();

    let stmts: Vec<&str> = listed.iter().map(|c| c.statement.as_str()).collect();
    assert!(
        stmts.contains(&"ping [email] about the rollout"),
        "Public claim must survive with email redacted, got: {stmts:?}"
    );
    assert!(
        !stmts.iter().any(|s| s.contains("alice@corp.com")),
        "raw email must not leak through, got: {stmts:?}"
    );
    assert!(
        !stmts.iter().any(|s| s.contains("internal partnership")),
        "Confidential claim must be dropped, got: {stmts:?}"
    );
    assert!(
        !stmts.iter().any(|s| s.contains("legal hold")),
        "Restricted claim must be dropped, got: {stmts:?}"
    );
}

#[test]
fn get_sensitivities_for_claims_roundtrips() {
    use thinkingroot_core::{Claim, ClaimType, ContentHash, Source, SourceType, TrustLevel};

    let dir = tempdir().unwrap();
    let graph_dir = dir.path().join("graph");
    std::fs::create_dir_all(&graph_dir).unwrap();
    let g = GraphStore::init(&graph_dir).unwrap();

    let workspace = WorkspaceId::new();
    let source = Source::new("file:///s".into(), SourceType::Document)
        .with_trust(TrustLevel::Trusted)
        .with_hash(ContentHash("h".into()));
    g.insert_source(&source).unwrap();

    let public = Claim::new("public claim", ClaimType::Fact, source.id, workspace);
    let confid = Claim::new("confid claim", ClaimType::Fact, source.id, workspace)
        .with_sensitivity(Sensitivity::Confidential);
    let restricted = Claim::new("restricted claim", ClaimType::Fact, source.id, workspace)
        .with_sensitivity(Sensitivity::Restricted);

    let public_id = public.id.to_string();
    let confid_id = confid.id.to_string();
    let restricted_id = restricted.id.to_string();
    g.insert_claim(&public).unwrap();
    g.insert_claim(&confid).unwrap();
    g.insert_claim(&restricted).unwrap();

    let map = g
        .get_sensitivities_for_claims(&[
            public_id.clone(),
            confid_id.clone(),
            restricted_id.clone(),
            "no-such-id".to_string(),
        ])
        .unwrap();
    assert_eq!(map.get(&public_id).map(|s| s.as_str()), Some("Public"));
    assert_eq!(
        map.get(&confid_id).map(|s| s.as_str()),
        Some("Confidential")
    );
    assert_eq!(
        map.get(&restricted_id).map(|s| s.as_str()),
        Some("Restricted")
    );
    // Missing claim → absent from map (caller defaults to Public).
    assert!(!map.contains_key("no-such-id"));
}
