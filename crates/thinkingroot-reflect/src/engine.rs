//! Reflect engine — pattern discovery + gap generation.

use std::collections::{HashMap, HashSet};

use chrono::Utc;
use thinkingroot_core::Result;
use thinkingroot_graph::graph::GraphStore;

use crate::types::{GapReport, GapStatus, KnownUnknown, ReflectResult, StructuralPattern};

/// Configuration for one `reflect()` run.
#[derive(Debug, Clone)]
pub struct ReflectConfig {
    /// Minimum number of entities sharing the condition claim-type before
    /// a pattern is statistically meaningful. Below this threshold the
    /// pattern is dropped entirely — no gaps are emitted.
    pub min_sample_size: usize,
    /// Minimum frequency (0.0–1.0) for a pattern to generate gap claims.
    /// High enough that the `1 - frequency` tail doesn't flood noise.
    pub min_frequency: f64,
    /// Hard cap on retained patterns (post-threshold, sorted by
    /// `frequency × sample_size`). Zero disables the cap.
    pub max_patterns: usize,
    /// How many consecutive reflect cycles a pattern must survive
    /// before its gaps emit at full confidence. A brand-new pattern
    /// (stability_runs = 1) emits gaps at `frequency * 1/ramp`; after
    /// `ramp` runs at the same thresholds, gaps emit at `frequency`.
    ///
    /// Prevents one-off noise patterns from immediately firing
    /// high-confidence gap claims when the graph is still settling.
    /// Set to `1` to disable damping entirely. Default: 5.
    pub stability_ramp_runs: u32,
}

impl Default for ReflectConfig {
    fn default() -> Self {
        // Matches the spec (docs/2026-04-19-reflexive-knowledge-architecture.md
        // §"Minimum Threshold" and §"Implementation Sketch").
        Self {
            min_sample_size: 30,
            min_frequency: 0.70,
            max_patterns: 500,
            stability_ramp_runs: 5,
        }
    }
}

impl ReflectConfig {
    /// Compute the stability-damped confidence for a gap, given the
    /// pattern's `stability_runs` counter and the configured ramp.
    pub fn stability_factor(&self, stability_runs: u32) -> f64 {
        if self.stability_ramp_runs <= 1 {
            return 1.0;
        }
        (stability_runs as f64 / self.stability_ramp_runs as f64).min(1.0)
    }
}

/// Phase 9 Reflect: observe the graph's topology, derive patterns,
/// surface gaps as queryable records.
pub struct ReflectEngine {
    cfg: ReflectConfig,
}

impl ReflectEngine {
    pub fn new(cfg: ReflectConfig) -> Self {
        Self { cfg }
    }

    pub fn config(&self) -> &ReflectConfig {
        &self.cfg
    }

    /// Run one full reflect cycle: discover patterns, generate gaps,
    /// resolve gaps that have since been filled. Idempotent.
    pub fn reflect(&self, graph: &GraphStore) -> Result<ReflectResult> {
        let now = Utc::now().timestamp() as f64;

        // ── Load prior patterns for stability preservation ────────
        // Patterns that persist across runs must keep their
        // `first_seen_at` and increment `stability_runs`. Dropping
        // this state would reset every pattern's damping curve to
        // square one on every reflect cycle.
        let previous_patterns: HashMap<String, (f64, u32)> = graph
            .reflect_load_structural_patterns()?
            .into_iter()
            .map(|(id, _, _, _, _, _, _, _, first_seen, stability, _scope)| {
                (id, (first_seen, stability))
            })
            .collect();

        // ── 1. Discover patterns ──────────────────────────────────
        let raw_pairs = graph.reflect_co_occurrences()?;
        let entity_types_scanned = raw_pairs
            .iter()
            .map(|(etype, _, _, _, _)| etype.as_str())
            .collect::<HashSet<_>>()
            .len();

        let mut patterns: Vec<StructuralPattern> = raw_pairs
            .into_iter()
            .filter_map(|(etype, cta, ctb, cond_n, both_n)| {
                if cond_n < self.cfg.min_sample_size {
                    return None;
                }
                if both_n > cond_n {
                    // Defensive: should be impossible (both ⊆ condition).
                    tracing::warn!(
                        etype = %etype,
                        cta = %cta,
                        ctb = %ctb,
                        both_n,
                        cond_n,
                        "reflect: both > condition — skipping row"
                    );
                    return None;
                }
                let frequency = both_n as f64 / cond_n as f64;
                if frequency < self.cfg.min_frequency {
                    return None;
                }
                let id = pattern_id(&etype, &cta, &ctb);
                let (first_seen_at, stability_runs) = match previous_patterns.get(&id) {
                    // Pattern survived another run — keep first_seen,
                    // bump the stability counter (saturating at u32::MAX).
                    Some((first_seen, prev_runs)) => (*first_seen, prev_runs.saturating_add(1)),
                    // Brand new pattern — start the damping curve at 1.
                    None => (now, 1),
                };
                Some(StructuralPattern {
                    id,
                    entity_type: etype,
                    condition_claim_type: cta,
                    expected_claim_type: ctb,
                    frequency,
                    sample_size: cond_n,
                    last_computed: now,
                    min_sample_threshold: self.cfg.min_sample_size,
                    first_seen_at,
                    stability_runs,
                    source_scope: "local".to_string(),
                })
            })
            .collect();

        // Deterministic order (strongest patterns first).
        patterns.sort_by(|a, b| {
            let la = a.frequency * a.sample_size as f64;
            let lb = b.frequency * b.sample_size as f64;
            lb.partial_cmp(&la)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        if self.cfg.max_patterns > 0 && patterns.len() > self.cfg.max_patterns {
            patterns.truncate(self.cfg.max_patterns);
        }

        // ── 2. Rewrite structural_patterns in bulk ────────────────
        let rows: Vec<(
            String,
            String,
            String,
            String,
            f64,
            usize,
            f64,
            usize,
            f64,
            u32,
            String,
        )> = patterns
            .iter()
            .map(|p| {
                (
                    p.id.clone(),
                    p.entity_type.clone(),
                    p.condition_claim_type.clone(),
                    p.expected_claim_type.clone(),
                    p.frequency,
                    p.sample_size,
                    p.last_computed,
                    p.min_sample_threshold,
                    p.first_seen_at,
                    p.stability_runs,
                    p.source_scope.clone(),
                )
            })
            .collect();
        graph.reflect_rewrite_patterns_for_scope("local", &rows)?;

        // ── 3. Compute current gap set from patterns ──────────────
        // Key: gap_id; Value: (entity_id, pattern_id, expected, confidence).
        // The stored confidence is the stability-damped value, NOT the raw
        // pattern frequency — so fresh patterns can't immediately claim
        // 92% certainty for their gaps.
        let mut current: HashMap<String, (String, String, String, f64)> = HashMap::new();
        for p in &patterns {
            let damped_confidence = p.frequency * self.cfg.stability_factor(p.stability_runs);
            let missing = graph.reflect_entities_missing_expected(
                &p.entity_type,
                &p.condition_claim_type,
                &p.expected_claim_type,
            )?;
            for eid in missing {
                let gid = gap_id(&eid, &p.id);
                current.insert(
                    gid,
                    (
                        eid,
                        p.id.clone(),
                        p.expected_claim_type.clone(),
                        damped_confidence,
                    ),
                );
            }
        }

        // ── 4. Diff against previously-stored known_unknowns ──────
        let previous: HashMap<String, KnownUnknown> = graph
            .reflect_load_known_unknowns()?
            .into_iter()
            .map(
                |(id, eid, pid, expected, conf, status, created, resolved, resolved_by)| {
                    (
                        id.clone(),
                        KnownUnknown {
                            id,
                            entity_id: eid,
                            pattern_id: pid,
                            expected_claim_type: expected,
                            confidence: conf,
                            status: GapStatus::from_str(&status).unwrap_or(GapStatus::Open),
                            created_at: created,
                            resolved_at: resolved,
                            resolved_by,
                        },
                    )
                },
            )
            .collect();

        let mut gaps_created = 0usize;
        let mut gaps_resolved = 0usize;
        let mut gaps_still_open = 0usize;

        for (gid, (eid, pid, expected, conf)) in &current {
            match previous.get(gid) {
                None => {
                    graph.reflect_upsert_known_unknown(
                        gid,
                        eid,
                        pid,
                        expected,
                        *conf,
                        GapStatus::Open.as_str(),
                        now,
                        0.0,
                        "",
                    )?;
                    gaps_created += 1;
                }
                Some(prev) if prev.status == GapStatus::Open => {
                    // Gap persists. Refresh its confidence if the
                    // stability-damped value has moved (e.g. the
                    // pattern's stability just incremented so the damping
                    // factor relaxed). Without this refresh, the stored
                    // confidence is frozen at first-discovery and never
                    // reflects the pattern's current strength.
                    if (prev.confidence - *conf).abs() > f64::EPSILON {
                        graph.reflect_upsert_known_unknown(
                            gid,
                            eid,
                            pid,
                            expected,
                            *conf,
                            GapStatus::Open.as_str(),
                            prev.created_at,
                            0.0,
                            "",
                        )?;
                    }
                    gaps_still_open += 1;
                }
                Some(prev) if prev.status == GapStatus::Resolved => {
                    // Pattern re-detects the claim-type as missing after
                    // a previous resolution (entity lost the claim).
                    // Transition back to open with a fresh created_at.
                    graph.reflect_upsert_known_unknown(
                        gid,
                        eid,
                        pid,
                        expected,
                        *conf,
                        GapStatus::Open.as_str(),
                        now,
                        0.0,
                        "",
                    )?;
                    gaps_created += 1;
                }
                Some(_dismissed) => {
                    // Respect user intent — dismissed stays dismissed.
                }
            }
        }

        // Previously open gaps that are no longer detected → resolved.
        for (gid, prev) in &previous {
            if prev.status != GapStatus::Open {
                continue;
            }
            if current.contains_key(gid) {
                continue;
            }
            let resolver = graph
                .find_claim_id_for_entity_by_type(&prev.entity_id, &prev.expected_claim_type)?
                .unwrap_or_default();
            graph.reflect_upsert_known_unknown(
                gid,
                &prev.entity_id,
                &prev.pattern_id,
                &prev.expected_claim_type,
                prev.confidence,
                GapStatus::Resolved.as_str(),
                prev.created_at,
                now,
                &resolver,
            )?;
            gaps_resolved += 1;
        }

        let open_gaps_total = graph.reflect_count_open_known_unknowns()?;

        tracing::info!(
            target: "reflect",
            patterns = patterns.len(),
            entity_types_scanned,
            gaps_created,
            gaps_resolved,
            gaps_still_open,
            open_gaps_total,
            "reflect cycle complete"
        );

        Ok(ReflectResult {
            patterns,
            gaps_created,
            gaps_resolved,
            gaps_still_open,
            open_gaps_total,
            entity_types_scanned,
        })
    }
}

// ---------------------------------------------------------------------------
// Public read APIs (queried by the `gaps` MCP tool + the verifier).
// ---------------------------------------------------------------------------

/// List open gaps, optionally scoped to one entity canonical name and
/// filtered by `min_confidence`. Returns `GapReport` records joined with
/// entity and pattern metadata.
pub fn list_open_gaps(
    graph: &GraphStore,
    entity_name: Option<&str>,
    min_confidence: f64,
) -> Result<Vec<GapReport>> {
    let rows = graph.reflect_list_open_gap_rows(entity_name, min_confidence)?;

    let mut out: Vec<GapReport> = rows
        .into_iter()
        .map(
            |(gid, eid, ename, etype, expected, confidence, pid, sample, created)| GapReport {
                id: gid,
                entity_id: eid,
                entity_name: ename.clone(),
                entity_type: etype.clone(),
                expected_claim_type: expected.clone(),
                confidence,
                reason: format!(
                    "{pct:.0}% of {etype} entities with condition also carry '{expected}' — {ename} does not.",
                    pct = confidence * 100.0,
                    etype = etype,
                    expected = expected,
                    ename = ename,
                ),
                pattern_id: pid,
                sample_size: sample,
                created_at: created,
            },
        )
        .collect();
    // Highest confidence first; ties broken by sample_size desc.
    out.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.sample_size.cmp(&a.sample_size))
    });
    Ok(out)
}

/// Total count of open gaps — used by the verifier's coverage metric.
pub fn count_open_gaps(graph: &GraphStore) -> Result<usize> {
    graph.reflect_count_open_known_unknowns()
}

/// Cross-workspace reflect: aggregate co-occurrence counts across all
/// supplied graphs to discover patterns that might be below threshold
/// in any single workspace but meaningful in aggregate. Per-workspace,
/// the aggregate patterns drive gap generation against that workspace's
/// local entity set.
///
/// # Semantics
///
/// 1. For each input graph, fetch raw co-occurrence rows.
/// 2. Sum `cond_count` and `both_count` across workspaces per
///    `(entity_type, cta, ctb)`.
/// 3. Apply `min_sample_size` + `min_frequency` to the *aggregate*.
/// 4. For each passing pattern, scope id = `"cross:<hash-of-names>"`.
/// 5. For each workspace, find entities that match the pattern's
///    condition locally but lack the expected claim — emit a gap in
///    that workspace's `known_unknowns`. Gaps carry the cross pattern's
///    id (with scope tagging) so they never collide with local gaps.
/// 6. Stability tracking carries per-scope — a cross pattern that
///    survives multiple `reflect_across_graphs` runs earns higher
///    damped confidence the same way a local pattern does.
///
/// Local patterns are untouched: each workspace's own `reflect()`
/// continues to maintain `source_scope = 'local'` rows independently.
pub fn reflect_across_graphs(
    workspaces: &[(String, &GraphStore)],
    cfg: &ReflectConfig,
) -> Result<crate::types::CrossReflectResult> {
    let now = chrono::Utc::now().timestamp() as f64;
    let scope_id = scope_id_for(workspaces);

    // ── 1. Aggregate co-occurrence counts across all graphs ──────
    // Key: (etype, cta, ctb). Value: (sum_cond, sum_both).
    let mut agg: HashMap<(String, String, String), (usize, usize)> = HashMap::new();
    for (_name, g) in workspaces {
        for (etype, cta, ctb, cond_n, both_n) in g.reflect_co_occurrences()? {
            let entry = agg.entry((etype, cta, ctb)).or_insert((0, 0));
            entry.0 += cond_n;
            entry.1 += both_n;
        }
    }

    // ── 2. Load prior cross patterns from any workspace for stability ──
    // All participating workspaces share the same `scope_id`, so picking
    // the first is sufficient — re-runs write the same pattern ids to
    // every workspace.
    let previous: HashMap<String, (f64, u32)> = if let Some((_, first)) = workspaces.first() {
        first
            .reflect_load_structural_patterns()?
            .into_iter()
            .filter(|(_id, _, _, _, _, _, _, _, _, _, scope)| scope == &scope_id)
            .map(|(id, _, _, _, _, _, _, _, first_seen, stability, _scope)| {
                (id, (first_seen, stability))
            })
            .collect()
    } else {
        HashMap::new()
    };

    // ── 3. Filter by thresholds, compute stability-preserving state ──
    let mut patterns: Vec<StructuralPattern> = Vec::new();
    for ((etype, cta, ctb), (cond_n, both_n)) in agg.into_iter() {
        if cond_n < cfg.min_sample_size {
            continue;
        }
        if both_n > cond_n {
            tracing::warn!(
                etype = %etype,
                cta = %cta,
                ctb = %ctb,
                both_n,
                cond_n,
                "reflect_across: both > condition — skipping row"
            );
            continue;
        }
        let frequency = both_n as f64 / cond_n as f64;
        if frequency < cfg.min_frequency {
            continue;
        }
        let id = pattern_id_scoped(&scope_id, &etype, &cta, &ctb);
        let (first_seen_at, stability_runs) = match previous.get(&id) {
            Some((fs, runs)) => (*fs, runs.saturating_add(1)),
            None => (now, 1),
        };
        patterns.push(StructuralPattern {
            id,
            entity_type: etype,
            condition_claim_type: cta,
            expected_claim_type: ctb,
            frequency,
            sample_size: cond_n,
            last_computed: now,
            min_sample_threshold: cfg.min_sample_size,
            first_seen_at,
            stability_runs,
            source_scope: scope_id.clone(),
        });
    }

    patterns.sort_by(|a, b| {
        let la = a.frequency * a.sample_size as f64;
        let lb = b.frequency * b.sample_size as f64;
        lb.partial_cmp(&la)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    if cfg.max_patterns > 0 && patterns.len() > cfg.max_patterns {
        patterns.truncate(cfg.max_patterns);
    }

    // ── 4. Persist patterns + per-workspace gaps ─────────────────
    let pattern_rows: Vec<(
        String,
        String,
        String,
        String,
        f64,
        usize,
        f64,
        usize,
        f64,
        u32,
        String,
    )> = patterns
        .iter()
        .map(|p| {
            (
                p.id.clone(),
                p.entity_type.clone(),
                p.condition_claim_type.clone(),
                p.expected_claim_type.clone(),
                p.frequency,
                p.sample_size,
                p.last_computed,
                p.min_sample_threshold,
                p.first_seen_at,
                p.stability_runs,
                p.source_scope.clone(),
            )
        })
        .collect();

    let mut per_workspace: HashMap<String, ReflectResult> = HashMap::new();
    for (ws_name, g) in workspaces {
        // Write the aggregate patterns to each workspace's own
        // `structural_patterns` table, scoped to `scope_id`. This keeps
        // the local reflect cycle's `source_scope = 'local'` rows intact.
        g.reflect_rewrite_patterns_for_scope(&scope_id, &pattern_rows)?;

        // Apply aggregate patterns to find missing-expected entities in
        // this workspace, generate/update/resolve gaps in this
        // workspace's `known_unknowns`.
        let ws_result = apply_patterns_to_graph(g, &patterns, cfg, now)?;
        per_workspace.insert(ws_name.clone(), ws_result);
    }

    Ok(crate::types::CrossReflectResult {
        scope_id: scope_id.clone(),
        aggregate_patterns: patterns,
        per_workspace,
        workspaces: workspaces.iter().map(|(n, _)| n.clone()).collect(),
    })
}

/// Internal: given a pre-computed pattern set, generate/update/resolve
/// gap records on a single graph. Shared between local `reflect()` and
/// cross-workspace `reflect_across_graphs`.
fn apply_patterns_to_graph(
    graph: &GraphStore,
    patterns: &[StructuralPattern],
    cfg: &ReflectConfig,
    now: f64,
) -> Result<ReflectResult> {
    // Build the "expected gap set" for these patterns.
    let mut current: HashMap<String, (String, String, String, f64)> = HashMap::new();
    for p in patterns {
        let damped = p.frequency * cfg.stability_factor(p.stability_runs);
        for eid in graph.reflect_entities_missing_expected(
            &p.entity_type,
            &p.condition_claim_type,
            &p.expected_claim_type,
        )? {
            let gid = gap_id(&eid, &p.id);
            current.insert(
                gid,
                (eid, p.id.clone(), p.expected_claim_type.clone(), damped),
            );
        }
    }

    // Load prior gaps belonging to these patterns' ids only — this
    // function is pattern-set-scoped, so we don't touch gaps attached
    // to other scopes.
    let pattern_id_set: std::collections::HashSet<String> =
        patterns.iter().map(|p| p.id.clone()).collect();
    let previous: HashMap<String, KnownUnknown> = graph
        .reflect_load_known_unknowns()?
        .into_iter()
        .filter(|(_id, _eid, pid, _, _, _, _, _, _)| pattern_id_set.contains(pid))
        .map(
            |(id, eid, pid, expected, conf, status, created, resolved, resolved_by)| {
                (
                    id.clone(),
                    KnownUnknown {
                        id,
                        entity_id: eid,
                        pattern_id: pid,
                        expected_claim_type: expected,
                        confidence: conf,
                        status: GapStatus::from_str(&status).unwrap_or(GapStatus::Open),
                        created_at: created,
                        resolved_at: resolved,
                        resolved_by,
                    },
                )
            },
        )
        .collect();

    let mut gaps_created = 0usize;
    let mut gaps_resolved = 0usize;
    let mut gaps_still_open = 0usize;

    for (gid, (eid, pid, expected, conf)) in &current {
        match previous.get(gid) {
            None => {
                graph.reflect_upsert_known_unknown(
                    gid,
                    eid,
                    pid,
                    expected,
                    *conf,
                    GapStatus::Open.as_str(),
                    now,
                    0.0,
                    "",
                )?;
                gaps_created += 1;
            }
            Some(prev) if prev.status == GapStatus::Open => {
                if (prev.confidence - *conf).abs() > f64::EPSILON {
                    graph.reflect_upsert_known_unknown(
                        gid,
                        eid,
                        pid,
                        expected,
                        *conf,
                        GapStatus::Open.as_str(),
                        prev.created_at,
                        0.0,
                        "",
                    )?;
                }
                gaps_still_open += 1;
            }
            Some(prev) if prev.status == GapStatus::Resolved => {
                graph.reflect_upsert_known_unknown(
                    gid,
                    eid,
                    pid,
                    expected,
                    *conf,
                    GapStatus::Open.as_str(),
                    now,
                    0.0,
                    "",
                )?;
                gaps_created += 1;
            }
            Some(_dismissed) => {}
        }
    }

    for (gid, prev) in &previous {
        if prev.status != GapStatus::Open {
            continue;
        }
        if current.contains_key(gid) {
            continue;
        }
        let resolver = graph
            .find_claim_id_for_entity_by_type(&prev.entity_id, &prev.expected_claim_type)?
            .unwrap_or_default();
        graph.reflect_upsert_known_unknown(
            gid,
            &prev.entity_id,
            &prev.pattern_id,
            &prev.expected_claim_type,
            prev.confidence,
            GapStatus::Resolved.as_str(),
            prev.created_at,
            now,
            &resolver,
        )?;
        gaps_resolved += 1;
    }

    let open_gaps_total = graph.reflect_count_open_known_unknowns()?;
    let entity_types_scanned = patterns
        .iter()
        .map(|p| p.entity_type.as_str())
        .collect::<HashSet<_>>()
        .len();

    Ok(ReflectResult {
        patterns: patterns.to_vec(),
        gaps_created,
        gaps_resolved,
        gaps_still_open,
        open_gaps_total,
        entity_types_scanned,
    })
}

/// Build the scope id for a set of workspaces. Order-independent so
/// `reflect_across_graphs(["a", "b"])` and `(["b", "a"])` produce the
/// same patterns.
fn scope_id_for(workspaces: &[(String, &GraphStore)]) -> String {
    let mut names: Vec<&str> = workspaces.iter().map(|(n, _)| n.as_str()).collect();
    names.sort_unstable();
    let joined = names.join("|");
    format!("cross:{}", fnv1a_hex(&joined))
}

/// Transition an open gap to `dismissed` so future `reflect()` cycles
/// honor the user's "this isn't actually missing" signal instead of
/// re-raising the gap. No-op (returns `Ok`) if the gap id doesn't exist.
///
/// Dismissed gaps are still stored for audit — they just stop counting
/// toward the health-coverage score and stop appearing in `list_open_gaps`.
pub fn dismiss_gap(graph: &GraphStore, gap_id: &str) -> Result<()> {
    let Some(existing) = graph
        .reflect_load_known_unknowns()?
        .into_iter()
        .find(|row| row.0 == gap_id)
    else {
        // No such gap — nothing to do. Intentionally not an error: a
        // double-dismiss or a dismiss-after-reflect-removed-it is benign.
        return Ok(());
    };
    let (id, eid, pid, expected, conf, _prior_status, created, _prior_resolved, _prior_by) =
        existing;
    let now = chrono::Utc::now().timestamp() as f64;
    graph.reflect_upsert_known_unknown(
        &id,
        &eid,
        &pid,
        &expected,
        conf,
        GapStatus::Dismissed.as_str(),
        created,
        now,
        "", // resolver is only meaningful for Resolved status
    )
}

// ---------------------------------------------------------------------------
// ID helpers — deterministic so re-runs are idempotent.
// ---------------------------------------------------------------------------

fn pattern_id(entity_type: &str, condition: &str, expected: &str) -> String {
    pattern_id_scoped("local", entity_type, condition, expected)
}

/// Scope-qualified pattern id. Two patterns with identical (entity_type,
/// condition, expected) but different scopes (e.g. "local" vs
/// "cross:abc") get distinct ids so they can coexist in
/// `structural_patterns` without a primary-key collision.
fn pattern_id_scoped(scope: &str, entity_type: &str, condition: &str, expected: &str) -> String {
    let key = format!("pattern:{scope}:{entity_type}:{condition}:{expected}");
    format!("pat-{}", fnv1a_hex(&key))
}

fn gap_id(entity_id: &str, pattern_id: &str) -> String {
    let key = format!("gap:{entity_id}:{pattern_id}");
    format!("ku-{}", fnv1a_hex(&key))
}

/// FNV-1a 64-bit. Used only for stable non-cryptographic IDs on patterns
/// and gaps. Not exposed; collision risk at this scale is negligible.
fn fnv1a_hex(key: &str) -> String {
    let mut h: u64 = 1469598103934665603;
    for b in key.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    format!("{h:016x}")
}
