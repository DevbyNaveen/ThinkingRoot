//! RARP / Active Engram Protocol v2 — Datalog query catalogue.
//!
//! Spec: `docs/active-engram-protocol.md` §4 (cluster materialisation steps)
//! and §5.3 (per-probe-kind templates). Every query string in this module
//! is a verbatim translation of a numbered step in the spec, sized to the
//! actual CozoDB schema verified at `crates/thinkingroot-graph/src/graph.rs:140-605`.
//!
//! **Phase 4 Witness Mesh transition (2026-05-14).** Per
//! `.claude/rules/aep-v2.md`: the 20 cluster queries + 9 probe
//! templates in this module **deliberately remain on the legacy
//! `claims` substrate** during the dual-write transition. They join
//! against tables (`claim_temporal`, `admission_tier`, `trial_scores`,
//! `contradictions`, `supersession_chain`, `claim_entity_edges`) that
//! the Witness Mesh substrate does not populate. Migrating to the
//! `witnesses` table is the Commit-2 cutover work; that ship will
//! retarget the cluster materialisation onto Witness sub-meshes and
//! collapse `admission_tier` reads into the implicit "admitted by
//! construction" semantics of the Witness model. Until then,
//! workspaces that contain only Witnesses (no legacy claims) will see
//! empty AEP probe answers — surface this honestly via the existing
//! `ProbeCaveat::LowConfidence` path rather than fabricating rows.
//!
//! Conventions:
//!
//! - Queries take parameters via `BTreeMap<String, DataValue>` and execute
//!   through `GraphStore.db.run_script(QUERY, params, ScriptMutability::Immutable)`.
//! - Set membership (`x in $set`) requires `DataValue::List(Vec<DataValue::Str>)`.
//! - Recursive rules (`Q_SUPERSESSION_CHAIN`, `Q_DERIVATION_ROOT`) carry
//!   `parent != child` cycle guards so a self-loop in the source data never
//!   produces a non-terminating fixed-point on Cozo's stratified evaluator.
//! - `now()` is the Cozo built-in that returns epoch seconds — same idiom
//!   the v1 fixture rules use at `graph.rs:6614`.
//!
//! Why const strings (not `:create rule` named-rule registration): the
//! codebase has no precedent for named CozoDB rules and they are
//! per-`DbInstance` state that would need re-registration on every
//! workspace mount. Const strings + `run_script` are the
//! verified-working pattern.

use std::collections::BTreeMap;

use cozo::{DataValue, NamedRows, ScriptMutability};

use crate::graph::GraphStore;
use crate::Error;
use crate::Result;

// ---------------------------------------------------------------------------
// Step §4.2 — entity cluster expansion (2-hop traversal via entity_relations).
// Returns every entity reachable in <= 2 hops from any seed entity, plus the
// seeds themselves. Bound on input size by the seed set; bound on output
// size by the size of `entities` (Cozo's stratified evaluator de-dupes).
// ---------------------------------------------------------------------------
pub const Q_ENTITY_CLUSTER_2HOP: &str = r#"
    seed[entity_id] := entity_id in $seed_set
    hop1[neighbor_id] := seed[entity_id], *entity_relations{from_id: entity_id, to_id: neighbor_id}
    hop1[neighbor_id] := seed[entity_id], *entity_relations{from_id: neighbor_id, to_id: entity_id}
    hop2[neighbor_id] := hop1[mid], *entity_relations{from_id: mid, to_id: neighbor_id}
    hop2[neighbor_id] := hop1[mid], *entity_relations{from_id: neighbor_id, to_id: mid}
    cluster[entity_id] := seed[entity_id]
    cluster[entity_id] := hop1[entity_id]
    cluster[entity_id] := hop2[entity_id]
    ?[entity_id] := cluster[entity_id]
"#;

// ---------------------------------------------------------------------------
// Step §4.3 — alias resolution. Lets `probe_engram` accept "AWS" when the
// canonical entity is "Amazon Web Services" (and vice versa).
// ---------------------------------------------------------------------------
pub const Q_ALIAS_RESOLUTION: &str = r#"
    ?[entity_id, alias] :=
        entity_id in $cluster_set,
        *entity_aliases{entity_id, alias}
"#;

// ---------------------------------------------------------------------------
// Step §4.4 — trust gate (verbatim from existing v1 fixture rule).
// Drops claims with admission_tier in {quarantined, rejected}.
// ---------------------------------------------------------------------------
pub const Q_TRUST_GATE: &str = r#"
    ?[id] := *claims{id, admission_tier},
             admission_tier != 'quarantined',
             admission_tier != 'rejected'
"#;

// ---------------------------------------------------------------------------
// Step §4.5 — source-authority overlay. Returns (claim_id, trust_level)
// for every cluster claim, joining through claim_source_edges → sources.
// ---------------------------------------------------------------------------
pub const Q_SOURCE_AUTHORITY: &str = r#"
    ?[claim_id, source_id, uri, trust_level] :=
        claim_id in $cluster_claim_set,
        *claim_source_edges{claim_id, source_id},
        *sources{id: source_id, uri, trust_level}
"#;

// ---------------------------------------------------------------------------
// Step §4.6a — temporal active claims (verbatim from v1 fixture).
// `valid_until = 0.0` is the sentinel for "never expires".
// ---------------------------------------------------------------------------
pub const Q_TEMPORAL_ACTIVE: &str = r#"
    ?[id] := *claim_temporal{claim_id: id, valid_until},
             valid_until = 0.0 or valid_until > now()
"#;

// ---------------------------------------------------------------------------
// Step §4.6b — supersession chain walk to the terminal claim. New in v2.
//
// Cycle guard: every clause includes `mid != claim_id` / `terminal_id !=
// claim_id` so a self-loop (A → A) cannot extend the relation, and a 3-cycle
// (A → B → C → A) terminates at C without producing the spurious (A, A)
// pair on the next iteration. A claim with no successor is its own terminal
// (covered by `Q_PROBE_*` callers' fallback, not encoded here).
// ---------------------------------------------------------------------------
pub const Q_SUPERSESSION_CHAIN: &str = r#"
    chain[cid, term] :=
        *claim_temporal{claim_id: cid, superseded_by: term},
        term != '',
        term != cid,
        *claim_temporal{claim_id: term, superseded_by: ''}
    chain[cid, term] :=
        *claim_temporal{claim_id: cid, superseded_by: mid},
        mid != cid,
        chain[mid, term],
        term != cid
    ?[cid, term] := chain[cid, term], cid in $cluster_claim_set
"#;

// ---------------------------------------------------------------------------
// Step §4.7 — unresolved contradictions touching the cluster.
// Surfaces them as caveats — v1 silently auto-resolved.
// ---------------------------------------------------------------------------
pub const Q_CONTRADICTIONS: &str = r#"
    ?[id, claim_a, claim_b, explanation, status] :=
        claim_a in $cluster_claim_set,
        *contradictions{id, claim_a, claim_b, explanation, status},
        status != 'Resolved'
    ?[id, claim_a, claim_b, explanation, status] :=
        claim_b in $cluster_claim_set,
        *contradictions{id, claim_a, claim_b, explanation, status},
        status != 'Resolved'
"#;

// ---------------------------------------------------------------------------
// Step §4.8 — events window scan. Pulls SVO triples for cluster entities
// within the temporal window passed by the caller.
// ---------------------------------------------------------------------------
pub const Q_EVENTS_WINDOW: &str = r#"
    ?[id, subject_entity_id, verb, object_entity_id, timestamp, normalized_date, source_id] :=
        *events{id, subject_entity_id, verb, object_entity_id, timestamp, normalized_date, source_id},
        subject_entity_id in $cluster_set or object_entity_id in $cluster_set,
        timestamp >= $window_start,
        timestamp < $window_end
"#;

// ---------------------------------------------------------------------------
// Step §4.9 — structural-pattern overlay. Filters Reflect-discovered
// patterns to those matching cluster entity types and meeting the per-row
// `min_sample_threshold`.
// ---------------------------------------------------------------------------
pub const Q_PATTERN_OVERLAY: &str = r#"
    ?[id, entity_type, condition_claim_type, expected_claim_type, frequency, sample_size] :=
        entity_id in $cluster_set,
        *entities{id: entity_id, entity_type},
        *structural_patterns{id, entity_type, condition_claim_type, expected_claim_type,
                             frequency, sample_size, min_sample_threshold},
        sample_size >= min_sample_threshold
"#;

// ---------------------------------------------------------------------------
// Step §4.10 — known-unknowns gap scan (verbatim from v1 fixture, with
// cluster filter added).
// ---------------------------------------------------------------------------
pub const Q_GAP_SCAN: &str = r#"
    ?[id, entity_id, pattern_id, expected_claim_type, confidence] :=
        entity_id in $cluster_set,
        *known_unknowns{id, entity_id, pattern_id, expected_claim_type,
                        confidence, status: 'open'}
"#;

// ---------------------------------------------------------------------------
// Step §4.11 — code call graph traversal. function_calls.callee_claim_id is
// resolved at Phase 7e; rows where it's empty are external calls and still
// surface (the call is real even if the callee isn't in this workspace).
// ---------------------------------------------------------------------------
pub const Q_CALL_GRAPH: &str = r#"
    ?[caller_claim_id, callee_name, callee_claim_id, source_id, byte_start, byte_end] :=
        caller_claim_id in $cluster_claim_set,
        *function_calls{caller_claim_id, callee_name, callee_claim_id, source_id, byte_start, byte_end}
"#;

// ---------------------------------------------------------------------------
// Step §4.12 — doc-tag overlay (@param/@returns/@throws/@deprecated/@see).
// ---------------------------------------------------------------------------
pub const Q_DOC_TAGS: &str = r#"
    ?[claim_id, kind, target, description] :=
        claim_id in $cluster_claim_set,
        *doc_tags{claim_id, kind, target, description}
"#;

// ---------------------------------------------------------------------------
// Step §4.13 — code markers (TODO/FIXME/HACK/SAFETY/NOTE/XXX/BUG/PERF).
// ---------------------------------------------------------------------------
pub const Q_CODE_MARKERS: &str = r#"
    ?[id, source_id, kind, text, in_claim_id, byte_start, byte_end] :=
        in_claim_id in $cluster_claim_set,
        *code_markers{id, source_id, kind, text, in_claim_id, byte_start, byte_end}
"#;

// ---------------------------------------------------------------------------
// Step §4.14 — test-annotation gating. Lets the probe layer mark answers
// originating in test code with the `DerivedFromTest` caveat.
// ---------------------------------------------------------------------------
pub const Q_TEST_ORIGINS: &str = r#"
    ?[id, claim_id, framework, annotation_kind, name] :=
        claim_id in $cluster_claim_set,
        *test_annotations{id, claim_id, framework, annotation_kind, name}
"#;

// ---------------------------------------------------------------------------
// Step §4.15 — git_blame join. For "who introduced this?" probes.
// Composite-key on (source_id, line_start, line_end) — there can be many
// blame hunks per source.
// ---------------------------------------------------------------------------
pub const Q_GIT_BLAME: &str = r#"
    ?[source_id, line_start, line_end, commit_sha, author, author_email, blamed_at, byte_start, byte_end] :=
        claim_id in $cluster_claim_set,
        *claim_source_edges{claim_id, source_id},
        *git_blame{source_id, line_start, line_end, commit_sha, author, author_email,
                   blamed_at, byte_start, byte_end}
"#;

// ---------------------------------------------------------------------------
// Step §4.16 — code-metrics overlay (LOC, cyclomatic, fan-in/out).
// scope_claim_id empty for file-scope rows; cluster filter joins on the
// per-function/per-type rows.
// ---------------------------------------------------------------------------
pub const Q_CODE_METRICS: &str = r#"
    ?[source_id, scope, scope_claim_id, loc, cyclomatic, fan_in, fan_out, complexity_method] :=
        scope_claim_id in $cluster_claim_set,
        *code_metrics{source_id, scope, scope_claim_id, loc, cyclomatic, fan_in, fan_out, complexity_method}
"#;

// ---------------------------------------------------------------------------
// Step §4.17 — quantitative overlay. Returns typed numeric scalars.
// ---------------------------------------------------------------------------
pub const Q_QUANTITIES: &str = r#"
    ?[claim_id, metric_name, value, unit, qualifier, is_live, captured_at] :=
        claim_id in $cluster_claim_set,
        *quantities{claim_id, metric_name, value, unit, qualifier, is_live, captured_at}
"#;

// ---------------------------------------------------------------------------
// Step §4.18 — sensitivity filter. Drops claims whose sensitivity is not
// in the caller's clearance set; the caller emits a `SensitivityRedaction`
// caveat counting how many were dropped (set difference computed in Rust,
// not Datalog, since `count` requires aggregation).
// ---------------------------------------------------------------------------
pub const Q_SENSITIVITY_FILTER: &str = r#"
    ?[id, statement, sensitivity] :=
        id in $cluster_claim_set,
        *claims{id, statement, sensitivity},
        sensitivity in $caller_clearance_set
"#;

// ---------------------------------------------------------------------------
// Step §4.19 — derivation root walk. New in v2.
//
// Cycle guard: every clause requires `mid != child_claim_id` /
// `root_claim_id != child_claim_id`. A self-edge (A → A) cannot extend the
// relation; a cycle (A → B → C → A) terminates without producing
// (A, A). Same shape as `Q_SUPERSESSION_CHAIN` — see that doc-comment.
// ---------------------------------------------------------------------------
pub const Q_DERIVATION_ROOT: &str = r#"
    edge[parent, child] := *derivation_edges{parent_claim_id: parent, child_claim_id: child}
    has_parent[c] := edge[_parent, c]
    droot[child, root] :=
        edge[root, child],
        root != child,
        not has_parent[root]
    droot[child, root] :=
        edge[mid, child],
        mid != child,
        droot[mid, root],
        root != child
    ?[child, root] := droot[child, root], child in $cluster_claim_set
"#;

// ===========================================================================
// §5.3 — Per-probe-kind templates.
//
// Each template returns a typed row tuple sized for the corresponding
// `AnswerRow` variant in `engine.rs`. Parameter `$cluster_set` carries the
// Engram's entity ids; `$cluster_claim_set` carries the claim ids. Probe
// templates pull whatever provenance is needed for the matching `AnswerRow`
// shape — never less, so `ProbeAnswer` fields are never silently empty
// because the Datalog underwove the join.
// ===========================================================================

/// Factual probe — "what is the value of X?" Returns claims gated by the
/// trust filter, with full provenance for every returned row.
pub const Q_PROBE_FACTUAL: &str = r#"
    ?[statement, claim_id, source_id, byte_start, byte_end, content_blake3,
      admission_tier, sensitivity] :=
        *claims{id: claim_id, statement, claim_type, admission_tier, sensitivity,
                byte_start, byte_end, content_blake3},
        *claim_entity_edges{claim_id, entity_id},
        entity_id in $cluster_set,
        *claim_source_edges{claim_id, source_id},
        admission_tier != 'quarantined',
        admission_tier != 'rejected'
"#;

/// Quantitative probe — "how much / how fast?" Returns typed numeric scalars
/// from `quantities` joined to claims gated by trust + sensitivity.
pub const Q_PROBE_QUANTITATIVE: &str = r#"
    ?[metric_name, value, unit, qualifier, is_live, claim_id,
      source_id, byte_start, byte_end, content_blake3, sensitivity] :=
        *quantities{claim_id, metric_name, value, unit, qualifier, is_live,
                    source_id, byte_start, byte_end, content_blake3},
        *claim_entity_edges{claim_id, entity_id},
        entity_id in $cluster_set,
        *claims{id: claim_id, sensitivity, admission_tier},
        admission_tier != 'quarantined',
        admission_tier != 'rejected'
"#;

/// Temporal probe — "when did X happen?" SVO triples within window.
pub const Q_PROBE_TEMPORAL: &str = r#"
    ?[subject, verb, object, timestamp, normalized_date,
      source_id, byte_start, byte_end] :=
        *events{subject_entity_id: subject, verb, object_entity_id: object,
                timestamp, normalized_date, source_id},
        timestamp >= $window_start,
        timestamp < $window_end,
        subject in $cluster_set,
        *claims{id: claim_id, source_id, byte_start, byte_end},
        *claim_source_edges{claim_id, source_id}
    ?[subject, verb, object, timestamp, normalized_date,
      source_id, byte_start, byte_end] :=
        *events{subject_entity_id: subject, verb, object_entity_id: object,
                timestamp, normalized_date, source_id},
        timestamp >= $window_start,
        timestamp < $window_end,
        object in $cluster_set,
        *claims{id: claim_id, source_id, byte_start, byte_end},
        *claim_source_edges{claim_id, source_id}
"#;

/// Authorship probe — "who wrote / introduced X?" git_blame joined via
/// claim_source_edges.
pub const Q_PROBE_AUTHORSHIP: &str = r#"
    ?[author, blamed_at, commit_sha, source_id, line_start, line_end,
      byte_start, byte_end] :=
        claim_id in $cluster_claim_set,
        *claim_source_edges{claim_id, source_id},
        *git_blame{source_id, line_start, line_end, commit_sha, author,
                   blamed_at, byte_start, byte_end}
"#;

/// Structural probe — "what's the shape of function X?" code_signatures.
pub const Q_PROBE_STRUCTURAL: &str = r#"
    ?[parameters_json, return_type, visibility, trait_name, parent_scope,
      field_types_json, claim_id, source_id, byte_start, byte_end] :=
        claim_id in $cluster_claim_set,
        *code_signatures{claim_id, parameters_json, return_type, visibility,
                         trait_name, parent_scope, field_types_json,
                         source_id, byte_start, byte_end}
"#;

/// Relation-graph probe (callers): who calls `$target`?
pub const Q_PROBE_RELATION_CALLERS: &str = r#"
    ?[caller_claim_id, callee_name, source_id, byte_start, byte_end] :=
        *function_calls{caller_claim_id, callee_name, callee_claim_id: $target,
                        source_id, byte_start, byte_end}
"#;

/// Relation-graph probe (refs): inbound source_references for `$target`.
pub const Q_PROBE_RELATION_REFS: &str = r#"
    ?[from_source_id, reference_kind, fragment, byte_start, byte_end] :=
        *source_references{from_source_id, to_source_id: $target, reference_kind,
                           fragment, byte_start, byte_end}
"#;

/// Existential probe — "is there an X with property P?" Returns one
/// witness claim_id when present (the caller materialises the boolean).
pub const Q_PROBE_EXISTENTIAL: &str = r#"
    ?[claim_id] :=
        claim_id in $cluster_claim_set,
        *claims{id: claim_id, claim_type, admission_tier},
        claim_type = $claim_type,
        admission_tier != 'quarantined',
        admission_tier != 'rejected'
"#;

/// Comparative probe — pair claims of matching `claim_type` across two
/// cluster sets. The caller pairs them in Rust by entity proximity.
pub const Q_PROBE_COMPARATIVE: &str = r#"
    ?[claim_id, statement, claim_type, source_id, byte_start, byte_end, side] :=
        *claims{id: claim_id, statement, claim_type, byte_start, byte_end, admission_tier},
        *claim_entity_edges{claim_id, entity_id},
        *claim_source_edges{claim_id, source_id},
        entity_id in $set_a,
        side = 'a',
        admission_tier != 'rejected'
    ?[claim_id, statement, claim_type, source_id, byte_start, byte_end, side] :=
        *claims{id: claim_id, statement, claim_type, byte_start, byte_end, admission_tier},
        *claim_entity_edges{claim_id, entity_id},
        *claim_source_edges{claim_id, source_id},
        entity_id in $set_b,
        side = 'b',
        admission_tier != 'rejected'
"#;

// Counterfactual probe reuses Q_DERIVATION_ROOT walked forward (the caller
// finds descendants of $target via the same recursive shape with parent and
// child swapped). No separate const because the walk direction is a
// rust-side choice.

// ---------------------------------------------------------------------------
// Parameter helpers — turn slices of strings into `DataValue::List` for use
// with set-membership predicates.
// ---------------------------------------------------------------------------

/// Build a `DataValue::List<DataValue::Str>` for an `x in $set` predicate.
pub fn dv_str_list<S: AsRef<str>>(items: &[S]) -> DataValue {
    DataValue::List(
        items
            .iter()
            .map(|s| DataValue::Str(s.as_ref().into()))
            .collect(),
    )
}

/// Convenience: run a parameterised AEP query against the given store.
/// Wraps `db.run_script` with the immutable script flag so callers can
/// stay focused on the query string + params.
///
/// On failure the error message includes the first 80 chars of the query
/// itself so log readers can identify which AEP query went wrong without
/// having to instrument every call site.
pub fn run_aep(
    graph: &GraphStore,
    query: &str,
    params: BTreeMap<String, DataValue>,
) -> Result<NamedRows> {
    graph
        .raw_db()
        .run_script(query, params, ScriptMutability::Immutable)
        .map_err(|e| {
            // First non-empty trimmed line gives a recognisable preview
            // (e.g. `?[id, statement, sensitivity] :=`).
            let preview = query
                .lines()
                .map(|s| s.trim())
                .find(|s| !s.is_empty())
                .map(|s| {
                    if s.len() > 100 {
                        format!("{}…", &s[..100])
                    } else {
                        s.to_string()
                    }
                })
                .unwrap_or_else(|| "(empty query)".to_string());
            Error::GraphStorage(format!("aep query failed [{preview}]: {e}"))
        })
}

// ===========================================================================
// Tests — golden-fixture pattern mirroring graph.rs:6531. Each new query
// gets at least one assertion against a curated fixture so a future schema
// rename or column drop fails *here* loudly rather than silently underweaving
// at probe time.
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::GraphStore;
    use cozo::{DbInstance, Num};

    fn mem_store() -> GraphStore {
        let db = DbInstance::new("mem", "", "")
            .expect("mem cozo db must open");
        let store = GraphStore::from_db_for_testing(db);
        store.init_for_testing().expect("init must succeed");
        store
    }

    fn dv_to_string(val: &DataValue) -> String {
        match val {
            DataValue::Str(s) => s.to_string(),
            DataValue::Num(Num::Int(i)) => i.to_string(),
            DataValue::Num(Num::Float(f)) => format!("{f}"),
            DataValue::Bool(b) => b.to_string(),
            other => format!("{other:?}"),
        }
    }

    fn seed_basic_cluster(store: &GraphStore) {
        // 3 claims linked to e-auth + e-db; 2 entities + alias.
        store
            .raw_db()
            .run_default(
                r#"?[id, statement, claim_type, source_id, admission_tier, sensitivity, byte_start, byte_end, content_blake3] <- [
                    ['c-1', 'config bound', 'configuration', 's1', 'rooted',      'public',     0,   10, 'blake3:aa'],
                    ['c-2', 'pii email',    'observation',   's1', 'attested',    'restricted', 11,  20, 'blake3:bb'],
                    ['c-3', 'rejected one', 'observation',   's1', 'rejected',    'public',     21,  30, 'blake3:cc']
                ]
                :put claims {id => statement, claim_type, source_id, admission_tier, sensitivity, byte_start, byte_end, content_blake3}"#,
            )
            .expect("seed claims");
        store
            .raw_db()
            .run_default(
                r#"?[id, canonical_name, entity_type] <- [
                    ['e-auth', 'AuthService', 'service'],
                    ['e-db',   'Database',    'service']
                ]
                :put entities {id => canonical_name, entity_type}"#,
            )
            .expect("seed entities");
        store
            .raw_db()
            .run_default(
                r#"?[claim_id, entity_id] <- [
                    ['c-1', 'e-auth'],
                    ['c-2', 'e-auth'],
                    ['c-3', 'e-db']
                ]
                :put claim_entity_edges {claim_id, entity_id}"#,
            )
            .expect("seed claim_entity_edges");
        store
            .raw_db()
            .run_default(
                r#"?[claim_id, source_id] <- [
                    ['c-1', 's1'],
                    ['c-2', 's1'],
                    ['c-3', 's1']
                ]
                :put claim_source_edges {claim_id, source_id}"#,
            )
            .expect("seed claim_source_edges");
        store
            .raw_db()
            .run_default(
                r#"?[id, uri, source_type, trust_level] <- [
                    ['s1', 'file://test.rs', 'code', 'Verified']
                ]
                :put sources {id => uri, source_type, trust_level}"#,
            )
            .expect("seed sources");
        store
            .raw_db()
            .run_default(
                r#"?[from_id, to_id, relation_type] <- [
                    ['e-auth', 'e-db', 'depends_on']
                ]
                :put entity_relations {from_id, to_id, relation_type}"#,
            )
            .expect("seed entity_relations");
        store
            .raw_db()
            .run_default(
                r#"?[entity_id, alias] <- [
                    ['e-auth', 'AWS-Auth']
                ]
                :put entity_aliases {entity_id, alias}"#,
            )
            .expect("seed entity_aliases");
    }

    fn cluster_set() -> BTreeMap<String, DataValue> {
        let mut p = BTreeMap::new();
        p.insert("seed_set".into(), dv_str_list(&["e-auth"]));
        p.insert("cluster_set".into(), dv_str_list(&["e-auth", "e-db"]));
        p.insert(
            "cluster_claim_set".into(),
            dv_str_list(&["c-1", "c-2", "c-3"]),
        );
        p
    }

    // ─── Cluster queries (Steps §4.2 – §4.19) ──────────────────────────────

    #[test]
    fn q_entity_cluster_2hop_returns_self_and_neighbors() {
        let store = mem_store();
        seed_basic_cluster(&store);
        let mut p = BTreeMap::new();
        p.insert("seed_set".into(), dv_str_list(&["e-auth"]));
        let r = run_aep(&store, Q_ENTITY_CLUSTER_2HOP, p).expect("query runs");
        let ids: Vec<String> = r.rows.iter().map(|row| dv_to_string(&row[0])).collect();
        assert!(ids.contains(&"e-auth".to_string()));
        assert!(ids.contains(&"e-db".to_string()));
    }

    #[test]
    fn q_alias_resolution_returns_alias_for_cluster_entity() {
        let store = mem_store();
        seed_basic_cluster(&store);
        let r = run_aep(&store, Q_ALIAS_RESOLUTION, cluster_set()).expect("runs");
        assert!(r.rows.iter().any(|row| dv_to_string(&row[1]) == "AWS-Auth"));
    }

    #[test]
    fn q_trust_gate_drops_quarantined_and_rejected() {
        let store = mem_store();
        seed_basic_cluster(&store);
        let r = run_aep(&store, Q_TRUST_GATE, BTreeMap::new()).expect("runs");
        let ids: Vec<String> = r.rows.iter().map(|row| dv_to_string(&row[0])).collect();
        assert!(ids.contains(&"c-1".to_string()));
        assert!(ids.contains(&"c-2".to_string()));
        assert!(!ids.contains(&"c-3".to_string()), "rejected must be dropped");
    }

    #[test]
    fn q_source_authority_joins_through_edges() {
        let store = mem_store();
        seed_basic_cluster(&store);
        let r = run_aep(&store, Q_SOURCE_AUTHORITY, cluster_set()).expect("runs");
        assert_eq!(r.rows.len(), 3, "every cluster claim has one source row");
        for row in &r.rows {
            assert_eq!(dv_to_string(&row[3]), "Verified");
        }
    }

    #[test]
    fn q_temporal_active_keeps_never_expire_and_future() {
        let store = mem_store();
        seed_basic_cluster(&store);
        store
            .raw_db()
            .run_default(
                r#"?[claim_id, valid_from, valid_until, superseded_by] <- [
                    ['c-1', 0.0, 0.0,            ''],
                    ['c-2', 0.0, 99999999999.0,  ''],
                    ['c-3', 0.0, 1.0,            '']
                ]
                :put claim_temporal {claim_id => valid_from, valid_until, superseded_by}"#,
            )
            .expect("seed temporal");
        let r = run_aep(&store, Q_TEMPORAL_ACTIVE, BTreeMap::new()).expect("runs");
        let ids: Vec<String> = r.rows.iter().map(|row| dv_to_string(&row[0])).collect();
        assert!(ids.contains(&"c-1".to_string()));
        assert!(ids.contains(&"c-2".to_string()));
        assert!(!ids.contains(&"c-3".to_string()), "expired must be dropped");
    }

    #[test]
    fn q_supersession_chain_returns_terminal_for_two_step() {
        let store = mem_store();
        seed_basic_cluster(&store);
        // c-1 → c-2 (terminal).
        store
            .raw_db()
            .run_default(
                r#"?[claim_id, valid_from, valid_until, superseded_by] <- [
                    ['c-1', 0.0, 0.0, 'c-2'],
                    ['c-2', 0.0, 0.0, '']
                ]
                :put claim_temporal {claim_id => valid_from, valid_until, superseded_by}"#,
            )
            .expect("seed");
        let r = run_aep(&store, Q_SUPERSESSION_CHAIN, cluster_set()).expect("runs");
        let pairs: Vec<(String, String)> = r
            .rows
            .iter()
            .map(|row| (dv_to_string(&row[0]), dv_to_string(&row[1])))
            .collect();
        assert!(pairs.contains(&("c-1".to_string(), "c-2".to_string())));
    }

    #[test]
    fn q_supersession_chain_terminates_on_self_loop() {
        let store = mem_store();
        seed_basic_cluster(&store);
        // c-1 → c-1 (self-loop) — the cycle guard must drop this row.
        store
            .raw_db()
            .run_default(
                r#"?[claim_id, valid_from, valid_until, superseded_by] <- [
                    ['c-1', 0.0, 0.0, 'c-1']
                ]
                :put claim_temporal {claim_id => valid_from, valid_until, superseded_by}"#,
            )
            .expect("seed");
        let r = run_aep(&store, Q_SUPERSESSION_CHAIN, cluster_set()).expect("must terminate");
        // The (c-1, c-1) self-pair must NOT appear because of `term != cid`.
        for row in &r.rows {
            assert!(
                !(dv_to_string(&row[0]) == "c-1" && dv_to_string(&row[1]) == "c-1"),
                "self-loop must be filtered"
            );
        }
    }

    #[test]
    fn q_call_graph_returns_cluster_calls() {
        let store = mem_store();
        seed_basic_cluster(&store);
        store
            .raw_db()
            .run_default(
                r#"?[id, caller_claim_id, callee_name, callee_claim_id, source_id, byte_start, byte_end, content_blake3] <- [
                    ['fc-1', 'c-1', 'login', 'c-2', 's1', 0, 10, 'blake3:aa']
                ]
                :put function_calls {id => caller_claim_id, callee_name, callee_claim_id, source_id, byte_start, byte_end, content_blake3}"#,
            )
            .expect("seed");
        let r = run_aep(&store, Q_CALL_GRAPH, cluster_set()).expect("runs");
        assert_eq!(r.rows.len(), 1);
        assert_eq!(dv_to_string(&r.rows[0][1]), "login");
    }

    #[test]
    fn q_doc_tags_returns_cluster_tags() {
        let store = mem_store();
        seed_basic_cluster(&store);
        store
            .raw_db()
            .run_default(
                r#"?[id, claim_id, kind, target, description, source_id, byte_start, byte_end, content_blake3] <- [
                    ['dt-1', 'c-1', 'deprecated', 'login', 'use login_v2 instead', 's1', 0, 10, 'blake3:aa']
                ]
                :put doc_tags {id => claim_id, kind, target, description, source_id, byte_start, byte_end, content_blake3}"#,
            )
            .expect("seed");
        let r = run_aep(&store, Q_DOC_TAGS, cluster_set()).expect("runs");
        assert_eq!(r.rows.len(), 1);
        assert_eq!(dv_to_string(&r.rows[0][1]), "deprecated");
    }

    #[test]
    fn q_code_markers_finds_todo_in_cluster_claim() {
        let store = mem_store();
        seed_basic_cluster(&store);
        store
            .raw_db()
            .run_default(
                r#"?[id, source_id, kind, text, in_claim_id, byte_start, byte_end, content_blake3] <- [
                    ['m-1', 's1', 'TODO', 'rate-limit me', 'c-1', 0, 10, 'blake3:aa']
                ]
                :put code_markers {id => source_id, kind, text, in_claim_id, byte_start, byte_end, content_blake3}"#,
            )
            .expect("seed");
        let r = run_aep(&store, Q_CODE_MARKERS, cluster_set()).expect("runs");
        assert_eq!(r.rows.len(), 1);
        assert_eq!(dv_to_string(&r.rows[0][2]), "TODO");
    }

    #[test]
    fn q_test_origins_finds_test_annotation() {
        let store = mem_store();
        seed_basic_cluster(&store);
        store
            .raw_db()
            .run_default(
                r#"?[id, source_id, claim_id, framework, annotation_kind, name, byte_start, byte_end, content_blake3] <- [
                    ['ta-1', 's1', 'c-1', 'rust_test', 'test', 'login_succeeds', 0, 10, 'blake3:aa']
                ]
                :put test_annotations {id => source_id, claim_id, framework, annotation_kind, name, byte_start, byte_end, content_blake3}"#,
            )
            .expect("seed");
        let r = run_aep(&store, Q_TEST_ORIGINS, cluster_set()).expect("runs");
        assert_eq!(r.rows.len(), 1);
        assert_eq!(dv_to_string(&r.rows[0][2]), "rust_test");
    }

    #[test]
    fn q_git_blame_joins_via_claim_source_edges() {
        let store = mem_store();
        seed_basic_cluster(&store);
        store
            .raw_db()
            .run_default(
                r#"?[source_id, line_start, line_end, commit_sha, author, author_email, blamed_at, byte_start, byte_end, content_blake3] <- [
                    ['s1', 1, 10, 'sha-abc', 'Alice', 'a@x', 12345.0, 0, 100, 'blake3:bb']
                ]
                :put git_blame {source_id, line_start, line_end => commit_sha, author, author_email, blamed_at, byte_start, byte_end, content_blake3}"#,
            )
            .expect("seed");
        let r = run_aep(&store, Q_GIT_BLAME, cluster_set()).expect("runs");
        assert!(r.rows.len() >= 1);
        assert_eq!(dv_to_string(&r.rows[0][4]), "Alice");
    }

    #[test]
    fn q_code_metrics_returns_cluster_metrics() {
        let store = mem_store();
        seed_basic_cluster(&store);
        store
            .raw_db()
            .run_default(
                r#"?[id, source_id, scope, scope_claim_id, loc, cyclomatic, fan_in, fan_out, complexity_method, byte_start, byte_end, content_blake3] <- [
                    ['cm-1', 's1', 'function', 'c-1', 42, 3, 1, 2, 'mccabe', 0, 100, 'blake3:cc']
                ]
                :put code_metrics {id => source_id, scope, scope_claim_id, loc, cyclomatic, fan_in, fan_out, complexity_method, byte_start, byte_end, content_blake3}"#,
            )
            .expect("seed");
        let r = run_aep(&store, Q_CODE_METRICS, cluster_set()).expect("runs");
        assert_eq!(r.rows.len(), 1);
        assert_eq!(dv_to_string(&r.rows[0][1]), "function");
    }

    #[test]
    fn q_quantities_returns_typed_values() {
        let store = mem_store();
        seed_basic_cluster(&store);
        store
            .raw_db()
            .run_default(
                r#"?[id, claim_id, metric_name, value, unit, qualifier, is_live, captured_at, source_id, byte_start, byte_end, content_blake3] <- [
                    ['q-1', 'c-1', 'latency', 120.0, 'ms', 'p99', true, 12345.0, 's1', 0, 10, 'blake3:dd']
                ]
                :put quantities {id => claim_id, metric_name, value, unit, qualifier, is_live, captured_at, source_id, byte_start, byte_end, content_blake3}"#,
            )
            .expect("seed");
        let r = run_aep(&store, Q_QUANTITIES, cluster_set()).expect("runs");
        assert_eq!(r.rows.len(), 1);
        assert_eq!(dv_to_string(&r.rows[0][1]), "latency");
    }

    #[test]
    fn q_sensitivity_filter_drops_restricted_when_only_public_cleared() {
        let store = mem_store();
        seed_basic_cluster(&store);
        let mut p = cluster_set();
        p.insert("caller_clearance_set".into(), dv_str_list(&["public"]));
        let r = run_aep(&store, Q_SENSITIVITY_FILTER, p).expect("runs");
        let ids: Vec<String> = r.rows.iter().map(|row| dv_to_string(&row[0])).collect();
        assert!(ids.contains(&"c-1".to_string()), "public claim passes");
        assert!(!ids.contains(&"c-2".to_string()), "restricted dropped");
    }

    #[test]
    fn q_sensitivity_filter_passes_restricted_when_cleared() {
        let store = mem_store();
        seed_basic_cluster(&store);
        let mut p = cluster_set();
        p.insert(
            "caller_clearance_set".into(),
            dv_str_list(&["public", "restricted"]),
        );
        let r = run_aep(&store, Q_SENSITIVITY_FILTER, p).expect("runs");
        let ids: Vec<String> = r.rows.iter().map(|row| dv_to_string(&row[0])).collect();
        assert!(ids.contains(&"c-1".to_string()));
        assert!(ids.contains(&"c-2".to_string()));
    }

    #[test]
    fn q_derivation_root_returns_root_for_two_step() {
        let store = mem_store();
        seed_basic_cluster(&store);
        // root = c-3, mid = c-2, child = c-1: c-3 → c-2 → c-1.
        store
            .raw_db()
            .run_default(
                r#"?[parent_claim_id, child_claim_id, derivation_rule] <- [
                    ['c-3', 'c-2', 'reflect'],
                    ['c-2', 'c-1', 'reflect']
                ]
                :put derivation_edges {parent_claim_id, child_claim_id => derivation_rule}"#,
            )
            .expect("seed");
        let r = run_aep(&store, Q_DERIVATION_ROOT, cluster_set()).expect("runs");
        let pairs: Vec<(String, String)> = r
            .rows
            .iter()
            .map(|row| (dv_to_string(&row[0]), dv_to_string(&row[1])))
            .collect();
        assert!(pairs.contains(&("c-1".to_string(), "c-3".to_string())));
    }

    #[test]
    fn q_derivation_root_handles_empty_derivation_edges() {
        // Pins the production bug surfaced 2026-05-07: when
        // derivation_edges has zero rows (the common case for a fresh
        // workspace that's never had Reflect compose claims),
        // Q_DERIVATION_ROOT must return zero rows without raising
        // "Symbol '~1' in rule head is unbound".
        let store = mem_store();
        // No seeding — derivation_edges schema exists from `init_for_testing`
        // but has no rows.
        let r = run_aep(&store, Q_DERIVATION_ROOT, cluster_set())
            .expect("Q_DERIVATION_ROOT must run cleanly against empty derivation_edges");
        assert_eq!(
            r.rows.len(),
            0,
            "empty derivation_edges must produce zero roots, not crash the rule"
        );
    }

    #[test]
    fn q_derivation_root_terminates_on_three_cycle() {
        let store = mem_store();
        seed_basic_cluster(&store);
        // c-1 → c-2 → c-3 → c-1 forms a cycle. With the cycle guard the
        // recursion still terminates without producing self-pairs.
        store
            .raw_db()
            .run_default(
                r#"?[parent_claim_id, child_claim_id, derivation_rule] <- [
                    ['c-1', 'c-2', 'r'],
                    ['c-2', 'c-3', 'r'],
                    ['c-3', 'c-1', 'r']
                ]
                :put derivation_edges {parent_claim_id, child_claim_id => derivation_rule}"#,
            )
            .expect("seed");
        let r = run_aep(&store, Q_DERIVATION_ROOT, cluster_set())
            .expect("recursion must terminate");
        for row in &r.rows {
            assert_ne!(
                dv_to_string(&row[0]),
                dv_to_string(&row[1]),
                "no claim is its own derivation root in a pure cycle"
            );
        }
    }

    #[test]
    fn q_contradictions_surfaces_unresolved() {
        let store = mem_store();
        seed_basic_cluster(&store);
        store
            .raw_db()
            .run_default(
                r#"?[id, claim_a, claim_b, explanation, status, detected_at] <- [
                    ['contra-1', 'c-1', 'c-2', 'config disagrees', 'Detected', 0.0],
                    ['contra-2', 'c-2', 'c-3', 'irrelevant',       'Resolved', 0.0]
                ]
                :put contradictions {id => claim_a, claim_b, explanation, status, detected_at}"#,
            )
            .expect("seed");
        let r = run_aep(&store, Q_CONTRADICTIONS, cluster_set()).expect("runs");
        let ids: Vec<String> = r.rows.iter().map(|row| dv_to_string(&row[0])).collect();
        assert!(ids.contains(&"contra-1".to_string()));
        assert!(!ids.contains(&"contra-2".to_string()), "Resolved excluded");
    }

    #[test]
    fn q_events_window_filters_by_timestamp() {
        let store = mem_store();
        seed_basic_cluster(&store);
        store
            .raw_db()
            .run_default(
                r#"?[id, subject_entity_id, verb, object_entity_id, timestamp, normalized_date, source_id] <- [
                    ['ev-1', 'e-auth', 'deployed', 'e-db', 100.0, '2026-01-01', 's1'],
                    ['ev-2', 'e-auth', 'rolled_back', 'e-db', 1000.0, '2026-02-01', 's1']
                ]
                :put events {id => subject_entity_id, verb, object_entity_id, timestamp, normalized_date, source_id}"#,
            )
            .expect("seed");
        let mut p = cluster_set();
        p.insert("window_start".into(), DataValue::Num(Num::Float(50.0)));
        p.insert("window_end".into(), DataValue::Num(Num::Float(500.0)));
        let r = run_aep(&store, Q_EVENTS_WINDOW, p).expect("runs");
        let ids: Vec<String> = r.rows.iter().map(|row| dv_to_string(&row[0])).collect();
        assert!(ids.contains(&"ev-1".to_string()));
        assert!(!ids.contains(&"ev-2".to_string()), "outside window");
    }

    #[test]
    fn q_pattern_overlay_filters_below_threshold() {
        let store = mem_store();
        seed_basic_cluster(&store);
        store
            .raw_db()
            .run_default(
                r#"?[id, entity_type, condition_claim_type, expected_claim_type, frequency, sample_size, last_computed, min_sample_threshold, first_seen_at, stability_runs, source_scope] <- [
                    ['p-1', 'service', 'configuration', 'rate_limit_policy', 0.85, 50, 0.0, 30, 0.0, 1, 'local'],
                    ['p-2', 'service', 'configuration', 'cache_policy',      0.90, 10, 0.0, 30, 0.0, 1, 'local']
                ]
                :put structural_patterns {id => entity_type, condition_claim_type, expected_claim_type, frequency, sample_size, last_computed, min_sample_threshold, first_seen_at, stability_runs, source_scope}"#,
            )
            .expect("seed");
        let r = run_aep(&store, Q_PATTERN_OVERLAY, cluster_set()).expect("runs");
        let ids: Vec<String> = r.rows.iter().map(|row| dv_to_string(&row[0])).collect();
        assert!(ids.contains(&"p-1".to_string()), "above threshold");
        assert!(!ids.contains(&"p-2".to_string()), "below threshold dropped");
    }

    #[test]
    fn q_gap_scan_returns_open_only() {
        let store = mem_store();
        seed_basic_cluster(&store);
        store
            .raw_db()
            .run_default(
                r#"?[id, entity_id, pattern_id, expected_claim_type, confidence, status, created_at, resolved_at, resolved_by] <- [
                    ['g-1', 'e-auth', 'p-1', 'rate_limit_policy', 0.9, 'open',   0.0, 0.0, ''],
                    ['g-2', 'e-auth', 'p-2', 'cache_policy',      0.8, 'closed', 0.0, 0.0, '']
                ]
                :put known_unknowns {id => entity_id, pattern_id, expected_claim_type, confidence, status, created_at, resolved_at, resolved_by}"#,
            )
            .expect("seed");
        let r = run_aep(&store, Q_GAP_SCAN, cluster_set()).expect("runs");
        let ids: Vec<String> = r.rows.iter().map(|row| dv_to_string(&row[0])).collect();
        assert!(ids.contains(&"g-1".to_string()));
        assert!(!ids.contains(&"g-2".to_string()), "closed gap dropped");
    }

    // ─── Probe templates (§5.3) ────────────────────────────────────────────

    #[test]
    fn q_probe_factual_returns_provenance() {
        let store = mem_store();
        seed_basic_cluster(&store);
        let r = run_aep(&store, Q_PROBE_FACTUAL, cluster_set()).expect("runs");
        assert!(!r.rows.is_empty());
        // Row shape: [statement, claim_id, source_id, byte_start, byte_end, content_blake3, admission_tier, sensitivity]
        assert_eq!(r.rows[0].len(), 8);
    }

    #[test]
    fn q_probe_quantitative_returns_typed_scalars() {
        let store = mem_store();
        seed_basic_cluster(&store);
        store
            .raw_db()
            .run_default(
                r#"?[id, claim_id, metric_name, value, unit, qualifier, is_live, captured_at, source_id, byte_start, byte_end, content_blake3] <- [
                    ['q-1', 'c-1', 'rps', 50000.0, 'rps', 'avg', true, 0.0, 's1', 0, 10, 'blake3:aa']
                ]
                :put quantities {id => claim_id, metric_name, value, unit, qualifier, is_live, captured_at, source_id, byte_start, byte_end, content_blake3}"#,
            )
            .expect("seed");
        let r = run_aep(&store, Q_PROBE_QUANTITATIVE, cluster_set()).expect("runs");
        assert!(!r.rows.is_empty());
    }

    #[test]
    fn q_probe_authorship_returns_blame() {
        let store = mem_store();
        seed_basic_cluster(&store);
        store
            .raw_db()
            .run_default(
                r#"?[source_id, line_start, line_end, commit_sha, author, author_email, blamed_at, byte_start, byte_end, content_blake3] <- [
                    ['s1', 1, 10, 'sha-abc', 'Alice', 'a@x', 100.0, 0, 100, 'blake3:bb']
                ]
                :put git_blame {source_id, line_start, line_end => commit_sha, author, author_email, blamed_at, byte_start, byte_end, content_blake3}"#,
            )
            .expect("seed");
        let r = run_aep(&store, Q_PROBE_AUTHORSHIP, cluster_set()).expect("runs");
        assert!(!r.rows.is_empty());
        assert_eq!(dv_to_string(&r.rows[0][0]), "Alice");
    }

    #[test]
    fn q_probe_structural_returns_signature() {
        let store = mem_store();
        seed_basic_cluster(&store);
        store
            .raw_db()
            .run_default(
                r#"?[claim_id, parameters_json, return_type, visibility, trait_name, parent_scope, field_types_json, source_id, byte_start, byte_end, content_blake3] <- [
                    ['c-1', '[{"name":"x","ty":"u32"}]', 'bool', 'pub', '', 'mod auth', '[]', 's1', 0, 50, 'blake3:cc']
                ]
                :put code_signatures {claim_id => parameters_json, return_type, visibility, trait_name, parent_scope, field_types_json, source_id, byte_start, byte_end, content_blake3}"#,
            )
            .expect("seed");
        let r = run_aep(&store, Q_PROBE_STRUCTURAL, cluster_set()).expect("runs");
        assert_eq!(r.rows.len(), 1);
        assert_eq!(dv_to_string(&r.rows[0][1]), "bool");
    }

    #[test]
    fn q_probe_relation_callers_returns_inbound_calls() {
        let store = mem_store();
        seed_basic_cluster(&store);
        store
            .raw_db()
            .run_default(
                r#"?[id, caller_claim_id, callee_name, callee_claim_id, source_id, byte_start, byte_end, content_blake3] <- [
                    ['fc-1', 'c-2', 'login', 'c-1', 's1', 0, 10, 'blake3:aa'],
                    ['fc-2', 'c-3', 'login', 'c-1', 's1', 11, 20, 'blake3:bb']
                ]
                :put function_calls {id => caller_claim_id, callee_name, callee_claim_id, source_id, byte_start, byte_end, content_blake3}"#,
            )
            .expect("seed");
        let mut p = BTreeMap::new();
        p.insert("target".into(), DataValue::Str("c-1".into()));
        let r = run_aep(&store, Q_PROBE_RELATION_CALLERS, p).expect("runs");
        assert_eq!(r.rows.len(), 2);
    }

    #[test]
    fn q_probe_existential_returns_witness() {
        let store = mem_store();
        seed_basic_cluster(&store);
        let mut p = cluster_set();
        p.insert("claim_type".into(), DataValue::Str("configuration".into()));
        let r = run_aep(&store, Q_PROBE_EXISTENTIAL, p).expect("runs");
        assert!(!r.rows.is_empty());
        assert_eq!(dv_to_string(&r.rows[0][0]), "c-1");
    }

    #[test]
    fn q_probe_comparative_partitions_by_side() {
        let store = mem_store();
        seed_basic_cluster(&store);
        // Seed a non-rejected claim on e-db so the trust filter still
        // returns a side-`b` row. Without it, the only e-db claim is c-3
        // (admission_tier='rejected') and side-b would be empty by design.
        store
            .raw_db()
            .run_default(
                r#"?[id, statement, claim_type, source_id, admission_tier, sensitivity, byte_start, byte_end, content_blake3] <- [
                    ['c-4', 'db config', 'configuration', 's1', 'rooted', 'public', 31, 40, 'blake3:dd']
                ]
                :put claims {id => statement, claim_type, source_id, admission_tier, sensitivity, byte_start, byte_end, content_blake3}"#,
            )
            .expect("seed c-4");
        store
            .raw_db()
            .run_default(
                r#"?[claim_id, entity_id] <- [['c-4', 'e-db']] :put claim_entity_edges {claim_id, entity_id}"#,
            )
            .expect("link c-4 to e-db");
        store
            .raw_db()
            .run_default(
                r#"?[claim_id, source_id] <- [['c-4', 's1']] :put claim_source_edges {claim_id, source_id}"#,
            )
            .expect("link c-4 to s1");
        let mut p = BTreeMap::new();
        p.insert("set_a".into(), dv_str_list(&["e-auth"]));
        p.insert("set_b".into(), dv_str_list(&["e-db"]));
        let r = run_aep(&store, Q_PROBE_COMPARATIVE, p).expect("runs");
        let sides: Vec<String> = r.rows.iter().map(|row| dv_to_string(&row[6])).collect();
        assert!(sides.contains(&"a".to_string()));
        assert!(sides.contains(&"b".to_string()));
    }

    // ─── Hash-format pin (Plan §3.6 cross-test) ────────────────────────────

    #[test]
    fn row_blake3_format_matches_blake3_hash_to_hex() {
        // Pins the lowercase-hex assumption. RARP's verify path computes
        // `format!("blake3:{}", blake3::hash(bytes).to_hex())` and compares
        // it byte-for-byte to the row's stored `content_blake3`. If
        // `to_hex()` ever returned mixed case this comparison would
        // silently fail and flood every probe with `StaleRow` caveats.
        let bytes = b"hello world";
        let by_helper = crate::row_blake3::row_blake3(bytes, 0, bytes.len() as u64);
        let by_direct = format!("blake3:{}", blake3::hash(bytes).to_hex());
        assert_eq!(by_helper, by_direct);
        assert!(
            by_direct.chars().skip(7).all(|c| !c.is_ascii_uppercase()),
            "hex must be lowercase: {by_direct}"
        );
    }
}
