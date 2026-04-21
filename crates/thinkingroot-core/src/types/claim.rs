use chrono::{DateTime, Utc};
use ordered_float::OrderedFloat;
use serde::{Deserialize, Serialize};

use super::{AdmissionTier, ClaimId, DerivationProof, Predicate, SourceId, WorkspaceId};

/// The fundamental unit of knowledge in ThinkingRoot.
/// A claim is an atomic, source-locked, typed, timestamped statement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claim {
    pub id: ClaimId,
    pub statement: String,
    pub claim_type: ClaimType,
    pub source: SourceId,
    pub source_span: Option<SourceSpan>,
    pub confidence: Confidence,
    pub valid_from: DateTime<Utc>,
    pub valid_until: Option<DateTime<Utc>>,
    pub sensitivity: Sensitivity,
    pub workspace: WorkspaceId,
    pub extracted_by: PipelineVersion,
    pub superseded_by: Option<ClaimId>,
    pub created_at: DateTime<Utc>,
    pub grounding_score: Option<f64>,
    pub grounding_method: Option<GroundingMethod>,
    pub extraction_tier: ExtractionTier,
    /// When the event described by this claim actually occurred.
    /// Distinct from `valid_from` (ingestion timestamp) and `created_at`.
    /// Example: "I graduated in 2018" ingested in 2025 → event_date=2018-05-01, valid_from=2025.
    /// Used by the event calendar (SVO extractor) for accurate temporal reasoning.
    pub event_date: Option<DateTime<Utc>>,
    /// Admission tier set by the Rooting gate.
    ///
    /// Defaults to `Attested` for claims that never underwent the full trial
    /// (extracted claims, pre-Rooting claims). Derived claims admitted through
    /// Phase 6.5 Rooting carry `Rooted` or `Quarantined`.
    #[serde(default)]
    pub admission_tier: AdmissionTier,
    /// Populated only for derived claims. Records the parent claim IDs and
    /// the derivation rule so probes can re-verify the projection onto source.
    #[serde(default)]
    pub derivation: Option<DerivationProof>,
    /// Executable assertion attached to this claim. When present, the
    /// predicate probe re-runs it against source bytes during Rooting and
    /// during daily re-rooting sweeps.
    #[serde(default)]
    pub predicate: Option<Predicate>,
    /// Timestamp of the most recent successful Rooting trial. `None` means
    /// this claim has never been through Rooting (pre-Rooting claims).
    #[serde(default)]
    pub last_rooted_at: Option<DateTime<Utc>>,
}

impl Claim {
    pub fn new(
        statement: impl Into<String>,
        claim_type: ClaimType,
        source: SourceId,
        workspace: WorkspaceId,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: ClaimId::new(),
            statement: statement.into(),
            claim_type,
            source,
            source_span: None,
            confidence: Confidence::new(0.8),
            valid_from: now,
            valid_until: None,
            sensitivity: Sensitivity::Public,
            workspace,
            extracted_by: PipelineVersion::current(),
            superseded_by: None,
            created_at: now,
            grounding_score: None,
            grounding_method: None,
            extraction_tier: ExtractionTier::default(),
            event_date: None,
            admission_tier: AdmissionTier::default(),
            derivation: None,
            predicate: None,
            last_rooted_at: None,
        }
    }

    pub fn with_confidence(mut self, confidence: f64) -> Self {
        self.confidence = Confidence::new(confidence);
        self
    }

    pub fn with_span(mut self, span: SourceSpan) -> Self {
        self.source_span = Some(span);
        self
    }

    pub fn with_sensitivity(mut self, sensitivity: Sensitivity) -> Self {
        self.sensitivity = sensitivity;
        self
    }

    pub fn with_extraction_tier(mut self, tier: ExtractionTier) -> Self {
        self.extraction_tier = tier;
        self
    }

    pub fn with_grounding(mut self, score: f64, method: GroundingMethod) -> Self {
        self.grounding_score = Some(score.clamp(0.0, 1.0));
        self.grounding_method = Some(method);
        self
    }

    pub fn with_event_date(mut self, date: DateTime<Utc>) -> Self {
        self.event_date = Some(date);
        self
    }

    pub fn with_admission_tier(mut self, tier: AdmissionTier) -> Self {
        self.admission_tier = tier;
        self
    }

    pub fn with_derivation(mut self, derivation: DerivationProof) -> Self {
        self.derivation = Some(derivation);
        self
    }

    pub fn with_predicate(mut self, predicate: Predicate) -> Self {
        self.predicate = Some(predicate);
        self
    }

    pub fn with_last_rooted_at(mut self, at: DateTime<Utc>) -> Self {
        self.last_rooted_at = Some(at);
        self
    }

    /// Returns true if this claim is a derivation (has parent claims recorded).
    pub fn is_derived(&self) -> bool {
        self.derivation.is_some()
    }

    /// Mark this claim as superseded by another.
    pub fn supersede(&mut self, by: ClaimId) {
        self.superseded_by = Some(by);
        self.valid_until = Some(Utc::now());
    }

    /// Returns true if this claim is currently active (not superseded, not expired).
    pub fn is_active(&self) -> bool {
        self.superseded_by.is_none() && self.valid_until.is_none_or(|until| until > Utc::now())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimType {
    Fact,
    Decision,
    Opinion,
    Plan,
    Requirement,
    Metric,
    Definition,
    Dependency,
    ApiSignature,
    Architecture,
    /// Implicit user preferences: food/drink likes, habits, communication style, scheduling.
    Preference,
}

/// Confidence score clamped to [0.0, 1.0].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Confidence(OrderedFloat<f64>);

impl Confidence {
    pub fn new(value: f64) -> Self {
        Self(OrderedFloat(value.clamp(0.0, 1.0)))
    }

    pub fn value(&self) -> f64 {
        self.0.into_inner()
    }

    pub fn is_high(&self) -> bool {
        self.value() >= 0.8
    }

    pub fn is_low(&self) -> bool {
        self.value() < 0.5
    }
}

/// Byte-range span within the source document.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SourceSpan {
    pub start_line: u32,
    pub end_line: u32,
    pub start_col: Option<u32>,
    pub end_col: Option<u32>,
}

impl SourceSpan {
    pub fn lines(start: u32, end: u32) -> Self {
        Self {
            start_line: start,
            end_line: end,
            start_col: None,
            end_col: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    Public,
    Internal,
    Confidential,
    Restricted,
}

/// Tracks which version of the pipeline produced a claim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineVersion {
    pub version: String,
    pub extractor: String,
}

impl PipelineVersion {
    pub fn current() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            extractor: "thinkingroot".to_string(),
        }
    }
}

/// How a claim's grounding score was determined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroundingMethod {
    /// Judge 1: keyword/n-gram overlap with source text.
    Lexical,
    /// Judge 2: LLM-cited source quote verified in source text.
    Span,
    /// Judge 3: embedding cosine similarity with source text.
    Semantic,
    /// Combined score from multiple judges.
    Combined,
    /// Not grounded (legacy claims or grounding disabled).
    Unverified,
    /// Structurally extracted from AST — deterministic, no LLM involved.
    Structural,
}

/// Which extraction tier produced this claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionTier {
    /// Tier 0: deterministic structural extraction (tree-sitter AST, imports, type defs).
    /// Zero LLM calls. Zero hallucination. Confidence = 0.99.
    Structural,
    /// Tier 2: LLM extraction with focused prompts and graph-primed context.
    /// Uses API calls. Subject to grounding tribunal.
    #[default]
    Llm,
    /// Tier 3: agent-contributed knowledge written back via the `contribute` MCP tool.
    /// Bypasses the parse→extract pipeline entirely. Trust level is always Untrusted
    /// until a subsequent `root compile` cross-validates against source code.
    AgentInferred,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confidence_clamping() {
        assert_eq!(Confidence::new(1.5).value(), 1.0);
        assert_eq!(Confidence::new(-0.5).value(), 0.0);
        assert_eq!(Confidence::new(0.75).value(), 0.75);
    }

    #[test]
    fn claim_lifecycle() {
        let ws = WorkspaceId::new();
        let src = SourceId::new();
        let mut claim = Claim::new("Rust is fast", ClaimType::Fact, src, ws);

        assert!(claim.is_active());

        let new_claim_id = ClaimId::new();
        claim.supersede(new_claim_id);

        assert!(!claim.is_active());
        assert_eq!(claim.superseded_by, Some(new_claim_id));
    }

    #[test]
    fn claim_grounding_defaults_to_none() {
        let ws = WorkspaceId::new();
        let src = SourceId::new();
        let claim = Claim::new("Rust is fast", ClaimType::Fact, src, ws);
        assert!(claim.grounding_score.is_none());
        assert!(claim.grounding_method.is_none());
    }

    #[test]
    fn claim_with_grounding() {
        let ws = WorkspaceId::new();
        let src = SourceId::new();
        let claim = Claim::new("Rust is fast", ClaimType::Fact, src, ws)
            .with_grounding(0.92, GroundingMethod::Lexical);
        assert_eq!(claim.grounding_score, Some(0.92));
        assert_eq!(claim.grounding_method, Some(GroundingMethod::Lexical));
    }

    #[test]
    fn claim_extraction_tier_defaults_to_llm() {
        let ws = WorkspaceId::new();
        let src = SourceId::new();
        let claim = Claim::new("test", ClaimType::Fact, src, ws);
        assert_eq!(claim.extraction_tier, ExtractionTier::Llm);
    }

    #[test]
    fn claim_admission_tier_defaults_to_attested() {
        let ws = WorkspaceId::new();
        let src = SourceId::new();
        let claim = Claim::new("test", ClaimType::Fact, src, ws);
        assert_eq!(claim.admission_tier, AdmissionTier::Attested);
        assert!(claim.derivation.is_none());
        assert!(claim.predicate.is_none());
        assert!(claim.last_rooted_at.is_none());
        assert!(!claim.is_derived());
    }

    #[test]
    fn claim_with_derivation_marks_as_derived() {
        let ws = WorkspaceId::new();
        let src = SourceId::new();
        let parent_a = ClaimId::new();
        let parent_b = ClaimId::new();
        let claim = Claim::new("derived", ClaimType::Fact, src, ws)
            .with_derivation(DerivationProof {
                parent_claim_ids: vec![parent_a, parent_b],
                derivation_rule: "test-rule".into(),
            })
            .with_admission_tier(AdmissionTier::Rooted);
        assert!(claim.is_derived());
        assert_eq!(claim.admission_tier, AdmissionTier::Rooted);
        let derivation = claim.derivation.as_ref().expect("derivation set");
        assert_eq!(derivation.parent_claim_ids.len(), 2);
        assert_eq!(derivation.derivation_rule, "test-rule");
    }

    #[test]
    fn claim_backward_compat_deserializes_without_new_fields() {
        // Older versions of the Claim struct had no admission_tier / derivation /
        // predicate / last_rooted_at fields. Ensure deserialization populates
        // defaults without error.
        let ws = WorkspaceId::new();
        let src = SourceId::new();
        let original = Claim::new("legacy", ClaimType::Fact, src, ws);
        let mut json_value = serde_json::to_value(&original).unwrap();
        let obj = json_value.as_object_mut().unwrap();
        obj.remove("admission_tier");
        obj.remove("derivation");
        obj.remove("predicate");
        obj.remove("last_rooted_at");

        let round: Claim = serde_json::from_value(json_value).unwrap();
        assert_eq!(round.admission_tier, AdmissionTier::Attested);
        assert!(round.derivation.is_none());
        assert!(round.predicate.is_none());
        assert!(round.last_rooted_at.is_none());
    }
}
