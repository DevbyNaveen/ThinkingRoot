// crates/thinkingroot-branch/src/diff.rs
use std::collections::{HashMap, HashSet};

use chrono::Utc;
use thinkingroot_core::{
    AutoResolution, Claim, ClaimId, ClaimType, Confidence, ContradictionPair, DiffClaim,
    DiffEntity, DiffRelation, DiffStatus, KnowledgeDiff, PipelineVersion, Result, Sensitivity,
    SourceId, WorkspaceId, config::Config,
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
        if (a_l.contains(pos) && b_l.contains(neg)) || (a_l.contains(neg) && b_l.contains(pos)) {
            return true;
        }
    }
    false
}

/// Jaccard token similarity between two statements.
/// Returns a value in [0.0, 1.0].
fn jaccard_similarity(a: &str, b: &str) -> f64 {
    let tokens_a: HashSet<&str> = a.split_whitespace().collect();
    let tokens_b: HashSet<&str> = b.split_whitespace().collect();
    let intersection = tokens_a.intersection(&tokens_b).count();
    let union = tokens_a.union(&tokens_b).count();
    if union == 0 {
        return 0.0;
    }
    intersection as f64 / union as f64
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
        .map(|(_, stmt, _, _, _, _)| semantic_hash(stmt))
        .collect();

    // Identify new claims (branch claims not in main by semantic hash)
    let new_claim_rows: Vec<&(String, String, String, f64, String, f64)> = branch_claims_raw
        .iter()
        .filter(|(_, stmt, _, _, _, _)| !main_hashes.contains(&semantic_hash(stmt)))
        .collect();

    // Get entity context for new claims
    let new_claim_id_strs: Vec<&str> = new_claim_rows
        .iter()
        .map(|(id, _, _, _, _, _)| id.as_str())
        .collect();
    let entity_map: HashMap<String, Vec<String>> =
        branch_graph.get_entity_names_for_claims(&new_claim_id_strs)?;

    // Get real source IDs so merged claims are not orphaned in main.
    let claim_source_map = branch_graph.get_claim_source_id_map()?;

    // Check new claims for contradictions against main claims
    let mut new_claims: Vec<DiffClaim> = Vec::new();
    let mut auto_resolved: Vec<AutoResolution> = Vec::new();
    let mut needs_review: Vec<ContradictionPair> = Vec::new();

    for (id, statement, claim_type_str, confidence, _uri, _) in &new_claim_rows {
        let entity_context = entity_map.get(id.as_str()).cloned().unwrap_or_default();

        let mut contradiction_found = false;
        for (main_id, main_stmt, _, main_conf, _, _) in &main_claims_raw {
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
            // Use the real source ID from the branch graph so that when this claim
            // is merged into main, the source record can be copied over and the
            // claim won't be reported as orphaned.
            let real_source_id = claim_source_map
                .get(id.as_str())
                .and_then(|sid| sid.parse::<SourceId>().ok())
                .unwrap_or_else(SourceId::new);
            let claim = Claim {
                id: id.parse::<ClaimId>().unwrap_or_else(|_| ClaimId::new()),
                statement: statement.to_string(),
                claim_type: parse_claim_type(claim_type_str),
                source: real_source_id,
                source_span: None,
                confidence: Confidence::new(*confidence),
                valid_from: now,
                valid_until: None,
                sensitivity: Sensitivity::Public,
                workspace: WorkspaceId::new(),
                extracted_by: PipelineVersion::current(),
                superseded_by: None,
                created_at: now,
                grounding_score: None,
                grounding_method: None,
                extraction_tier: thinkingroot_core::types::ExtractionTier::default(),
                event_date: None,
                admission_tier: thinkingroot_core::types::AdmissionTier::default(),
                derivation: None,
                predicate: None,
                last_rooted_at: None,
            };
            new_claims.push(DiffClaim {
                claim,
                entity_context,
                diff_status: DiffStatus::Added,
            });
        }
    }

    // Second-pass contradiction detection via Jaccard token similarity.
    // Claims that share >60% token overlap but different semantic hashes and
    // share entity context are flagged as potential conflicts, even when the
    // negation-pair heuristic missed them.
    for (id, statement, _, confidence, _, _) in &new_claim_rows {
        let entity_context = entity_map.get(id.as_str()).cloned().unwrap_or_default();
        if entity_context.is_empty() {
            continue;
        }
        for (main_id, main_stmt, _, main_conf, _, _) in &main_claims_raw {
            // Skip pairs already caught by negation-pair pass.
            let already_flagged = auto_resolved
                .iter()
                .any(|r| &r.branch_claim_id == id && r.main_claim_id == *main_id)
                || needs_review
                    .iter()
                    .any(|p| &p.branch_claim_id == id && p.main_claim_id == *main_id);
            if already_flagged {
                continue;
            }
            // Only compare claims with overlapping entity context.
            let main_entities = entity_map
                .get(main_id.as_str())
                .cloned()
                .unwrap_or_default();
            let shared_entities = entity_context
                .iter()
                .filter(|e| main_entities.contains(e))
                .count();
            if shared_entities == 0 {
                continue;
            }
            let sim = jaccard_similarity(&statement.to_lowercase(), &main_stmt.to_lowercase());
            // High overlap but different hashes → potential conflict.
            if sim > 0.6 && semantic_hash(statement) != semantic_hash(main_stmt) {
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
                            "Jaccard similarity {:.2} > 0.60 with confidence delta {:.2} > threshold",
                            sim, delta
                        ),
                    });
                } else {
                    needs_review.push(ContradictionPair {
                        main_claim_id: main_id.clone(),
                        branch_claim_id: id.to_string(),
                        main_statement: main_stmt.clone(),
                        branch_statement: statement.to_string(),
                        explanation: format!(
                            "Potentially conflicting claims about the same subject (Jaccard={:.2}, confidence delta {:.2} below threshold)",
                            sim, delta
                        ),
                    });
                }
            }
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

    // Identify new relations (in branch, not in main by (from_name, to_name, rel_type) key).
    let main_relation_keys: HashSet<(String, String, String)> = main_graph
        .get_all_relations()?
        .into_iter()
        .map(|(from, to, rel, _, _, _)| (from, to, rel))
        .collect();

    let new_relations: Vec<DiffRelation> = branch_graph
        .get_all_relations()?
        .into_iter()
        .filter(|(from, to, rel, _, _, _)| {
            !main_relation_keys.contains(&(from.clone(), to.clone(), rel.clone()))
        })
        .map(
            |(from_name, to_name, relation_type, _, _, strength)| DiffRelation {
                from_name,
                to_name,
                relation_type,
                strength,
                diff_status: DiffStatus::Added,
            },
        )
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
        new_relations,
        auto_resolved,
        needs_review,
        health_before,
        health_after,
        merge_allowed: blocking_reasons.is_empty(),
        blocking_reasons,
    })
}
