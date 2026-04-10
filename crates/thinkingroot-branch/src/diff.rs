// crates/thinkingroot-branch/src/diff.rs
use std::collections::{HashMap, HashSet};

use chrono::Utc;
use thinkingroot_core::{
    config::Config, AutoResolution, Claim, ClaimId, ClaimType, Confidence, ContradictionPair,
    DiffClaim, DiffEntity, DiffStatus, KnowledgeDiff, PipelineVersion, Result, Sensitivity,
    SourceId, WorkspaceId,
};
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_verify::Verifier;

/// Compute a BLAKE3 hash of a normalised claim statement.
/// Normalisation: lowercase + collapse whitespace.
/// Same fact extracted twice with minor formatting differences → same hash.
pub fn semantic_hash(statement: &str) -> String {
    let normalised: String = statement
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let hash = blake3::hash(normalised.as_bytes());
    hash.to_hex().to_string()
}

/// Negation keyword pairs for contradiction-as-conflict detection.
const NEGATION_PAIRS: &[(&str, &str)] = &[
    ("is", "is not"),
    ("uses", "does not use"),
    ("supports", "does not support"),
    ("requires", "does not require"),
    ("implements", "does not implement"),
    ("depends on", "does not depend on"),
    ("has", "does not have"),
    ("can", "cannot"),
    ("should", "should not"),
    ("must", "must not"),
];

fn is_contradiction_pair(a: &str, b: &str) -> bool {
    let a_l = a.to_lowercase();
    let b_l = b.to_lowercase();
    for (pos, neg) in NEGATION_PAIRS {
        if (a_l.contains(pos) && b_l.contains(neg))
            || (a_l.contains(neg) && b_l.contains(pos))
        {
            return true;
        }
    }
    false
}

fn parse_claim_type(s: &str) -> ClaimType {
    match s {
        "Decision" => ClaimType::Decision,
        "Opinion" => ClaimType::Opinion,
        "Plan" => ClaimType::Plan,
        "Requirement" => ClaimType::Requirement,
        "Metric" => ClaimType::Metric,
        "Definition" => ClaimType::Definition,
        "Dependency" => ClaimType::Dependency,
        "ApiSignature" => ClaimType::ApiSignature,
        "Architecture" => ClaimType::Architecture,
        _ => ClaimType::Fact,
    }
}

/// Compute the semantic diff between main and a branch.
///
/// Returns a `KnowledgeDiff` describing:
/// - `new_claims`: claims in branch not in main (by semantic hash)
/// - `auto_resolved`: contradiction pairs where confidence delta > threshold
/// - `needs_review`: contradiction pairs below threshold
/// - `new_entities`: entities in branch not in main
/// - `health_before` / `health_after` with `merge_allowed` gate
pub fn compute_diff(
    main_graph: &GraphStore,
    branch_graph: &GraphStore,
    from_branch: &str,
    auto_resolve_threshold: f64,
    max_health_drop: f64,
    block_on_contradictions: bool,
) -> Result<KnowledgeDiff> {
    let verifier = Verifier::new(&Config::default());
    let health_before = verifier.verify(main_graph)?.health_score;
    let health_after = verifier.verify(branch_graph)?.health_score;

    // Load claims from both graphs
    let main_claims_raw = main_graph.get_all_claims_with_sources()?;
    let branch_claims_raw = branch_graph.get_all_claims_with_sources()?;

    // Build main hash set for deduplication
    let main_hashes: HashSet<String> = main_claims_raw
        .iter()
        .map(|(_, stmt, _, _, _)| semantic_hash(stmt))
        .collect();

    // Identify new claims (branch claims not in main by semantic hash)
    let new_claim_rows: Vec<&(String, String, String, f64, String)> = branch_claims_raw
        .iter()
        .filter(|(_, stmt, _, _, _)| !main_hashes.contains(&semantic_hash(stmt)))
        .collect();

    // Get entity context for new claims
    let new_claim_id_strs: Vec<&str> = new_claim_rows
        .iter()
        .map(|(id, _, _, _, _)| id.as_str())
        .collect();
    let entity_map: HashMap<String, Vec<String>> =
        branch_graph.get_entity_names_for_claims(&new_claim_id_strs)?;

    // Check new claims for contradictions against main claims
    let mut new_claims: Vec<DiffClaim> = Vec::new();
    let mut auto_resolved: Vec<AutoResolution> = Vec::new();
    let mut needs_review: Vec<ContradictionPair> = Vec::new();

    for (id, statement, claim_type_str, confidence, _uri) in &new_claim_rows {
        let entity_context = entity_map.get(id.as_str()).cloned().unwrap_or_default();

        let mut contradiction_found = false;
        for (main_id, main_stmt, _, main_conf, _) in &main_claims_raw {
            if is_contradiction_pair(statement, main_stmt) {
                contradiction_found = true;
                let delta = (confidence - main_conf).abs();
                if delta > auto_resolve_threshold {
                    let winner = if confidence > main_conf {
                        id.to_string()
                    } else {
                        main_id.clone()
                    };
                    auto_resolved.push(AutoResolution {
                        main_claim_id: main_id.clone(),
                        branch_claim_id: id.to_string(),
                        winner,
                        confidence_delta: delta,
                        reason: format!(
                            "Confidence delta {:.2} > threshold {:.2}",
                            delta, auto_resolve_threshold
                        ),
                    });
                } else {
                    needs_review.push(ContradictionPair {
                        main_claim_id: main_id.clone(),
                        branch_claim_id: id.to_string(),
                        main_statement: main_stmt.clone(),
                        branch_statement: statement.to_string(),
                        explanation: format!(
                            "Contradiction: '{}' vs '{}' (confidence delta {:.2} below threshold)",
                            main_stmt, statement, delta
                        ),
                    });
                }
                break;
            }
        }

        if !contradiction_found {
            let now = Utc::now();
            let claim = Claim {
                id: id.parse::<ClaimId>().unwrap_or_else(|_| ClaimId::new()),
                statement: statement.to_string(),
                claim_type: parse_claim_type(claim_type_str),
                source: SourceId::new(), // placeholder — URI is not a SourceId ULID
                source_span: None,
                confidence: Confidence::new(*confidence),
                valid_from: now,
                valid_until: None,
                sensitivity: Sensitivity::Public,
                workspace: WorkspaceId::new(),
                extracted_by: PipelineVersion::current(),
                superseded_by: None,
                created_at: now,
            };
            new_claims.push(DiffClaim {
                claim,
                entity_context,
                diff_status: DiffStatus::Added,
            });
        }
    }

    // Identify new entities (in branch, not in main by canonical name)
    let main_entity_names: HashSet<String> = main_graph
        .get_entities_with_aliases()?
        .into_iter()
        .map(|e| e.canonical_name.clone())
        .collect();

    let new_entities: Vec<DiffEntity> = branch_graph
        .get_entities_with_aliases()?
        .into_iter()
        .filter(|e| !main_entity_names.contains(&e.canonical_name))
        .map(|e| DiffEntity {
            entity: e,
            diff_status: DiffStatus::Added,
        })
        .collect();

    // Determine merge_allowed
    let health_drop = health_before.overall - health_after.overall;
    let mut blocking_reasons: Vec<String> = Vec::new();

    if health_drop > max_health_drop {
        blocking_reasons.push(format!(
            "Health drop {:.1}% exceeds maximum {:.1}%",
            health_drop * 100.0,
            max_health_drop * 100.0
        ));
    }
    if block_on_contradictions && !needs_review.is_empty() {
        blocking_reasons.push(format!(
            "{} unresolved contradiction(s) require review",
            needs_review.len()
        ));
    }

    Ok(KnowledgeDiff {
        from_branch: from_branch.to_string(),
        to_branch: "main".to_string(),
        computed_at: Utc::now(),
        new_claims,
        new_entities,
        new_relations: vec![],
        auto_resolved,
        needs_review,
        health_before,
        health_after,
        merge_allowed: blocking_reasons.is_empty(),
        blocking_reasons,
    })
}
