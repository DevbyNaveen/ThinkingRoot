//! Phase 7e — Structural Resolution (Compile Completeness Contract §5).
//!
//! Runs at the tail of `Linker::link` (after entity / claim / relation
//! insert and contradiction detection) to fill in the three resolution
//! deferrals Phase 6.7 left in place:
//!
//! 1. **`function_calls.callee_claim_id`** — match `callee_name` against
//!    every claim's `symbol` column. External callees (std lib, deps)
//!    leave the column at `""`.
//! 2. **`code_links.is_internal` + `target_source_id`** — normalise the
//!    `url` against `sources.uri`. Matches set `is_internal = true` and
//!    stamp the matched source's id; non-matches leave both at the
//!    schema defaults.
//! 3. **`source_references` build** — emit one row per resolved
//!    `code_links` (kind = `"link"`) plus one per cross-source
//!    `function_calls` row with a resolved `callee_claim_id`
//!    (kind = `"import"`).
//! 4. **`code_metrics.fan_in` / `fan_out`** — group `function_calls`
//!    by caller/callee and stamp the per-FunctionDef row counts.
//!    `fan_out` counts distinct callee names (external callees count
//!    — they're real out-edges); `fan_in` counts distinct
//!    caller_claim_ids (external callers aren't in our graph, so
//!    they're correctly absent).
//!
//! All four steps are idempotent: re-running Phase 7e against the same
//! workspace produces identical updates because Phase 6.7's row IDs are
//! deterministic and the underlying lookup tables are stable.

use std::collections::{HashMap, HashSet};

use thinkingroot_core::Result;
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_graph::rows::{CodeLink, CodeMetric, FunctionCall, SourceReference};

/// Stats surfaced to the linker's `tracing::info` summary line.
///
/// `calls_updated` and `links_updated` count rows whose stored value changed
/// in this pass — including both newly-resolved rows (callee_claim_id gained a
/// value) and dangling-reset rows (callee_claim_id cleared to "" because the
/// target claim was deleted). "Updated" is the honest term; "resolved" would
/// imply only new connections were made.
#[derive(Debug, Default)]
pub struct ResolutionStats {
    pub calls_updated: usize,
    pub links_updated: usize,
    pub references_built: usize,
    pub metrics_resolved: usize,
}

/// Run all four resolution passes workspace-wide. Convenience wrapper
/// around [`resolve_scoped`] with `scope = None`. Kept for callers
/// (migration backfill, tests) that don't yet have a scope to pass.
pub fn resolve(graph: &GraphStore) -> Result<ResolutionStats> {
    resolve_scoped(graph, None)
}

/// Scoped revalidation. `scope = None` means workspace-wide (preserves
/// the pre-2026-05-18 contract). `scope = Some(set)` restricts the
/// revalidation reads at steps 1+2 to rows whose `source_id` is in
/// the set — typically `truly_changed ∪ list_dependent_sources_for_many
/// (truly_changed)`, captured at the Phase 7e call site.
///
/// Step 4 (fan_in / fan_out metric aggregation) intentionally stays
/// workspace-wide: a function's fan_in depends on calls from every
/// source in the workspace, and scoping the read would miscompute
/// metrics for functions in unchanged sources whose callers landed
/// in `truly_changed`. The workspace-wide read at step 4 is one
/// CozoDB query + an in-memory aggregation; the dominant cost is the
/// scoped reads at steps 1+2 plus the writes.
///
/// T4 (I-W3): every `function_calls` and `code_links` row in scope
/// is revalidated. Previously-resolved rows whose target claim /
/// source was deleted in a later compile are reset to `""` (external).
/// If a live target now exists under the same callee_name or URL,
/// the row is re-resolved to the new target — covering the case
/// where a function or file moved between sources between compiles.
/// The scope captures every row that could need this treatment in
/// the current compile: source X's calls can only point at rows X
/// resolves to (recorded in `resolution_deps:by_from`), and the
/// reverse lookup `:by_to` captures every X that resolves to any
/// truly-changed source. Combined with truly_changed itself
/// (whose own rows may have changed), the union is the smallest
/// sound scope.
pub fn resolve_scoped(
    graph: &GraphStore,
    scope: Option<&HashSet<String>>,
) -> Result<ResolutionStats> {
    let mut stats = ResolutionStats::default();

    // ── 1. function_calls.callee_claim_id (revalidation) ───────────────
    // Build symbol → claim_id map (first-write-wins on duplicate symbols;
    // v1.1 will key on (callee_name, parent_scope) for scope-aware
    // resolution once Phase 6.7 emits caller parent scope).
    let symbol_pairs = graph.list_claim_symbols()?;
    let mut symbol_to_claim: HashMap<String, String> = HashMap::with_capacity(symbol_pairs.len());
    for (claim_id, symbol) in symbol_pairs {
        symbol_to_claim.entry(symbol).or_insert(claim_id);
    }

    // Build live claim-id set for revalidation of previously-resolved rows.
    // We query all claims (with or without a symbol) so that a resolved
    // callee_claim_id pointing at an import-only claim (no symbol) is still
    // correctly kept alive when that claim still exists.
    let all_claim_ids = graph.get_all_claim_ids()?;
    let live_claim_ids: HashSet<String> = all_claim_ids.into_iter().collect();

    // Revalidate function_calls rows. Scoped when `scope` is Some;
    // workspace-wide when None.
    let all_calls = match scope {
        Some(s) => graph.list_function_calls_for_sources(s)?,
        None => graph.list_all_function_calls()?,
    };
    let mut updated_calls: Vec<FunctionCall> = Vec::new();
    for mut call in all_calls {
        let original = call.callee_claim_id.clone();
        if !original.is_empty() && !live_claim_ids.contains(&original) {
            // Was resolved but the target claim is gone — re-resolve by
            // callee_name (may land on a new live claim, or stay "").
            call.callee_claim_id = symbol_to_claim
                .get(&call.callee_name)
                .cloned()
                .unwrap_or_default();
            updated_calls.push(call);
        } else if original.is_empty() {
            // Was unresolved — attempt fresh resolution.
            if let Some(claim_id) = symbol_to_claim.get(&call.callee_name) {
                call.callee_claim_id = claim_id.clone();
                updated_calls.push(call);
            }
        }
        // else: original is non-empty AND still live — no change needed.
    }
    stats.calls_updated = updated_calls.len();
    if !updated_calls.is_empty() {
        graph.insert_function_calls_batch(&updated_calls)?;
    }

    // Record resolution_deps for each successful cross-source function_call
    // resolve (T5).  Runs over the full updated set (newly resolved + re-
    // resolved after a move); rows that reset to "" are skipped by the
    // `callee_claim_id.is_empty()` guard.  `record_resolution_dep` is
    // idempotent so re-runs on stable edges are safe.
    for call in &updated_calls {
        if call.callee_claim_id.is_empty() {
            continue;
        }
        if let Some(to_source) = graph.get_claim_source_id(&call.callee_claim_id)? {
            if to_source != call.source_id {
                graph.record_resolution_dep(
                    &call.source_id,
                    &to_source,
                    "function_call",
                    &call.id,
                )?;
            }
        }
    }

    // ── 2. code_links.is_internal + target_source_id (revalidation) ────
    let source_uris = graph.list_source_uris()?;
    let mut uri_lookup: HashMap<String, String> = HashMap::with_capacity(source_uris.len());
    for (sid, uri) in source_uris {
        uri_lookup.insert(normalise_uri(&uri), sid);
    }
    let live_source_ids: HashSet<String> = uri_lookup.values().cloned().collect();

    // Revalidate code_links rows. Scoped when `scope` is Some;
    // workspace-wide when None.
    let all_links = match scope {
        Some(s) => graph.list_code_links_for_sources(s)?,
        None => graph.list_all_code_links()?,
    };
    let mut updated_links: Vec<CodeLink> = Vec::new();
    for mut link in all_links {
        let original = link.target_source_id.clone();
        let new_target = uri_lookup
            .get(&normalise_uri(&link.url))
            .cloned()
            .unwrap_or_default();
        let new_internal = !new_target.is_empty();
        if !original.is_empty() && !live_source_ids.contains(&original) {
            // Was resolved but the target source is gone — re-resolve by
            // URL (may land on a new live source, or reset to "").
            link.target_source_id = new_target;
            link.is_internal = new_internal;
            updated_links.push(link);
        } else if original.is_empty() && new_internal {
            // Was unresolved — attempt fresh resolution.
            link.target_source_id = new_target;
            link.is_internal = true;
            updated_links.push(link);
        }
        // else: original non-empty AND still live, or external — no change.
    }
    stats.links_updated = updated_links.len();
    if !updated_links.is_empty() {
        graph.insert_code_links_batch(&updated_links)?;
    }

    // Record resolution_deps for each successful cross-source code_link
    // resolve (T5).  Only `is_internal` links with a non-empty target that
    // crosses source boundaries are recorded.
    for link in &updated_links {
        if !link.is_internal || link.target_source_id.is_empty() {
            continue;
        }
        if link.target_source_id != link.source_id {
            graph.record_resolution_dep(
                &link.source_id,
                &link.target_source_id,
                "code_link",
                &link.id,
            )?;
        }
    }

    // ── 3. source_references build ─────────────────────────────────────
    let mut references: Vec<SourceReference> = Vec::new();

    // 3a. From newly-(re-)resolved code_links → reference_kind = "link".
    // `updated_links` contains both freshly-resolved links and dangling
    // links that were reset.  Only emit source_references for rows that
    // now have a valid (non-empty) target_source_id; dangling resets have
    // an empty target and must not produce a source_references row.
    // Previously-resolved stable links that weren't in `updated_links`
    // already have a source_references row from when they were first
    // resolved; `:put` keeps those rows intact and upserts over any
    // re-resolved rows from this compile.
    for link in &updated_links {
        if link.target_source_id.is_empty() {
            // Reset-to-external: no valid target — skip.
            continue;
        }
        let id = stable_reference_id(
            &link.source_id,
            &link.target_source_id,
            "link",
            link.byte_start,
            link.byte_end,
            &link.url,
        );
        references.push(SourceReference {
            id,
            from_source_id: link.source_id.clone(),
            to_source_id: link.target_source_id.clone(),
            reference_kind: "link".to_string(),
            fragment: extract_fragment(&link.url),
            byte_start: link.byte_start,
            byte_end: link.byte_end,
            content_blake3: link.content_blake3.clone(),
        });
    }

    // 3b. From cross-source function_calls → reference_kind = "import".
    // Use the full resolved set (not just updated_calls) so that stable
    // cross-source calls from prior compiles also appear in source_references.
    let resolved_calls = graph.list_resolved_function_calls()?;
    // Cache claim → source lookups so we don't N+1 query CozoDB.
    let mut claim_to_source: HashMap<String, String> = HashMap::new();
    for call in &resolved_calls {
        if call.callee_claim_id.is_empty() {
            continue;
        }
        let callee_source = match claim_to_source.get(&call.callee_claim_id) {
            Some(s) => s.clone(),
            None => match graph.lookup_claim_source(&call.callee_claim_id)? {
                Some(s) => {
                    claim_to_source.insert(call.callee_claim_id.clone(), s.clone());
                    s
                }
                None => continue, // callee claim disappeared — skip silently
            },
        };
        if callee_source == call.source_id {
            // Same-source call — no source_references row (a function
            // calling itself / a sibling in the same file isn't a
            // cross-doc citation).
            continue;
        }
        let id = stable_reference_id(
            &call.source_id,
            &callee_source,
            "import",
            call.byte_start,
            call.byte_end,
            &call.callee_name,
        );
        references.push(SourceReference {
            id,
            from_source_id: call.source_id.clone(),
            to_source_id: callee_source,
            reference_kind: "import".to_string(),
            fragment: format!("::{}", call.callee_name),
            byte_start: call.byte_start,
            byte_end: call.byte_end,
            content_blake3: call.content_blake3.clone(),
        });
    }

    stats.references_built = references.len();
    if !references.is_empty() {
        graph.insert_source_references_batch(&references)?;
    }

    // ── 4. code_metrics.fan_in / fan_out ───────────────────────────────
    // Build the call-graph aggregation in Rust (CozoDB group-by works
    // but two passes + an in-memory roll-up is simpler and runs in one
    // table scan + one HashMap-keyed update). Reads `function_calls`
    // *after* Step 1's resolutions land so callee_claim_id is fresh.
    let all_calls_fresh = graph.list_all_function_calls()?;

    // fan_out: per caller_claim_id, the set of distinct callee_names
    // we observed. Includes external callees (any name we saw the
    // function reach, even if callee_claim_id stayed empty because
    // it's a stdlib / dep call) — those are real out-edges.
    let mut fan_out_map: HashMap<String, HashSet<String>> = HashMap::new();
    // fan_in: per callee_claim_id, the set of distinct caller_claim_ids.
    // External callers aren't in the table, so they're absent — fan_in
    // is correctly internal-only.
    let mut fan_in_map: HashMap<String, HashSet<String>> = HashMap::new();
    for call in &all_calls_fresh {
        if !call.caller_claim_id.is_empty() && !call.callee_name.is_empty() {
            fan_out_map
                .entry(call.caller_claim_id.clone())
                .or_default()
                .insert(call.callee_name.clone());
        }
        if !call.callee_claim_id.is_empty() && !call.caller_claim_id.is_empty() {
            fan_in_map
                .entry(call.callee_claim_id.clone())
                .or_default()
                .insert(call.caller_claim_id.clone());
        }
    }

    let metrics = graph.list_code_metrics()?;
    let mut updated_metrics: Vec<CodeMetric> = Vec::new();
    for mut metric in metrics {
        if metric.scope_claim_id.is_empty() {
            // file-scope rows have no per-claim fan_in / fan_out.
            continue;
        }
        let new_fan_out = fan_out_map
            .get(&metric.scope_claim_id)
            .map(|s| s.len() as u32)
            .unwrap_or(0);
        let new_fan_in = fan_in_map
            .get(&metric.scope_claim_id)
            .map(|s| s.len() as u32)
            .unwrap_or(0);
        if new_fan_out != metric.fan_out || new_fan_in != metric.fan_in {
            metric.fan_out = new_fan_out;
            metric.fan_in = new_fan_in;
            updated_metrics.push(metric);
        }
    }
    stats.metrics_resolved = updated_metrics.len();
    if !updated_metrics.is_empty() {
        graph.insert_code_metrics_batch(&updated_metrics)?;
    }

    Ok(stats)
}

/// Normalise a URI for cross-doc lookup. Strips `file://`, lowercases,
/// trims trailing slashes. Fragment (`#section`) is preserved as part of
/// the lookup key only when no scheme-stripped path matches — see
/// `extract_fragment` for the inverse.
fn normalise_uri(uri: &str) -> String {
    let mut s = uri.trim().to_lowercase();
    if let Some(without_scheme) = s.strip_prefix("file://") {
        s = without_scheme.to_string();
    }
    if let Some((path, _frag)) = s.split_once('#') {
        s = path.to_string();
    }
    s.trim_end_matches('/').to_string()
}

/// Extract the URL fragment (`#section-id`) for source_references.
fn extract_fragment(url: &str) -> String {
    url.split_once('#')
        .map(|(_, frag)| format!("#{frag}"))
        .unwrap_or_default()
}

/// Deterministic source_references id derived from the link's
/// positional context. Re-running Phase 7e on identical inputs
/// produces identical IDs — `:put` is upsert-safe.
fn stable_reference_id(
    from_source_id: &str,
    to_source_id: &str,
    kind: &str,
    byte_start: u64,
    byte_end: u64,
    suffix: &str,
) -> String {
    let mut h = blake3::Hasher::new();
    h.update(b"source_references|");
    h.update(from_source_id.as_bytes());
    h.update(b"|");
    h.update(to_source_id.as_bytes());
    h.update(b"|");
    h.update(kind.as_bytes());
    h.update(b"|");
    h.update(&byte_start.to_le_bytes());
    h.update(b"|");
    h.update(&byte_end.to_le_bytes());
    h.update(b"|");
    h.update(suffix.as_bytes());
    format!("source_references:{}", h.finalize().to_hex())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_uri_strips_scheme_and_lowercases() {
        assert_eq!(normalise_uri("file:///Path/To/File.md"), "/path/to/file.md");
        assert_eq!(normalise_uri("FILE:///A.md"), "/a.md");
    }

    #[test]
    fn normalise_uri_drops_fragment_for_lookup() {
        assert_eq!(
            normalise_uri("file:///path/to/file.md#section-1"),
            "/path/to/file.md"
        );
    }

    #[test]
    fn extract_fragment_returns_hash_prefixed() {
        assert_eq!(extract_fragment("file:///a.md#sec-1"), "#sec-1");
        assert_eq!(extract_fragment("file:///a.md"), "");
    }

    #[test]
    fn stable_reference_id_is_deterministic() {
        let a = stable_reference_id("s1", "s2", "link", 100, 200, "url");
        let b = stable_reference_id("s1", "s2", "link", 100, 200, "url");
        assert_eq!(a, b);
        let c = stable_reference_id("s1", "s2", "import", 100, 200, "url");
        assert_ne!(a, c);
    }

    /// Reproduces the Step-4 fan_in/fan_out aggregation in isolation —
    /// the same maps the production path builds, exercised against a
    /// hand-crafted call set so we can assert exact counts.
    fn build_fan_maps(
        calls: &[(String, String, String)], // (caller_claim, callee_name, callee_claim)
    ) -> (
        HashMap<String, HashSet<String>>,
        HashMap<String, HashSet<String>>,
    ) {
        let mut fan_out: HashMap<String, HashSet<String>> = HashMap::new();
        let mut fan_in: HashMap<String, HashSet<String>> = HashMap::new();
        for (caller, callee_name, callee_claim) in calls {
            if !caller.is_empty() && !callee_name.is_empty() {
                fan_out.entry(caller.clone()).or_default().insert(callee_name.clone());
            }
            if !callee_claim.is_empty() && !caller.is_empty() {
                fan_in.entry(callee_claim.clone()).or_default().insert(caller.clone());
            }
        }
        (fan_out, fan_in)
    }

    #[test]
    fn fan_out_counts_distinct_callee_names_including_external() {
        // Caller A calls: B (resolved), C (resolved), printf (external).
        // Calling printf twice should still count as one out-edge.
        let calls = vec![
            ("A".into(), "B".into(), "claim:B".into()),
            ("A".into(), "C".into(), "claim:C".into()),
            ("A".into(), "printf".into(), "".into()),
            ("A".into(), "printf".into(), "".into()),
        ];
        let (fan_out, _fan_in) = build_fan_maps(&calls);
        assert_eq!(fan_out.get("A").unwrap().len(), 3);
    }

    #[test]
    fn fan_in_counts_distinct_internal_callers_only() {
        // Callee X is called by A and B (internal), and by something
        // external (caller_claim_id "" should never appear because
        // Phase 6.7 only emits FunctionDef-scoped callers, but defend
        // anyway).
        let calls = vec![
            ("A".into(), "X".into(), "claim:X".into()),
            ("A".into(), "X".into(), "claim:X".into()), // dup caller
            ("B".into(), "X".into(), "claim:X".into()),
            ("".into(), "X".into(), "claim:X".into()),  // skipped — empty caller
        ];
        let (_fan_out, fan_in) = build_fan_maps(&calls);
        assert_eq!(fan_in.get("claim:X").unwrap().len(), 2); // A and B
    }

    #[test]
    fn fan_maps_skip_unresolved_callees_for_fan_in() {
        // External callee → no entry in fan_in.
        let calls = vec![
            ("A".into(), "stdlib_fn".into(), "".into()),
        ];
        let (fan_out, fan_in) = build_fan_maps(&calls);
        assert_eq!(fan_out.get("A").unwrap().len(), 1);
        assert!(fan_in.is_empty());
    }

    #[test]
    fn fan_maps_handle_self_call() {
        // A function calling itself: fan_out includes itself, fan_in
        // includes itself. Self-loops are real edges in the call graph.
        let calls = vec![
            ("A".into(), "A".into(), "claim:A".into()),
        ];
        let (fan_out, fan_in) = build_fan_maps(&calls);
        assert_eq!(fan_out.get("A").unwrap().len(), 1);
        assert_eq!(fan_in.get("claim:A").unwrap().len(), 1);
    }
}
