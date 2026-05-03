//! Named `ScoringProfile` presets.
//!
//! `ScoringProfile::default()` lives in `hybrid_types.rs` (the balanced
//! profile). Named presets that deviate from the balanced defaults live
//! here so callers can opt in by name without spelling out every weight.
//!
//! Spec: `docs/2026-05-02-hybrid-retrieval-spec.md` §5.2.

use super::hybrid_types::ScoringProfile;

impl ScoringProfile {
    /// Compliance / audit profile: rooted-only hits, admission + trial
    /// scores doubled, all penalty weights doubled. For legal review,
    /// security audits, regulator queries.
    pub fn compliance() -> Self {
        let base = ScoringProfile::default();
        Self {
            w_vector: 0.10,
            w_admission: 0.30,
            w_trial: 0.30,
            w_source_authority: 0.10,
            w_recency: 0.05,
            w_complexity: 0.05,
            w_marker: 0.05,
            w_gap_proximity: 0.05,
            w_contradiction: base.w_contradiction * 2.0,
            w_test_origin_penalty: base.w_test_origin_penalty * 2.0,
            recency_half_life_days: base.recency_half_life_days,
            require_rooted_only: true,
            total_candidate_threshold: base.total_candidate_threshold,
        }
    }

    /// Look up a preset by name. Used by the MCP `hybrid_retrieve` tool's
    /// `scoring_profile: "default" | "compliance" | "custom"` field.
    /// Returns `None` for `"custom"` (caller supplies the full struct in
    /// `scoring_profile_custom`).
    pub fn by_name(name: &str) -> Option<Self> {
        match name {
            "default" => Some(Self::default()),
            "compliance" => Some(Self::compliance()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoring_profile_compliance_doubles_penalty_weights() {
        let d = ScoringProfile::default();
        let c = ScoringProfile::compliance();
        assert!((c.w_contradiction - d.w_contradiction * 2.0).abs() < 1e-6);
        assert!((c.w_test_origin_penalty - d.w_test_origin_penalty * 2.0).abs() < 1e-6);
    }

    #[test]
    fn scoring_profile_compliance_requires_rooted_only() {
        assert!(ScoringProfile::compliance().require_rooted_only);
        assert!(!ScoringProfile::default().require_rooted_only);
    }

    #[test]
    fn scoring_profile_compliance_emphasises_admission_and_trial() {
        let c = ScoringProfile::compliance();
        // Admission + trial together carry 60% of the positive-side weight,
        // up from 30% in the balanced profile.
        assert!((c.w_admission - 0.30).abs() < 1e-6);
        assert!((c.w_trial - 0.30).abs() < 1e-6);
    }

    #[test]
    fn scoring_profile_by_name_resolves_known_profiles() {
        assert!(ScoringProfile::by_name("default").is_some());
        assert!(ScoringProfile::by_name("compliance").is_some());
        assert!(ScoringProfile::by_name("custom").is_none());
        assert!(ScoringProfile::by_name("nonexistent").is_none());
    }
}
