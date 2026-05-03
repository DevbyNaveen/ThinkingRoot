//! Sensitivity classifier — Compile Completeness Contract §5.1.
//!
//! Decorates `ExtractedClaim` with a `Sensitivity` tier so Phase 6.7 can
//! populate `claims.sensitivity` (which has been a column since the
//! original schema but was always written as the default `Public` until
//! this contract). Branch T2.6 (PII redaction per branch) and the v3
//! mount-time grant model both consume this column.
//!
//! The classifier is a **regex-first hybrid**: a fast deterministic pass
//! catches the common high-confidence patterns (email, IPv4/v6, SSN,
//! credit card, AWS/JWT tokens, Bearer auth, internal hostnames). The
//! LLM-prompt addendum (in `prompts.rs`) asks the extractor to flag
//! `Confidential` / `Internal` / `PII` / `Public` per claim. Final tier
//! is `max(regex_tier, llm_tier)` — the more restrictive wins.
//!
//! The classifier is intentionally over-eager on PII: missing a PII tag
//! costs more (data leak) than over-tagging (a public README marked
//! Internal). Branch T2.6 will allow per-branch overrides for
//! false-positives.

use std::sync::OnceLock;

use regex::Regex;
use thinkingroot_core::types::Sensitivity;

/// Classify a single claim statement's sensitivity tier from its text.
///
/// Returns `None` only when the input is empty (preserves the existing
/// default-to-Public storage path). Otherwise returns the highest tier
/// matched by the regex layer; the LLM layer's tier is merged in by
/// `max_tier` at the call site (see `merge` below).
pub fn classify_text(text: &str) -> Option<Sensitivity> {
    if text.trim().is_empty() {
        return None;
    }
    let mut highest = Sensitivity::Public;
    for (pattern, tier) in patterns() {
        if pattern.is_match(text) {
            highest = max_tier(highest, *tier);
        }
    }
    if highest == Sensitivity::Public {
        // Public is the default storage value — no need to tag.
        None
    } else {
        Some(highest)
    }
}

/// Merge two sensitivity tiers using `max` semantics — the more
/// restrictive wins. Used to combine the regex-detected tier with the
/// LLM-suggested tier from the extractor prompt.
pub fn merge(a: Option<Sensitivity>, b: Option<Sensitivity>) -> Option<Sensitivity> {
    match (a, b) {
        (None, None) => None,
        (Some(x), None) | (None, Some(x)) => Some(x),
        (Some(x), Some(y)) => Some(max_tier(x, y)),
    }
}

fn max_tier(a: Sensitivity, b: Sensitivity) -> Sensitivity {
    // Sensitivity already derives Ord (claim.rs:259); use `max` directly.
    a.max(b)
}

/// The regex catalog. Patterns chosen for low false-positive rate on
/// typical workspaces — every match is something a maintainer would
/// genuinely want flagged.
fn patterns() -> &'static [(Regex, Sensitivity)] {
    static CELL: OnceLock<Vec<(Regex, Sensitivity)>> = OnceLock::new();
    CELL.get_or_init(|| {
        let entries: &[(&str, Sensitivity)] = &[
            // PII — Confidential
            (r"[\w.+-]+@[A-Za-z0-9-]+\.[A-Za-z0-9.-]+", Sensitivity::Confidential), // email
            (r"\b\d{3}-\d{2}-\d{4}\b", Sensitivity::Restricted),                    // US SSN
            (
                r"\b(?:\d[ -]*?){13,19}\b",
                Sensitivity::Restricted,
            ), // credit-card-shape numbers (loose, will catch some non-CC numbers; LLM layer prunes)
            // Cloud + auth secrets — Restricted
            (r"\bAKIA[0-9A-Z]{16}\b", Sensitivity::Restricted),                  // AWS access key id
            (r"\bASIA[0-9A-Z]{16}\b", Sensitivity::Restricted),                  // AWS STS key id
            (r"\bAIza[0-9A-Za-z_-]{35}\b", Sensitivity::Restricted),             // Google API key
            (r"\bghp_[A-Za-z0-9]{36}\b", Sensitivity::Restricted),               // GitHub PAT
            (r"\bgho_[A-Za-z0-9]{36}\b", Sensitivity::Restricted),               // GitHub OAuth
            (
                r"eyJ[A-Za-z0-9_=-]+\.[A-Za-z0-9_=-]+\.[A-Za-z0-9_.+/=-]+",
                Sensitivity::Restricted,
            ), // JWT
            (
                r"(?i)\bBearer\s+[A-Za-z0-9_\-.=]{20,}\b",
                Sensitivity::Restricted,
            ), // Bearer token
            (
                r"(?i)\b(?:password|passwd|secret|api[_-]?key)\s*[:=]\s*[\S]+",
                Sensitivity::Restricted,
            ), // assignment
            // Internal-only — Internal
            (r"\bipv4\b|\b(?:\d{1,3}\.){3}\d{1,3}\b", Sensitivity::Internal),    // IPv4
            (r"(?i)\b[\w-]+\.corp\.\w+\b", Sensitivity::Internal),               // *.corp.<tld>
            (r"(?i)\b[\w-]+\.internal\b", Sensitivity::Internal),                // *.internal
            (r"(?i)\b[\w-]+\.lan\b", Sensitivity::Internal),                     // *.lan
            (r"(?i)\binternal[- ]only\b", Sensitivity::Internal),                // explicit tag
            (r"(?i)\bdo not share\b", Sensitivity::Internal),                    // explicit tag
            (r"(?i)\bdo not distribute\b", Sensitivity::Confidential),           // explicit tag
            (r"(?i)\bconfidential\b", Sensitivity::Confidential),                // explicit tag
        ];
        entries
            .iter()
            .map(|(pat, tier)| (Regex::new(pat).expect("valid sensitivity regex"), *tier))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_returns_none() {
        assert!(classify_text("").is_none());
        assert!(classify_text("   \n\t  ").is_none());
    }

    #[test]
    fn plain_prose_returns_none() {
        assert!(classify_text("Rust is a systems programming language.").is_none());
    }

    #[test]
    fn email_classifies_as_confidential() {
        assert_eq!(
            classify_text("contact alice@example.com for details"),
            Some(Sensitivity::Confidential),
        );
    }

    #[test]
    fn aws_key_classifies_as_restricted() {
        assert_eq!(
            classify_text("export AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE"),
            Some(Sensitivity::Restricted),
        );
    }

    #[test]
    fn jwt_classifies_as_restricted() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        assert_eq!(
            classify_text(&format!("Authorization: {jwt}")),
            Some(Sensitivity::Restricted),
        );
    }

    #[test]
    fn internal_hostname_classifies_as_internal() {
        assert_eq!(
            classify_text("connect to db01.corp.example"),
            Some(Sensitivity::Internal),
        );
        assert_eq!(
            classify_text("see metrics.internal/dashboard"),
            Some(Sensitivity::Internal),
        );
    }

    #[test]
    fn ipv4_address_classifies_as_internal() {
        assert_eq!(
            classify_text("the server at 10.0.1.42 hosts the API"),
            Some(Sensitivity::Internal),
        );
    }

    #[test]
    fn restrictive_wins_over_internal() {
        // String contains both an IPv4 and an AWS key — Restricted wins.
        let txt = "deploy to 10.0.1.42 with AKIAIOSFODNN7EXAMPLE";
        assert_eq!(classify_text(txt), Some(Sensitivity::Restricted));
    }

    #[test]
    fn merge_picks_higher_tier() {
        assert_eq!(
            merge(Some(Sensitivity::Internal), Some(Sensitivity::Confidential)),
            Some(Sensitivity::Confidential)
        );
        assert_eq!(
            merge(Some(Sensitivity::Public), Some(Sensitivity::Restricted)),
            Some(Sensitivity::Restricted)
        );
        assert_eq!(merge(None, Some(Sensitivity::Internal)), Some(Sensitivity::Internal));
        assert_eq!(merge(None, None), None);
    }
}
