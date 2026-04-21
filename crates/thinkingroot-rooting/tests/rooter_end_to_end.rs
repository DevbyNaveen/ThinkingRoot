//! End-to-end integration test for the Rooter.
//!
//! Covers the Week 2 demoable flow: (1) source bytes persisted to a real
//! filesystem store, (2) a claim whose tokens appear in the source bytes is
//! admitted, (3) a claim with fabricated tokens is rejected, (4) verdicts
//! + certificates round-trip through CozoDB.

use std::sync::Arc;

use thinkingroot_core::types::{
    AdmissionTier, Claim, ClaimType, ContentHash, DerivationProof, Entity, EntityType, Predicate,
    PredicateLanguage, PredicateScope, Source, SourceType, WorkspaceId,
};
use thinkingroot_rooting::{
    CandidateClaim, FileSystemSourceStore, Rooter, RootingConfig, SourceByteStore,
};

#[test]
fn rooter_admits_grounded_claim_and_rejects_fabricated_one() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let graph = thinkingroot_graph::graph::GraphStore::init(dir.path()).expect("graph init");
    let store = FileSystemSourceStore::new(dir.path()).expect("byte store");

    let source_body = "PaymentService uses Stripe for card processing";
    let hash = ContentHash::from_bytes(source_body.as_bytes());
    let source = Source::new("file:///payment.rs".into(), SourceType::File).with_hash(hash.clone());
    graph.insert_source(&source).unwrap();
    store.put(source.id, &hash, source_body.as_bytes()).unwrap();

    let good_claim = Claim::new(
        "PaymentService uses Stripe",
        ClaimType::Fact,
        source.id,
        WorkspaceId::new(),
    );
    let bogus_claim = Claim::new(
        "Acme cluster runs Kubernetes autoscaler for Redis",
        ClaimType::Fact,
        source.id,
        WorkspaceId::new(),
    );

    let candidates = [
        CandidateClaim {
            claim: &good_claim,
            predicate: None,
            derivation: None,
        },
        CandidateClaim {
            claim: &bogus_claim,
            predicate: None,
            derivation: None,
        },
    ];

    // Progress callback wired just to prove the hook fires.
    let call_count = Arc::new(std::sync::Mutex::new(0usize));
    let cc = Arc::clone(&call_count);
    let progress = Arc::new(move |_done: usize, _total: usize| {
        *cc.lock().unwrap() += 1;
    });

    let rooter = Rooter::new(&graph, &store, RootingConfig::default()).with_progress(progress);
    let output = rooter.root_batch(&candidates).expect("root_batch");

    // Sanity: progress fired per claim.
    assert_eq!(*call_count.lock().unwrap(), 2);
    assert_eq!(output.verdicts.len(), 2);

    let good_verdict = output
        .verdict_for(good_claim.id)
        .expect("verdict for grounded claim");
    // After Week 5 the Temporal probe is active for non-derived claims
    // (verifies valid_from <= now). That counts as an active non-fatal signal,
    // so a claim passing every probe — even without a predicate — earns
    // `Rooted`. Pre-Week-5 this would have been `Attested`.
    assert_eq!(
        good_verdict.admission_tier,
        AdmissionTier::Rooted,
        "grounded claim whose temporal probe passes should earn Rooted"
    );

    let bogus_verdict = output
        .verdict_for(bogus_claim.id)
        .expect("verdict for bogus claim");
    assert_eq!(
        bogus_verdict.admission_tier,
        AdmissionTier::Rejected,
        "fabricated claim should be Rejected on provenance"
    );
    assert!(bogus_verdict
        .failure_reason
        .as_deref()
        .unwrap_or_default()
        .contains("provenance"));

    // Persist verdicts + certificates and read them back.
    thinkingroot_rooting::storage::insert_verdicts_batch(&graph, &output.verdicts)
        .expect("persist verdicts");
    thinkingroot_rooting::storage::insert_certificates_batch(&graph, &output.certificates)
        .expect("persist certificates");

    let fetched_good = graph
        .get_trial_verdicts_for_claim(&good_claim.id.to_string())
        .unwrap();
    assert_eq!(fetched_good.len(), 1);
    assert_eq!(fetched_good[0].2, "rooted");

    let fetched_bogus = graph
        .get_trial_verdicts_for_claim(&bogus_claim.id.to_string())
        .unwrap();
    assert_eq!(fetched_bogus.len(), 1);
    assert_eq!(fetched_bogus[0].2, "rejected");

    // Certificates only exist for admitted claims.
    assert_eq!(
        output.certificates.len(),
        1,
        "only the admitted claim should have a certificate"
    );
    let cert = &output.certificates[0];
    let fetched_cert = graph
        .get_certificate_by_hash(&cert.hash)
        .unwrap()
        .expect("certificate persisted");
    assert_eq!(fetched_cert.0, good_claim.id.to_string());
}

#[test]
fn claim_with_matching_regex_predicate_reaches_rooted_tier() {
    // Matching regex predicate proves a claim is still true against source.
    // A non-matching predicate drops it to Quarantined (non-fatal demotion).
    let dir = tempfile::tempdir().unwrap();
    let graph = thinkingroot_graph::graph::GraphStore::init(dir.path()).unwrap();
    let store = FileSystemSourceStore::new(dir.path()).unwrap();

    let source_body = "// AuthService module\n// exposes token validation helpers\npub fn validate_token(t: &str) -> bool { t.starts_with(\"eyJ\") }";
    let hash = ContentHash::from_bytes(source_body.as_bytes());
    let source = Source::new("file:///auth.rs".into(), SourceType::File).with_hash(hash.clone());
    graph.insert_source(&source).unwrap();
    store.put(source.id, &hash, source_body.as_bytes()).unwrap();

    let matching_pred = Predicate {
        language: PredicateLanguage::Regex,
        query: r"fn\s+validate_token".into(),
        scope: PredicateScope::empty(),
    };
    let claim_rooted = Claim::new(
        "AuthService exposes validate_token",
        ClaimType::Fact,
        source.id,
        WorkspaceId::new(),
    )
    .with_predicate(matching_pred.clone());

    let bad_pred = Predicate {
        language: PredicateLanguage::Regex,
        query: r"fn\s+deprecated_api".into(),
        scope: PredicateScope::empty(),
    };
    let claim_quarantined = Claim::new(
        "AuthService exposes validate_token",
        ClaimType::Fact,
        source.id,
        WorkspaceId::new(),
    )
    .with_predicate(bad_pred);

    let candidates = [
        CandidateClaim {
            claim: &claim_rooted,
            predicate: claim_rooted.predicate.as_ref(),
            derivation: None,
        },
        CandidateClaim {
            claim: &claim_quarantined,
            predicate: claim_quarantined.predicate.as_ref(),
            derivation: None,
        },
    ];

    let rooter = Rooter::new(&graph, &store, RootingConfig::default());
    let output = rooter.root_batch(&candidates).expect("root_batch");

    let v_rooted = output
        .verdict_for(claim_rooted.id)
        .expect("verdict for matching-predicate claim");
    assert_eq!(
        v_rooted.admission_tier,
        AdmissionTier::Rooted,
        "matching predicate should promote to Rooted (not just Attested)"
    );

    let v_quar = output
        .verdict_for(claim_quarantined.id)
        .expect("verdict for non-matching-predicate claim");
    assert_eq!(
        v_quar.admission_tier,
        AdmissionTier::Quarantined,
        "non-matching predicate should demote to Quarantined (non-fatal)"
    );
    assert!(
        v_quar
            .failure_reason
            .as_deref()
            .unwrap_or_default()
            .contains("did not match"),
        "quarantine reason should surface the predicate failure"
    );

    // Sanity: certificates issued only for admitted claims (Rooted and Attested).
    // Quarantined does not produce a certificate.
    assert_eq!(output.certificates.len(), 1);
    assert_eq!(output.certificates[0].claim_id, claim_rooted.id.to_string());
}

#[test]
fn derived_claim_with_matching_ast_predicate_and_shared_parents_reaches_rooted() {
    // End-to-end: a derived claim with two parents that share an entity,
    // plus a tree-sitter-rust AST predicate that matches the source, should
    // pass every active probe and reach the `Rooted` tier.
    let dir = tempfile::tempdir().unwrap();
    let graph = thinkingroot_graph::graph::GraphStore::init(dir.path()).unwrap();
    let store = FileSystemSourceStore::new(dir.path()).unwrap();

    // Source containing a validate_token function — the AST predicate will
    // target this definition.
    let source_body = r#"// AuthService module
// exposes token validation helpers
pub fn validate_token(t: &str) -> bool { t.starts_with("eyJ") }
"#;
    let hash = ContentHash::from_bytes(source_body.as_bytes());
    let source = Source::new("file:///auth.rs".into(), SourceType::File).with_hash(hash.clone());
    graph.insert_source(&source).unwrap();
    store.put(source.id, &hash, source_body.as_bytes()).unwrap();

    // Parents share the `AuthService` entity — topology probe should pass.
    let auth_entity = Entity::new("AuthService", EntityType::Service);
    graph.insert_entity(&auth_entity).unwrap();
    let shared_entity_id = auth_entity.id.to_string();

    let parent_a = Claim::new(
        "AuthService module exposes token helpers",
        ClaimType::Fact,
        source.id,
        WorkspaceId::new(),
    );
    graph.insert_claim(&parent_a).unwrap();
    graph
        .link_claim_to_entity(&parent_a.id.to_string(), &shared_entity_id)
        .unwrap();

    let parent_b = Claim::new(
        "AuthService validate_token checks JWT prefix",
        ClaimType::Fact,
        source.id,
        WorkspaceId::new(),
    );
    graph.insert_claim(&parent_b).unwrap();
    graph
        .link_claim_to_entity(&parent_b.id.to_string(), &shared_entity_id)
        .unwrap();

    // Derived claim with an AST predicate targeting a `validate_token` fn.
    let ast_pred = Predicate {
        language: PredicateLanguage::RustAst,
        query: r#"(function_item name: (identifier) @n (#eq? @n "validate_token"))"#.into(),
        scope: PredicateScope::empty(),
    };
    let derivation = DerivationProof {
        parent_claim_ids: vec![parent_a.id, parent_b.id],
        derivation_rule: "reflect/auth-token-pattern-v1".into(),
    };
    let derived = Claim::new(
        "AuthService exposes validate_token",
        ClaimType::Fact,
        source.id,
        WorkspaceId::new(),
    )
    .with_predicate(ast_pred.clone())
    .with_derivation(derivation.clone());

    let candidates = [CandidateClaim {
        claim: &derived,
        predicate: derived.predicate.as_ref(),
        derivation: derived.derivation.as_ref(),
    }];

    let rooter = Rooter::new(&graph, &store, RootingConfig::default());
    let output = rooter.root_batch(&candidates).expect("root_batch");

    let verdict = output
        .verdict_for(derived.id)
        .expect("verdict for derived claim");
    assert_eq!(
        verdict.admission_tier,
        AdmissionTier::Rooted,
        "derived claim with matching AST predicate + shared parent entity must reach Rooted"
    );

    // A certificate must have been issued, and it must survive a
    // round-trip through CozoDB.
    thinkingroot_rooting::storage::insert_verdicts_batch(&graph, &output.verdicts).unwrap();
    thinkingroot_rooting::storage::insert_certificates_batch(&graph, &output.certificates)
        .unwrap();

    assert_eq!(output.certificates.len(), 1);
    let cert = &output.certificates[0];
    assert_eq!(cert.claim_id, derived.id.to_string());
    let fetched = graph.get_certificate_by_hash(&cert.hash).unwrap().unwrap();
    assert_eq!(fetched.0, derived.id.to_string());
}

#[test]
fn derived_claim_with_failing_ast_predicate_is_quarantined() {
    // Same setup as the previous test but with an AST query that targets
    // a function name that doesn't exist in the source. Topology probe
    // still passes (parents share an entity), but the predicate probe
    // fails — non-fatal, so the claim demotes to `Quarantined`.
    let dir = tempfile::tempdir().unwrap();
    let graph = thinkingroot_graph::graph::GraphStore::init(dir.path()).unwrap();
    let store = FileSystemSourceStore::new(dir.path()).unwrap();

    let source_body = r#"// AuthService module — exposes token validation
pub fn validate_token(t: &str) -> bool { true }
"#;
    let hash = ContentHash::from_bytes(source_body.as_bytes());
    let source = Source::new("file:///auth.rs".into(), SourceType::File).with_hash(hash.clone());
    graph.insert_source(&source).unwrap();
    store.put(source.id, &hash, source_body.as_bytes()).unwrap();

    let auth_entity = Entity::new("AuthService", EntityType::Service);
    graph.insert_entity(&auth_entity).unwrap();

    let parent_a = Claim::new(
        "AuthService exposes token validation",
        ClaimType::Fact,
        source.id,
        WorkspaceId::new(),
    );
    graph.insert_claim(&parent_a).unwrap();
    graph
        .link_claim_to_entity(&parent_a.id.to_string(), &auth_entity.id.to_string())
        .unwrap();

    // Query that won't match: no function named `deprecated_api`.
    let ast_pred = Predicate {
        language: PredicateLanguage::RustAst,
        query: r#"(function_item name: (identifier) @n (#eq? @n "deprecated_api"))"#.into(),
        scope: PredicateScope::empty(),
    };
    let derivation = DerivationProof {
        parent_claim_ids: vec![parent_a.id],
        derivation_rule: "reflect/test-v1".into(),
    };
    let derived = Claim::new(
        "AuthService exposes validate_token",
        ClaimType::Fact,
        source.id,
        WorkspaceId::new(),
    )
    .with_predicate(ast_pred)
    .with_derivation(derivation);

    let candidates = [CandidateClaim {
        claim: &derived,
        predicate: derived.predicate.as_ref(),
        derivation: derived.derivation.as_ref(),
    }];

    let rooter = Rooter::new(&graph, &store, RootingConfig::default());
    let output = rooter.root_batch(&candidates).expect("root_batch");
    let verdict = output.verdict_for(derived.id).unwrap();
    assert_eq!(
        verdict.admission_tier,
        AdmissionTier::Quarantined,
        "derived claim whose AST predicate doesn't match should demote to Quarantined"
    );
}
