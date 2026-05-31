//! Hybrid Retrieval — 9-layer pipeline orchestrator.
//!
//! Spec: `docs/2026-05-02-hybrid-retrieval-spec.md` §3.1 (layer diagram),
//! §3.2 (RetrievalHit), §4 (planner), §5 (score fusion), §6
//! (provenance verification), §7 (byte-span stitching), §8 (sensitivity
//! filter), §11 (composition with AEP).
//!
//! Single async fn `hybrid_retrieve` owns one cloned `GraphStore` for the
//! whole call; concurrent reads serialise on Cozo's internal SQLite mutex
//! rather than on the outer `Arc<Mutex<StorageEngine>>`. Cancellation is
//! checked at every layer boundary.
//!
//! **Phase 4 Witness Mesh transition (2026-05-14).** Per
//! `.claude/rules/hybrid-retrieval.md` "Witness Mesh transition":
//! `hybrid_retrieve` still scores legacy `claims`, not `witnesses` —
//! the 11-component fusion joins through `admission_tier` /
//! `trial_scores` / `claim_entity_edges` / `claim_temporal` /
//! `contradictions` / `code_signatures` / `code_metrics` /
//! `git_blame` / `quantities` / `events`, none of which reference
//! the `witnesses` table today. The Commit-2 cutover retargets BM25
//! onto `witness_type + content_blake3 + spans_json` and recall
//! onto Witness span text materialised from `source.tar.zst` — that
//! work belongs in a follow-up that also adds an `engine.search_scoped`
//! variant that knows how to read span text at index time. Until
//! then, witness-only workspaces see empty hybrid retrieval; this is
//! honest behaviour, not a bug.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

use chrono::{DateTime, Utc};
use cozo::{DataValue, NamedRows, Num};
use thinkingroot_core::types::{AdmissionTier, GroundingMethod, Sensitivity, TrustLevel};
use thinkingroot_core::{Error, Result};
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_graph::hybrid_queries::{
    build_in_heading_path, dv_str_list, run_hybrid, Q_HR_AUTHORED_AFTER, Q_HR_AUTHORED_BY,
    Q_HR_CLAIM_TYPE, Q_HR_ENTITY_NAME, Q_HR_ENTITY_TYPE, Q_HR_HAS_DOC_TAG_ANY_TARGET,
    Q_HR_HAS_DOC_TAG_WITH_TARGET, Q_HR_HAS_MARKER, Q_HR_IN_CALL_GRAPH_OF, Q_HR_QUANTITY_RANGE,
    Q_HR_REFERENCED_BY, Q_HR_SOURCE_TRUST_AT_LEAST, Q_HR_SUPERSEDES_CLAIM,
};
use thinkingroot_graph::SourceByteStore;
use tokio_util::sync::CancellationToken;

use crate::engine::{
    CallEdge, CodeMarkerRef, CodeMetricRef, ContradictionRef, DocTagRef, EventTriple, GitBlameRef,
    KnownUnknown, QuantityRef, QueryEngine, SourceByteSpan, TestAnnotationRef, TrialScores,
};

use super::byte_span::{coalesce, DEFAULT_MAX_GAP_BYTES};
use super::dsl;
use super::hybrid_types::{
    ByteSpan, ByteSpanBundle, CodeSignatureRef, HybridResponse, RetrievalCaveat, RetrievalHit,
    RetrievalRequest, RoutingShape, ScoreBreakdown, ScoringProfile, TypedPredicate,
};

// ===========================================================================
// Public entry point
// ===========================================================================

/// Heuristic "is this a human-readable statement, not binary garbage?" guard.
/// A bad ingest (e.g. a PDF materialised from raw source bytes) can persist
/// claims whose `statement` is FlateDecode/binary noise; those must never
/// surface as recall hits. Cheap + deterministic: reject empty, reject obvious
/// PDF/stream markers, and reject low printable-character ratio.
pub(crate) fn is_probably_text(s: &str) -> bool {
    let t = s.trim();
    if t.is_empty() {
        return false;
    }
    // Obvious binary/PDF object markers.
    if t.contains("FlateDecode") || t.contains("endstream") || t.contains("/Filter") {
        return false;
    }
    if t.starts_with("<<") && t.contains('/') {
        return false;
    }
    // Printable ratio: letters/digits/punct/space are fine; control chars and
    // replacement/garbage chars are not. Newlines/tabs count as printable.
    let total = t.chars().count();
    let printable = t
        .chars()
        .filter(|c| *c == '\n' || *c == '\t' || *c == '\r' || (!c.is_control() && *c != '\u{FFFD}'))
        .count();
    (printable as f64 / total as f64) >= 0.85
}

/// Run the 9-layer Hybrid Retrieval pipeline. Acquires `GraphStore` and
/// `SourceByteStore` once, holds them for the duration of the call, and
/// releases the workspace mutex before any Datalog work runs.
///
/// `cancel`: when set, every layer boundary checks the token and returns
/// `Error::Cancelled` on trip. SSE/REST callers bind it to their
/// response-drop guard so client disconnect aborts the call cleanly.
pub async fn hybrid_retrieve(
    engine: &QueryEngine,
    ws: &str,
    mut req: RetrievalRequest,
    cancel: Option<CancellationToken>,
) -> Result<HybridResponse> {
    let start = Instant::now();
    let now_dt = req.now.unwrap_or_else(Utc::now);
    req.now = Some(now_dt);

    let graph = engine
        .graph_store(ws)
        .await
        .ok_or_else(|| Error::GraphStorage(format!("workspace not mounted: {ws}")))?;
    let byte_store = engine.byte_store(ws);

    // ---- Layer 1: parse + DSL fold ----
    let parsed = parse_query(&req)?;
    check_cancel(&cancel)?;

    // ---- Layer 1.5: candidate count preflight ----
    let candidate_count = preflight_count(&graph)?;
    check_cancel(&cancel)?;

    // ---- Layer 2: planner ----
    let shape = plan_routing(&parsed, candidate_count, &req.scoring_profile);
    check_cancel(&cancel)?;

    // ---- Layer 3: vector recall + datalog filters ----
    let (vector_hits, datalog_ids) = run_recall(engine, ws, &graph, &req, &parsed, shape).await?;
    check_cancel(&cancel)?;

    // ---- Layer 4: candidate merger ----
    let merged = merge_candidates(vector_hits, datalog_ids, shape, req.top_k * 2, &req);
    check_cancel(&cancel)?;

    // ---- Layer 5: structural enricher ----
    let enriched = enrich_candidates(&graph, merged, &req)?;
    check_cancel(&cancel)?;

    // ---- Layer 6: score fusion ----
    let mut quarantined_dropped: u32 = 0;
    let mut scored: Vec<(EnrichedCandidate, f32, ScoreBreakdown)> = Vec::with_capacity(enriched.len());
    for c in enriched {
        if !req.include_quarantined && c.admission_tier == AdmissionTier::Quarantined {
            quarantined_dropped = quarantined_dropped.saturating_add(1);
            continue;
        }
        if c.admission_tier == AdmissionTier::Rejected {
            continue;
        }
        if let Some((s, b)) = fuse_score(&c, &req.scoring_profile, now_dt, &req) {
            scored.push((c, s, b));
        }
    }
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // ---- Layer 6.5: SOTA cross-encoder rerank (opt-in, lever 1) ----
    //
    // Re-orders the top `req.top_k * 2` survivors of fuse_score by
    // cross-encoder relevance, then blends with the fused score so callers
    // who tune via `cross_encoder_weight` get smooth interpolation between
    // pure 11-component scoring and pure CE scoring.
    //
    // Skip-conditions (any one trips the bypass):
    //   - `use_cross_encoder = false` (set by `ScoringProfile::instant()`
    //     for typeahead; `Default::default()` enables it post-Track-32)
    //   - workspace is structural-only (CE adds no signal over BM25 there)
    //   - `scored.len() < 2` (rerank of a single hit is a no-op)
    //
    // The model is loaded on first call only; ~300 MB gte-modernbert
    // ONNX bundle is staged at install time by install.sh / install.ps1
    // under `<dirs::cache_dir>/thinkingroot/models/rerank.{onnx,tokenizer.json}`.
    // No HF-Hub fetch at runtime (Track 32, 2026-05-16).
    if req.scoring_profile.use_cross_encoder && scored.len() >= 2 {
        let pool_size = (req.top_k * 2).max(req.top_k).min(scored.len());
        let pool = &scored[..pool_size];
        let docs: Vec<&str> = pool.iter().map(|(c, _, _)| c.statement.as_str()).collect();
        let workspace_path = engine.workspace_root_path(ws).unwrap_or_else(|| {
            // Last-resort fallback: use the system temp dir so model cache
            // can still land somewhere stable across the process lifetime.
            // Workspace-mounted callers (the typical path) hit the success
            // branch above and never see this.
            std::env::temp_dir()
        });
        match thinkingroot_graph::rerank::CrossEncoder::new(&workspace_path) {
            Ok(reranker) => match reranker.rerank(&req.query_text, &docs) {
                Ok(ce_scores) if !ce_scores.is_empty() => {
                    // Min-max normalise CE scores to [0,1] so the blend with the
                    // 11-component fused score (also in [0,1] after fusion's
                    // own normalisation) is dimensionally honest.
                    let (mn, mx) = ce_scores.iter().fold(
                        (f32::INFINITY, f32::NEG_INFINITY),
                        |(mn, mx), s| (mn.min(*s), mx.max(*s)),
                    );
                    let range = (mx - mn).max(1e-6);
                    let w = req
                        .scoring_profile
                        .cross_encoder_weight
                        .clamp(0.0, 1.0);
                    let mut blended: Vec<(EnrichedCandidate, f32, ScoreBreakdown)> =
                        Vec::with_capacity(pool_size);
                    for (i, ce_raw) in ce_scores.iter().enumerate() {
                        let (cand, fused, mut breakdown) = scored[i].clone();
                        let ce_norm = (ce_raw - mn) / range;
                        let new_score = w * ce_norm + (1.0 - w) * fused;
                        breakdown.cross_encoder = Some(ce_norm);
                        blended.push((cand, new_score, breakdown));
                    }
                    // Replace the pool prefix in `scored`, leaving the tail
                    // (rank > pool_size) untouched — the tail is dropped
                    // by `truncate` below anyway, but preserving it would
                    // matter if a future change raises pool_size dynamically.
                    for (i, item) in blended.into_iter().enumerate() {
                        scored[i] = item;
                    }
                    scored[..pool_size].sort_by(|a, b| {
                        b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
                    });
                }
                Ok(_) => {
                    tracing::warn!(
                        target: "rerank",
                        "cross-encoder returned empty scores; using fuse_score order"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        target: "rerank",
                        error = %e,
                        "cross-encoder rerank failed; using fuse_score order"
                    );
                }
            },
            Err(e) => {
                tracing::warn!(
                    target: "rerank",
                    error = %e,
                    "cross-encoder construct failed; using fuse_score order"
                );
            }
        }
    }

    scored.truncate(req.top_k);
    check_cancel(&cancel)?;

    // ---- Layers 7+8+9: stitch, verify, filter ----
    let mut hits: Vec<RetrievalHit> = Vec::with_capacity(scored.len());
    let mut blake_cache: HashMap<(String, u64, u64), bool> = HashMap::new();
    let mut redactions: Vec<RetrievalCaveat> = Vec::new();
    if quarantined_dropped > 0 {
        redactions.push(RetrievalCaveat::DroppedQuarantined {
            count: quarantined_dropped,
        });
    }

    let mut junk_dropped = 0usize;
    for (c, fused, breakdown) in scored {
        // Sensitivity gate (Layer 9 — applied per-hit so we accumulate
        // redactions even when later layers also drop the hit).
        if !req.clearance.contains(&c.sensitivity) {
            redactions.push(RetrievalCaveat::SensitivityRedaction {
                hidden_field: format!("claim:{}", c.claim_id),
                required_clearance: c.sensitivity,
            });
            continue;
        }

        // Junk gate — a statement that materialised to non-text (binary/PDF
        // bytes from a bad ingest) must never surface as a recall hit. This is
        // a defensive filter so existing index pollution is invisible without a
        // destructive purge; the extraction-side guard stops new junk at ingest.
        if !is_probably_text(&c.statement) {
            junk_dropped += 1;
            continue;
        }

        // Layer 7: stitch byte spans
        let bundle = stitch_byte_spans(&c);

        // Layer 8: provenance verifier (eager, top-K only). When byte_store
        // is missing (legacy workspaces) we skip silently and emit a single
        // `BytesUnavailable` caveat per hit instead of erroring.
        let mut hit_caveats: Vec<RetrievalCaveat> = Vec::new();
        let stale = match byte_store.as_deref() {
            Some(store) => verify_provenance(store, &c, &mut blake_cache, &mut hit_caveats),
            None => {
                hit_caveats.push(RetrievalCaveat::BytesUnavailable {
                    source_id: c.primary_source_id.clone(),
                    reason: "byte_store_unavailable".into(),
                });
                false
            }
        };
        if stale && req.require_provenance_verified {
            continue;
        }

        // Cross-table caveats (UnresolvedContradiction, SupersededByNewerClaim,
        // DerivedFromTest, GapAdjacent, LowConfidence) come from the enriched
        // candidate's joined rows.
        for ctr in &c.cluster_contradictions {
            hit_caveats.push(RetrievalCaveat::UnresolvedContradiction {
                with_claim_id: if ctr.claim_a == c.claim_id {
                    ctr.claim_b.clone()
                } else {
                    ctr.claim_a.clone()
                },
                explanation: ctr.explanation.clone(),
            });
        }
        if let Some(succ) = c.superseded_by_chain.last() {
            if succ != &c.claim_id {
                hit_caveats.push(RetrievalCaveat::SupersededByNewerClaim {
                    successor_id: succ.clone(),
                });
            }
        }
        if let Some(t) = &c.test_origin {
            hit_caveats.push(RetrievalCaveat::DerivedFromTest {
                framework: t.framework.clone(),
            });
        }
        for gap in &c.cluster_gaps {
            hit_caveats.push(RetrievalCaveat::GapAdjacent {
                gap_id: gap.gap_id.clone(),
                expected_claim_type: gap.expected_claim_type.clone(),
            });
        }

        // Build the hit
        hits.push(build_hit(c, fused, breakdown, bundle, hit_caveats));
    }
    if junk_dropped > 0 {
        tracing::debug!(
            junk_dropped,
            "hybrid_retrieve: filtered non-text (binary/PDF) claims from results"
        );
    }

    let elapsed_ms = start.elapsed().as_secs_f32() * 1000.0;
    Ok(HybridResponse {
        hits,
        redactions,
        routing_shape: shape,
        elapsed_ms,
    })
}

fn check_cancel(token: &Option<CancellationToken>) -> Result<()> {
    if let Some(t) = token {
        if t.is_cancelled() {
            return Err(Error::Cancelled);
        }
    }
    Ok(())
}

// ===========================================================================
// Layer 1 — QueryParser
// ===========================================================================

#[derive(Debug, Clone)]
struct ParsedQuery {
    query_text: String,
    predicates: Vec<TypedPredicate>,
}

fn parse_query(req: &RetrievalRequest) -> Result<ParsedQuery> {
    // Optional inline DSL: "free text @@ entity:Service AND markers:TODO"
    let (query_text, dsl_part) = match req.query_text.split_once("@@") {
        Some((free, dsl)) => (free.trim().to_string(), Some(dsl.trim())),
        None => (req.query_text.clone(), None),
    };
    let mut predicates = req.typed_predicates.clone();
    if let Some(d) = dsl_part {
        let parsed = dsl::parse(d).map_err(|e| {
            Error::StructuredOutput {
                message: format!("hybrid retrieval DSL: {e}"),
            }
        })?;
        predicates.extend(parsed);
    }
    Ok(ParsedQuery {
        query_text,
        predicates,
    })
}

// ===========================================================================
// Layer 1.5 — Candidate count preflight
// ===========================================================================

fn preflight_count(graph: &GraphStore) -> Result<usize> {
    let rows = run_hybrid(
        graph,
        "?[count(id)] := *claims{id}",
        BTreeMap::new(),
    )?;
    let count = rows
        .rows
        .into_iter()
        .next()
        .and_then(|r| r.into_iter().next())
        .map(|v| cell_i64(&v).max(0) as usize)
        .unwrap_or(0);
    Ok(count)
}

// ===========================================================================
// Layer 2 — Query planner
// ===========================================================================

fn plan_routing(
    parsed: &ParsedQuery,
    candidate_count: usize,
    profile: &ScoringProfile,
) -> RoutingShape {
    if candidate_count < profile.total_candidate_threshold {
        return RoutingShape::DatalogOnlyForced;
    }
    let has_text = !parsed.query_text.trim().is_empty();
    let has_predicates = !parsed.predicates.is_empty();
    match (has_text, has_predicates) {
        (true, false) => RoutingShape::VectorFirst,
        (false, true) => RoutingShape::DatalogFirst,
        (true, true) => RoutingShape::Interleaved,
        (false, false) => RoutingShape::DatalogFirst, // empty input → empty output
    }
}

// ===========================================================================
// Layer 3 — VectorRecall + DatalogFilters
// ===========================================================================

async fn run_recall(
    engine: &QueryEngine,
    ws: &str,
    graph: &GraphStore,
    req: &RetrievalRequest,
    parsed: &ParsedQuery,
    shape: RoutingShape,
) -> Result<(Vec<(String, f32)>, HashSet<String>)> {
    let needs_vector = matches!(shape, RoutingShape::VectorFirst | RoutingShape::Interleaved)
        && !parsed.query_text.trim().is_empty();
    let needs_datalog = !parsed.predicates.is_empty();

    let vector = if needs_vector {
        vector_recall(engine, ws, &parsed.query_text, req).await?
    } else {
        Vec::new()
    };
    let datalog = if needs_datalog {
        datalog_candidates(graph, &parsed.predicates)?
    } else {
        HashSet::new()
    };
    Ok((vector, datalog))
}

async fn vector_recall(
    engine: &QueryEngine,
    ws: &str,
    query: &str,
    req: &RetrievalRequest,
) -> Result<Vec<(String, f32)>> {
    // Honest about the recall backend: this calls the existing in-memory
    // fastembed (AllMiniLML6V2 384-dim cosine) via `engine.search_scoped`.
    // No HNSW index in CozoDB today; world-class part is the Datalog
    // fan-in and score fusion downstream.
    let allowed: HashSet<String> = req
        .scoped_claim_ids
        .as_ref()
        .map(|v| v.iter().cloned().collect())
        .unwrap_or_default();
    let result = engine
        .search_scoped(ws, query, req.top_k * 4, &allowed)
        .await?;
    Ok(result
        .claims
        .into_iter()
        .map(|c| (c.id, c.relevance))
        .collect())
}

fn datalog_candidates(
    graph: &GraphStore,
    predicates: &[TypedPredicate],
) -> Result<HashSet<String>> {
    if predicates.is_empty() {
        return Ok(HashSet::new());
    }
    let mut acc: Option<HashSet<String>> = None;
    for pred in predicates {
        let ids = run_predicate(graph, pred)?;
        let set: HashSet<String> = ids.into_iter().collect();
        acc = Some(match acc {
            None => set,
            Some(prev) => prev.intersection(&set).cloned().collect(),
        });
    }
    Ok(acc.unwrap_or_default())
}

fn run_predicate(graph: &GraphStore, pred: &TypedPredicate) -> Result<Vec<String>> {
    let (query, params) = build_predicate_query(pred);
    let rows: NamedRows = match query {
        PredicateQuery::Static(q) => run_hybrid(graph, q, params)?,
        PredicateQuery::Dynamic(q) => run_hybrid(graph, &q, params)?,
    };
    Ok(rows
        .rows
        .into_iter()
        .filter_map(|r| match r.into_iter().next() {
            Some(DataValue::Str(s)) => Some(s.to_string()),
            _ => None,
        })
        .collect())
}

enum PredicateQuery {
    Static(&'static str),
    Dynamic(String),
}

fn build_predicate_query(pred: &TypedPredicate) -> (PredicateQuery, BTreeMap<String, DataValue>) {
    let mut params = BTreeMap::new();
    let q = match pred {
        TypedPredicate::EntityType { value } => {
            params.insert("entity_type".into(), DataValue::Str(value.clone().into()));
            PredicateQuery::Static(Q_HR_ENTITY_TYPE)
        }
        TypedPredicate::EntityName { value } => {
            params.insert("entity_name".into(), DataValue::Str(value.clone().into()));
            PredicateQuery::Static(Q_HR_ENTITY_NAME)
        }
        TypedPredicate::ClaimType { value } => {
            params.insert("claim_type".into(), DataValue::Str(value.clone().into()));
            PredicateQuery::Static(Q_HR_CLAIM_TYPE)
        }
        TypedPredicate::SourceTrustAtLeast { value } => {
            let levels = trust_levels_at_least(*value);
            params.insert(
                "accepted_levels".into(),
                dv_str_list(&levels),
            );
            PredicateQuery::Static(Q_HR_SOURCE_TRUST_AT_LEAST)
        }
        TypedPredicate::AuthoredBy { value } => {
            params.insert("author".into(), DataValue::Str(value.clone().into()));
            PredicateQuery::Static(Q_HR_AUTHORED_BY)
        }
        TypedPredicate::AuthoredAfter { value } => {
            params.insert(
                "after_epoch".into(),
                DataValue::from(value.timestamp() as f64),
            );
            PredicateQuery::Static(Q_HR_AUTHORED_AFTER)
        }
        TypedPredicate::InCallGraphOf { entity_name, depth } => {
            params.insert(
                "entity_name".into(),
                DataValue::Str(entity_name.clone().into()),
            );
            params.insert("max_depth".into(), DataValue::from(*depth as i64));
            PredicateQuery::Static(Q_HR_IN_CALL_GRAPH_OF)
        }
        TypedPredicate::HasDocTag { tag_kind, target } => {
            params.insert("tag_kind".into(), DataValue::Str(tag_kind.clone().into()));
            match target {
                Some(t) => {
                    params.insert("target".into(), DataValue::Str(t.clone().into()));
                    PredicateQuery::Static(Q_HR_HAS_DOC_TAG_WITH_TARGET)
                }
                None => PredicateQuery::Static(Q_HR_HAS_DOC_TAG_ANY_TARGET),
            }
        }
        TypedPredicate::HasMarker { kinds } => {
            params.insert("marker_kinds".into(), dv_str_list(kinds));
            PredicateQuery::Static(Q_HR_HAS_MARKER)
        }
        TypedPredicate::QuantityRange { metric, min, max } => {
            params.insert("metric".into(), DataValue::Str(metric.clone().into()));
            params.insert("min".into(), DataValue::from(*min));
            params.insert("max".into(), DataValue::from(*max));
            PredicateQuery::Static(Q_HR_QUANTITY_RANGE)
        }
        TypedPredicate::InHeadingPath { path } => {
            for (i, txt) in path.iter().enumerate() {
                params.insert(format!("path_{i}"), DataValue::Str(txt.clone().into()));
            }
            let q = build_in_heading_path(path.len());
            PredicateQuery::Dynamic(q)
        }
        TypedPredicate::SupersedesClaim { claim_id } => {
            params.insert(
                "target_claim_id".into(),
                DataValue::Str(claim_id.clone().into()),
            );
            PredicateQuery::Static(Q_HR_SUPERSEDES_CLAIM)
        }
        TypedPredicate::ReferencedBy { source_id } => {
            params.insert(
                "target_source_id".into(),
                DataValue::Str(source_id.clone().into()),
            );
            PredicateQuery::Static(Q_HR_REFERENCED_BY)
        }
    };
    (q, params)
}

fn trust_levels_at_least(min: TrustLevel) -> Vec<&'static str> {
    let all = [
        ("Quarantined", TrustLevel::Quarantined),
        ("Untrusted", TrustLevel::Untrusted),
        ("Unknown", TrustLevel::Unknown),
        ("Trusted", TrustLevel::Trusted),
        ("Verified", TrustLevel::Verified),
    ];
    all.iter()
        .filter(|(_, lvl)| *lvl >= min)
        .map(|(n, _)| *n)
        .collect()
}

// ===========================================================================
// Layer 4 — Candidate merger
// ===========================================================================

#[derive(Debug, Clone)]
struct Candidate {
    claim_id: String,
    vector_relevance: f32,
}

fn merge_candidates(
    vector_hits: Vec<(String, f32)>,
    datalog_ids: HashSet<String>,
    shape: RoutingShape,
    cap: usize,
    req: &RetrievalRequest,
) -> Vec<Candidate> {
    // Apply scoped_claim_ids gate first when the call carries one (used by
    // AEP composition to bound candidates to the Engram cluster).
    let scope: Option<HashSet<String>> = req
        .scoped_claim_ids
        .as_ref()
        .map(|v| v.iter().cloned().collect());
    let in_scope = |id: &str| scope.as_ref().map_or(true, |s| s.contains(id));

    let mut out: Vec<Candidate> = match shape {
        RoutingShape::VectorFirst => {
            let mut v: Vec<Candidate> = vector_hits
                .into_iter()
                .filter(|(id, _)| in_scope(id))
                .filter(|(id, _)| datalog_ids.is_empty() || datalog_ids.contains(id))
                .map(|(claim_id, r)| Candidate {
                    claim_id,
                    vector_relevance: r,
                })
                .collect();
            v.truncate(cap);
            v
        }
        RoutingShape::DatalogFirst | RoutingShape::DatalogOnlyForced => datalog_ids
            .into_iter()
            .filter(|id| in_scope(id))
            .take(cap)
            .map(|claim_id| Candidate {
                claim_id,
                vector_relevance: 0.0,
            })
            .collect(),
        RoutingShape::Interleaved => {
            let dl: HashSet<String> = datalog_ids;
            vector_hits
                .into_iter()
                .filter(|(id, _)| in_scope(id) && dl.contains(id))
                .take(cap)
                .map(|(claim_id, r)| Candidate {
                    claim_id,
                    vector_relevance: r,
                })
                .collect()
        }
    };
    // Stable de-dupe by claim_id (keeps highest vector relevance first because
    // `search_scoped` already sorted that way; we just remove repeats).
    let mut seen: HashSet<String> = HashSet::new();
    out.retain(|c| seen.insert(c.claim_id.clone()));
    out
}

// ===========================================================================
// Layer 5 — Structural enricher
// ===========================================================================

#[derive(Debug, Clone)]
struct EnrichedCandidate {
    claim_id: String,
    statement: String,
    claim_type: String,
    vector_relevance: f32,
    primary_source_id: String,
    primary_byte_start: u64,
    primary_byte_end: u64,
    primary_blake3: String,
    sensitivity: Sensitivity,
    admission_tier: AdmissionTier,
    grounding_score: Option<f64>,
    grounding_method: Option<GroundingMethod>,
    valid_from: Option<f64>,
    valid_until: Option<f64>,
    superseded_by_chain: Vec<String>,
    derivation_parents: Vec<String>,
    derivation_root: Option<String>,
    source_authority: TrustLevel,
    source_uri: String,
    source_blake3s: Vec<String>,
    trial_scores: Option<TrialScores>,
    certificate_hash: Option<String>,
    code_signature: Option<CodeSignatureRef>,
    code_metrics: Option<CodeMetricRef>,
    callers: Vec<CallEdge>,
    callees: Vec<CallEdge>,
    doc_tags: Vec<DocTagRef>,
    markers: Vec<CodeMarkerRef>,
    test_origin: Option<TestAnnotationRef>,
    git_blame: Vec<GitBlameRef>,
    quantities: Vec<QuantityRef>,
    related_events: Vec<EventTriple>,
    cluster_contradictions: Vec<ContradictionRef>,
    cluster_gaps: Vec<KnownUnknown>,
    enrichment_byte_spans: Vec<(String, u64, u64, &'static str, String)>, // (source, start, end, table, blake3)
}

fn enrich_candidates(
    graph: &GraphStore,
    candidates: Vec<Candidate>,
    req: &RetrievalRequest,
) -> Result<Vec<EnrichedCandidate>> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let claim_ids: Vec<String> = candidates.iter().map(|c| c.claim_id.clone()).collect();
    let claim_set = dv_str_list(&claim_ids);

    // 1. Claims metadata
    let claim_meta = fetch_claim_metadata(graph, &claim_set)?;
    // 2. Source authority
    let (sa_by_claim, _source_uri_by_claim, source_blake3_by_claim) =
        fetch_source_authority(graph, &claim_set)?;
    // 3. Temporal + supersession chain
    let (valid_from, valid_until, supersession) = fetch_temporal(graph, &claim_set)?;
    // 4. Trial scores + certificate hash
    let trial = fetch_trial_scores(graph, &claim_set)?;
    // 5. Derivation lineage
    let (deriv_parents, deriv_root) = fetch_derivation(graph, &claim_set)?;
    // 6. Code-aware bundles (signatures, calls, doc_tags, markers, metrics, tests, blame)
    let code_sig = fetch_code_signatures(graph, &claim_set)?;
    let (callers, callees) = fetch_call_edges(graph, &claim_set)?;
    let doc_tags = fetch_doc_tags(graph, &claim_set)?;
    let markers = fetch_markers(graph, &claim_set)?;
    let code_metrics = fetch_code_metrics(graph, &claim_set)?;
    let test_origins = fetch_test_origins(graph, &claim_set)?;
    let git_blame = fetch_git_blame(graph, &claim_set)?;
    let quantities = fetch_quantities(graph, &claim_set)?;
    let contradictions = fetch_contradictions(graph, &claim_set)?;
    let gaps_by_entity = fetch_gaps(graph, &claim_set)?;
    let events = fetch_events(graph, &claim_set, req.time_window.as_ref())?;

    // Build EnrichedCandidate per input
    let mut out = Vec::with_capacity(candidates.len());
    for c in candidates {
        let meta = match claim_meta.get(&c.claim_id) {
            Some(m) => m.clone(),
            None => continue, // claim id missing from claims table — drop
        };
        let (auth, uri) = sa_by_claim
            .get(&c.claim_id)
            .cloned()
            .unwrap_or_else(|| (TrustLevel::Unknown, String::new()));
        let blake3s = source_blake3_by_claim
            .get(&c.claim_id)
            .cloned()
            .unwrap_or_default();
        let (vf, vu) = valid_from
            .get(&c.claim_id)
            .copied()
            .map(|v| (Some(v), valid_until.get(&c.claim_id).copied()))
            .unwrap_or((None, None));
        let chain = supersession.get(&c.claim_id).cloned().unwrap_or_default();
        let (ts, cert) = trial
            .get(&c.claim_id)
            .cloned()
            .unwrap_or((None, None));
        let dparents = deriv_parents.get(&c.claim_id).cloned().unwrap_or_default();
        let droot = deriv_root.get(&c.claim_id).cloned();
        let csig = code_sig.get(&c.claim_id).cloned();
        let callers_v = callers.get(&c.claim_id).cloned().unwrap_or_default();
        let callees_v = callees.get(&c.claim_id).cloned().unwrap_or_default();
        let dtags = doc_tags.get(&c.claim_id).cloned().unwrap_or_default();
        let mkrs = markers.get(&c.claim_id).cloned().unwrap_or_default();
        let cms = code_metrics.get(&c.claim_id).cloned();
        let tst = test_origins.get(&c.claim_id).cloned();
        let blame = git_blame.get(&c.claim_id).cloned().unwrap_or_default();
        let qts = quantities.get(&c.claim_id).cloned().unwrap_or_default();
        let ctrs = contradictions.get(&c.claim_id).cloned().unwrap_or_default();
        let gps = gaps_by_entity.get(&c.claim_id).cloned().unwrap_or_default();
        let evs = events.get(&c.claim_id).cloned().unwrap_or_default();

        // Build per-row byte_span breakdown for the stitcher (Layer 7)
        let mut spans: Vec<(String, u64, u64, &'static str, String)> = Vec::new();
        spans.push((
            meta.source_id.clone(),
            meta.byte_start,
            meta.byte_end,
            "claims",
            meta.content_blake3.clone(),
        ));
        if let Some(s) = &csig {
            spans.push((
                meta.source_id.clone(),
                meta.byte_start,
                meta.byte_end,
                "code_signatures",
                blake3s.first().cloned().unwrap_or_default(),
            ));
            let _ = s; // signatures share the seed claim's byte range
        }
        for ce in callers_v.iter().chain(callees_v.iter()) {
            spans.push((
                ce.source_id.clone(),
                ce.byte_start,
                ce.byte_end,
                "function_calls",
                String::new(),
            ));
        }
        for m in &mkrs {
            spans.push((
                m.source_id.clone(),
                m.byte_start,
                m.byte_end,
                "code_markers",
                String::new(),
            ));
        }

        out.push(EnrichedCandidate {
            claim_id: c.claim_id.clone(),
            statement: meta.statement,
            claim_type: meta.claim_type,
            vector_relevance: c.vector_relevance,
            primary_source_id: meta.source_id,
            primary_byte_start: meta.byte_start,
            primary_byte_end: meta.byte_end,
            primary_blake3: meta.content_blake3,
            sensitivity: meta.sensitivity,
            admission_tier: meta.admission_tier,
            grounding_score: meta.grounding_score,
            grounding_method: meta.grounding_method,
            valid_from: vf,
            valid_until: vu,
            superseded_by_chain: chain,
            derivation_parents: dparents,
            derivation_root: droot,
            source_authority: auth,
            source_uri: uri,
            source_blake3s: blake3s,
            trial_scores: ts,
            certificate_hash: cert,
            code_signature: csig,
            code_metrics: cms,
            callers: callers_v,
            callees: callees_v,
            doc_tags: dtags,
            markers: mkrs,
            test_origin: tst,
            git_blame: blame,
            quantities: qts,
            related_events: evs,
            cluster_contradictions: ctrs,
            cluster_gaps: gps,
            enrichment_byte_spans: spans,
        });
    }
    Ok(out)
}

#[derive(Debug, Clone)]
struct ClaimMeta {
    statement: String,
    claim_type: String,
    source_id: String,
    byte_start: u64,
    byte_end: u64,
    content_blake3: String,
    sensitivity: Sensitivity,
    admission_tier: AdmissionTier,
    grounding_score: Option<f64>,
    grounding_method: Option<GroundingMethod>,
}

fn cell_str(v: &DataValue) -> String {
    match v {
        DataValue::Str(s) => s.to_string(),
        _ => String::new(),
    }
}

fn cell_i64(v: &DataValue) -> i64 {
    match v {
        DataValue::Num(Num::Int(i)) => *i,
        DataValue::Num(Num::Float(f)) => *f as i64,
        _ => 0,
    }
}

fn cell_u64(v: &DataValue) -> u64 {
    cell_i64(v).max(0) as u64
}

fn cell_f64(v: &DataValue) -> f64 {
    match v {
        DataValue::Num(Num::Float(f)) => *f,
        DataValue::Num(Num::Int(i)) => *i as f64,
        _ => 0.0,
    }
}

fn cell_bool(v: &DataValue) -> bool {
    matches!(v, DataValue::Bool(true))
}

fn fetch_claim_metadata(
    graph: &GraphStore,
    claim_set: &DataValue,
) -> Result<HashMap<String, ClaimMeta>> {
    let mut params = BTreeMap::new();
    params.insert("cset".into(), claim_set.clone());
    let rows = run_hybrid(
        graph,
        r#"?[id, statement, claim_type, source_id, byte_start, byte_end, content_blake3,
             sensitivity, admission_tier, grounding_score, grounding_method] :=
            id in $cset,
            *claims{id, statement, claim_type, source_id, byte_start, byte_end, content_blake3,
                    sensitivity, admission_tier, grounding_score, grounding_method}"#,
        params,
    )?;
    let mut out = HashMap::new();
    for r in rows.rows {
        if r.len() < 11 {
            continue;
        }
        let id = cell_str(&r[0]);
        out.insert(
            id.clone(),
            ClaimMeta {
                statement: cell_str(&r[1]),
                claim_type: cell_str(&r[2]),
                source_id: cell_str(&r[3]),
                byte_start: cell_u64(&r[4]),
                byte_end: cell_u64(&r[5]),
                content_blake3: cell_str(&r[6]),
                sensitivity: parse_sensitivity(&cell_str(&r[7])),
                admission_tier: AdmissionTier::from_str(&cell_str(&r[8])),
                grounding_score: {
                    let f = cell_f64(&r[9]);
                    if f < 0.0 {
                        None
                    } else {
                        Some(f)
                    }
                },
                grounding_method: parse_grounding_method(&cell_str(&r[10])),
            },
        );
    }

    // ── Witness-substrate resolution (Phase 5 cutover) ──────────────────────
    // Vector candidates are keyed by WITNESS id, and freshly-compiled prose
    // lives in the `witnesses` relation — NOT `claims`. Without this, every
    // vector candidate was dropped at the `claim_meta.get(..) => None` guard,
    // so `search/hybrid` returned 0 hits even with a populated index (while
    // `ask`, which reads `get_all_claims_with_sources`, worked). Resolve any
    // candidate id NOT already satisfied by `claims` from `witnesses`,
    // materialising the statement from the source bytes (same path as `ask`)
    // and defaulting the Rooting/grounding columns the witness substrate
    // doesn't carry. `claims` rows always win (legacy code claims stay exact).
    let wrows = run_hybrid(
        graph,
        r#"?[id, witness_type, rule, source_id, byte_start, byte_end, content_blake3, sensitivity] :=
            id in $cset,
            *witnesses{id, witness_type, rule, source_id, byte_start, byte_end, content_blake3, sensitivity}"#,
        {
            let mut p = BTreeMap::new();
            p.insert("cset".into(), claim_set.clone());
            p
        },
    )?;
    for r in wrows.rows {
        if r.len() < 8 {
            continue;
        }
        let id = cell_str(&r[0]);
        if out.contains_key(&id) {
            continue;
        }
        let witness_type = cell_str(&r[1]);
        let rule = cell_str(&r[2]);
        let source_id = cell_str(&r[3]);
        let byte_start = cell_u64(&r[4]);
        let byte_end = cell_u64(&r[5]);
        let statement = graph
            .materialize_statement(&source_id, byte_start, byte_end)
            .ok()
            .flatten()
            .unwrap_or_else(|| format!("[{witness_type}] via {rule} @{byte_start}..{byte_end}"));
        out.insert(
            id,
            ClaimMeta {
                statement,
                claim_type: witness_type,
                source_id,
                byte_start,
                byte_end,
                content_blake3: cell_str(&r[6]),
                sensitivity: parse_sensitivity(&cell_str(&r[7])),
                admission_tier: AdmissionTier::from_str("attested"),
                grounding_score: None,
                grounding_method: parse_grounding_method(""),
            },
        );
    }
    Ok(out)
}

fn parse_sensitivity(s: &str) -> Sensitivity {
    Sensitivity::parse(s).unwrap_or(Sensitivity::Public)
}

fn parse_grounding_method(s: &str) -> Option<GroundingMethod> {
    match s {
        "lexical" => Some(GroundingMethod::Lexical),
        "span" => Some(GroundingMethod::Span),
        "semantic" => Some(GroundingMethod::Semantic),
        "combined" => Some(GroundingMethod::Combined),
        "structural" => Some(GroundingMethod::Structural),
        "unverified" => Some(GroundingMethod::Unverified),
        _ => None,
    }
}

fn parse_trust_level(s: &str) -> TrustLevel {
    match s {
        "Verified" => TrustLevel::Verified,
        "Trusted" => TrustLevel::Trusted,
        "Untrusted" => TrustLevel::Untrusted,
        "Quarantined" => TrustLevel::Quarantined,
        _ => TrustLevel::Unknown,
    }
}

fn fetch_source_authority(
    graph: &GraphStore,
    claim_set: &DataValue,
) -> Result<(
    HashMap<String, (TrustLevel, String)>,
    HashMap<String, String>,
    HashMap<String, Vec<String>>,
)> {
    let mut params = BTreeMap::new();
    params.insert("cset".into(), claim_set.clone());
    let rows = run_hybrid(
        graph,
        r#"?[claim_id, source_id, uri, trust_level, content_hash] :=
            claim_id in $cset,
            *claim_source_edges{claim_id, source_id},
            *sources{id: source_id, uri, trust_level, content_hash}"#,
        params,
    )?;
    let mut auth = HashMap::new();
    let mut uri_by_claim = HashMap::new();
    let mut blake3s: HashMap<String, Vec<String>> = HashMap::new();
    for r in rows.rows {
        if r.len() < 5 {
            continue;
        }
        let cid = cell_str(&r[0]);
        let uri = cell_str(&r[2]);
        let lvl = parse_trust_level(&cell_str(&r[3]));
        let hash = cell_str(&r[4]);
        // Keep the highest trust seen + first uri.
        auth.entry(cid.clone())
            .and_modify(|(tl, _)| {
                if lvl > *tl {
                    *tl = lvl;
                }
            })
            .or_insert((lvl, uri.clone()));
        uri_by_claim.entry(cid.clone()).or_insert(uri);
        if !hash.is_empty() {
            blake3s.entry(cid).or_default().push(format!("blake3:{hash}"));
        }
    }
    Ok((auth, uri_by_claim, blake3s))
}

fn fetch_temporal(
    graph: &GraphStore,
    claim_set: &DataValue,
) -> Result<(
    HashMap<String, f64>,
    HashMap<String, f64>,
    HashMap<String, Vec<String>>,
)> {
    let mut params = BTreeMap::new();
    params.insert("cset".into(), claim_set.clone());
    let rows = run_hybrid(
        graph,
        r#"?[claim_id, valid_from, valid_until, superseded_by] :=
            claim_id in $cset,
            *claim_temporal{claim_id, valid_from, valid_until, superseded_by}"#,
        params,
    )?;
    let mut vf = HashMap::new();
    let mut vu = HashMap::new();
    let mut chain: HashMap<String, Vec<String>> = HashMap::new();
    for r in rows.rows {
        if r.len() < 4 {
            continue;
        }
        let cid = cell_str(&r[0]);
        let from = cell_f64(&r[1]);
        let until = cell_f64(&r[2]);
        let succ = cell_str(&r[3]);
        if from > 0.0 {
            vf.insert(cid.clone(), from);
        }
        if until > 0.0 {
            vu.insert(cid.clone(), until);
        }
        if !succ.is_empty() {
            chain.entry(cid).or_default().push(succ);
        }
    }
    Ok((vf, vu, chain))
}

fn fetch_trial_scores(
    graph: &GraphStore,
    claim_set: &DataValue,
) -> Result<HashMap<String, (Option<TrialScores>, Option<String>)>> {
    let mut params = BTreeMap::new();
    params.insert("cset".into(), claim_set.clone());
    // Take the most recent verdict per claim. Cozo ordering by `trial_at desc`
    // requires a sort step; we sort in Rust.
    let rows = run_hybrid(
        graph,
        r#"?[claim_id, trial_at, provenance_score, contradiction_score, predicate_score,
             topology_score, temporal_score, certificate_hash] :=
            claim_id in $cset,
            *trial_verdicts{claim_id, trial_at, provenance_score, contradiction_score,
                             predicate_score, topology_score, temporal_score, certificate_hash}"#,
        params,
    )?;
    let mut latest: HashMap<String, (f64, TrialScores, String)> = HashMap::new();
    for r in rows.rows {
        if r.len() < 8 {
            continue;
        }
        let cid = cell_str(&r[0]);
        let at = cell_f64(&r[1]);
        let ts = TrialScores {
            provenance_score: cell_f64(&r[2]),
            contradiction_score: cell_f64(&r[3]),
            predicate_score: cell_f64(&r[4]),
            topology_score: cell_f64(&r[5]),
            temporal_score: cell_f64(&r[6]),
        };
        let cert = cell_str(&r[7]);
        latest
            .entry(cid)
            .and_modify(|(prev_at, prev_ts, prev_cert)| {
                if at > *prev_at {
                    *prev_at = at;
                    *prev_ts = ts.clone();
                    *prev_cert = cert.clone();
                }
            })
            .or_insert((at, ts, cert));
    }
    Ok(latest
        .into_iter()
        .map(|(cid, (_, ts, cert))| {
            (
                cid,
                (
                    Some(ts),
                    if cert.is_empty() { None } else { Some(cert) },
                ),
            )
        })
        .collect())
}

fn fetch_derivation(
    graph: &GraphStore,
    claim_set: &DataValue,
) -> Result<(HashMap<String, Vec<String>>, HashMap<String, String>)> {
    let mut params = BTreeMap::new();
    params.insert("cset".into(), claim_set.clone());
    let rows = run_hybrid(
        graph,
        r#"?[child, parent] :=
            child in $cset,
            *derivation_edges{parent_claim_id: parent, child_claim_id: child}"#,
        params,
    )?;
    let mut parents: HashMap<String, Vec<String>> = HashMap::new();
    let mut roots: HashMap<String, String> = HashMap::new();
    for r in rows.rows {
        if r.len() < 2 {
            continue;
        }
        let child = cell_str(&r[0]);
        let parent = cell_str(&r[1]);
        parents.entry(child.clone()).or_default().push(parent.clone());
        roots.entry(child).or_insert(parent);
    }
    Ok((parents, roots))
}

fn fetch_code_signatures(
    graph: &GraphStore,
    claim_set: &DataValue,
) -> Result<HashMap<String, CodeSignatureRef>> {
    let mut params = BTreeMap::new();
    params.insert("cset".into(), claim_set.clone());
    let rows = run_hybrid(
        graph,
        r#"?[claim_id, parameters_json, return_type, visibility, trait_name] :=
            claim_id in $cset,
            *code_signatures{claim_id, parameters_json, return_type, visibility, trait_name}"#,
        params,
    )?;
    let mut out = HashMap::new();
    for r in rows.rows {
        if r.len() < 5 {
            continue;
        }
        let cid = cell_str(&r[0]);
        out.insert(
            cid.clone(),
            CodeSignatureRef {
                claim_id: cid,
                parameters_json: cell_str(&r[1]),
                return_type: cell_str(&r[2]),
                visibility: cell_str(&r[3]),
                trait_name: cell_str(&r[4]),
            },
        );
    }
    Ok(out)
}

fn fetch_call_edges(
    graph: &GraphStore,
    claim_set: &DataValue,
) -> Result<(HashMap<String, Vec<CallEdge>>, HashMap<String, Vec<CallEdge>>)> {
    // Callees: edges where claim_id is the caller.
    let mut callee_params = BTreeMap::new();
    callee_params.insert("cset".into(), claim_set.clone());
    let callee_rows = run_hybrid(
        graph,
        r#"?[caller_claim_id, callee_name, callee_claim_id, source_id, byte_start, byte_end] :=
            caller_claim_id in $cset,
            *function_calls{caller_claim_id, callee_name, callee_claim_id, source_id, byte_start, byte_end}"#,
        callee_params,
    )?;
    let mut callees: HashMap<String, Vec<CallEdge>> = HashMap::new();
    for r in callee_rows.rows {
        if r.len() < 6 {
            continue;
        }
        let caller = cell_str(&r[0]);
        callees.entry(caller.clone()).or_default().push(CallEdge {
            caller_claim_id: caller,
            callee_name: cell_str(&r[1]),
            callee_claim_id: cell_str(&r[2]),
            source_id: cell_str(&r[3]),
            byte_start: cell_u64(&r[4]),
            byte_end: cell_u64(&r[5]),
        });
    }
    // Callers: edges where claim_id is the callee.
    let mut caller_params = BTreeMap::new();
    caller_params.insert("cset".into(), claim_set.clone());
    let caller_rows = run_hybrid(
        graph,
        r#"?[caller_claim_id, callee_name, callee_claim_id, source_id, byte_start, byte_end] :=
            callee_claim_id in $cset,
            *function_calls{caller_claim_id, callee_name, callee_claim_id, source_id, byte_start, byte_end}"#,
        caller_params,
    )?;
    let mut callers: HashMap<String, Vec<CallEdge>> = HashMap::new();
    for r in caller_rows.rows {
        if r.len() < 6 {
            continue;
        }
        let callee = cell_str(&r[2]);
        callers.entry(callee).or_default().push(CallEdge {
            caller_claim_id: cell_str(&r[0]),
            callee_name: cell_str(&r[1]),
            callee_claim_id: cell_str(&r[2]),
            source_id: cell_str(&r[3]),
            byte_start: cell_u64(&r[4]),
            byte_end: cell_u64(&r[5]),
        });
    }
    Ok((callers, callees))
}

fn fetch_doc_tags(
    graph: &GraphStore,
    claim_set: &DataValue,
) -> Result<HashMap<String, Vec<DocTagRef>>> {
    let mut params = BTreeMap::new();
    params.insert("cset".into(), claim_set.clone());
    let rows = run_hybrid(
        graph,
        r#"?[claim_id, kind, target, description] :=
            claim_id in $cset,
            *doc_tags{claim_id, kind, target, description}"#,
        params,
    )?;
    let mut out: HashMap<String, Vec<DocTagRef>> = HashMap::new();
    for r in rows.rows {
        if r.len() < 4 {
            continue;
        }
        let cid = cell_str(&r[0]);
        out.entry(cid.clone()).or_default().push(DocTagRef {
            claim_id: cid,
            kind: cell_str(&r[1]),
            target: cell_str(&r[2]),
            description: cell_str(&r[3]),
        });
    }
    Ok(out)
}

fn fetch_markers(
    graph: &GraphStore,
    claim_set: &DataValue,
) -> Result<HashMap<String, Vec<CodeMarkerRef>>> {
    let mut params = BTreeMap::new();
    params.insert("cset".into(), claim_set.clone());
    let rows = run_hybrid(
        graph,
        r#"?[id, source_id, kind, text, in_claim_id, byte_start, byte_end] :=
            in_claim_id in $cset,
            *code_markers{id, source_id, kind, text, in_claim_id, byte_start, byte_end}"#,
        params,
    )?;
    let mut out: HashMap<String, Vec<CodeMarkerRef>> = HashMap::new();
    for r in rows.rows {
        if r.len() < 7 {
            continue;
        }
        let cid = cell_str(&r[4]);
        out.entry(cid.clone()).or_default().push(CodeMarkerRef {
            id: cell_str(&r[0]),
            source_id: cell_str(&r[1]),
            kind: cell_str(&r[2]),
            text: cell_str(&r[3]),
            in_claim_id: cid,
            byte_start: cell_u64(&r[5]),
            byte_end: cell_u64(&r[6]),
        });
    }
    Ok(out)
}

fn fetch_code_metrics(
    graph: &GraphStore,
    claim_set: &DataValue,
) -> Result<HashMap<String, CodeMetricRef>> {
    let mut params = BTreeMap::new();
    params.insert("cset".into(), claim_set.clone());
    let rows = run_hybrid(
        graph,
        r#"?[source_id, scope, scope_claim_id, loc, cyclomatic, fan_in, fan_out, complexity_method] :=
            scope_claim_id in $cset,
            *code_metrics{source_id, scope, scope_claim_id, loc, cyclomatic, fan_in, fan_out, complexity_method}"#,
        params,
    )?;
    let mut out = HashMap::new();
    for r in rows.rows {
        if r.len() < 8 {
            continue;
        }
        let cid = cell_str(&r[2]);
        out.insert(
            cid.clone(),
            CodeMetricRef {
                source_id: cell_str(&r[0]),
                scope: cell_str(&r[1]),
                scope_claim_id: cid,
                loc: cell_i64(&r[3]).max(0) as u32,
                cyclomatic: cell_i64(&r[4]).max(0) as u32,
                fan_in: cell_i64(&r[5]).max(0) as u32,
                fan_out: cell_i64(&r[6]).max(0) as u32,
                complexity_method: cell_str(&r[7]),
            },
        );
    }
    Ok(out)
}

fn fetch_test_origins(
    graph: &GraphStore,
    claim_set: &DataValue,
) -> Result<HashMap<String, TestAnnotationRef>> {
    let mut params = BTreeMap::new();
    params.insert("cset".into(), claim_set.clone());
    let rows = run_hybrid(
        graph,
        r#"?[id, claim_id, framework, annotation_kind, name] :=
            claim_id in $cset,
            *test_annotations{id, claim_id, framework, annotation_kind, name}"#,
        params,
    )?;
    let mut out = HashMap::new();
    for r in rows.rows {
        if r.len() < 5 {
            continue;
        }
        let cid = cell_str(&r[1]);
        out.insert(
            cid.clone(),
            TestAnnotationRef {
                id: cell_str(&r[0]),
                claim_id: cid,
                framework: cell_str(&r[2]),
                annotation_kind: cell_str(&r[3]),
                name: cell_str(&r[4]),
            },
        );
    }
    Ok(out)
}

fn fetch_git_blame(
    graph: &GraphStore,
    claim_set: &DataValue,
) -> Result<HashMap<String, Vec<GitBlameRef>>> {
    let mut params = BTreeMap::new();
    params.insert("cset".into(), claim_set.clone());
    let rows = run_hybrid(
        graph,
        r#"?[claim_id, source_id, line_start, line_end, commit_sha, author, blamed_at] :=
            claim_id in $cset,
            *claim_source_edges{claim_id, source_id},
            *git_blame{source_id, line_start, line_end, commit_sha, author, blamed_at}"#,
        params,
    )?;
    let mut out: HashMap<String, Vec<GitBlameRef>> = HashMap::new();
    for r in rows.rows {
        if r.len() < 7 {
            continue;
        }
        let cid = cell_str(&r[0]);
        out.entry(cid).or_default().push(GitBlameRef {
            source_id: cell_str(&r[1]),
            line_start: cell_i64(&r[2]).max(0) as u32,
            line_end: cell_i64(&r[3]).max(0) as u32,
            commit_sha: cell_str(&r[4]),
            author: cell_str(&r[5]),
            blamed_at: cell_f64(&r[6]),
        });
    }
    Ok(out)
}

fn fetch_quantities(
    graph: &GraphStore,
    claim_set: &DataValue,
) -> Result<HashMap<String, Vec<QuantityRef>>> {
    let mut params = BTreeMap::new();
    params.insert("cset".into(), claim_set.clone());
    let rows = run_hybrid(
        graph,
        r#"?[claim_id, metric_name, value, unit, qualifier, is_live, captured_at] :=
            claim_id in $cset,
            *quantities{claim_id, metric_name, value, unit, qualifier, is_live, captured_at}"#,
        params,
    )?;
    let mut out: HashMap<String, Vec<QuantityRef>> = HashMap::new();
    for r in rows.rows {
        if r.len() < 7 {
            continue;
        }
        let cid = cell_str(&r[0]);
        out.entry(cid.clone()).or_default().push(QuantityRef {
            claim_id: cid,
            metric_name: cell_str(&r[1]),
            value: cell_f64(&r[2]),
            unit: cell_str(&r[3]),
            qualifier: cell_str(&r[4]),
            is_live: cell_bool(&r[5]),
            captured_at: cell_f64(&r[6]),
        });
    }
    Ok(out)
}

fn fetch_contradictions(
    graph: &GraphStore,
    claim_set: &DataValue,
) -> Result<HashMap<String, Vec<ContradictionRef>>> {
    let mut params = BTreeMap::new();
    params.insert("cset".into(), claim_set.clone());
    let rows = run_hybrid(
        graph,
        r#"?[id, claim_a, claim_b, explanation, status] :=
            claim_a in $cset,
            *contradictions{id, claim_a, claim_b, explanation, status},
            status != 'Resolved'
        ?[id, claim_a, claim_b, explanation, status] :=
            claim_b in $cset,
            *contradictions{id, claim_a, claim_b, explanation, status},
            status != 'Resolved'"#,
        params,
    )?;
    let mut out: HashMap<String, Vec<ContradictionRef>> = HashMap::new();
    for r in rows.rows {
        if r.len() < 5 {
            continue;
        }
        let id = cell_str(&r[0]);
        let a = cell_str(&r[1]);
        let b = cell_str(&r[2]);
        let ctr = ContradictionRef {
            id,
            claim_a: a.clone(),
            claim_b: b.clone(),
            explanation: cell_str(&r[3]),
            status: cell_str(&r[4]),
        };
        out.entry(a).or_default().push(ctr.clone());
        out.entry(b).or_default().push(ctr);
    }
    Ok(out)
}

fn fetch_gaps(
    graph: &GraphStore,
    claim_set: &DataValue,
) -> Result<HashMap<String, Vec<KnownUnknown>>> {
    // Gaps are per-entity; we join via claim_entity_edges to attribute each
    // gap to every claim that touches the same entity.
    let mut params = BTreeMap::new();
    params.insert("cset".into(), claim_set.clone());
    let rows = run_hybrid(
        graph,
        r#"?[claim_id, gap_id, entity_id, expected_claim_type, confidence] :=
            claim_id in $cset,
            *claim_entity_edges{claim_id, entity_id},
            *known_unknowns{id: gap_id, entity_id, expected_claim_type, confidence, status: 'open'}"#,
        params,
    )?;
    let mut out: HashMap<String, Vec<KnownUnknown>> = HashMap::new();
    for r in rows.rows {
        if r.len() < 5 {
            continue;
        }
        let cid = cell_str(&r[0]);
        out.entry(cid).or_default().push(KnownUnknown {
            gap_id: cell_str(&r[1]),
            entity_id: cell_str(&r[2]),
            expected_claim_type: cell_str(&r[3]),
            confidence: cell_f64(&r[4]),
        });
    }
    Ok(out)
}

fn fetch_events(
    graph: &GraphStore,
    claim_set: &DataValue,
    time_window: Option<&(DateTime<Utc>, DateTime<Utc>)>,
) -> Result<HashMap<String, Vec<EventTriple>>> {
    let (start, end) = time_window
        .map(|(s, e)| (s.timestamp() as f64, e.timestamp() as f64))
        .unwrap_or((0.0, f64::MAX));
    let mut params = BTreeMap::new();
    params.insert("cset".into(), claim_set.clone());
    params.insert("ws".into(), DataValue::from(start));
    params.insert("we".into(), DataValue::from(end));
    // Events are entity-keyed, not claim-keyed. Attach per claim through
    // its claim_entity_edges.
    // Cozo idiom: bind every column referenced in the head via the body's
    // relation pattern, then constrain via predicate. Renaming a column at
    // bind time (`{col: localvar}`) makes the column unbound in the head —
    // Cozo's stratified evaluator rejects this with `Symbol '<col>' in rule
    // head is unbound`. See `.claude/rules/witness-mesh.md` Datalog query
    // idiom and `.claude/rules/hybrid-retrieval.md`.
    let rows = run_hybrid(
        graph,
        r#"?[claim_id, subject_entity_id, verb, object_entity_id, timestamp, normalized_date] :=
            claim_id in $cset,
            *claim_entity_edges{claim_id, entity_id},
            *events{subject_entity_id, verb, object_entity_id, timestamp, normalized_date},
            subject_entity_id = entity_id,
            timestamp >= $ws,
            timestamp <= $we
        ?[claim_id, subject_entity_id, verb, object_entity_id, timestamp, normalized_date] :=
            claim_id in $cset,
            *claim_entity_edges{claim_id, entity_id},
            *events{subject_entity_id, verb, object_entity_id, timestamp, normalized_date},
            object_entity_id = entity_id,
            timestamp >= $ws,
            timestamp <= $we"#,
        params,
    )?;
    let mut out: HashMap<String, Vec<EventTriple>> = HashMap::new();
    for r in rows.rows {
        if r.len() < 6 {
            continue;
        }
        let cid = cell_str(&r[0]);
        out.entry(cid).or_default().push(EventTriple {
            subject_entity_id: cell_str(&r[1]),
            verb: cell_str(&r[2]),
            object_entity_id: cell_str(&r[3]),
            timestamp: cell_f64(&r[4]),
            normalized_date: cell_str(&r[5]),
        });
    }
    Ok(out)
}

// ===========================================================================
// Layer 6 — Score fusion
// ===========================================================================

fn fuse_score(
    c: &EnrichedCandidate,
    profile: &ScoringProfile,
    now: DateTime<Utc>,
    req: &RetrievalRequest,
) -> Option<(f32, ScoreBreakdown)> {
    if profile.require_rooted_only && c.admission_tier != AdmissionTier::Rooted {
        return None;
    }
    if !req.include_test_origin && c.test_origin.is_some() {
        // Test-origin claims drop entirely when caller didn't ask for them.
        return None;
    }
    if req.require_certificate && c.certificate_hash.is_none() {
        return None;
    }

    let v_vector = profile.w_vector * c.vector_relevance;
    let v_admission = profile.w_admission * admission_score(c.admission_tier);
    let v_trial = profile.w_trial * trial_aggregate(&c.trial_scores);
    let v_source_authority = profile.w_source_authority * trust_score(c.source_authority);
    let recency = recency_factor(c.valid_from, now, profile.recency_half_life_days);
    let freshness = freshness_factor(c.valid_until, now);
    let v_recency = profile.w_recency * recency * freshness;
    let v_complexity = profile.w_complexity * complexity_signal(c.code_metrics.as_ref());
    let v_marker = profile.w_marker * marker_signal(&c.markers);
    let v_gap_proximity = profile.w_gap_proximity * gap_proximity_signal(&c.cluster_gaps);
    let v_contradiction =
        profile.w_contradiction * contradiction_penalty(&c.cluster_contradictions);
    let v_test_origin =
        profile.w_test_origin_penalty * test_origin_penalty(&c.test_origin, req.include_test_origin);

    // IEEE 754 determinism: fixed source order sum, no `iter().sum`.
    let fused = v_vector
        + v_admission
        + v_trial
        + v_source_authority
        + v_recency
        + v_complexity
        + v_marker
        + v_gap_proximity
        - v_contradiction
        - v_test_origin;

    Some((
        fused,
        ScoreBreakdown {
            vector: v_vector,
            admission: v_admission,
            trial: v_trial,
            source_authority: v_source_authority,
            recency: profile.w_recency * recency,
            freshness_penalty: freshness,
            complexity: v_complexity,
            marker: v_marker,
            gap_proximity: v_gap_proximity,
            contradiction_penalty: v_contradiction,
            test_origin_penalty: v_test_origin,
            fused,
            cross_encoder: None,
        },
    ))
}

fn admission_score(tier: AdmissionTier) -> f32 {
    match tier {
        AdmissionTier::Rooted => 1.0,
        AdmissionTier::Attested => 0.7,
        AdmissionTier::Quarantined | AdmissionTier::Rejected => 0.0,
    }
}

fn trial_aggregate(ts: &Option<TrialScores>) -> f32 {
    match ts {
        Some(t) => {
            ((t.provenance_score
                + t.contradiction_score
                + t.predicate_score
                + t.topology_score
                + t.temporal_score)
                / 5.0) as f32
        }
        None => 0.0,
    }
}

fn trust_score(level: TrustLevel) -> f32 {
    match level {
        TrustLevel::Verified => 1.0,
        TrustLevel::Trusted => 0.85,
        TrustLevel::Unknown => 0.5,
        TrustLevel::Untrusted => 0.2,
        TrustLevel::Quarantined => 0.0,
    }
}

fn recency_factor(valid_from: Option<f64>, now: DateTime<Utc>, half_life_days: f32) -> f32 {
    // Standard half-life decay: at t = half_life, value = 0.5.
    // 0.5^(t/half_life) = exp(-ln(2) * t / half_life).
    let from = match valid_from {
        Some(v) if v > 0.0 => v,
        _ => return 1.0,
    };
    let elapsed = (now.timestamp() as f64 - from).max(0.0);
    let half_life_seconds = (half_life_days as f64) * 86_400.0;
    if half_life_seconds <= 0.0 {
        return 1.0;
    }
    (-(2.0_f64.ln()) * elapsed / half_life_seconds).exp() as f32
}

fn freshness_factor(valid_until: Option<f64>, now: DateTime<Utc>) -> f32 {
    match valid_until {
        Some(v) if v > 0.0 && v < (now.timestamp() as f64) => 0.0,
        _ => 1.0,
    }
}

fn complexity_signal(metrics: Option<&CodeMetricRef>) -> f32 {
    let m = match metrics {
        Some(m) => m,
        None => return 0.0,
    };
    let cyc_norm = (m.cyclomatic.min(20) as f32) / 20.0;
    let loc_norm = if m.loc <= 50 {
        0.0
    } else {
        (((m.loc - 50) as f32) / 200.0).min(1.0)
    };
    0.5 * (1.0 - cyc_norm) + 0.5 * (1.0 - loc_norm)
}

fn marker_signal(markers: &[CodeMarkerRef]) -> f32 {
    if markers.is_empty() {
        0.0
    } else {
        0.5
    }
}

fn gap_proximity_signal(gaps: &[KnownUnknown]) -> f32 {
    if gaps.is_empty() {
        0.0
    } else {
        let n = gaps.len().min(5) as f32;
        0.3 * (1.0 - 0.2 * n).max(0.0)
    }
}

fn contradiction_penalty(contradictions: &[ContradictionRef]) -> f32 {
    if contradictions.is_empty() {
        0.0
    } else {
        1.0
    }
}

fn test_origin_penalty(origin: &Option<TestAnnotationRef>, include_test_origin: bool) -> f32 {
    if origin.is_some() && !include_test_origin {
        1.0
    } else {
        0.0
    }
}

// ===========================================================================
// Layer 7 — ByteSpanStitcher
// ===========================================================================

fn stitch_byte_spans(c: &EnrichedCandidate) -> ByteSpanBundle {
    let mut by_source: HashMap<String, Vec<ByteSpan>> = HashMap::new();
    let mut row_count: HashMap<String, u32> = HashMap::new();
    for (src, start, end, table, _) in &c.enrichment_byte_spans {
        if start >= end {
            continue;
        }
        by_source.entry(src.clone()).or_default().push(ByteSpan {
            byte_start: *start,
            byte_end: *end,
            contributed_by: vec![(*table).into()],
        });
        *row_count.entry((*table).into()).or_insert(0) += 1;
    }
    let mut total_bytes: u64 = 0;
    let mut spans_by_source: HashMap<String, Vec<ByteSpan>> = HashMap::new();
    for (src, spans) in by_source {
        let coalesced = coalesce(spans, DEFAULT_MAX_GAP_BYTES);
        for s in &coalesced {
            total_bytes += s.byte_end.saturating_sub(s.byte_start);
        }
        spans_by_source.insert(src, coalesced);
    }
    ByteSpanBundle {
        spans_by_source,
        primary_span: SourceByteSpan {
            source_id: c.primary_source_id.clone(),
            byte_start: c.primary_byte_start,
            byte_end: c.primary_byte_end,
        },
        stitched_byte_count: total_bytes,
        row_count_per_table: row_count,
    }
}

// ===========================================================================
// Layer 8 — ProvenanceVerifier
// ===========================================================================

fn verify_provenance(
    byte_store: &dyn SourceByteStore,
    c: &EnrichedCandidate,
    cache: &mut HashMap<(String, u64, u64), bool>,
    out_caveats: &mut Vec<RetrievalCaveat>,
) -> bool {
    if c.primary_blake3.is_empty() {
        return false;
    }
    let key = (
        c.primary_source_id.clone(),
        c.primary_byte_start,
        c.primary_byte_end,
    );
    if let Some(&ok) = cache.get(&key) {
        if !ok {
            out_caveats.push(RetrievalCaveat::StaleRow {
                table: "claims".into(),
                expected_blake3: c.primary_blake3.clone(),
                actual_blake3: String::new(),
            });
        }
        return !ok;
    }
    // Resolve content_hash via primary source. byte_store is keyed on hash,
    // not source_id; we rely on the substrate's source_id == content_hash
    // convention from `crates/thinkingroot-graph/src/source_store.rs` for
    // FileSystemSourceStore. When no row hash matches we treat as unknown
    // (cache as false) so subsequent verify calls don't re-spend disk IO.
    let bytes = byte_store
        .get_range(
            &thinkingroot_core::types::ContentHash(c.primary_source_id.clone()),
            c.primary_byte_start as usize,
            c.primary_byte_end as usize,
        )
        .ok()
        .flatten();
    let stale = match bytes {
        Some(b) => {
            let computed = format!("blake3:{}", blake3::hash(&b).to_hex());
            let mismatch = computed != c.primary_blake3;
            if mismatch {
                out_caveats.push(RetrievalCaveat::StaleRow {
                    table: "claims".into(),
                    expected_blake3: c.primary_blake3.clone(),
                    actual_blake3: computed,
                });
            }
            mismatch
        }
        None => {
            out_caveats.push(RetrievalCaveat::BytesUnavailable {
                source_id: c.primary_source_id.clone(),
                reason: "byte_range_missing".into(),
            });
            false
        }
    };
    cache.insert(key, !stale);
    stale
}

// ===========================================================================
// Hit assembly
// ===========================================================================

fn build_hit(
    c: EnrichedCandidate,
    fused: f32,
    breakdown: ScoreBreakdown,
    bundle: ByteSpanBundle,
    caveats: Vec<RetrievalCaveat>,
) -> RetrievalHit {
    RetrievalHit {
        claim_id: c.claim_id,
        statement: c.statement,
        claim_type: c.claim_type,
        byte_spans: bundle,
        source_blake3s: c.source_blake3s,
        source_authority: c.source_authority,
        source_uri: c.source_uri,
        admission_tier: c.admission_tier,
        trial_scores: c.trial_scores,
        certificate_hash: c.certificate_hash,
        grounding_score: c.grounding_score,
        grounding_method: c.grounding_method,
        valid_window: (c.valid_from, c.valid_until),
        superseded_by_chain: c.superseded_by_chain,
        derivation_parents: c.derivation_parents,
        derivation_root: c.derivation_root,
        sensitivity: c.sensitivity,
        code_signature: c.code_signature,
        code_metrics: c.code_metrics,
        callers: c.callers,
        callees: c.callees,
        doc_tags: c.doc_tags,
        markers: c.markers,
        test_origin: c.test_origin,
        git_blame: c.git_blame,
        quantities: c.quantities,
        related_events: c.related_events,
        cluster_contradictions: c.cluster_contradictions,
        cluster_gaps: c.cluster_gaps,
        fused_score: fused,
        score_breakdown: breakdown,
        caveats,
    }
}

// ===========================================================================
// Tests — pure-layer unit tests. End-to-end integration tests live in
// `crates/thinkingroot-serve/tests/hybrid_e2e.rs`.
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::TrialScores;

    fn now_fixed() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-05-03T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn empty_candidate() -> EnrichedCandidate {
        EnrichedCandidate {
            claim_id: "c1".into(),
            statement: "x".into(),
            claim_type: "fact".into(),
            vector_relevance: 0.5,
            primary_source_id: "src".into(),
            primary_byte_start: 0,
            primary_byte_end: 10,
            primary_blake3: String::new(),
            sensitivity: Sensitivity::Public,
            admission_tier: AdmissionTier::Rooted,
            grounding_score: None,
            grounding_method: None,
            valid_from: None,
            valid_until: None,
            superseded_by_chain: vec![],
            derivation_parents: vec![],
            derivation_root: None,
            source_authority: TrustLevel::Trusted,
            source_uri: "u".into(),
            source_blake3s: vec![],
            trial_scores: None,
            certificate_hash: None,
            code_signature: None,
            code_metrics: None,
            callers: vec![],
            callees: vec![],
            doc_tags: vec![],
            markers: vec![],
            test_origin: None,
            git_blame: vec![],
            quantities: vec![],
            related_events: vec![],
            cluster_contradictions: vec![],
            cluster_gaps: vec![],
            enrichment_byte_spans: vec![],
        }
    }

    fn req() -> RetrievalRequest {
        RetrievalRequest {
            query_text: "x".into(),
            typed_predicates: vec![],
            session_id: "s".into(),
            clearance: vec![Sensitivity::Public],
            top_k: 50,
            time_window: None,
            scoring_profile: ScoringProfile::default(),
            require_certificate: false,
            include_test_origin: false,
            include_quarantined: false,
            require_provenance_verified: false,
            now: Some(now_fixed()),
            scoped_claim_ids: None,
        }
    }

    #[test]
    fn plan_routing_forces_datalog_only_under_threshold() {
        let parsed = ParsedQuery {
            query_text: "anything".into(),
            predicates: vec![],
        };
        let profile = ScoringProfile::default();
        // Threshold lowered to 1 (2026-05-30) so semantic/vector recall engages
        // on small, growing memories (a cognition DB starts near-empty). Only an
        // EMPTY graph (count < 1) now forces datalog-only — so assert with 0.
        let s = plan_routing(&parsed, 0, &profile);
        assert_eq!(s, RoutingShape::DatalogOnlyForced);
    }

    #[test]
    fn plan_routing_picks_vector_first_when_only_text() {
        let parsed = ParsedQuery {
            query_text: "auth".into(),
            predicates: vec![],
        };
        let s = plan_routing(&parsed, 1000, &ScoringProfile::default());
        assert_eq!(s, RoutingShape::VectorFirst);
    }

    #[test]
    fn plan_routing_picks_datalog_first_when_only_predicates() {
        let parsed = ParsedQuery {
            query_text: "".into(),
            predicates: vec![TypedPredicate::ClaimType {
                value: "fact".into(),
            }],
        };
        let s = plan_routing(&parsed, 1000, &ScoringProfile::default());
        assert_eq!(s, RoutingShape::DatalogFirst);
    }

    #[test]
    fn plan_routing_picks_interleaved_when_both() {
        let parsed = ParsedQuery {
            query_text: "auth".into(),
            predicates: vec![TypedPredicate::ClaimType {
                value: "fact".into(),
            }],
        };
        let s = plan_routing(&parsed, 1000, &ScoringProfile::default());
        assert_eq!(s, RoutingShape::Interleaved);
    }

    #[test]
    fn parse_query_extracts_inline_dsl_after_double_at() {
        let mut r = req();
        r.query_text = "auth flow @@ entity:Service AND markers:TODO".into();
        let p = parse_query(&r).unwrap();
        assert_eq!(p.query_text, "auth flow");
        assert_eq!(p.predicates.len(), 2);
    }

    #[test]
    fn fuse_score_drops_non_rooted_when_rooted_only_required() {
        let mut profile = ScoringProfile::default();
        profile.require_rooted_only = true;
        let mut c = empty_candidate();
        c.admission_tier = AdmissionTier::Attested;
        let r = req();
        assert!(fuse_score(&c, &profile, now_fixed(), &r).is_none());
    }

    #[test]
    fn fuse_score_drops_test_origin_when_not_requested() {
        let mut c = empty_candidate();
        c.test_origin = Some(TestAnnotationRef {
            id: "t".into(),
            claim_id: "c1".into(),
            framework: "rust_test".into(),
            annotation_kind: "test".into(),
            name: "n".into(),
        });
        let r = req();
        assert!(fuse_score(&c, &r.scoring_profile, now_fixed(), &r).is_none());
    }

    #[test]
    fn fuse_score_admission_tier_rooted_yields_full_admission_component() {
        let c = empty_candidate(); // Rooted by default
        let r = req();
        let (_, b) = fuse_score(&c, &r.scoring_profile, now_fixed(), &r).expect("scored");
        assert!((b.admission - r.scoring_profile.w_admission * 1.0).abs() < 1e-6);
    }

    #[test]
    fn fuse_score_is_deterministic_across_two_runs() {
        let mut c = empty_candidate();
        c.vector_relevance = 0.42;
        c.trial_scores = Some(TrialScores {
            provenance_score: 0.9,
            contradiction_score: 0.8,
            predicate_score: 0.7,
            topology_score: 0.6,
            temporal_score: 0.5,
        });
        c.valid_from = Some(now_fixed().timestamp() as f64 - 3600.0);
        let r = req();
        let (s1, _) = fuse_score(&c, &r.scoring_profile, now_fixed(), &r).expect("first");
        let (s2, _) = fuse_score(&c, &r.scoring_profile, now_fixed(), &r).expect("second");
        assert_eq!(s1.to_bits(), s2.to_bits(), "f32 bit-equal across runs");
    }

    #[test]
    fn admission_score_maps_each_tier_correctly() {
        assert_eq!(admission_score(AdmissionTier::Rooted), 1.0);
        assert_eq!(admission_score(AdmissionTier::Attested), 0.7);
        assert_eq!(admission_score(AdmissionTier::Quarantined), 0.0);
        assert_eq!(admission_score(AdmissionTier::Rejected), 0.0);
    }

    #[test]
    fn trust_score_increases_monotonically() {
        assert!(trust_score(TrustLevel::Quarantined) < trust_score(TrustLevel::Untrusted));
        assert!(trust_score(TrustLevel::Untrusted) < trust_score(TrustLevel::Unknown));
        assert!(trust_score(TrustLevel::Unknown) < trust_score(TrustLevel::Trusted));
        assert!(trust_score(TrustLevel::Trusted) < trust_score(TrustLevel::Verified));
    }

    #[test]
    fn recency_factor_is_one_when_valid_from_is_none() {
        assert_eq!(recency_factor(None, now_fixed(), 180.0), 1.0);
    }

    #[test]
    fn recency_factor_decays_exponentially() {
        let half_life = 180.0_f32;
        let one_half_life_ago = now_fixed().timestamp() as f64 - (half_life as f64 * 86_400.0);
        let r = recency_factor(Some(one_half_life_ago), now_fixed(), half_life);
        assert!((r - 0.5).abs() < 0.01, "got {r}");
    }

    #[test]
    fn freshness_factor_zeroes_when_valid_until_in_past() {
        let past = now_fixed().timestamp() as f64 - 1.0;
        assert_eq!(freshness_factor(Some(past), now_fixed()), 0.0);
        let future = now_fixed().timestamp() as f64 + 1.0;
        assert_eq!(freshness_factor(Some(future), now_fixed()), 1.0);
        assert_eq!(freshness_factor(None, now_fixed()), 1.0);
    }

    #[test]
    fn merge_candidates_vector_first_drops_outside_datalog_set() {
        let v = vec![("a".into(), 0.9), ("b".into(), 0.8), ("c".into(), 0.7)];
        let dl: HashSet<String> = ["a", "c"].iter().map(|s| s.to_string()).collect();
        let r = req();
        let merged = merge_candidates(v, dl, RoutingShape::VectorFirst, 100, &r);
        let ids: Vec<String> = merged.iter().map(|c| c.claim_id.clone()).collect();
        assert_eq!(ids, vec!["a", "c"]);
    }

    #[test]
    fn merge_candidates_interleaved_intersects() {
        let v = vec![("a".into(), 0.9), ("b".into(), 0.8), ("c".into(), 0.7)];
        let dl: HashSet<String> = ["b", "c"].iter().map(|s| s.to_string()).collect();
        let r = req();
        let merged = merge_candidates(v, dl, RoutingShape::Interleaved, 100, &r);
        let ids: Vec<String> = merged.iter().map(|c| c.claim_id.clone()).collect();
        assert_eq!(ids, vec!["b", "c"]);
    }

    #[test]
    fn merge_candidates_respects_scoped_claim_ids() {
        let v = vec![("a".into(), 0.9), ("b".into(), 0.8)];
        let dl = HashSet::new();
        let mut r = req();
        r.scoped_claim_ids = Some(vec!["b".into()]);
        let merged = merge_candidates(v, dl, RoutingShape::VectorFirst, 100, &r);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].claim_id, "b");
    }

    #[test]
    fn trust_levels_at_least_includes_self_and_higher() {
        let levels = trust_levels_at_least(TrustLevel::Trusted);
        assert_eq!(levels, vec!["Trusted", "Verified"]);
        let levels = trust_levels_at_least(TrustLevel::Quarantined);
        assert_eq!(levels.len(), 5);
    }

    #[test]
    fn complexity_signal_rewards_low_cyclomatic_low_loc() {
        let m = CodeMetricRef {
            source_id: "s".into(),
            scope: "function".into(),
            scope_claim_id: "c".into(),
            loc: 30,
            cyclomatic: 2,
            fan_in: 0,
            fan_out: 0,
            complexity_method: "mccabe".into(),
        };
        let s = complexity_signal(Some(&m));
        // cyc_norm = 2/20 = 0.1; loc_norm = 0 (loc <= 50)
        // expected = 0.5 * 0.9 + 0.5 * 1.0 = 0.95
        assert!((s - 0.95).abs() < 1e-6, "got {s}");
    }

    #[test]
    fn complexity_signal_returns_zero_when_metrics_absent() {
        assert_eq!(complexity_signal(None), 0.0);
    }

    #[test]
    fn junk_guard_rejects_binary_keeps_text() {
        // real human statements pass
        assert!(is_probably_text(
            "The deployment pipeline uses blue-green rollout with a five minute soak window"
        ));
        assert!(is_probably_text("Customer Acme Corp is on the enterprise plan"));
        assert!(is_probably_text("graph TD\n    A[Frontend] --> B[API]"));
        // binary / PDF garbage is rejected
        assert!(!is_probably_text("<</N 3\n/Filter /FlateDecode\n/Length 294>> stream"));
        assert!(!is_probably_text("D\u{0}O\u{1}Z\u{1e}\u{0}\u{5}\u{1a}c\u{6}8q\u{3}3"));
        assert!(!is_probably_text("\u{FFFD}\u{FFFD}\u{FFFD}5\u{13}\u{FFFD}U\u{FFFD}bY"));
        assert!(!is_probably_text("   "));
        assert!(!is_probably_text(""));
    }
}
