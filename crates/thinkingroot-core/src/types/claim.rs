use chrono::{DateTime, Utc};
use ordered_float::OrderedFloat;
use serde::{Deserialize, Serialize};

use super::{ClaimId, SourceId, WorkspaceId};

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

    pub fn with_grounding(mut self, score: f64, method: GroundingMethod) -> Self {
        self.grounding_score = Some(score.clamp(0.0, 1.0));
        self.grounding_method = Some(method);
        self
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
}
