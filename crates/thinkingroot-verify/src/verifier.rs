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

        // Coverage: ratio of claims to entities (more claims per entity = better coverage).
        let coverage = if entities > 0 {
            (claims as f64 / entities as f64).min(1.0)
        } else {
            0.0
        };

        // Provenance: all claims should have valid source links.
        let provenance = if claims > 0 && sources > 0 { 1.0 } else { 0.0 };

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
