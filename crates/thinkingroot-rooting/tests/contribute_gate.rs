//! A4 — Contribute gate advisory vs enforce integration test.
//!
//! Runs the Rooter directly with both `contribute_gate = "advisory"` and
//! `contribute_gate = "enforce"` against a pre-seeded workspace where one
//! of two candidate claims will be Rejected by the Contradiction probe.
//! Then invokes `GraphStore::remove_claim_fully` on the Rejected claim
//! (the same call the serve-layer enforce path makes) and asserts the
//! graph state matches expectations for each mode.
//!
//! This avoids spinning up the full MCP + tokio stack and isolates the
//! contract that enforce mode must uphold:
//!
//!   Advisory: Rejected claim persists in the `claims` relation with
//!             `admission_tier = 'rejected'` (for audit / dashboards).
//!   Enforce : Rejected claim is excised from `claims` and every edge
//!             relation that named it; its `trial_verdicts` row is kept
//!             so the audit trail survives removal.

use thinkingroot_core::types::{
    AdmissionTier, Claim, ClaimType, ContentHash, ContradictionId, Source, SourceType, WorkspaceId,
};
use thinkingroot_rooting::{
    CandidateClaim, FileSystemSourceStore, Rooter, RootingConfig, SourceByteStore,
};

/// Build a graph with two claims: an admitted high-confidence incumbent
/// and a candidate that contradicts it. Returns the two claims so the
/// caller can drive the Rooter against them.
fn setup_graph_with_contradiction() -> (
    tempfile::TempDir,
    thinkingroot_graph::graph::GraphStore,
    FileSystemSourceStore,
    Claim,
    Claim,
) {
    let dir = tempfile::tempdir().expect("tmpdir");
    let graph = thinkingroot_graph::graph::GraphStore::init(dir.path()).expect("graph init");
    let store = FileSystemSourceStore::new(dir.path()).expect("byte store");

    // Source used by both claims — the statements reuse its vocabulary so
    // provenance always passes; contradiction is the only fatal failure.
    let source_body = "PaymentService charges cards via Stripe for card processing";
    let hash = ContentHash::from_bytes(source_body.as_bytes());
    let source = Source::new("file:///payment.rs".into(), SourceType::File).with_hash(hash.clone());
    graph.insert_source(&source).unwrap();
    store.put(source.id, &hash, source_body.as_bytes()).unwrap();

    // Incumbent: high-confidence established fact.
    let incumbent = Claim::new(
        "PaymentService charges cards via Stripe",
        ClaimType::Fact,
        source.id,
        WorkspaceId::new(),
    )
    .with_confidence(0.95);
    graph.insert_claim(&incumbent).unwrap();

    // Candidate: grounded vocabulary so provenance passes, but a pre-
    // registered contradiction against the incumbent will fail
    // Contradiction (fatal) → Rejected tier.
    let candidate = Claim::new(
        "PaymentService charges cards via Stripe for card processing",
        ClaimType::Fact,
        source.id,
        WorkspaceId::new(),
    )
    .with_confidence(0.80);
    graph.insert_claim(&candidate).unwrap();

    let cid = ContradictionId::new().to_string();
    graph
        .insert_contradiction(
            &cid,
            &candidate.id.to_string(),
            &incumbent.id.to_string(),
            "conflict — test fixture",
        )
        .unwrap();

    (dir, graph, store, incumbent, candidate)
}

fn count_claims(graph: &thinkingroot_graph::graph::GraphStore) -> usize {
    graph.get_all_claim_ids().expect("count claims").len()
}

fn claim_exists(graph: &thinkingroot_graph::graph::GraphStore, claim_id: &str) -> bool {
    matches!(graph.get_claim_by_id(claim_id), Ok(Some(_)))
}

#[test]
fn advisory_mode_keeps_rejected_claim_in_graph() {
    let (_dir, graph, store, incumbent, candidate) = setup_graph_with_contradiction();

    // Baseline: graph has exactly two claims (incumbent + candidate).
    assert_eq!(count_claims(&graph), 2);

    // Run Rooter with advisory gate (default).
    let cfg = RootingConfig {
        contribute_gate: "advisory".into(),
        ..RootingConfig::default()
    };
    let rooter = Rooter::new(&graph, &store, cfg);
    let candidates = [CandidateClaim {
        claim: &candidate,
        predicate: None,
        derivation: None,
    }];
    let out = rooter.root_batch(&candidates).expect("root_batch");
    let verdict = out.verdict_for(candidate.id).unwrap();
    assert_eq!(
        verdict.admission_tier,
        AdmissionTier::Rejected,
        "candidate should be Rejected by contradiction probe"
    );

    // Persist verdicts to match the real serve-layer flow.
    thinkingroot_rooting::storage::insert_verdicts_batch(&graph, &out.verdicts).unwrap();

    // Advisory contract: rejected claim still in the graph, verdict row
    // recorded for audit, no certificate (certificates are for admitted
    // tiers only).
    assert!(
        claim_exists(&graph, &candidate.id.to_string()),
        "advisory mode must not remove Rejected claim"
    );
    assert!(claim_exists(&graph, &incumbent.id.to_string()));
    assert_eq!(count_claims(&graph), 2, "advisory preserves claim count");

    let audit = graph
        .get_trial_verdicts_for_claim(&candidate.id.to_string())
        .unwrap();
    assert_eq!(audit.len(), 1, "advisory writes one verdict row");
    assert_eq!(audit[0].2, "rejected");
    assert_eq!(
        out.certificates.len(),
        0,
        "no certificate for Rejected claim"
    );
}

#[test]
fn enforce_mode_removes_rejected_claim_from_graph() {
    let (_dir, graph, store, incumbent, candidate) = setup_graph_with_contradiction();
    assert_eq!(count_claims(&graph), 2);

    // Run Rooter with enforce gate.
    let cfg = RootingConfig {
        contribute_gate: "enforce".into(),
        ..RootingConfig::default()
    };
    let rooter = Rooter::new(&graph, &store, cfg);
    let candidates = [CandidateClaim {
        claim: &candidate,
        predicate: None,
        derivation: None,
    }];
    let out = rooter.root_batch(&candidates).expect("root_batch");
    let verdict = out.verdict_for(candidate.id).unwrap();
    assert_eq!(verdict.admission_tier, AdmissionTier::Rejected);

    // Enforce path (mirrored from serve-layer engine.rs): write verdicts
    // first, then remove the Rejected claim. Verdicts survive so the
    // audit trail is not erased by enforcement.
    thinkingroot_rooting::storage::insert_verdicts_batch(&graph, &out.verdicts).unwrap();
    if verdict.admission_tier == AdmissionTier::Rejected {
        graph
            .remove_claim_fully(&candidate.id.to_string())
            .expect("remove_claim_fully");
    }

    // Enforce contract: Rejected claim excised; incumbent preserved.
    assert!(
        !claim_exists(&graph, &candidate.id.to_string()),
        "enforce mode must remove the Rejected claim"
    );
    assert!(
        claim_exists(&graph, &incumbent.id.to_string()),
        "enforce must not touch unrelated claims"
    );
    assert_eq!(count_claims(&graph), 1, "enforce reduces claim count by 1");

    // Audit trail preserved: trial_verdicts row survives the removal.
    let audit = graph
        .get_trial_verdicts_for_claim(&candidate.id.to_string())
        .unwrap();
    assert_eq!(
        audit.len(),
        1,
        "enforce keeps the verdict row for audit even after removing the claim"
    );
    assert_eq!(audit[0].2, "rejected");
}

#[test]
fn enforce_mode_keeps_rooted_claims() {
    // Control case: a candidate that passes every probe (no
    // contradiction, source matches) must survive enforce mode with its
    // tier upgraded.
    let dir = tempfile::tempdir().unwrap();
    let graph = thinkingroot_graph::graph::GraphStore::init(dir.path()).unwrap();
    let store = FileSystemSourceStore::new(dir.path()).unwrap();

    let source_body = "AuthService validates JWT tokens and rotates keys hourly";
    let hash = ContentHash::from_bytes(source_body.as_bytes());
    let source = Source::new("file:///auth.rs".into(), SourceType::File).with_hash(hash.clone());
    graph.insert_source(&source).unwrap();
    store.put(source.id, &hash, source_body.as_bytes()).unwrap();

    let candidate = Claim::new(
        "AuthService validates JWT tokens",
        ClaimType::Fact,
        source.id,
        WorkspaceId::new(),
    )
    .with_confidence(0.90);
    graph.insert_claim(&candidate).unwrap();

    let cfg = RootingConfig {
        contribute_gate: "enforce".into(),
        ..RootingConfig::default()
    };
    let rooter = Rooter::new(&graph, &store, cfg);
    let out = rooter
        .root_batch(&[CandidateClaim {
            claim: &candidate,
            predicate: None,
            derivation: None,
        }])
        .expect("root_batch");
    let tier = out.verdict_for(candidate.id).unwrap().admission_tier;
    assert!(
        matches!(tier, AdmissionTier::Rooted | AdmissionTier::Attested),
        "unopposed grounded claim should be admitted, got {:?}",
        tier
    );

    // Nothing to remove; enforce is a no-op for admitted claims.
    thinkingroot_rooting::storage::insert_verdicts_batch(&graph, &out.verdicts).unwrap();
    assert!(claim_exists(&graph, &candidate.id.to_string()));
    assert_eq!(count_claims(&graph), 1);
}
