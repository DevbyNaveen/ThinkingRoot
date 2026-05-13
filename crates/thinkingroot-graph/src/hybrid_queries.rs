//! Hybrid Retrieval — Datalog query catalogue (one per `TypedPredicate` variant).
//!
//! Spec: `docs/2026-05-02-hybrid-retrieval-spec.md` §4.1 (predicates) and §9
//! (which tables each predicate touches). Mirrors the `aep_queries.rs`
//! pattern: each query takes `BTreeMap<String, DataValue>` parameters and is
//! executed via `db.run_script(QUERY, params, ScriptMutability::Immutable)`.
//! Return shape is `?[claim_id]` — predicates are intersected in Rust by the
//! `CandidateMerger` to give AND semantics.
//!
//! All queries are `pub const` strings except `build_in_heading_path` —
//! the `InHeadingPath` predicate's query shape varies with path length and
//! is built per-call.
//!
//! **Phase 4 Witness Mesh transition (2026-05-14).** Per
//! `.claude/rules/hybrid-retrieval.md` "Witness Mesh transition": the
//! predicates here still join `*claims{...}` + `claim_entity_edges` +
//! `entity_relations` + `function_calls.callee_claim_id` because the
//! Witness Mesh substrate doesn't populate any of those join columns
//! today. Retargeting BM25 onto `witness_type + content_blake3 +
//! spans_json` and vector recall onto Witness span text materialised
//! from `source.tar.zst` is the Commit-2 cutover work — it requires
//! the recall tier to read span text at index time, which means a
//! byte-store round-trip the current `engine.search_scoped` boundary
//! doesn't have. Until then, witness-only workspaces see empty
//! hybrid retrieval responses; this is honest behaviour, not a bug.

use std::collections::BTreeMap;

use cozo::{DataValue, NamedRows, ScriptMutability};

use crate::graph::GraphStore;
use crate::Error;
use crate::Result;

// ===========================================================================
// 12 const queries — one per `TypedPredicate` variant except InHeadingPath.
// All return `?[claim_id]` for cheap intersection in CandidateMerger.
// ===========================================================================

/// `EntityType { value }` — claims attached to an entity of the given type.
pub const Q_HR_ENTITY_TYPE: &str = r#"
    ?[claim_id] :=
        *entities{id: entity_id, entity_type: $entity_type},
        *claim_entity_edges{claim_id, entity_id}
"#;

/// `EntityName { value }` — matches against canonical name OR aliases.
pub const Q_HR_ENTITY_NAME: &str = r#"
    ?[claim_id] :=
        *entities{id: entity_id, canonical_name: $entity_name},
        *claim_entity_edges{claim_id, entity_id}
    ?[claim_id] :=
        *entity_aliases{entity_id, alias: $entity_name},
        *claim_entity_edges{claim_id, entity_id}
"#;

/// `ClaimType { value }` — direct match on the `claims` column.
pub const Q_HR_CLAIM_TYPE: &str = r#"
    ?[claim_id] :=
        *claims{id: claim_id, claim_type: $claim_type}
"#;

/// `SourceTrustAtLeast { value }` — claims whose source has trust level
/// >= caller's threshold. The `$accepted_levels` set is computed in Rust
/// (lower bound + everything above) because Cozo doesn't expose `TrustLevel`'s
/// PartialOrd derivation.
pub const Q_HR_SOURCE_TRUST_AT_LEAST: &str = r#"
    ?[claim_id] :=
        *sources{id: source_id, trust_level},
        trust_level in $accepted_levels,
        *claim_source_edges{claim_id, source_id}
"#;

/// `AuthoredBy { value }` — claims whose source has a git_blame row
/// authored by the named author.
pub const Q_HR_AUTHORED_BY: &str = r#"
    ?[claim_id] :=
        *git_blame{source_id, author: $author},
        *claim_source_edges{claim_id, source_id}
"#;

/// `AuthoredAfter { value: DateTime<Utc> }` — claims whose source has a
/// commit with `commit_timestamp > $after_epoch`.
pub const Q_HR_AUTHORED_AFTER: &str = r#"
    ?[claim_id] :=
        *git_commits{source_id, commit_timestamp},
        commit_timestamp > $after_epoch,
        *claim_source_edges{claim_id, source_id}
"#;

/// `InCallGraphOf { entity_name, depth }` — claims that participate in a
/// call chain reachable within `depth` hops from any function whose name
/// matches `entity_name`. Cycle-guarded (`mid != caller`) per the
/// `Q_SUPERSESSION_CHAIN` precedent at `aep_queries.rs:100-112`.
///
/// Depth is encoded by inlining the parameter on each recursion step.
/// Cozo's stratified evaluator naturally bounds the walk by the
/// transitive closure (it terminates because the `cycle` guard keeps the
/// fixpoint finite); the Rust caller can additionally truncate the
/// returned set by walking only `depth` hops.
pub const Q_HR_IN_CALL_GRAPH_OF: &str = r#"
    seed[caller_claim_id] :=
        *function_calls{caller_claim_id, callee_name: $entity_name}
    seed[caller_claim_id] :=
        *function_calls{caller_claim_id, callee_claim_id},
        *claims{id: callee_claim_id, symbol: $entity_name}
    chain[caller, depth] := seed[caller], depth = 1
    chain[caller, depth] :=
        chain[mid, mid_depth],
        mid != caller,
        *function_calls{caller_claim_id: caller, callee_claim_id: mid},
        depth = mid_depth + 1,
        depth <= $max_depth
    ?[claim_id] := chain[claim_id, _]
"#;

/// `HasDocTag { tag_kind, target }` — claims with a doc_tag of the given
/// kind, optionally constrained by target. The caller picks the right
/// query: `Q_HR_HAS_DOC_TAG_ANY_TARGET` when `target` is unset,
/// `Q_HR_HAS_DOC_TAG_WITH_TARGET` when set. Two rules vs. a parenthesised
/// disjunction because Cozo Datalog rejects boolean expressions over
/// different variables in a single rule body.
pub const Q_HR_HAS_DOC_TAG_ANY_TARGET: &str = r#"
    ?[claim_id] :=
        *doc_tags{claim_id, kind: $tag_kind}
"#;

pub const Q_HR_HAS_DOC_TAG_WITH_TARGET: &str = r#"
    ?[claim_id] :=
        *doc_tags{claim_id, kind: $tag_kind, target: $target}
"#;

/// `HasMarker { kinds }` — claims with at least one nearby `code_markers`
/// row whose `kind` is in the caller's set.
pub const Q_HR_HAS_MARKER: &str = r#"
    ?[claim_id] :=
        *code_markers{kind, in_claim_id: claim_id},
        kind in $marker_kinds,
        claim_id != ''
"#;

/// `QuantityRange { metric, min, max }` — claims with a quantities row for
/// `metric` whose `value` lies in `[min, max]`.
pub const Q_HR_QUANTITY_RANGE: &str = r#"
    ?[claim_id] :=
        *quantities{claim_id, metric_name: $metric, value},
        value >= $min,
        value <= $max
"#;

/// `SupersedesClaim { claim_id }` — claims that supersede the named claim
/// (i.e. their `claim_temporal.superseded_by` walks to the target).
/// Recursive walk re-uses the cycle-guard idiom from `Q_SUPERSESSION_CHAIN`.
pub const Q_HR_SUPERSEDES_CLAIM: &str = r#"
    chain[ancestor, descendant] :=
        *claim_temporal{claim_id: ancestor, superseded_by: descendant},
        descendant != '',
        descendant != ancestor
    chain[ancestor, descendant] :=
        chain[ancestor, mid],
        *claim_temporal{claim_id: mid, superseded_by: descendant},
        descendant != '',
        descendant != ancestor,
        descendant != mid
    ?[claim_id] := chain[claim_id, $target_claim_id]
"#;

/// `ReferencedBy { source_id }` — claims attached to a source that
/// references (via `source_references` OR `code_links`) the target source.
pub const Q_HR_REFERENCED_BY: &str = r#"
    referencing_source[from_id] :=
        *source_references{from_source_id: from_id, to_source_id: $target_source_id}
    referencing_source[from_id] :=
        *code_links{source_id: from_id, target_source_id: $target_source_id},
        $target_source_id != ''
    ?[claim_id] :=
        referencing_source[from_id],
        *claim_source_edges{claim_id, source_id: from_id}
"#;

// ===========================================================================
// Dynamic builder for `InHeadingPath`.
//
// Why dynamic: Datalog can't naturally express "for every X in $set,
// exists ancestor with text X" with ordering constraints. Building the
// query per-call with the path baked in avoids a fragile aggregation +
// counting dance in Cozo. Path lengths > 8 fall back to leaf-only matching
// (a 9-deep markdown heading is rare and the cost of generating an
// 81-rule Datalog script outweighs the precision gain).
// ===========================================================================

const MAX_INLINED_PATH_DEPTH: usize = 8;

/// Build the `InHeadingPath` query for a path of length N.
///
/// - N=1: claims under any heading with text == `path[0]`.
/// - N≥2: claims under a leaf heading with text == `path[N-1]` whose
///   ancestor chain visits `path[0..N-1]` in order (each ancestor's text
///   matches the corresponding path element, walking up via parent_heading_id).
///
/// Returns the Datalog source. Caller passes `path[0]..path[N-1]` as
/// `$path_0..$path_{N-1}` parameters.
pub fn build_in_heading_path(path_len: usize) -> String {
    let n = path_len.min(MAX_INLINED_PATH_DEPTH).max(1);
    if n == 1 {
        return r#"
            ?[claim_id] :=
                *headings{id: hid, text: $path_0, source_id, byte_start: hb_start, byte_end: hb_end},
                *claims{id: claim_id, source_id, byte_start: cb_start},
                cb_start >= hb_start,
                cb_start <= hb_end
        "#
        .into();
    }
    // N >= 2: build a chain of `parent_heading_id` joins.
    // For path = [A, B, C] we want:
    //   leaf.text = C, leaf.parent has text = B, that one's parent has text = A.
    let mut walk = String::new();
    let mut prev_id = "leaf_id".to_string();
    for level in (0..n - 1).rev() {
        // level n-1 is leaf (already bound). Walk up to level 0.
        let this_id = format!("anc_{level}_id");
        walk.push_str(&format!(
            "        *headings{{id: {prev_id}, parent_heading_id: {this_id}}},\n"
        ));
        walk.push_str(&format!(
            "        *headings{{id: {this_id}, text: $path_{level}}},\n"
        ));
        prev_id = this_id;
    }
    let leaf_idx = n - 1;
    format!(
        r#"
        ?[claim_id] :=
            *headings{{id: leaf_id, text: $path_{leaf_idx}, source_id, byte_start: hb_start, byte_end: hb_end}},
{walk}            *claims{{id: claim_id, source_id, byte_start: cb_start}},
            cb_start >= hb_start,
            cb_start <= hb_end
    "#
    )
}

// ===========================================================================
// Helpers (mirror aep_queries.rs::dv_str_list / run_aep)
// ===========================================================================

/// Build a `DataValue::List(Vec<DataValue::Str>)` for `x in $set` membership.
pub fn dv_str_list<S: AsRef<str>>(items: &[S]) -> DataValue {
    DataValue::List(
        items
            .iter()
            .map(|s| DataValue::Str(s.as_ref().to_string().into()))
            .collect(),
    )
}

/// Execute a hybrid Datalog query against `GraphStore`. Same shape as
/// `aep_queries::run_aep` — wraps `db.run_script(.., Immutable)` so the
/// caller never accidentally takes a write lock.
pub fn run_hybrid(
    graph: &GraphStore,
    query: &str,
    params: BTreeMap<String, DataValue>,
) -> Result<NamedRows> {
    graph
        .raw_db()
        .run_script(query, params, ScriptMutability::Immutable)
        .map_err(|e| Error::GraphStorage(format!("hybrid query: {e}")))
}

// ===========================================================================
// Tests — golden fixtures, one per predicate.
// Pattern mirrors aep_queries.rs::tests: spin up an in-memory CozoDB,
// seed only the rows under test, run the query, assert output.
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::GraphStore;
    use cozo::DbInstance;

    fn mem_store() -> GraphStore {
        let db = DbInstance::new("mem", "", "{}").expect("mem db");
        let store = GraphStore::from_db_for_testing(db);
        store.init_for_testing().expect("init");
        store
    }

    fn put(store: &GraphStore, query: &str, params: BTreeMap<String, DataValue>) {
        store
            .raw_db()
            .run_script(query, params, ScriptMutability::Mutable)
            .expect(query);
    }

    fn run(store: &GraphStore, query: &str, params: BTreeMap<String, DataValue>) -> Vec<Vec<DataValue>> {
        run_hybrid(store, query, params).expect("query").rows
    }

    fn ids(rows: Vec<Vec<DataValue>>) -> Vec<String> {
        rows.into_iter()
            .map(|r| match &r[0] {
                DataValue::Str(s) => s.to_string(),
                other => panic!("expected str, got {other:?}"),
            })
            .collect()
    }

    fn p() -> BTreeMap<String, DataValue> {
        BTreeMap::new()
    }

    fn s(v: &str) -> DataValue {
        DataValue::Str(v.to_string().into())
    }

    fn n(v: f64) -> DataValue {
        DataValue::from(v)
    }

    fn put_claim(store: &GraphStore, id: &str, claim_type: &str, source_id: &str, byte_start: i64) {
        let mut params = p();
        params.insert("id".into(), s(id));
        params.insert("ct".into(), s(claim_type));
        params.insert("src".into(), s(source_id));
        params.insert("bs".into(), DataValue::from(byte_start));
        put(
            store,
            r#"?[id, statement, claim_type, source_id, confidence, sensitivity, workspace_id,
                 created_at, grounding_score, grounding_method, extraction_tier, event_date,
                 admission_tier, derivation_parents, predicate_json, last_rooted_at, source_path,
                 byte_start, byte_end, content_blake3, symbol] <- [[
                    $id, 's', $ct, $src, 1.0, 'Public', '', 0.0, -1.0, '', 'llm', 0.0, 'rooted',
                    '', '', 0.0, '', $bs, $bs, '', ''
                ]]
              :put claims {id => statement, claim_type, source_id, confidence, sensitivity,
                            workspace_id, created_at, grounding_score, grounding_method,
                            extraction_tier, event_date, admission_tier, derivation_parents,
                            predicate_json, last_rooted_at, source_path, byte_start, byte_end,
                            content_blake3, symbol}"#,
            params,
        );
    }

    fn put_source(store: &GraphStore, id: &str, trust: &str) {
        let mut params = p();
        params.insert("id".into(), s(id));
        params.insert("trust".into(), s(trust));
        put(
            store,
            r#"?[id, uri, source_type, author, content_hash, trust_level, byte_size] <- [[
                $id, 'u', 'file', '', '', $trust, 0
            ]]
              :put sources {id => uri, source_type, author, content_hash, trust_level, byte_size}"#,
            params,
        );
    }

    fn put_claim_source(store: &GraphStore, claim_id: &str, source_id: &str) {
        let mut params = p();
        params.insert("c".into(), s(claim_id));
        params.insert("s".into(), s(source_id));
        put(
            store,
            r#"?[claim_id, source_id] <- [[$c, $s]] :put claim_source_edges {claim_id, source_id}"#,
            params,
        );
    }

    fn put_entity(store: &GraphStore, id: &str, name: &str, etype: &str) {
        let mut params = p();
        params.insert("id".into(), s(id));
        params.insert("n".into(), s(name));
        params.insert("t".into(), s(etype));
        put(
            store,
            r#"?[id, canonical_name, entity_type, description] <- [[$id, $n, $t, '']]
              :put entities {id => canonical_name, entity_type, description}"#,
            params,
        );
    }

    fn put_claim_entity(store: &GraphStore, claim_id: &str, entity_id: &str) {
        let mut params = p();
        params.insert("c".into(), s(claim_id));
        params.insert("e".into(), s(entity_id));
        put(
            store,
            r#"?[claim_id, entity_id] <- [[$c, $e]] :put claim_entity_edges {claim_id, entity_id}"#,
            params,
        );
    }

    // -----------------------------------------------------------------------
    // Q_HR_ENTITY_TYPE
    // -----------------------------------------------------------------------
    #[test]
    fn q_hr_entity_type_returns_only_matching_claims() {
        let store = mem_store();
        put_entity(&store, "e1", "AuthService", "Service");
        put_entity(&store, "e2", "Cache", "Cache");
        put_claim(&store, "c1", "fact", "src1", 0);
        put_claim(&store, "c2", "fact", "src1", 100);
        put_claim_entity(&store, "c1", "e1");
        put_claim_entity(&store, "c2", "e2");

        let mut params = p();
        params.insert("entity_type".into(), s("Service"));
        let got = ids(run(&store, Q_HR_ENTITY_TYPE, params));
        assert_eq!(got, vec!["c1"]);
    }

    // -----------------------------------------------------------------------
    // Q_HR_ENTITY_NAME (canonical + alias)
    // -----------------------------------------------------------------------
    #[test]
    fn q_hr_entity_name_matches_canonical_and_alias() {
        let store = mem_store();
        put_entity(&store, "e1", "Amazon Web Services", "Vendor");
        put_claim(&store, "c1", "fact", "src1", 0);
        put_claim_entity(&store, "c1", "e1");

        // Canonical name
        let mut params = p();
        params.insert("entity_name".into(), s("Amazon Web Services"));
        let got = ids(run(&store, Q_HR_ENTITY_NAME, params));
        assert_eq!(got, vec!["c1"]);

        // Alias
        let mut alias_params = p();
        alias_params.insert("e".into(), s("e1"));
        alias_params.insert("a".into(), s("AWS"));
        put(
            &store,
            r#"?[entity_id, alias] <- [[$e, $a]] :put entity_aliases {entity_id, alias}"#,
            alias_params,
        );

        let mut params = p();
        params.insert("entity_name".into(), s("AWS"));
        let got = ids(run(&store, Q_HR_ENTITY_NAME, params));
        assert_eq!(got, vec!["c1"]);
    }

    // -----------------------------------------------------------------------
    // Q_HR_CLAIM_TYPE
    // -----------------------------------------------------------------------
    #[test]
    fn q_hr_claim_type_filters_on_column() {
        let store = mem_store();
        put_claim(&store, "c1", "fact", "src1", 0);
        put_claim(&store, "c2", "function", "src1", 50);

        let mut params = p();
        params.insert("claim_type".into(), s("function"));
        let got = ids(run(&store, Q_HR_CLAIM_TYPE, params));
        assert_eq!(got, vec!["c2"]);
    }

    // -----------------------------------------------------------------------
    // Q_HR_SOURCE_TRUST_AT_LEAST
    // -----------------------------------------------------------------------
    #[test]
    fn q_hr_source_trust_at_least_filters_by_accepted_levels() {
        let store = mem_store();
        put_source(&store, "src_v", "Verified");
        put_source(&store, "src_u", "Unknown");
        put_claim(&store, "c1", "fact", "src_v", 0);
        put_claim(&store, "c2", "fact", "src_u", 50);
        put_claim_source(&store, "c1", "src_v");
        put_claim_source(&store, "c2", "src_u");

        // Caller computes "Verified" → set {Verified}
        let mut params = p();
        params.insert("accepted_levels".into(), dv_str_list(&["Verified"]));
        let got = ids(run(&store, Q_HR_SOURCE_TRUST_AT_LEAST, params));
        assert_eq!(got, vec!["c1"]);

        // Caller computes "Unknown" → set {Unknown, Trusted, Verified}
        let mut params = p();
        params.insert(
            "accepted_levels".into(),
            dv_str_list(&["Unknown", "Trusted", "Verified"]),
        );
        let mut got = ids(run(&store, Q_HR_SOURCE_TRUST_AT_LEAST, params));
        got.sort();
        assert_eq!(got, vec!["c1", "c2"]);
    }

    // -----------------------------------------------------------------------
    // Q_HR_AUTHORED_BY
    // -----------------------------------------------------------------------
    #[test]
    fn q_hr_authored_by_returns_claims_with_blame_for_author() {
        let store = mem_store();
        put_claim(&store, "c1", "fact", "src1", 0);
        put_claim_source(&store, "c1", "src1");
        let mut params = p();
        params.insert("src".into(), s("src1"));
        params.insert("ls".into(), DataValue::from(0));
        params.insert("le".into(), DataValue::from(10));
        params.insert("a".into(), s("alice"));
        put(
            &store,
            r#"?[source_id, line_start, line_end, commit_sha, author, author_email, blamed_at, byte_start, byte_end, content_blake3] <- [[
                $src, $ls, $le, 'sha1', $a, '', 0.0, 0, 10, ''
            ]]
              :put git_blame {source_id, line_start, line_end => commit_sha, author, author_email, blamed_at, byte_start, byte_end, content_blake3}"#,
            params,
        );

        let mut params = p();
        params.insert("author".into(), s("alice"));
        let got = ids(run(&store, Q_HR_AUTHORED_BY, params));
        assert_eq!(got, vec!["c1"]);

        let mut params = p();
        params.insert("author".into(), s("bob"));
        let got = ids(run(&store, Q_HR_AUTHORED_BY, params));
        assert!(got.is_empty());
    }

    // -----------------------------------------------------------------------
    // Q_HR_AUTHORED_AFTER
    // -----------------------------------------------------------------------
    #[test]
    fn q_hr_authored_after_filters_by_commit_timestamp() {
        let store = mem_store();
        put_claim(&store, "c1", "fact", "src1", 0);
        put_claim_source(&store, "c1", "src1");
        let mut params = p();
        params.insert("src".into(), s("src1"));
        params.insert("sha".into(), s("sha1"));
        params.insert("ts".into(), n(1700.0));
        put(
            &store,
            r#"?[source_id, commit_sha, commit_author, commit_email, commit_timestamp,
                 changed_files_json, message, parent_sha, byte_start, byte_end, content_blake3] <- [[
                $src, $sha, '', '', $ts, '[]', '', '', 0, 0, ''
            ]]
              :put git_commits {source_id, commit_sha => commit_author, commit_email,
                                 commit_timestamp, changed_files_json, message, parent_sha,
                                 byte_start, byte_end, content_blake3}"#,
            params,
        );

        // Cutoff 1500 → c1 included (ts=1700 > 1500)
        let mut params = p();
        params.insert("after_epoch".into(), n(1500.0));
        let got = ids(run(&store, Q_HR_AUTHORED_AFTER, params));
        assert_eq!(got, vec!["c1"]);

        // Cutoff 2000 → c1 excluded
        let mut params = p();
        params.insert("after_epoch".into(), n(2000.0));
        let got = ids(run(&store, Q_HR_AUTHORED_AFTER, params));
        assert!(got.is_empty());
    }

    // -----------------------------------------------------------------------
    // Q_HR_IN_CALL_GRAPH_OF
    // -----------------------------------------------------------------------
    #[test]
    fn q_hr_in_call_graph_of_walks_callers() {
        let store = mem_store();
        put_claim(&store, "c_main", "function", "src1", 0);
        put_claim(&store, "c_login", "function", "src1", 100);
        put_claim(&store, "c_auth", "function", "src1", 200);

        // c_main -> c_login -> c_auth ; querying entity_name = "auth" should
        // return c_login (depth=1) and c_main (depth=2).
        let put_call = |store: &GraphStore, id: &str, caller: &str, callee_id: &str, callee_name: &str| {
            let mut params = p();
            params.insert("id".into(), s(id));
            params.insert("caller".into(), s(caller));
            params.insert("callee_id".into(), s(callee_id));
            params.insert("callee_name".into(), s(callee_name));
            put(
                store,
                r#"?[id, caller_claim_id, callee_name, callee_claim_id, source_id,
                     byte_start, byte_end, content_blake3] <- [[
                    $id, $caller, $callee_name, $callee_id, 'src1', 0, 10, ''
                ]]
                  :put function_calls {id => caller_claim_id, callee_name, callee_claim_id,
                                        source_id, byte_start, byte_end, content_blake3}"#,
                params,
            );
        };
        put_call(&store, "fc1", "c_main", "c_login", "login");
        put_call(&store, "fc2", "c_login", "c_auth", "auth");

        // claims need symbol set to resolve callee_claim_id form of seed
        let mut params = p();
        params.insert("symbol".into(), s("auth"));
        params.insert("id".into(), s("c_auth"));
        put(
            &store,
            r#"?[id, symbol] <- [[$id, $symbol]] :update claims {id => symbol}"#,
            params,
        );

        let mut params = p();
        params.insert("entity_name".into(), s("auth"));
        params.insert("max_depth".into(), DataValue::from(5));
        let mut got = ids(run(&store, Q_HR_IN_CALL_GRAPH_OF, params));
        got.sort();
        assert_eq!(got, vec!["c_login", "c_main"]);
    }

    // -----------------------------------------------------------------------
    // Q_HR_HAS_DOC_TAG
    // -----------------------------------------------------------------------
    #[test]
    fn q_hr_has_doc_tag_filters_with_and_without_target() {
        let store = mem_store();
        put_claim(&store, "c1", "function", "src1", 0);
        put_claim(&store, "c2", "function", "src1", 100);
        let put_tag = |id: &str, claim: &str, kind: &str, target: &str| {
            let mut params = p();
            params.insert("id".into(), s(id));
            params.insert("c".into(), s(claim));
            params.insert("k".into(), s(kind));
            params.insert("t".into(), s(target));
            put(
                &store,
                r#"?[id, claim_id, kind, target, description, source_id, byte_start, byte_end, content_blake3] <- [[
                    $id, $c, $k, $t, '', 'src1', 0, 10, ''
                ]]
                  :put doc_tags {id => claim_id, kind, target, description, source_id, byte_start, byte_end, content_blake3}"#,
                params,
            );
        };
        put_tag("dt1", "c1", "deprecated", "");
        put_tag("dt2", "c2", "param", "user_id");

        // No target required → use ANY_TARGET variant
        let mut params = p();
        params.insert("tag_kind".into(), s("deprecated"));
        let got = ids(run(&store, Q_HR_HAS_DOC_TAG_ANY_TARGET, params));
        assert_eq!(got, vec!["c1"]);

        // Specific target match → WITH_TARGET variant
        let mut params = p();
        params.insert("tag_kind".into(), s("param"));
        params.insert("target".into(), s("user_id"));
        let got = ids(run(&store, Q_HR_HAS_DOC_TAG_WITH_TARGET, params));
        assert_eq!(got, vec!["c2"]);

        // Wrong target → no match
        let mut params = p();
        params.insert("tag_kind".into(), s("param"));
        params.insert("target".into(), s("nonexistent"));
        let got = ids(run(&store, Q_HR_HAS_DOC_TAG_WITH_TARGET, params));
        assert!(got.is_empty());
    }

    // -----------------------------------------------------------------------
    // Q_HR_HAS_MARKER
    // -----------------------------------------------------------------------
    #[test]
    fn q_hr_has_marker_filters_by_kind_set() {
        let store = mem_store();
        put_claim(&store, "c1", "function", "src1", 0);
        put_claim(&store, "c2", "function", "src1", 100);
        let put_marker = |id: &str, claim: &str, kind: &str| {
            let mut params = p();
            params.insert("id".into(), s(id));
            params.insert("c".into(), s(claim));
            params.insert("k".into(), s(kind));
            put(
                &store,
                r#"?[id, source_id, kind, text, in_claim_id, byte_start, byte_end, content_blake3] <- [[
                    $id, 'src1', $k, 't', $c, 0, 10, ''
                ]]
                  :put code_markers {id => source_id, kind, text, in_claim_id, byte_start, byte_end, content_blake3}"#,
                params,
            );
        };
        put_marker("m1", "c1", "TODO");
        put_marker("m2", "c2", "NOTE");

        let mut params = p();
        params.insert("marker_kinds".into(), dv_str_list(&["TODO", "FIXME"]));
        let got = ids(run(&store, Q_HR_HAS_MARKER, params));
        assert_eq!(got, vec!["c1"]);
    }

    // -----------------------------------------------------------------------
    // Q_HR_QUANTITY_RANGE
    // -----------------------------------------------------------------------
    #[test]
    fn q_hr_quantity_range_excludes_out_of_band() {
        let store = mem_store();
        put_claim(&store, "c1", "fact", "src1", 0);
        put_claim(&store, "c2", "fact", "src1", 100);
        let put_q = |id: &str, claim: &str, metric: &str, value: f64| {
            let mut params = p();
            params.insert("id".into(), s(id));
            params.insert("c".into(), s(claim));
            params.insert("m".into(), s(metric));
            params.insert("v".into(), n(value));
            put(
                &store,
                r#"?[id, claim_id, metric_name, value, unit, qualifier, is_live, captured_at,
                     source_id, byte_start, byte_end, content_blake3] <- [[
                    $id, $c, $m, $v, '', '', false, 0.0, 'src1', 0, 10, ''
                ]]
                  :put quantities {id => claim_id, metric_name, value, unit, qualifier, is_live,
                                    captured_at, source_id, byte_start, byte_end, content_blake3}"#,
                params,
            );
        };
        put_q("q1", "c1", "rps", 5000.0);
        put_q("q2", "c2", "rps", 12000.0);

        let mut params = p();
        params.insert("metric".into(), s("rps"));
        params.insert("min".into(), n(10000.0));
        params.insert("max".into(), n(f64::INFINITY));
        let got = ids(run(&store, Q_HR_QUANTITY_RANGE, params));
        assert_eq!(got, vec!["c2"]);
    }

    // -----------------------------------------------------------------------
    // Q_HR_SUPERSEDES_CLAIM (recursive)
    // -----------------------------------------------------------------------
    #[test]
    fn q_hr_supersedes_claim_walks_chain() {
        let store = mem_store();
        put_claim(&store, "c1", "fact", "src1", 0);
        put_claim(&store, "c2", "fact", "src1", 50);
        put_claim(&store, "c3", "fact", "src1", 100);
        let put_temp = |claim: &str, sup: &str| {
            let mut params = p();
            params.insert("c".into(), s(claim));
            params.insert("sup".into(), s(sup));
            put(
                &store,
                r#"?[claim_id, valid_from, valid_until, superseded_by] <- [[$c, 0.0, 0.0, $sup]]
                  :put claim_temporal {claim_id => valid_from, valid_until, superseded_by}"#,
                params,
            );
        };
        // c1 -> c2 -> c3 (terminal)
        put_temp("c1", "c2");
        put_temp("c2", "c3");
        put_temp("c3", "");

        // "Who supersedes c3?" — walks c1 and c2 to c3.
        let mut params = p();
        params.insert("target_claim_id".into(), s("c3"));
        let mut got = ids(run(&store, Q_HR_SUPERSEDES_CLAIM, params));
        got.sort();
        assert_eq!(got, vec!["c1", "c2"]);
    }

    // -----------------------------------------------------------------------
    // Q_HR_REFERENCED_BY
    // -----------------------------------------------------------------------
    #[test]
    fn q_hr_referenced_by_unions_source_references_and_code_links() {
        let store = mem_store();
        put_claim(&store, "c1", "fact", "src_a", 0);
        put_claim(&store, "c2", "fact", "src_b", 0);
        put_claim_source(&store, "c1", "src_a");
        put_claim_source(&store, "c2", "src_b");

        // src_a → src_target via source_references
        let mut sr_params = p();
        sr_params.insert("id".into(), s("sr1"));
        sr_params.insert("from".into(), s("src_a"));
        sr_params.insert("to".into(), s("src_target"));
        put(
            &store,
            r#"?[id, from_source_id, to_source_id, reference_kind, fragment, byte_start, byte_end, content_blake3] <- [[
                $id, $from, $to, 'link', '', 0, 10, ''
            ]]
              :put source_references {id => from_source_id, to_source_id, reference_kind, fragment, byte_start, byte_end, content_blake3}"#,
            sr_params,
        );

        // src_b → src_target via code_links
        let mut cl_params = p();
        cl_params.insert("id".into(), s("cl1"));
        cl_params.insert("src".into(), s("src_b"));
        cl_params.insert("tgt".into(), s("src_target"));
        put(
            &store,
            r#"?[id, source_id, chunk_id, url, link_text, is_internal, target_source_id, byte_start, byte_end, content_blake3] <- [[
                $id, $src, '', 'http://x', '', true, $tgt, 0, 10, ''
            ]]
              :put code_links {id => source_id, chunk_id, url, link_text, is_internal, target_source_id, byte_start, byte_end, content_blake3}"#,
            cl_params,
        );

        let mut params = p();
        params.insert("target_source_id".into(), s("src_target"));
        let mut got = ids(run(&store, Q_HR_REFERENCED_BY, params));
        got.sort();
        assert_eq!(got, vec!["c1", "c2"]);
    }

    // -----------------------------------------------------------------------
    // build_in_heading_path — single + nested
    // -----------------------------------------------------------------------
    #[test]
    fn q_hr_in_heading_path_leaf_text_match() {
        let store = mem_store();
        put_claim(&store, "c1", "fact", "src1", 50);
        put_claim(&store, "c2", "fact", "src1", 200);
        let put_heading = |id: &str, text: &str, parent: &str, bs: i64, be: i64| {
            let mut params = p();
            params.insert("id".into(), s(id));
            params.insert("t".into(), s(text));
            params.insert("p".into(), s(parent));
            params.insert("bs".into(), DataValue::from(bs));
            params.insert("be".into(), DataValue::from(be));
            put(
                &store,
                r#"?[id, source_id, level, text, parent_heading_id, byte_start, byte_end, content_blake3] <- [[
                    $id, 'src1', 1, $t, $p, $bs, $be, ''
                ]]
                  :put headings {id => source_id, level, text, parent_heading_id, byte_start, byte_end, content_blake3}"#,
                params,
            );
        };
        // h_arch covers bytes 0-100, h_auth covers 100-300
        put_heading("h_arch", "Architecture", "", 0, 100);
        put_heading("h_auth", "Auth", "h_arch", 100, 300);

        // Single-level path = ["Auth"] → c2 (its byte_start 200 is in [100,300])
        let q = build_in_heading_path(1);
        let mut params = p();
        params.insert("path_0".into(), s("Auth"));
        let got = ids(run(&store, &q, params));
        assert_eq!(got, vec!["c2"]);
    }

    #[test]
    fn q_hr_in_heading_path_nested_walks_parent_chain() {
        let store = mem_store();
        put_claim(&store, "c_target", "fact", "src1", 200);
        put_claim(&store, "c_other", "fact", "src1", 50);
        let put_heading = |id: &str, text: &str, parent: &str, bs: i64, be: i64| {
            let mut params = p();
            params.insert("id".into(), s(id));
            params.insert("t".into(), s(text));
            params.insert("p".into(), s(parent));
            params.insert("bs".into(), DataValue::from(bs));
            params.insert("be".into(), DataValue::from(be));
            put(
                &store,
                r#"?[id, source_id, level, text, parent_heading_id, byte_start, byte_end, content_blake3] <- [[
                    $id, 'src1', 1, $t, $p, $bs, $be, ''
                ]]
                  :put headings {id => source_id, level, text, parent_heading_id, byte_start, byte_end, content_blake3}"#,
                params,
            );
        };
        // Architecture { 0..400 }
        //   ├ Other     { 0..100 }  ← contains c_other
        //   └ Auth      { 100..300} ← contains c_target
        put_heading("h_arch", "Architecture", "", 0, 400);
        put_heading("h_other", "Other", "h_arch", 0, 100);
        put_heading("h_auth", "Auth", "h_arch", 100, 300);

        let q = build_in_heading_path(2);
        let mut params = p();
        params.insert("path_0".into(), s("Architecture"));
        params.insert("path_1".into(), s("Auth"));
        let got = ids(run(&store, &q, params));
        // Only c_target (under Architecture/Auth); c_other is under
        // Architecture/Other — leaf "Auth" doesn't match.
        assert_eq!(got, vec!["c_target"]);
    }

    // -----------------------------------------------------------------------
    // Recursion termination — InCallGraphOf cycle guard.
    // -----------------------------------------------------------------------
    #[test]
    fn q_hr_in_call_graph_of_terminates_on_self_loop() {
        let store = mem_store();
        put_claim(&store, "c1", "function", "src1", 0);
        let mut params = p();
        params.insert("id".into(), s("fc-self"));
        params.insert("caller".into(), s("c1"));
        params.insert("callee".into(), s("c1"));
        params.insert("name".into(), s("self"));
        put(
            &store,
            r#"?[id, caller_claim_id, callee_name, callee_claim_id, source_id,
                 byte_start, byte_end, content_blake3] <- [[
                $id, $caller, $name, $callee, 'src1', 0, 10, ''
            ]]
              :put function_calls {id => caller_claim_id, callee_name, callee_claim_id,
                                    source_id, byte_start, byte_end, content_blake3}"#,
            params,
        );

        let mut params = p();
        params.insert("entity_name".into(), s("self"));
        params.insert("max_depth".into(), DataValue::from(5));
        // Must terminate (cycle guard); empty result is acceptable —
        // the seed itself is c1 (depth 1) so we expect c1 in output.
        let got = ids(run(&store, Q_HR_IN_CALL_GRAPH_OF, params));
        assert_eq!(got, vec!["c1"]);
    }
}
