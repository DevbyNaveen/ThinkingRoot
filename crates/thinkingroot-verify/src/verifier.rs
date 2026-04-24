use chrono::Utc;
use thinkingroot_core::Result;
use thinkingroot_core::config::Config;
use thinkingroot_core::types::HealthScore;
use thinkingroot_graph::graph::GraphStore;

/// The Verifier runs health checks on the knowledge base.
pub struct Verifier {
    staleness_days: u32,
}

#[derive(Debug, serde::Serialize)]
pub struct VerificationResult {
    pub health_score: HealthScore,
    pub stale_claims: usize,
    pub contradictions: usize,
    pub orphaned_claims: usize,
    pub warnings: Vec<String>,
}

impl Verifier {
    pub fn new(config: &Config) -> Self {
        Self {
            staleness_days: config.verification.staleness_days,
        }
    }

    /// Run all verification checks against the knowledge graph.
    pub fn verify(&self, graph: &GraphStore) -> Result<VerificationResult> {
        let (sources, claims, entities) = graph.get_counts()?;

        let mut warnings = Vec::new();

        // Staleness: count claims older than staleness_days.
        let cutoff = Utc::now().timestamp() as f64 - (self.staleness_days as f64 * 86400.0);
        let stale_claims = graph.count_stale_claims(cutoff)?;

        let freshness = if claims > 0 {
            1.0 - (stale_claims as f64 / claims as f64)
        } else {
            0.0
        };

        // Consistency: based on unresolved contradictions.
        let contradictions_list = graph.get_contradictions()?;
        let unresolved = contradictions_list
            .iter()
            .filter(|(_, _, _, _, status)| status == "Detected" || status == "UnderReview")
            .count();
        let total_contradictions = contradictions_list.len();

        let consistency = if claims > 0 {
            1.0 - (unresolved as f64 / claims as f64).min(1.0)
        } else {
            0.0
        };

        // Coverage: ratio of claims to entities, further discounted by any
        // open gaps (Phase 9 Reflect known-unknowns) the graph has
        // discovered about itself.
        //
        // Base factor:  min(claims / entities, 1.0) — how many claims per entity.
        // Gap factor:   claims / (claims + open_gaps) — fraction of expected
        //               claims present. 1.0 when no gaps are known (backward-
        //               compatible for workspaces that have never run Reflect).
        //
        // Composite multiplies: filling gaps directly improves the score.
        let open_gaps = graph.reflect_count_open_known_unknowns().unwrap_or(0);
        let base_coverage = if entities > 0 {
            (claims as f64 / entities as f64).min(1.0)
        } else {
            0.0
        };
        let gap_factor = if open_gaps == 0 {
            1.0
        } else {
            let denom = (claims + open_gaps) as f64;
            if denom > 0.0 {
                (claims as f64) / denom
            } else {
                0.0
            }
        };
        let coverage = (base_coverage * gap_factor).clamp(0.0, 1.0);
        if open_gaps > 0 {
            warnings.push(format!(
                "{open_gaps} open knowledge gap(s) from reflexive pattern discovery — query with 'gaps' tool."
            ));
        }

        // Provenance: weighted Rooting survival rate.
        //
        // Rooting graduates the legacy binary provenance check (sources>0 AND claims>0)
        // into a per-claim verifiable score. Tier weights:
        // - Rooted      = 1.0  (passed all active probes + has certificate)
        // - Attested    = 0.5  (fatal probes passed, no active non-fatal evidence)
        // - Quarantined = 0.25 (non-fatal probe failed — explicit signal, not silent drop)
        // - Rejected    = 0.0  (excluded from retrieval anyway; kept for audit)
        //
        // Backward compat: when no Rooted claims exist yet (a pack that pre-dates
        // Rooting or one whose Phase 6.5 is disabled), we fall back to the legacy
        // binary check so existing tests + dashboards don't regress to 0%.
        let (rooted, attested, quarantined, rejected) = graph.count_claims_by_admission_tier()?;
        let tier_total = rooted + attested + quarantined + rejected;
        let provenance = if tier_total == 0 {
            // No claims at all.
            0.0
        } else if rooted == 0 && quarantined == 0 && rejected == 0 {
            // Pure-Attested graph — no Rooting has run. Preserve legacy semantics.
            if claims > 0 && sources > 0 { 1.0 } else { 0.0 }
        } else {
            let weighted = (rooted as f64) * 1.0
                + (attested as f64) * 0.5
                + (quarantined as f64) * 0.25
                + (rejected as f64) * 0.0;
            (weighted / tier_total as f64).clamp(0.0, 1.0)
        };

        if sources == 0 {
            warnings.push("No sources ingested yet.".to_string());
        }
        if entities == 0 {
            warnings.push("No entities extracted yet.".to_string());
        }
        if claims == 0 {
            warnings.push("No claims extracted yet.".to_string());
        }
        if stale_claims > 0 {
            warnings.push(format!(
                "{stale_claims} claims are older than {} days.",
                self.staleness_days
            ));
        }
        if unresolved > 0 {
            warnings.push(format!("{unresolved} unresolved contradictions detected."));
        }

        // Orphan detection: claims whose source no longer exists.
        let orphaned_claims = graph.count_orphaned_claims()?;
        if orphaned_claims > 0 {
            warnings.push(format!(
                "{orphaned_claims} orphaned claims (source deleted or missing)."
            ));
        }

        // Confidence decay: count superseded claims still referenced.
        let superseded = graph.count_superseded_claims()?;
        if superseded > 0 {
            warnings.push(format!(
                "{superseded} claims have been superseded by newer information."
            ));
        }

        // Grounding: count claims with low grounding scores.
        let low_grounding = graph.count_low_grounding_claims(0.5)?;
        if low_grounding > 0 {
            warnings.push(format!(
                "{low_grounding} claims have low grounding scores (< 0.5) — review recommended."
            ));
        }

        let health_score = HealthScore::compute(freshness, consistency, coverage, provenance);

        tracing::info!(
            "verification: health={}%, fresh={:.0}%, consistent={:.0}%, coverage={:.0}%, provenance={:.0}%",
            health_score.as_percentage(),
            freshness * 100.0,
            consistency * 100.0,
            coverage * 100.0,
            provenance * 100.0,
        );

        Ok(VerificationResult {
            health_score,
            stale_claims,
            contradictions: total_contradictions,
            orphaned_claims,
            warnings,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use thinkingroot_core::types::{ClaimType, PipelineVersion, Sensitivity, SourceType};
    use thinkingroot_core::{Claim, ClaimId, Source, SourceId, WorkspaceId};
    use thinkingroot_graph::graph::GraphStore;

    fn make_graph() -> (TempDir, GraphStore) {
        let dir = TempDir::new().expect("temp dir");
        let graph = GraphStore::init(dir.path()).expect("graph init");
        (dir, graph)
    }

    fn make_source(uri: &str) -> Source {
        Source::new(uri.to_string(), SourceType::File)
    }

    fn make_claim(statement: &str, source: &Source) -> Claim {
        Claim::new(statement, ClaimType::Fact, source.id, WorkspaceId::new())
    }

    fn default_verifier() -> Verifier {
        Verifier::new(&Config::default())
    }

    // ── Health score formula ──────────────────────────────────────────────

    #[test]
    fn empty_graph_produces_zero_health() {
        let (_dir, graph) = make_graph();
        let result = default_verifier().verify(&graph).unwrap();
        assert_eq!(result.health_score.overall, 0.0);
        assert_eq!(result.health_score.freshness, 0.0);
        assert_eq!(result.health_score.consistency, 0.0);
        assert_eq!(result.health_score.coverage, 0.0);
        assert_eq!(result.health_score.provenance, 0.0);
    }

    #[test]
    fn empty_graph_emits_no_source_warning() {
        let (_dir, graph) = make_graph();
        let result = default_verifier().verify(&graph).unwrap();
        assert!(result.warnings.iter().any(|w| w.contains("No sources")));
    }

    #[test]
    fn graph_with_source_and_claim_reaches_full_provenance() {
        let (_dir, graph) = make_graph();
        let source = make_source("test://doc.md");
        graph.insert_source(&source).unwrap();
        let claim = make_claim("The sky is blue.", &source);
        graph.insert_claim(&claim).unwrap();

        let result = default_verifier().verify(&graph).unwrap();
        // 1 claim, 1 entity (none) → coverage = claims/entities = 0 (no entities)
        // provenance = 1.0 (sources > 0 && claims > 0)
        assert_eq!(result.health_score.provenance, 1.0);
        assert_eq!(result.orphaned_claims, 0);
        assert_eq!(result.stale_claims, 0);
    }

    #[test]
    fn coverage_formula_is_claims_over_entities_capped_at_one() {
        let (_dir, graph) = make_graph();
        // Insert 3 claims, 2 entities — coverage = min(3/2, 1.0) = 1.0
        let source = make_source("test://doc.md");
        graph.insert_source(&source).unwrap();
        for i in 0..3 {
            let claim = make_claim(&format!("Claim number {i}."), &source);
            graph.insert_claim(&claim).unwrap();
        }
        use thinkingroot_core::{Entity, EntityType};
        for i in 0..2 {
            let entity = Entity::new(format!("Entity{i}"), EntityType::System);
            graph.insert_entity(&entity).unwrap();
        }

        let result = default_verifier().verify(&graph).unwrap();
        // coverage = min(3/2, 1.0) = 1.0 (claims > entities)
        assert_eq!(result.health_score.coverage, 1.0);
    }

    #[test]
    fn coverage_below_one_when_claims_fewer_than_entities() {
        let (_dir, graph) = make_graph();
        let source = make_source("test://doc.md");
        graph.insert_source(&source).unwrap();
        // 1 claim, 4 entities → coverage = 1/4 = 0.25
        let claim = make_claim("Single claim.", &source);
        graph.insert_claim(&claim).unwrap();
        use thinkingroot_core::{Entity, EntityType};
        for i in 0..4 {
            let entity = Entity::new(format!("Entity{i}"), EntityType::System);
            graph.insert_entity(&entity).unwrap();
        }

        let result = default_verifier().verify(&graph).unwrap();
        assert!((result.health_score.coverage - 0.25).abs() < 0.001);
    }

    // ── Staleness ────────────────────────────────────────────────────────

    #[test]
    fn fresh_claims_do_not_trigger_stale_warning() {
        let (_dir, graph) = make_graph();
        let source = make_source("test://fresh.md");
        graph.insert_source(&source).unwrap();
        // Claim created now — not stale
        let claim = make_claim("Very recent fact.", &source);
        graph.insert_claim(&claim).unwrap();

        let result = default_verifier().verify(&graph).unwrap();
        assert_eq!(result.stale_claims, 0);
        assert!(!result.warnings.iter().any(|w| w.contains("older than")));
    }

    // ── Orphan detection ─────────────────────────────────────────────────

    #[test]
    fn orphaned_claim_detected_when_source_missing() {
        let (_dir, graph) = make_graph();
        // Insert a claim whose source_id has no corresponding source row.
        let phantom_source_id = SourceId::new();
        let claim = Claim {
            id: ClaimId::new(),
            statement: "Orphaned statement.".to_string(),
            claim_type: ClaimType::Fact,
            source: phantom_source_id,
            source_span: None,
            confidence: thinkingroot_core::types::Confidence::new(0.9),
            valid_from: chrono::Utc::now(),
            valid_until: None,
            sensitivity: Sensitivity::Public,
            workspace: WorkspaceId::new(),
            extracted_by: PipelineVersion::current(),
            superseded_by: None,
            created_at: chrono::Utc::now(),
            grounding_score: None,
            grounding_method: None,
            extraction_tier: thinkingroot_core::types::ExtractionTier::default(),
            event_date: None,
            admission_tier: thinkingroot_core::types::AdmissionTier::default(),
            derivation: None,
            predicate: None,
            last_rooted_at: None,
        };
        graph.insert_claim(&claim).unwrap();

        let result = default_verifier().verify(&graph).unwrap();
        assert_eq!(result.orphaned_claims, 1);
        assert!(result.warnings.iter().any(|w| w.contains("orphaned")));
    }

    #[test]
    fn no_orphans_when_source_present() {
        let (_dir, graph) = make_graph();
        let source = make_source("test://linked.md");
        graph.insert_source(&source).unwrap();
        let claim = make_claim("Properly linked claim.", &source);
        graph.insert_claim(&claim).unwrap();

        let result = default_verifier().verify(&graph).unwrap();
        assert_eq!(result.orphaned_claims, 0);
    }

    // ── Grounding ────────────────────────────────────────────────────────

    #[test]
    fn low_grounding_claims_produce_warning() {
        let (_dir, graph) = make_graph();
        let source = make_source("test://grounding.md");
        graph.insert_source(&source).unwrap();

        // Insert a claim with low grounding score.
        use thinkingroot_core::types::GroundingMethod;
        let claim = make_claim("Weakly grounded claim.", &source)
            .with_grounding(0.3, GroundingMethod::Lexical);
        graph.insert_claim(&claim).unwrap();

        let result = default_verifier().verify(&graph).unwrap();
        assert!(result.warnings.iter().any(|w| w.contains("low grounding")));
    }

    #[test]
    fn well_grounded_claims_no_warning() {
        let (_dir, graph) = make_graph();
        let source = make_source("test://grounding2.md");
        graph.insert_source(&source).unwrap();

        use thinkingroot_core::types::GroundingMethod;
        let claim = make_claim("Well grounded claim.", &source)
            .with_grounding(0.9, GroundingMethod::Combined);
        graph.insert_claim(&claim).unwrap();

        let result = default_verifier().verify(&graph).unwrap();
        assert!(!result.warnings.iter().any(|w| w.contains("low grounding")));
    }

    // ── Rooting-aware provenance formula ─────────────────────────────────

    #[test]
    fn provenance_reflects_weighted_rooting_tiers_when_active() {
        use thinkingroot_core::types::AdmissionTier;
        let (_dir, graph) = make_graph();
        let source = make_source("test://rooting.md");
        graph.insert_source(&source).unwrap();

        // 2 Rooted (1.0 each) + 1 Attested (0.5) + 1 Quarantined (0.25) = 2.75 / 4 = 0.6875
        let make = |tier: AdmissionTier, label: &str| {
            let claim = make_claim(label, &source).with_admission_tier(tier);
            graph.insert_claim(&claim).unwrap();
        };
        make(AdmissionTier::Rooted, "r1");
        make(AdmissionTier::Rooted, "r2");
        make(AdmissionTier::Attested, "a1");
        make(AdmissionTier::Quarantined, "q1");

        let result = default_verifier().verify(&graph).unwrap();
        let expected = (2.0 + 0.5 + 0.25) / 4.0;
        assert!(
            (result.health_score.provenance - expected).abs() < 1e-6,
            "expected provenance {expected}, got {}",
            result.health_score.provenance
        );
    }

    #[test]
    fn provenance_falls_back_to_legacy_binary_when_only_attested_exists() {
        // A pack that pre-dates Rooting or whose Rooting phase was disabled
        // should not silently drop to 50% provenance. Preserve the legacy
        // binary signal: present source + present claim → 1.0.
        let (_dir, graph) = make_graph();
        let source = make_source("test://legacy.md");
        graph.insert_source(&source).unwrap();
        let claim = make_claim("legacy claim", &source);
        // Defaults to AdmissionTier::Attested.
        graph.insert_claim(&claim).unwrap();

        let result = default_verifier().verify(&graph).unwrap();
        assert_eq!(result.health_score.provenance, 1.0);
    }

    #[test]
    fn provenance_drops_when_rejected_claims_dominate() {
        use thinkingroot_core::types::AdmissionTier;
        let (_dir, graph) = make_graph();
        let source = make_source("test://rejected.md");
        graph.insert_source(&source).unwrap();

        let make = |tier: AdmissionTier, label: &str| {
            let claim = make_claim(label, &source).with_admission_tier(tier);
            graph.insert_claim(&claim).unwrap();
        };
        make(AdmissionTier::Rooted, "r1");
        make(AdmissionTier::Rejected, "x1");
        make(AdmissionTier::Rejected, "x2");
        make(AdmissionTier::Rejected, "x3");

        let result = default_verifier().verify(&graph).unwrap();
        // Weighted: (1.0 + 0 + 0 + 0) / 4 = 0.25
        assert!(
            (result.health_score.provenance - 0.25).abs() < 1e-6,
            "expected 0.25, got {}",
            result.health_score.provenance
        );
    }

    // ── Overall score formula ────────────────────────────────────────────

    #[test]
    fn overall_score_uses_weighted_formula() {
        // overall = freshness*0.3 + consistency*0.3 + coverage*0.2 + provenance*0.2
        let score = HealthScore::compute(1.0, 1.0, 0.5, 1.0);
        // 1.0*0.3 + 1.0*0.3 + 0.5*0.2 + 1.0*0.2 = 0.30 + 0.30 + 0.10 + 0.20 = 0.90
        assert!((score.overall - 0.90).abs() < 0.001);
        assert_eq!(score.as_percentage(), 90);
    }

    #[test]
    fn overall_score_perfect_when_all_dimensions_one() {
        let score = HealthScore::compute(1.0, 1.0, 1.0, 1.0);
        assert!((score.overall - 1.0).abs() < 0.001);
        assert_eq!(score.as_percentage(), 100);
    }

    #[test]
    fn overall_score_zero_when_all_dimensions_zero() {
        let score = HealthScore::compute(0.0, 0.0, 0.0, 0.0);
        assert_eq!(score.overall, 0.0);
        assert_eq!(score.as_percentage(), 0);
    }
}
