//! Temporal probe — timestamp consistency across derivation parents.
//!
//! A derived claim must be temporally *after* each of its parents (you cannot
//! derive something before the evidence existed). For non-derived claims the
//! probe verifies `valid_from <= now` (no future-dated provenance).
//!
//! When `event_date` is set on both child and parents, the probe also
//! verifies the child's event_date does not precede the earliest parent's
//! event_date — preventing derivations that claim knowledge of events that
//! hadn't yet happened relative to their supporting facts.
//!
//! Non-fatal probe: failure → `Quarantined`.

use chrono::Utc;

use super::{Probe, ProbeContext, ProbeName, ProbeResult};
use crate::{Result, RootingError};

pub(crate) struct TemporalProbe;

impl Probe for TemporalProbe {
    const NAME: ProbeName = ProbeName::Temporal;
    const FATAL: bool = false;

    fn run(&self, ctx: &ProbeContext<'_>) -> Result<ProbeResult> {
        let now = Utc::now();
        let claim_valid_from = ctx.claim.valid_from;

        // Non-derived claim: forbid future-dated provenance.
        let derivation = match ctx.derivation {
            Some(d) => d,
            None => {
                let passed = claim_valid_from <= now;
                return Ok(ProbeResult {
                    name: ProbeName::Temporal,
                    score: if passed { 1.0 } else { 0.0 },
                    passed,
                    detail: if passed {
                        "valid_from is not in the future".into()
                    } else {
                        format!(
                            "valid_from {} is after now {}",
                            claim_valid_from, now
                        )
                    },
                });
            }
        };

        if derivation.parent_claim_ids.is_empty() {
            return Ok(ProbeResult::skipped(
                ProbeName::Temporal,
                "derivation has no parent claim ids",
            ));
        }

        // Fetch parents and check (a) parent.valid_from <= child.valid_from,
        // (b) when both have event_date: child.event_date >= earliest parent.event_date.
        let mut earliest_parent_event: Option<chrono::DateTime<Utc>> = None;
        for parent_id in &derivation.parent_claim_ids {
            let parent = ctx
                .graph
                .get_claim_by_id(&parent_id.to_string())
                .map_err(|e| RootingError::Graph(format!("get_claim_by_id: {e}")))?;
            let parent = match parent {
                Some(c) => c,
                None => {
                    // Orphaned parent — evidence vanished. Treat as a
                    // temporal failure rather than a hard error so the claim
                    // still produces a verdict row for audit.
                    return Ok(ProbeResult {
                        name: ProbeName::Temporal,
                        score: 0.0,
                        passed: false,
                        detail: format!("parent claim {} not found in graph", parent_id),
                    });
                }
            };
            if parent.valid_from > claim_valid_from {
                return Ok(ProbeResult {
                    name: ProbeName::Temporal,
                    score: 0.0,
                    passed: false,
                    detail: format!(
                        "parent {} valid_from {} is after child valid_from {}",
                        parent_id, parent.valid_from, claim_valid_from
                    ),
                });
            }
            if let Some(parent_event) = parent.event_date {
                earliest_parent_event = Some(match earliest_parent_event {
                    Some(existing) if existing <= parent_event => existing,
                    _ => parent_event,
                });
            }
        }

        if let (Some(child_event), Some(earliest)) = (ctx.claim.event_date, earliest_parent_event) {
            if child_event < earliest {
                return Ok(ProbeResult {
                    name: ProbeName::Temporal,
                    score: 0.0,
                    passed: false,
                    detail: format!(
                        "child event_date {} precedes earliest parent event_date {}",
                        child_event, earliest
                    ),
                });
            }
        }

        Ok(ProbeResult {
            name: ProbeName::Temporal,
            score: 1.0,
            passed: true,
            detail: "parent timestamps precede child valid_from".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RootingConfig;
    use crate::source_store::FileSystemSourceStore;
    use chrono::{Duration, TimeZone};
    use thinkingroot_core::types::{
        Claim, ClaimType, DerivationProof, Source, SourceType, WorkspaceId,
    };

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

    fn insert_parent(
        graph: &thinkingroot_graph::graph::GraphStore,
        statement: &str,
        valid_from: chrono::DateTime<Utc>,
    ) -> Claim {
        let source = Source::new("file:///p.rs".into(), SourceType::File);
        graph.insert_source(&source).unwrap();
        let mut claim = Claim::new(statement, ClaimType::Fact, source.id, WorkspaceId::new());
        // Both fields set: `created_at` is what survives the current
        // `insert_claim` schema round-trip (Rooting reads that back as
        // `valid_from`), so temporal tests need to synchronize them.
        claim.valid_from = valid_from;
        claim.created_at = valid_from;
        graph.insert_claim(&claim).unwrap();
        claim
    }

    #[test]
    fn non_derived_claim_passes_when_valid_from_is_past() {
        let (_dir, graph, store, config) = env();
        let src = Source::new("file:///x.rs".into(), SourceType::File);
        graph.insert_source(&src).unwrap();
        let claim = Claim::new("x", ClaimType::Fact, src.id, WorkspaceId::new());
        let ctx = ProbeContext {
            claim: &claim,
            predicate: None,
            derivation: None,
            graph: &graph,
            store: &store,
            config: &config,
        };
        let result = TemporalProbe.run(&ctx).unwrap();
        assert!(result.passed);
    }

    #[test]
    fn non_derived_claim_with_future_valid_from_fails() {
        let (_dir, graph, store, config) = env();
        let src = Source::new("file:///x.rs".into(), SourceType::File);
        graph.insert_source(&src).unwrap();
        let mut claim = Claim::new("x", ClaimType::Fact, src.id, WorkspaceId::new());
        claim.valid_from = Utc::now() + Duration::days(30);
        let ctx = ProbeContext {
            claim: &claim,
            predicate: None,
            derivation: None,
            graph: &graph,
            store: &store,
            config: &config,
        };
        let result = TemporalProbe.run(&ctx).unwrap();
        assert!(!result.passed);
        assert!(result.detail.contains("after now"));
    }

    #[test]
    fn derived_claim_passes_when_parents_precede_child() {
        let (_dir, graph, store, config) = env();
        let parent_a = insert_parent(&graph, "a", Utc::now() - Duration::days(10));
        let parent_b = insert_parent(&graph, "b", Utc::now() - Duration::days(5));

        let src = Source::new("file:///d.rs".into(), SourceType::File);
        graph.insert_source(&src).unwrap();
        let derived = Claim::new("derived", ClaimType::Fact, src.id, WorkspaceId::new());
        let derivation = DerivationProof {
            parent_claim_ids: vec![parent_a.id, parent_b.id],
            derivation_rule: "test".into(),
        };
        let ctx = ProbeContext {
            claim: &derived,
            predicate: None,
            derivation: Some(&derivation),
            graph: &graph,
            store: &store,
            config: &config,
        };
        let result = TemporalProbe.run(&ctx).unwrap();
        assert!(result.passed);
    }

    #[test]
    fn derived_claim_fails_when_parent_valid_from_is_after_child() {
        let (_dir, graph, store, config) = env();
        let future_parent = insert_parent(&graph, "future parent", Utc::now() + Duration::days(30));
        let src = Source::new("file:///d.rs".into(), SourceType::File);
        graph.insert_source(&src).unwrap();
        let derived = Claim::new("derived now", ClaimType::Fact, src.id, WorkspaceId::new());
        let derivation = DerivationProof {
            parent_claim_ids: vec![future_parent.id],
            derivation_rule: "test".into(),
        };
        let ctx = ProbeContext {
            claim: &derived,
            predicate: None,
            derivation: Some(&derivation),
            graph: &graph,
            store: &store,
            config: &config,
        };
        let result = TemporalProbe.run(&ctx).unwrap();
        assert!(!result.passed);
        assert!(result.detail.contains("after child"));
    }

    #[test]
    fn derived_claim_fails_when_missing_parent() {
        let (_dir, graph, store, config) = env();
        let src = Source::new("file:///d.rs".into(), SourceType::File);
        graph.insert_source(&src).unwrap();
        let derived = Claim::new("derived", ClaimType::Fact, src.id, WorkspaceId::new());
        let fake_parent = thinkingroot_core::ClaimId::new();
        let derivation = DerivationProof {
            parent_claim_ids: vec![fake_parent],
            derivation_rule: "test".into(),
        };
        let ctx = ProbeContext {
            claim: &derived,
            predicate: None,
            derivation: Some(&derivation),
            graph: &graph,
            store: &store,
            config: &config,
        };
        let result = TemporalProbe.run(&ctx).unwrap();
        assert!(!result.passed);
        assert!(result.detail.contains("not found"));
    }

    #[test]
    fn derived_claim_fails_when_child_event_date_precedes_parent() {
        let (_dir, graph, store, config) = env();
        // Parent event_date is 2024-06-01; child event_date is 2024-01-01.
        let parent_event = Utc.with_ymd_and_hms(2024, 6, 1, 0, 0, 0).unwrap();
        let child_event = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();

        let src = Source::new("file:///p.rs".into(), SourceType::File);
        graph.insert_source(&src).unwrap();
        let mut parent = Claim::new("parent", ClaimType::Fact, src.id, WorkspaceId::new());
        let past = Utc::now() - Duration::days(100);
        parent.valid_from = past;
        parent.created_at = past;
        parent.event_date = Some(parent_event);
        graph.insert_claim(&parent).unwrap();

        let src_d = Source::new("file:///d.rs".into(), SourceType::File);
        graph.insert_source(&src_d).unwrap();
        let mut derived = Claim::new("derived", ClaimType::Fact, src_d.id, WorkspaceId::new());
        derived.event_date = Some(child_event);
        let derivation = DerivationProof {
            parent_claim_ids: vec![parent.id],
            derivation_rule: "test".into(),
        };
        let ctx = ProbeContext {
            claim: &derived,
            predicate: None,
            derivation: Some(&derivation),
            graph: &graph,
            store: &store,
            config: &config,
        };
        let result = TemporalProbe.run(&ctx).unwrap();
        assert!(!result.passed);
        assert!(result.detail.contains("precedes earliest parent"));
    }
}
