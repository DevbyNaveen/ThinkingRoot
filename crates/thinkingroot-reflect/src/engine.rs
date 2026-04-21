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
}

impl Default for ReflectConfig {
    fn default() -> Self {
        // Matches the spec (docs/2026-04-19-reflexive-knowledge-architecture.md
        // §"Minimum Threshold" and §"Implementation Sketch").
        Self {
            min_sample_size: 30,
            min_frequency: 0.70,
            max_patterns: 500,
        }
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
                Some(StructuralPattern {
                    id: pattern_id(&etype, &cta, &ctb),
                    entity_type: etype,
                    condition_claim_type: cta,
                    expected_claim_type: ctb,
                    frequency,
                    sample_size: cond_n,
                    last_computed: now,
                    min_sample_threshold: self.cfg.min_sample_size,
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
        let rows: Vec<(String, String, String, String, f64, usize, f64, usize)> = patterns
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
                )
            })
            .collect();
        graph.reflect_rewrite_patterns(&rows)?;

        // ── 3. Compute current gap set from patterns ──────────────
        // Key: gap_id; Value: (entity_id, pattern_id, expected, confidence)
        let mut current: HashMap<String, (String, String, String, f64)> = HashMap::new();
        for p in &patterns {
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
                        p.frequency,
                    ),
                );
            }
        }

        // ── 4. Diff against previously-stored known_unknowns ──────
        let previous: HashMap<String, KnownUnknown> = graph
            .reflect_load_known_unknowns()?
            .into_iter()
            .map(|(id, eid, pid, expected, conf, status, created, resolved, resolved_by)| {
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
            })
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
            |(_gid, eid, ename, etype, expected, confidence, pid, sample, created)| GapReport {
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

// ---------------------------------------------------------------------------
// ID helpers — deterministic so re-runs are idempotent.
// ---------------------------------------------------------------------------

fn pattern_id(entity_type: &str, condition: &str, expected: &str) -> String {
    let key = format!("pattern:{entity_type}:{condition}:{expected}");
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
