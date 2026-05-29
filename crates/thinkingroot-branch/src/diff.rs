// crates/thinkingroot-branch/src/diff.rs
use std::collections::{HashMap, HashSet};
use std::path::Path;

use chrono::Utc;
use thinkingroot_core::{
    AutoResolution, Claim, ClaimId, ClaimType, Confidence, ConflictKind, ContradictionPair,
    DiffClaim, DiffEntity, DiffFunction, DiffRelation, DiffStatus, KnowledgeDiff, PipelineVersion,
    Result,
    Sensitivity, SourceId, WorkspaceId, config::Config,
};
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_graph::vector::VectorStore;
use thinkingroot_health::Verifier;

/// T1.1 — third-pass cosine threshold above which a candidate is treated
/// as a potential semantic contradiction.  0.75 is the same gate used by
/// the hybrid retrieval recall tier; well above the chance baseline for
/// 384-dim AllMiniLML6V2 embeddings (≈0.1) but below the deduplication
/// threshold (≈0.95) at which the negation/Jaccard passes already
/// classify rows as the same claim.
pub const VECTOR_CONTRADICTION_THRESHOLD: f32 = 0.75;

/// How many target neighbours to fetch per branch claim during the
/// vector contradiction pass.  Three is enough to surface the closest
/// semantic match plus the next two candidates in case the closest is
/// a duplicate caught by an earlier pass.
const VECTOR_CONTRADICTION_TOP_K: usize = 3;

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

/// Test-only re-export of the negation-pair predicate so unit tests
/// can pin the pre-condition that pass 1 misses a given pair before
/// asserting that the third (vector) pass catches it.  Keep the name
/// suffixed with `_for_test` so production callers stay on the public
/// `compute_diff_into` entry point.
#[doc(hidden)]
pub fn is_contradiction_pair_for_test(a: &str, b: &str) -> bool {
    is_contradiction_pair(a, b)
}

/// Test-only re-export of the Jaccard token-similarity helper.  Same
/// rationale as `is_contradiction_pair_for_test`.
#[doc(hidden)]
pub fn jaccard_similarity_for_test(a: &str, b: &str) -> f64 {
    jaccard_similarity(a, b)
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
    compute_diff_into(
        main_graph,
        branch_graph,
        from_branch,
        None,
        auto_resolve_threshold,
        max_health_drop,
        block_on_contradictions,
    )
}

/// Compute the semantic diff between a source branch and an explicit target branch.
pub fn compute_diff_into(
    target_graph: &GraphStore,
    source_graph: &GraphStore,
    from_branch: &str,
    target_branch: Option<&str>,
    auto_resolve_threshold: f64,
    max_health_drop: f64,
    block_on_contradictions: bool,
) -> Result<KnowledgeDiff> {
    let verifier = Verifier::new(&Config::default());
    let health_before = verifier.verify(target_graph)?.health_score;
    let health_after = verifier.verify(source_graph)?.health_score;

    // Load claims from both graphs
    let main_claims_raw = target_graph.get_all_claims_with_sources()?;
    let branch_claims_raw = source_graph.get_all_claims_with_sources()?;

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
        source_graph.get_entity_names_for_claims(&new_claim_id_strs)?;

    // Get real source IDs so merged claims are not orphaned in main.
    let claim_source_map = source_graph.get_claim_source_id_map()?;

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
                        // 2-way diff cannot classify the conflict
                        // shape — only `compute_three_way_diff` has
                        // the LCA needed to set this field.
                        conflict_kind: None,
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
                row_blake3: None,
                symbol: None,
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
                        // 2-way diff path; see compute_three_way_diff
                        // for LCA-aware classification.
                        conflict_kind: None,
                    });
                }
            }
        }
    }

    // Identify new entities (in branch, not in main by canonical name)
    let main_entity_names: HashSet<String> = target_graph
        .get_entities_with_aliases()?
        .into_iter()
        .map(|e| e.canonical_name.clone())
        .collect();

    let new_entities: Vec<DiffEntity> = source_graph
        .get_entities_with_aliases()?
        .into_iter()
        .filter(|e| !main_entity_names.contains(&e.canonical_name))
        .map(|e| DiffEntity {
            entity: e,
            diff_status: DiffStatus::Added,
        })
        .collect();

    // Identify new relations (in branch, not in main by (from_name, to_name, rel_type) key).
    let main_relation_keys: HashSet<(String, String, String)> = target_graph
        .get_all_relations()?
        .into_iter()
        .map(|(from, to, rel, _, _, _)| (from, to, rel))
        .collect();

    let new_relations: Vec<DiffRelation> = source_graph
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

    // New Root Functions: latest version present on the source branch but
    // absent (by name) on the target — so a branch-authored function can be
    // carried across the merge once it's verified.
    let target_fn_names: HashSet<String> = target_graph
        .list_functions()
        .unwrap_or_default()
        .into_iter()
        .map(|f| f.name)
        .collect();
    let new_functions: Vec<DiffFunction> = source_graph
        .list_functions()
        .unwrap_or_default()
        .into_iter()
        .filter(|f| !target_fn_names.contains(&f.name))
        .map(|f| DiffFunction {
            name: f.name,
            body: f.body,
            language: f.language,
            version: f.version,
            diff_status: DiffStatus::Added,
        })
        .collect();

    Ok(KnowledgeDiff {
        from_branch: from_branch.to_string(),
        to_branch: target_branch.unwrap_or("main").to_string(),
        computed_at: Utc::now(),
        new_claims,
        new_entities,
        new_relations,
        new_functions,
        auto_resolved,
        needs_review,
        health_before,
        health_after,
        merge_allowed: blocking_reasons.is_empty(),
        blocking_reasons,
    })
}

/// Compute the diff needed to rebase `branch_name` with claims from its parent.
pub fn compute_rebase_diff(
    branch_graph: &GraphStore,
    parent_graph: &GraphStore,
    branch_name: &str,
    parent_name: &str,
    auto_resolve_threshold: f64,
    max_health_drop: f64,
    block_on_contradictions: bool,
) -> Result<KnowledgeDiff> {
    compute_diff_into(
        branch_graph,
        parent_graph,
        parent_name,
        Some(branch_name),
        auto_resolve_threshold,
        max_health_drop,
        block_on_contradictions,
    )
}

/// T0.5 three-way merge with lowest common ancestor (LCA).
///
/// Two-way `compute_diff_into` cannot distinguish "added on theirs"
/// from "removed from ours" — it only sees what's in each graph at
/// merge time, not how each got there.  Three-way uses the LCA
/// (`base_graph` — the parent's `graph.db` snapshotted at fork time
/// and stored at `<branch>/graph/graph.db.parent-at-fork`) to
/// classify true conflicts:
///
/// | Case | Outcome |
/// |---|---|
/// | In `base` only | Removed-on-both — no-op |
/// | In `theirs` only | Add to target (clean) — handled by 2-way |
/// | In `ours` only | Keep in target (clean) — handled by 2-way |
/// | In `theirs` and `ours`, identical | No-op |
/// | In `theirs` and `ours`, different | **`ModifyModify` conflict** |
/// | In `base` and `theirs`, removed in `ours` | **`DeleteModify` conflict** |
///
/// Conflicts are pushed to `KnowledgeDiff::needs_review` with
/// `conflict_kind: Some(ConflictKind::*)` set; `merge_allowed` flips
/// to `false` when any conflict is recorded.  Auto-resolved /
/// non-conflicting deltas come straight from the underlying 2-way
/// diff so the existing semantic-hash + Jaccard contradiction
/// detection still applies.
///
/// Reference: `docs/branch-system-improvements.md` §T0.5 +
/// DSMCompare three-way semantic diff (Springer 2025).
pub fn compute_three_way_diff(
    base_graph: &GraphStore,
    target_graph: &GraphStore,
    source_graph: &GraphStore,
    from_branch: &str,
    target_branch: Option<&str>,
    auto_resolve_threshold: f64,
    max_health_drop: f64,
    block_on_contradictions: bool,
) -> Result<KnowledgeDiff> {
    // Start from the existing 2-way diff so all the auto-resolution,
    // Jaccard contradiction, and source-id mapping logic continues to
    // apply.  Three-way is purely additive — it tightens the
    // `needs_review` list with LCA-aware classifications.
    let mut diff = compute_diff_into(
        target_graph,
        source_graph,
        from_branch,
        target_branch,
        auto_resolve_threshold,
        max_health_drop,
        block_on_contradictions,
    )?;

    // Build (claim_id → statement) maps for all three graphs.  We
    // compare on claim_id (stable across branches when the claim was
    // forked from a common parent) and on statement text (the bit
    // each side might have edited).
    let base_map: HashMap<String, String> = base_graph
        .get_all_claims_with_sources()?
        .into_iter()
        .map(|(id, stmt, _, _, _, _)| (id, stmt))
        .collect();
    let ours_map: HashMap<String, String> = target_graph
        .get_all_claims_with_sources()?
        .into_iter()
        .map(|(id, stmt, _, _, _, _)| (id, stmt))
        .collect();
    let theirs_map: HashMap<String, String> = source_graph
        .get_all_claims_with_sources()?
        .into_iter()
        .map(|(id, stmt, _, _, _, _)| (id, stmt))
        .collect();

    let mut three_way_conflicts: Vec<ContradictionPair> = Vec::new();

    // Case 1: same id in both ours and theirs, statements differ →
    // both modified the same claim differently since the LCA.
    for (id, theirs_stmt) in &theirs_map {
        let Some(ours_stmt) = ours_map.get(id) else {
            continue;
        };
        if theirs_stmt == ours_stmt {
            continue;
        }
        let base_stmt = base_map.get(id);
        let kind = match base_stmt {
            // Both modified relative to a common base.  Even if one
            // side happens to match base — that's a no-op for that
            // side, but the OTHER side's change still needs to land,
            // and the standard 2-way diff already handles the clean
            // "only theirs changed" case, so skip when ours == base.
            Some(b) if b == ours_stmt => continue,
            // Symmetric: theirs is a no-op vs base; clean change on
            // ours alone — 2-way diff path handles it.
            Some(b) if b == theirs_stmt => continue,
            // Both differ from base — real ModifyModify conflict.
            Some(_) => ConflictKind::ModifyModify,
            // No base entry but same id in both — defensive case.
            // Treat as AddAdd so an operator can audit.
            None => ConflictKind::AddAdd,
        };
        three_way_conflicts.push(ContradictionPair {
            main_claim_id: id.clone(),
            branch_claim_id: id.clone(),
            main_statement: ours_stmt.clone(),
            branch_statement: theirs_stmt.clone(),
            explanation: format!(
                "Three-way {kind:?} conflict on claim {id}: both sides diverged \
                 from the lowest common ancestor"
            ),
            conflict_kind: Some(kind),
        });
    }

    // Case 2: id present in base AND theirs, but missing from ours →
    // ours deleted something theirs modified (or kept).
    for (id, theirs_stmt) in &theirs_map {
        if !base_map.contains_key(id) {
            continue;
        }
        if ours_map.contains_key(id) {
            continue;
        }
        // Skip when theirs is a no-op vs base — clean delete on ours,
        // nothing to merge.
        if base_map.get(id) == Some(theirs_stmt) {
            continue;
        }
        let base_stmt = base_map.get(id).cloned().unwrap_or_default();
        three_way_conflicts.push(ContradictionPair {
            main_claim_id: id.clone(),
            branch_claim_id: id.clone(),
            main_statement: format!("(deleted in ours; was: {base_stmt})"),
            branch_statement: theirs_stmt.clone(),
            explanation: format!(
                "Three-way DeleteModify conflict on claim {id}: ours deleted \
                 what theirs modified relative to the LCA"
            ),
            conflict_kind: Some(ConflictKind::DeleteModify),
        });
    }

    if !three_way_conflicts.is_empty() {
        let count = three_way_conflicts.len();
        diff.needs_review.extend(three_way_conflicts);
        diff.merge_allowed = false;
        diff.blocking_reasons.push(format!(
            "{count} three-way conflict(s) require resolution before merge \
             (see needs_review entries with conflict_kind set)"
        ));
    }

    Ok(diff)
}

/// T1.1 — vector-embedding contradiction pass.
///
/// The third detection pass on top of the negation-pair pass (`is`/`is
/// not`, …) and the Jaccard-token-similarity pass.  Catches semantic
/// conflicts where:
///
/// - cosine similarity between branch and target embedding > 0.75,
/// - the pair shares at least one entity context,
/// - the pair has different semantic hashes (i.e. the rows survived the
///   pass-0 dedup), AND
/// - neither pass 1 (negation) nor pass 2 (Jaccard) already flagged it.
///
/// Examples this pass catches that the earlier passes miss:
///
/// | Target claim                  | Branch claim                      | Why earlier passes miss      |
/// |-------------------------------|-----------------------------------|------------------------------|
/// | "uses JWT for authentication" | "migrated to OAuth2"              | low Jaccard, no negation kw  |
/// | "scales horizontally"         | "vertically scaled deployment"    | distinct vocabulary          |
/// | "stores tokens server-side"   | "tokens persisted to localStorage"| paraphrase, no antonym pair  |
///
/// `branch_claims` and `target_claims` carry the same shape returned by
/// `GraphStore::get_all_claims_with_sources` — `(id, statement,
/// claim_type, confidence, uri, _)`.  The function is sync because it
/// expects already-opened vector stores; the async I/O sits in the
/// `compute_diff_into_with_vector_dirs` wrapper below.
///
/// Returns `Ok(count)` where `count` is the number of new conflicts
/// added to `diff.needs_review` / `diff.auto_resolved`.  When `count >
/// 0` and `block_on_contradictions` was true at diff time, the caller
/// is responsible for re-checking `needs_review` and flipping
/// `merge_allowed` — the function intentionally does not mutate that
/// gate so the pass composes cleanly with the existing two-way and
/// three-way diff outputs.
pub fn apply_vector_contradiction_pass(
    diff: &mut KnowledgeDiff,
    target_vec: &VectorStore,
    source_vec: &VectorStore,
    target_claims: &[(String, String, String, f64, String, f64)],
    branch_claims: &[(String, String, String, f64, String, f64)],
    target_entity_map: &HashMap<String, Vec<String>>,
    branch_entity_map: &HashMap<String, Vec<String>>,
    auto_resolve_threshold: f64,
    cosine_threshold: f32,
) -> Result<usize> {
    if target_vec.is_empty() || source_vec.is_empty() {
        // Either side hasn't been embedded yet — happens on fresh
        // workspaces and during tests that haven't pre-seeded vectors.
        // Nothing to compare against; skip silently.
        return Ok(0);
    }

    // Index target rows by id for O(1) statement / confidence lookup.
    let target_by_id: HashMap<&str, (&str, f64)> = target_claims
        .iter()
        .map(|(id, stmt, _, conf, _, _)| (id.as_str(), (stmt.as_str(), *conf)))
        .collect();

    // Build the set of (branch_id, target_id) pairs already flagged by
    // earlier passes so the third pass never duplicates them.  Both
    // auto_resolved and needs_review are checked because pass 1+2 may
    // have classified the same logical pair into either bucket.
    let mut already_flagged: HashSet<(String, String)> = HashSet::new();
    for r in &diff.auto_resolved {
        already_flagged.insert((r.branch_claim_id.clone(), r.main_claim_id.clone()));
    }
    for p in &diff.needs_review {
        already_flagged.insert((p.branch_claim_id.clone(), p.main_claim_id.clone()));
    }

    let mut added = 0usize;

    for (branch_id, statement, _, confidence, _, _) in branch_claims {
        // Only run the pass on rows that the two-way diff classified as
        // genuinely new.  Rows that already match a target claim by
        // semantic hash are deduped at pass 0 and never reach the
        // contradiction passes.
        let is_new = diff
            .new_claims
            .iter()
            .any(|c| c.claim.id.to_string() == *branch_id);
        if !is_new {
            continue;
        }

        let branch_entities = branch_entity_map
            .get(branch_id.as_str())
            .cloned()
            .unwrap_or_default();
        if branch_entities.is_empty() {
            // Without entity context we cannot scope the search; the
            // global vector neighbourhood is too noisy for a useful
            // contradiction signal.
            continue;
        }

        // Reuse the embedding stored in the source vector store rather
        // than re-embedding the same text — saves an `ensure_model()`
        // round trip per row.  Source rows that lack an embedding (e.g.
        // claims contributed before the index was rebuilt) are skipped.
        let Some(query_vec) = source_vec.get_embedding(branch_id.as_str()) else {
            continue;
        };

        let neighbours = target_vec.search_by_vector(query_vec, VECTOR_CONTRADICTION_TOP_K);
        for (target_id, _, sim) in neighbours {
            if sim < cosine_threshold {
                continue;
            }
            // Skip already-flagged pairs (negation or Jaccard caught it).
            if already_flagged.contains(&(branch_id.clone(), target_id.clone())) {
                continue;
            }
            // Look up the target claim — the search hit's id is the
            // canonical claim id; if the target graph has been mutated
            // since the index was built, drop the row rather than
            // surface a stale conflict.
            let Some((target_stmt, target_conf)) = target_by_id.get(target_id.as_str()) else {
                continue;
            };
            // Same semantic hash means the negation/Jaccard passes
            // already deduped this pair; do not double-flag.
            if semantic_hash(statement) == semantic_hash(target_stmt) {
                continue;
            }
            // Require shared entity context — without it, two unrelated
            // claims that happen to share embedding-space proximity
            // would generate noise.
            let target_entities = target_entity_map
                .get(target_id.as_str())
                .cloned()
                .unwrap_or_default();
            let shared = branch_entities
                .iter()
                .filter(|e| target_entities.contains(e))
                .count();
            if shared == 0 {
                continue;
            }

            let delta = (confidence - target_conf).abs();
            if delta > auto_resolve_threshold {
                let winner = if confidence > target_conf {
                    branch_id.clone()
                } else {
                    target_id.clone()
                };
                diff.auto_resolved.push(AutoResolution {
                    main_claim_id: target_id.clone(),
                    branch_claim_id: branch_id.clone(),
                    winner,
                    confidence_delta: delta,
                    reason: format!(
                        "Vector cosine {sim:.2} > {cosine_threshold:.2} with confidence \
                         delta {delta:.2} > threshold {auto_resolve_threshold:.2}"
                    ),
                });
            } else {
                diff.needs_review.push(ContradictionPair {
                    main_claim_id: target_id.clone(),
                    branch_claim_id: branch_id.clone(),
                    main_statement: target_stmt.to_string(),
                    branch_statement: statement.clone(),
                    explanation: format!(
                        "Semantic contradiction by embedding (cosine {sim:.2} > \
                         {cosine_threshold:.2}, confidence delta {delta:.2} below \
                         auto-resolution threshold {auto_resolve_threshold:.2})"
                    ),
                    // 2-way path; LCA-aware classification stays the
                    // domain of `compute_three_way_diff`.
                    conflict_kind: None,
                });
            }
            already_flagged.insert((branch_id.clone(), target_id.clone()));
            added += 1;
            // One conflict per branch claim is enough to stop the merge
            // and surface the issue; subsequent neighbours rarely add
            // signal beyond noise.
            break;
        }
    }

    Ok(added)
}

/// Async wrapper around [`compute_diff_into`] that opens the per-branch
/// vector stores and runs [`apply_vector_contradiction_pass`] as a
/// third detection pass.  Used by `merge_into` in this crate's `lib.rs`.
///
/// Skips the pass entirely when either `vectors.bin` is missing — fresh
/// workspaces and tests that disable embeddings keep the existing
/// two-pass behaviour.
pub async fn compute_diff_into_with_vector_dirs(
    target_graph: &GraphStore,
    source_graph: &GraphStore,
    target_data_dir: &Path,
    source_data_dir: &Path,
    from_branch: &str,
    target_branch: Option<&str>,
    auto_resolve_threshold: f64,
    max_health_drop: f64,
    block_on_contradictions: bool,
) -> Result<KnowledgeDiff> {
    let mut diff = compute_diff_into(
        target_graph,
        source_graph,
        from_branch,
        target_branch,
        auto_resolve_threshold,
        max_health_drop,
        block_on_contradictions,
    )?;

    if !target_data_dir.join("vectors.bin").exists()
        || !source_data_dir.join("vectors.bin").exists()
    {
        return Ok(diff);
    }

    let target_vec = VectorStore::init(target_data_dir).await?;
    let source_vec = VectorStore::init(source_data_dir).await?;

    let target_claims = target_graph.get_all_claims_with_sources()?;
    let branch_claims = source_graph.get_all_claims_with_sources()?;

    let branch_ids: Vec<&str> = branch_claims.iter().map(|(id, ..)| id.as_str()).collect();
    let target_ids: Vec<&str> = target_claims.iter().map(|(id, ..)| id.as_str()).collect();
    let branch_entity_map = source_graph.get_entity_names_for_claims(&branch_ids)?;
    let target_entity_map = target_graph.get_entity_names_for_claims(&target_ids)?;

    let added = apply_vector_contradiction_pass(
        &mut diff,
        &target_vec,
        &source_vec,
        &target_claims,
        &branch_claims,
        &target_entity_map,
        &branch_entity_map,
        auto_resolve_threshold,
        VECTOR_CONTRADICTION_THRESHOLD,
    )?;

    if added > 0 && block_on_contradictions {
        // Refresh the merge gate — the pass intentionally does not
        // mutate `merge_allowed` so it composes with two-way + three-way
        // outputs.  Re-evaluate based on the now-extended needs_review.
        if !diff.needs_review.is_empty() {
            diff.merge_allowed = false;
            // Avoid duplicating an existing reason if the earlier diff
            // already added one for unresolved contradictions.
            let already_blocked = diff
                .blocking_reasons
                .iter()
                .any(|r| r.contains("contradiction"));
            if !already_blocked {
                diff.blocking_reasons.push(format!(
                    "{} unresolved contradiction(s) require review",
                    diff.needs_review.len()
                ));
            }
        }
    }

    Ok(diff)
}
