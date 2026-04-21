//! Contradiction probe — Datalog query against opposing high-confidence claims.
//!
//! The probe fails if this claim appears in any unresolved contradiction
//! where the opposing claim is already admitted at high confidence. Rooting
//! refuses to promote a claim that the graph already believes is contested
//! by something better-established.
//!
//! FATAL probe: a claim that fails contradiction is `Rejected`.

use super::{Probe, ProbeContext, ProbeName, ProbeResult};
use crate::{Result, RootingError};

pub(crate) struct ContradictionProbe;

impl Probe for ContradictionProbe {
    const NAME: ProbeName = ProbeName::Contradiction;
    const FATAL: bool = true;

    fn run(&self, ctx: &ProbeContext<'_>) -> Result<ProbeResult> {
        let claim_id_str = ctx.claim.id.to_string();

        let contradictions = ctx
            .graph
            .get_contradictions()
            .map_err(|e| RootingError::Graph(format!("get_contradictions: {e}")))?;

        for (_cid, claim_a, claim_b, _explanation, status) in contradictions {
            // Only consider unresolved contradictions.
            if status != "Detected" && status != "UnderReview" {
                continue;
            }
            // Determine which side is the candidate vs. the incumbent.
            let other_id = if claim_a == claim_id_str {
                &claim_b
            } else if claim_b == claim_id_str {
                &claim_a
            } else {
                continue;
            };

            // Look up the incumbent's confidence. If it meets the floor, fail.
            let other = ctx
                .graph
                .get_claim_by_id(other_id)
                .map_err(|e| RootingError::Graph(format!("get_claim_by_id: {e}")))?;
            let other = match other {
                Some(c) => c,
                None => continue, // Dangling contradiction — skip.
            };
            if other.confidence.value() >= ctx.config.contradiction_floor {
                return Ok(ProbeResult {
                    name: ProbeName::Contradiction,
                    score: 0.0,
                    passed: false,
                    detail: format!(
                        "contradicts claim {} (confidence {:.2})",
                        other_id,
                        other.confidence.value()
                    ),
                });
            }
        }

        Ok(ProbeResult {
            name: ProbeName::Contradiction,
            score: 1.0,
            passed: true,
            detail: "no high-confidence contradictions".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RootingConfig;
    use crate::source_store::FileSystemSourceStore;
    use thinkingroot_core::types::{Claim, ClaimId, ClaimType, Source, SourceType, WorkspaceId};

    fn env() -> (
        tempfile::TempDir,
        thinkingroot_graph::graph::GraphStore,
        FileSystemSourceStore,
        RootingConfig,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let graph = thinkingroot_graph::graph::GraphStore::init(dir.path()).unwrap();
        let store = FileSystemSourceStore::new(dir.path()).unwrap();
        let config = RootingConfig::default();
        (dir, graph, store, config)
    }

    fn insert_claim(
        graph: &thinkingroot_graph::graph::GraphStore,
        statement: &str,
        confidence: f64,
    ) -> Claim {
        let source = Source::new("file:///x.rs".into(), SourceType::File);
        graph.insert_source(&source).unwrap();
        let claim = Claim::new(statement, ClaimType::Fact, source.id, WorkspaceId::new())
            .with_confidence(confidence);
        graph.insert_claim(&claim).unwrap();
        claim
    }

    fn insert_contradiction(
        graph: &thinkingroot_graph::graph::GraphStore,
        a: ClaimId,
        b: ClaimId,
    ) {
        let id = thinkingroot_core::types::ContradictionId::new().to_string();
        graph
            .insert_contradiction(&id, &a.to_string(), &b.to_string(), "test")
            .unwrap();
    }

    #[test]
    fn passes_when_no_contradictions_exist() {
        let (_dir, graph, store, config) = env();
        let source = Source::new("file:///a.rs".into(), SourceType::File);
        graph.insert_source(&source).unwrap();
        let claim = Claim::new("a claim", ClaimType::Fact, source.id, WorkspaceId::new());

        let ctx = ProbeContext {
            claim: &claim,
            predicate: None,
            derivation: None,
            graph: &graph,
            store: &store,
            config: &config,
        };
        let result = ContradictionProbe.run(&ctx).unwrap();
        assert!(result.passed);
    }

    #[test]
    fn fails_when_high_confidence_contradiction_exists() {
        let (_dir, graph, store, config) = env();
        let incumbent = insert_claim(&graph, "incumbent claim", 0.95);
        let candidate = insert_claim(&graph, "candidate claim", 0.8);

        insert_contradiction(&graph, candidate.id, incumbent.id);

        let ctx = ProbeContext {
            claim: &candidate,
            predicate: None,
            derivation: None,
            graph: &graph,
            store: &store,
            config: &config,
        };
        let result = ContradictionProbe.run(&ctx).unwrap();
        assert!(!result.passed, "candidate should be rejected");
        assert!(result.detail.contains("contradicts"));
    }

    #[test]
    fn passes_when_contradicting_claim_is_low_confidence() {
        let (_dir, graph, store, config) = env();
        let weak = insert_claim(&graph, "weak claim", 0.60);
        let candidate = insert_claim(&graph, "candidate claim", 0.8);

        insert_contradiction(&graph, candidate.id, weak.id);

        let ctx = ProbeContext {
            claim: &candidate,
            predicate: None,
            derivation: None,
            graph: &graph,
            store: &store,
            config: &config,
        };
        let result = ContradictionProbe.run(&ctx).unwrap();
        assert!(
            result.passed,
            "low-confidence incumbent should not block admission"
        );
    }
}
