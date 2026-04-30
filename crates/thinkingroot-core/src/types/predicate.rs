use serde::{Deserialize, Serialize};

use super::ClaimId;

/// The admission tier of a claim after trial by the Rooting gate.
///
/// Derived claims must pass deterministic probes (provenance, contradiction,
/// predicate, topology, temporal) against the original source corpus before
/// admission. Extracted claims that never ran through Rooting default to
/// `Attested` for backward compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionTier {
    /// Passed all probes. Every derived claim reaching this tier has a
    /// verifiable certificate linking it to its source bytes.
    Rooted,
    /// Default tier for pre-Rooting claims and claims that never underwent
    /// the full trial. Treated as "source-backed by grounding only."
    #[default]
    Attested,
    /// One or more non-fatal probes failed (predicate / topology / temporal).
    /// Retained in the graph for review; excluded from `trust=rooted` queries
    /// by default.
    Quarantined,
    /// A fatal probe failed (provenance or contradiction). Excluded from all
    /// retrieval; kept in the rejection log only for audit purposes.
    Rejected,
}

impl AdmissionTier {
    /// Canonical string representation used in CozoDB and wire formats.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Rooted => "rooted",
            Self::Attested => "attested",
            Self::Quarantined => "quarantined",
            Self::Rejected => "rejected",
        }
    }

    /// Parse from the canonical string. Unknown strings default to `Attested`
    /// so historical rows without the column migrate cleanly.
    pub fn from_str(s: &str) -> Self {
        match s {
            "rooted" => Self::Rooted,
            "quarantined" => Self::Quarantined,
            "rejected" => Self::Rejected,
            _ => Self::Attested,
        }
    }
}

/// Proof that a derived claim was produced by composition of parent claims.
///
/// Populated only for claims that originate from the Derive pass (Phase 9
/// Reflect, agent contribution via the `contribute` MCP tool, etc.). Extracted
/// claims have `derivation = None`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DerivationProof {
    /// Parent claim IDs the derivation composed. Order is preserved for
    /// reproducibility of the probe run.
    pub parent_claim_ids: Vec<ClaimId>,
    /// Free-form identifier for the rule or pack that produced the derivation
    /// (e.g., "reflect/auth-pattern-v1", "agent/claude-code"). Used for
    /// grouping audit trails.
    pub derivation_rule: String,
}

/// An executable assertion attached to a claim.
///
/// Predicates are generated at extraction time (one extra LLM tool call per
/// claim) and re-executed during Rooting against the original source bytes.
/// Predicate execution is pure CPU — no LLM is in the verification loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Predicate {
    /// The language the `query` is written in. Determines which engine
    /// evaluates it during the predicate probe.
    pub language: PredicateLanguage,
    /// The query itself. Format depends on `language`:
    /// - `RustAst`: a tree-sitter-rust Query pattern
    /// - `Regex`: a standard Rust `regex` crate pattern
    /// - `JsonPath`: a JSONPath expression
    pub query: String,
    /// Which source files this predicate should run against. If empty, scope
    /// falls back to the parent claims' source_ids (for derived claims) or
    /// the claim's own `source` (for extracted claims).
    pub scope: PredicateScope,
}

/// Which languages the predicate engine supports. The MVP covers the three
/// most common source types for developer knowledge packs; more can be added
/// without breaking the wire format (the enum is forward-compatible because
/// unknown variants are rejected at parse time, not silently dropped).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PredicateLanguage {
    /// tree-sitter-rust AST query. Pattern syntax per the tree-sitter Query
    /// documentation.
    RustAst,
    /// Rust `regex` crate pattern. Must compile successfully at extraction
    /// time; invalid patterns are dropped silently so the claim falls back
    /// to `Attested`.
    Regex,
    /// JSONPath expression (jsonpath_lib). Suitable for config files,
    /// OpenAPI specs, and other structured JSON/YAML sources.
    JsonPath,
}

impl PredicateLanguage {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RustAst => "rust_ast",
            Self::Regex => "regex",
            Self::JsonPath => "jsonpath",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "rust_ast" => Some(Self::RustAst),
            "regex" => Some(Self::Regex),
            "jsonpath" => Some(Self::JsonPath),
            _ => None,
        }
    }
}

/// Where a predicate should run. Source URI globs are resolved against the
/// `sources` relation at probe time.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PredicateScope {
    /// Source URI globs (e.g., `["**/*.rs", "src/auth/**"]`). Empty means
    /// "use default scope resolution": parent sources for derived claims,
    /// own source for extracted claims.
    #[serde(default)]
    pub globs: Vec<String>,
}

impl PredicateScope {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn from_globs(globs: Vec<String>) -> Self {
        Self { globs }
    }

    pub fn is_default(&self) -> bool {
        self.globs.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_tier_default_is_attested() {
        assert_eq!(AdmissionTier::default(), AdmissionTier::Attested);
    }

    #[test]
    fn admission_tier_round_trip() {
        for tier in [
            AdmissionTier::Rooted,
            AdmissionTier::Attested,
            AdmissionTier::Quarantined,
            AdmissionTier::Rejected,
        ] {
            assert_eq!(AdmissionTier::from_str(tier.as_str()), tier);
        }
    }

    #[test]
    fn admission_tier_unknown_string_defaults_to_attested() {
        assert_eq!(AdmissionTier::from_str("garbage"), AdmissionTier::Attested);
        assert_eq!(AdmissionTier::from_str(""), AdmissionTier::Attested);
    }

    #[test]
    fn predicate_language_round_trip() {
        for lang in [
            PredicateLanguage::RustAst,
            PredicateLanguage::Regex,
            PredicateLanguage::JsonPath,
        ] {
            assert_eq!(PredicateLanguage::from_str(lang.as_str()), Some(lang));
        }
    }

    #[test]
    fn predicate_language_unknown_string_is_none() {
        assert!(PredicateLanguage::from_str("python_ast").is_none());
        assert!(PredicateLanguage::from_str("").is_none());
    }

    #[test]
    fn predicate_scope_default_is_empty() {
        let scope = PredicateScope::default();
        assert!(scope.is_default());
        assert!(scope.globs.is_empty());
    }

    #[test]
    fn predicate_scope_with_globs_not_default() {
        let scope = PredicateScope::from_globs(vec!["src/**/*.rs".into()]);
        assert!(!scope.is_default());
        assert_eq!(scope.globs.len(), 1);
    }

    #[test]
    fn predicate_serialization_round_trip() {
        let pred = Predicate {
            language: PredicateLanguage::Regex,
            query: r"rate_limit\s*=\s*\d+".into(),
            scope: PredicateScope::from_globs(vec!["config/**/*.yaml".into()]),
        };
        let json = serde_json::to_string(&pred).unwrap();
        let round: Predicate = serde_json::from_str(&json).unwrap();
        assert_eq!(round.language, PredicateLanguage::Regex);
        assert_eq!(round.query, pred.query);
        assert_eq!(round.scope.globs, pred.scope.globs);
    }
}
