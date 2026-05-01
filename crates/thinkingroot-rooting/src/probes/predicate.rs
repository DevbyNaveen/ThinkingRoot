//! Predicate probe — dispatches to the appropriate engine based on
//! `predicate.language` and runs it against source bytes.
//!
//! Non-fatal: a predicate that fails to match demotes the claim to
//! `Quarantined` (not `Rejected`). This mirrors the TDD intuition — a failing
//! test should surface the claim for review, not silently delete it.

use super::{Probe, ProbeContext, ProbeName, ProbeResult};
use crate::Result;
use crate::predicate::engine_for;

pub(crate) struct PredicateProbe;

impl Probe for PredicateProbe {
    const NAME: ProbeName = ProbeName::Predicate;
    const FATAL: bool = false;

    fn run(&self, ctx: &ProbeContext<'_>) -> Result<ProbeResult> {
        let predicate = match ctx.predicate {
            Some(p) => p,
            None => {
                return Ok(ProbeResult::skipped(
                    ProbeName::Predicate,
                    "no predicate attached",
                ));
            }
        };

        // Source lookup: the probe resolves against the claim's own source
        // only. For derived claims, the predicate runs against each parent's
        // source (the topology probe handles cross-parent entity relations).
        // Callers that want custom scope should use predicate.scope.globs
        // (not yet plumbed).
        let source_lookup_id = ctx.claim.source.to_string();
        let source = ctx
            .graph
            .get_source_by_id(&source_lookup_id)
            .map_err(|e| crate::RootingError::Graph(format!("source lookup: {e}")))?;
        let content_hash = match source {
            Some(s) if !s.content_hash.is_empty() => s.content_hash,
            _ => {
                return Ok(ProbeResult {
                    name: ProbeName::Predicate,
                    score: 0.0,
                    passed: false,
                    detail: "source content_hash missing — cannot evaluate predicate".into(),
                });
            }
        };

        let bytes = match ctx.store.get(&content_hash)? {
            Some(b) => b.bytes,
            None => {
                return Ok(ProbeResult {
                    name: ProbeName::Predicate,
                    score: 0.0,
                    passed: false,
                    detail: "source bytes not in store — cannot evaluate predicate".into(),
                });
            }
        };

        let engine = engine_for(predicate.language);
        match engine.evaluate(predicate, &bytes) {
            Ok(eval) => Ok(ProbeResult {
                name: ProbeName::Predicate,
                // Score carries the predicate's evidential strength (see
                // `PredicateEvaluation::strength`). Downstream in the rooter
                // this is compared against `config.predicate_strength_threshold`
                // to demote weak-predicate matches from Rooted to Attested.
                score: if eval.passed {
                    eval.strength as f64
                } else {
                    0.0
                },
                passed: eval.passed,
                detail: eval.detail,
            }),
            Err(e) => Ok(ProbeResult {
                name: ProbeName::Predicate,
                score: 0.0,
                passed: false,
                detail: format!("predicate engine error: {e}"),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RootingConfig;
    use crate::source_store::{FileSystemSourceStore, SourceByteStore};
    use thinkingroot_core::types::{
        Claim, ClaimType, ContentHash, Predicate, PredicateLanguage, PredicateScope, Source,
        SourceType, WorkspaceId,
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

    #[test]
    fn skipped_when_no_predicate_attached() {
        let (_dir, graph, store, config) = env();
        let source = Source::new("file:///a.rs".into(), SourceType::File);
        graph.insert_source(&source).unwrap();
        let claim = Claim::new("test", ClaimType::Fact, source.id, WorkspaceId::new());
        let ctx = ProbeContext {
            claim: &claim,
            predicate: None,
            derivation: None,
            graph: &graph,
            store: &store,
            config: &config,
        };
        let result = PredicateProbe.run(&ctx).unwrap();
        assert!(result.passed);
        assert_eq!(result.score, -1.0);
        assert!(result.detail.contains("no predicate"));
    }

    #[test]
    fn passes_when_regex_matches_source() {
        let (_dir, graph, store, config) = env();
        // Use a source long enough that a 17-byte match yields high strength
        // (coverage well under the 0.40 cap → strength > 0.6).
        let body = "pub mod auth {\n    pub fn validate_token(tok: &str) -> bool {\n        !tok.is_empty()\n    }\n    pub fn rotate_key() {}\n    pub fn revoke(tok: &str) {}\n}\n";
        let hash = ContentHash::from_bytes(body.as_bytes());
        let source =
            Source::new("file:///auth.rs".into(), SourceType::File).with_hash(hash.clone());
        graph.insert_source(&source).unwrap();
        store.put(source.id, &hash, body.as_bytes()).unwrap();

        let claim = Claim::new(
            "AuthService exposes validate_token",
            ClaimType::Fact,
            source.id,
            WorkspaceId::new(),
        );
        let predicate = Predicate {
            language: PredicateLanguage::Regex,
            query: r"fn\s+validate_token".into(),
            scope: PredicateScope::empty(),
        };
        let ctx = ProbeContext {
            claim: &claim,
            predicate: Some(&predicate),
            derivation: None,
            graph: &graph,
            store: &store,
            config: &config,
        };
        let result = PredicateProbe.run(&ctx).unwrap();
        assert!(result.passed);
        // Score now carries the predicate's coverage-based strength. A tight
        // match against a realistic source should be well above the default
        // 0.60 strength threshold.
        assert!(
            result.score > 0.6,
            "tight regex match should score strong, got {}",
            result.score
        );
    }

    #[test]
    fn weak_predicate_has_low_strength() {
        // A regex that matches everything (`.`) produces a match, but the
        // coverage is ~100 %. Strength should collapse near 0.0 so downstream
        // rooter logic demotes the claim to Attested rather than Rooted.
        let (_dir, graph, store, config) = env();
        let body = "fn one() {} fn two() {} fn three() {}";
        let hash = ContentHash::from_bytes(body.as_bytes());
        let source = Source::new("file:///b.rs".into(), SourceType::File).with_hash(hash.clone());
        graph.insert_source(&source).unwrap();
        store.put(source.id, &hash, body.as_bytes()).unwrap();

        let claim = Claim::new("weak claim", ClaimType::Fact, source.id, WorkspaceId::new());
        let predicate = Predicate {
            language: PredicateLanguage::Regex,
            query: r".".into(),
            scope: PredicateScope::empty(),
        };
        let ctx = ProbeContext {
            claim: &claim,
            predicate: Some(&predicate),
            derivation: None,
            graph: &graph,
            store: &store,
            config: &config,
        };
        let result = PredicateProbe.run(&ctx).unwrap();
        assert!(result.passed, "`.` always matches non-empty source");
        assert!(
            result.score < 0.1,
            "gaming regex should score weak, got {}",
            result.score
        );
    }

    #[test]
    fn fails_when_regex_does_not_match_source() {
        let (_dir, graph, store, config) = env();
        let body = "fn something_else() {}";
        let hash = ContentHash::from_bytes(body.as_bytes());
        let source = Source::new("file:///b.rs".into(), SourceType::File).with_hash(hash.clone());
        graph.insert_source(&source).unwrap();
        store.put(source.id, &hash, body.as_bytes()).unwrap();

        let claim = Claim::new(
            "claim about stripe",
            ClaimType::Fact,
            source.id,
            WorkspaceId::new(),
        );
        let predicate = Predicate {
            language: PredicateLanguage::Regex,
            query: r"\bStripe\b".into(),
            scope: PredicateScope::empty(),
        };
        let ctx = ProbeContext {
            claim: &claim,
            predicate: Some(&predicate),
            derivation: None,
            graph: &graph,
            store: &store,
            config: &config,
        };
        let result = PredicateProbe.run(&ctx).unwrap();
        assert!(!result.passed);
        assert_eq!(result.score, 0.0);
        assert!(result.detail.contains("did not match"));
    }

    #[test]
    fn fails_when_source_bytes_missing() {
        let (_dir, graph, store, config) = env();
        let source = Source::new("file:///ghost.rs".into(), SourceType::File)
            .with_hash(ContentHash::from_bytes(b"missing"));
        graph.insert_source(&source).unwrap();

        let claim = Claim::new("claim", ClaimType::Fact, source.id, WorkspaceId::new());
        let predicate = Predicate {
            language: PredicateLanguage::Regex,
            query: r"anything".into(),
            scope: PredicateScope::empty(),
        };
        let ctx = ProbeContext {
            claim: &claim,
            predicate: Some(&predicate),
            derivation: None,
            graph: &graph,
            store: &store,
            config: &config,
        };
        let result = PredicateProbe.run(&ctx).unwrap();
        assert!(!result.passed);
        assert!(result.detail.contains("not in store"));
    }
}
