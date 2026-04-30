//! Provenance probe — byte-range token overlap against source.
//!
//! The probe answers one question deterministically: "Does enough of this
//! claim's meaningful vocabulary appear in the source bytes it claims to
//! come from?" Reuses the tokenizer from `thinkingroot_ground::LexicalJudge`
//! so provenance scoring here is consistent with Phase 2b grounding.
//!
//! FATAL probe: a claim that fails provenance is `Rejected`.

use thinkingroot_ground::LexicalJudge;

use super::{Probe, ProbeContext, ProbeName, ProbeResult};
use crate::Result;

pub(crate) struct ProvenanceProbe;

impl Probe for ProvenanceProbe {
    const NAME: ProbeName = ProbeName::Provenance;
    const FATAL: bool = true;

    fn run(&self, ctx: &ProbeContext<'_>) -> Result<ProbeResult> {
        let source = ctx
            .graph
            .get_source_by_id(&ctx.claim.source.to_string())
            .map_err(|e| crate::RootingError::Graph(format!("source lookup: {e}")))?;
        let content_hash = match source {
            Some(s) if !s.content_hash.is_empty() => s.content_hash,
            _ => {
                // Fail-open: no content hash means we cannot re-verify, but
                // rejecting all such claims would destroy existing workspaces
                // on upgrade. Return `skipped` so the claim preserves its
                // prior tier (Attested by default).
                return Ok(ProbeResult::skipped(
                    ProbeName::Provenance,
                    "source content_hash missing — cannot re-verify",
                ));
            }
        };
        let bytes = ctx.store.get(&content_hash)?;
        let source_text = match bytes {
            Some(b) => match String::from_utf8(b.bytes.clone()) {
                Ok(s) => s,
                // Non-UTF-8 source (binary). The provenance probe only operates over text sources.
                Err(_) => {
                    return Ok(ProbeResult::skipped(
                        ProbeName::Provenance,
                        "source is non-UTF-8 — provenance probe not supported",
                    ));
                }
            },
            None => {
                // Fail-open: no bytes in store means either an existing
                // workspace that pre-dates Rooting or a non-persisted source
                // type (agent contributions, external URLs). Neither should
                // be rejected outright — skip and let the claim keep its
                // prior tier.
                return Ok(ProbeResult::skipped(
                    ProbeName::Provenance,
                    "source bytes not in store — cannot re-verify",
                ));
            }
        };

        // If the claim has a source_span, narrow to that region for a cheaper
        // and more focused match. v3 byte-range citations are preferred when
        // present (claim cites exact source bytes); pre-v3 line spans
        // continue to work as the fallback. No span at all → score against
        // the whole document.
        let scoped_text = match ctx.claim.source_span {
            Some(span) => match (span.byte_start, span.byte_end) {
                (Some(bs), Some(be)) if be > bs => {
                    extract_byte_range(&source_text, bs as usize, be as usize)
                }
                _ => extract_line_range(&source_text, span.start_line, span.end_line),
            },
            None => source_text.clone(),
        };

        let score = LexicalJudge::score(&ctx.claim.statement, &scoped_text);
        let passed = score >= ctx.config.provenance_threshold;

        let detail = if passed {
            format!("{:.0}% of claim tokens found in source", score * 100.0)
        } else {
            format!(
                "only {:.0}% of claim tokens found in source (threshold {:.0}%)",
                score * 100.0,
                ctx.config.provenance_threshold * 100.0
            )
        };

        Ok(ProbeResult {
            name: ProbeName::Provenance,
            score,
            passed,
            detail,
        })
    }
}

/// Return the substring of `text` spanning `start_line..=end_line`
/// (1-indexed, inclusive). Missing lines return an empty string.
fn extract_line_range(text: &str, start_line: u32, end_line: u32) -> String {
    if start_line == 0 || end_line < start_line {
        return String::new();
    }
    let start_idx = (start_line.saturating_sub(1)) as usize;
    let end_idx = end_line as usize;
    text.lines()
        .skip(start_idx)
        .take(end_idx.saturating_sub(start_idx))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Return the substring of `text` spanning `[byte_start, byte_end)`. Out-of-
/// range or non-UTF-8-aligned indices yield an empty string — the probe
/// then scores against an empty slice and rejects the claim, which is the
/// correct outcome (an unverifiable byte range fails P1 by design). Indices
/// that point partway into a multi-byte codepoint are not aligned to
/// `char_indices()` boundaries and would panic the standard slicing
/// operator, so we walk to the nearest safe boundary instead.
fn extract_byte_range(text: &str, byte_start: usize, byte_end: usize) -> String {
    if byte_end > text.len() || byte_start >= byte_end {
        return String::new();
    }
    // Walk to the nearest valid char boundary at or after byte_start, and
    // at or before byte_end. is_char_boundary returns true for valid UTF-8
    // boundaries; for byte indices that already align (the vast majority
    // for ASCII source files) this is a single check.
    let mut start = byte_start;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    let mut end = byte_end;
    while end > start && !text.is_char_boundary(end) {
        end -= 1;
    }
    if start >= end {
        return String::new();
    }
    text[start..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RootingConfig;
    use crate::source_store::{FileSystemSourceStore, SourceByteStore};
    use thinkingroot_core::types::{
        Claim, ClaimType, ContentHash, Source, SourceType, WorkspaceId,
    };

    fn make_ctx_env() -> (
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

    /// Insert a source row (with content_hash) and persist bytes to the store,
    /// returning the Source so callers can point claims at it.
    fn put_source(
        graph: &thinkingroot_graph::graph::GraphStore,
        store: &FileSystemSourceStore,
        uri: &str,
        body: &str,
    ) -> Source {
        let hash = ContentHash::from_bytes(body.as_bytes());
        let source = Source::new(uri.to_string(), SourceType::File).with_hash(hash.clone());
        graph.insert_source(&source).unwrap();
        store.put(source.id, &hash, body.as_bytes()).unwrap();
        source
    }

    #[test]
    fn passes_when_all_claim_tokens_appear_in_source() {
        let (_dir, graph, store, config) = make_ctx_env();
        let source = put_source(&graph, &store, "file:///a.rs", "PaymentService uses Stripe");
        let claim = Claim::new(
            "PaymentService uses Stripe",
            ClaimType::Fact,
            source.id,
            WorkspaceId::new(),
        );
        let ctx = ProbeContext {
            claim: &claim,
            predicate: None,
            derivation: None,
            graph: &graph,
            store: &store,
            config: &config,
        };
        let result = ProvenanceProbe.run(&ctx).unwrap();
        assert!(result.passed);
        assert!(result.score >= 0.99);
    }

    #[test]
    fn fails_when_claim_tokens_missing_from_source() {
        let (_dir, graph, store, config) = make_ctx_env();
        let source = put_source(
            &graph,
            &store,
            "file:///b.rs",
            "AuthService delegates to cookie jar",
        );
        let claim = Claim::new(
            "PaymentService uses Stripe to process Apple Pay tokens",
            ClaimType::Fact,
            source.id,
            WorkspaceId::new(),
        );
        let ctx = ProbeContext {
            claim: &claim,
            predicate: None,
            derivation: None,
            graph: &graph,
            store: &store,
            config: &config,
        };
        let result = ProvenanceProbe.run(&ctx).unwrap();
        assert!(!result.passed);
        assert!(result.score < config.provenance_threshold);
    }

    #[test]
    fn skipped_when_source_bytes_missing() {
        // Source row exists in the graph but no bytes were persisted. This
        // happens for (a) workspaces that pre-date Rooting and (b) agent
        // contributions whose synthetic sources don't get byte-persistence.
        // Fail-open: return `skipped` so the claim keeps its prior tier
        // rather than being wrongly rejected on upgrade.
        let dir = tempfile::tempdir().unwrap();
        let graph = thinkingroot_graph::graph::GraphStore::init(dir.path()).unwrap();
        let store = FileSystemSourceStore::new(dir.path()).unwrap();
        let config = RootingConfig::default();

        let source = Source::new("file:///ghost.rs".into(), SourceType::File)
            .with_hash(ContentHash::from_bytes(b"missing"));
        graph.insert_source(&source).unwrap();

        let claim = Claim::new(
            "a plausible fact",
            ClaimType::Fact,
            source.id,
            WorkspaceId::new(),
        );
        let ctx = ProbeContext {
            claim: &claim,
            predicate: None,
            derivation: None,
            graph: &graph,
            store: &store,
            config: &config,
        };
        let result = ProvenanceProbe.run(&ctx).unwrap();
        assert!(
            result.passed,
            "should pass-through when bytes are unavailable"
        );
        assert_eq!(result.score, -1.0, "skipped probes score -1.0");
        assert!(result.detail.contains("not in store"));
    }

    #[test]
    fn respects_source_span_when_present() {
        let (_dir, graph, store, config) = make_ctx_env();
        // Line 1: auth info.  Line 2: payment info.  Claim about payments
        // should only pass if the span points to line 2.
        let body = "AuthService handles tokens\nPaymentService uses Stripe for card processing";
        let source = put_source(&graph, &store, "file:///c.rs", body);

        let claim_about_payment = Claim::new(
            "PaymentService uses Stripe for card processing",
            ClaimType::Fact,
            source.id,
            WorkspaceId::new(),
        )
        .with_span(thinkingroot_core::types::SourceSpan::lines(2, 2));

        let ctx = ProbeContext {
            claim: &claim_about_payment,
            predicate: None,
            derivation: None,
            graph: &graph,
            store: &store,
            config: &config,
        };
        let result = ProvenanceProbe.run(&ctx).unwrap();
        assert!(result.passed);
    }
}
