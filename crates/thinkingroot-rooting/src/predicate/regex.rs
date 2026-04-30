//! Regex predicate engine.
//!
//! Compiles the `predicate.query` as a Rust `regex` crate pattern and runs it
//! against the source bytes (interpreted as UTF-8 text). Match count >= 1 is
//! a pass; zero matches is a fail.
//!
//! Invalid regex patterns produce an `InvalidPredicate` error so callers can
//! distinguish "predicate malformed" from "source doesn't match."

use regex::Regex;
use thinkingroot_core::types::{Predicate, PredicateLanguage};

use super::{PredicateEngine, PredicateEvaluation, coverage_strength};
use crate::{Result, RootingError};

pub(super) struct RegexEngine;

impl PredicateEngine for RegexEngine {
    fn language(&self) -> PredicateLanguage {
        PredicateLanguage::Regex
    }

    fn evaluate(&self, predicate: &Predicate, source_bytes: &[u8]) -> Result<PredicateEvaluation> {
        let pattern = Regex::new(&predicate.query)
            .map_err(|e| RootingError::InvalidPredicate(format!("regex compile: {e}")))?;

        // Treat non-UTF-8 bytes as a no-match rather than a hard error. The
        // regex engine only operates over text sources.
        let text = match std::str::from_utf8(source_bytes) {
            Ok(t) => t,
            Err(_) => {
                return Ok(PredicateEvaluation {
                    passed: false,
                    match_count: 0,
                    strength: 0.0,
                    detail: "source is non-UTF-8 — regex engine requires text".into(),
                });
            }
        };

        // Iterate once, accumulating both count and total matched bytes for
        // the strength calculation.
        let mut match_count = 0usize;
        let mut matched_bytes = 0usize;
        for m in pattern.find_iter(text) {
            match_count += 1;
            matched_bytes += m.end() - m.start();
        }
        let passed = match_count > 0;
        let strength = if passed {
            coverage_strength(matched_bytes, text.len())
        } else {
            0.0
        };

        let detail = if passed {
            format!(
                "regex matched {match_count} site(s) (strength {:.2})",
                strength
            )
        } else {
            format!("regex '{}' did not match", predicate.query)
        };

        Ok(PredicateEvaluation {
            passed,
            match_count,
            strength,
            detail,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thinkingroot_core::types::PredicateScope;

    fn pred(query: &str) -> Predicate {
        Predicate {
            language: PredicateLanguage::Regex,
            query: query.into(),
            scope: PredicateScope::empty(),
        }
    }

    #[test]
    fn matches_simple_pattern() {
        let eng = RegexEngine;
        let result = eng
            .evaluate(&pred(r"Stripe"), b"PaymentService uses Stripe")
            .unwrap();
        assert!(result.passed);
        assert_eq!(result.match_count, 1);
    }

    #[test]
    fn counts_all_matches() {
        let eng = RegexEngine;
        let result = eng
            .evaluate(&pred(r"fn\s+\w+"), b"fn a() {}\nfn b() {}\nfn c() {}")
            .unwrap();
        assert!(result.passed);
        assert_eq!(result.match_count, 3);
    }

    #[test]
    fn returns_no_match_for_absent_pattern() {
        let eng = RegexEngine;
        let result = eng
            .evaluate(&pred(r"Adyen"), b"PaymentService uses Stripe")
            .unwrap();
        assert!(!result.passed);
        assert_eq!(result.match_count, 0);
        assert!(result.detail.contains("did not match"));
    }

    #[test]
    fn invalid_regex_returns_error() {
        let eng = RegexEngine;
        // Unbalanced paren — invalid regex.
        let result = eng.evaluate(&pred("unbalanced("), b"ignored");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, RootingError::InvalidPredicate(_)));
    }

    #[test]
    fn gracefully_handles_non_utf8_source() {
        let eng = RegexEngine;
        let binary = [0xFFu8, 0xFE, 0xFD];
        let result = eng.evaluate(&pred(r"fn"), &binary).unwrap();
        assert!(!result.passed);
        assert!(result.detail.contains("non-UTF-8"));
    }

    #[test]
    fn strength_is_high_for_tight_unique_match() {
        // A specific function signature matching once in a realistic source
        // file should produce strength close to 1.0.
        let eng = RegexEngine;
        let source = b"pub mod auth {\n    pub fn validate_token(tok: &str) -> bool {\n        !tok.is_empty()\n    }\n    pub fn rotate_key() {}\n    pub fn revoke(tok: &str) {}\n}\n";
        let result = eng.evaluate(&pred(r"fn\s+validate_token"), source).unwrap();
        assert!(result.passed);
        assert_eq!(result.match_count, 1);
        assert!(
            result.strength > 0.85,
            "tight match should score strong, got {}",
            result.strength
        );
    }

    #[test]
    fn strength_is_low_for_dot_wildcard() {
        // A regex `.` matches every byte of the source → coverage ≈ 1.0 →
        // strength ≈ 0.0. This is the canonical "gamed" pattern the
        // predicate-strength scoring is designed to catch.
        let eng = RegexEngine;
        let source = b"fn one() {} fn two() {} fn three() {}";
        let result = eng.evaluate(&pred(r"."), source).unwrap();
        assert!(result.passed);
        assert!(
            result.strength < 0.1,
            "`.` should be flagged weak, got {}",
            result.strength
        );
    }

    #[test]
    fn strength_is_low_for_word_char_wildcard() {
        // `\w+` is another common gaming pattern: it matches nearly every
        // meaningful run of source text. Strength should be clearly below
        // the default 0.60 threshold.
        let eng = RegexEngine;
        let source = b"fn one() {} fn two() {} fn three() {}";
        let result = eng.evaluate(&pred(r"\w+"), source).unwrap();
        assert!(result.passed);
        assert!(
            result.strength < 0.6,
            "`\\w+` should be flagged weak, got {}",
            result.strength
        );
    }

    #[test]
    fn strength_stays_high_for_specific_multimatch_pattern() {
        // A legitimate pattern that matches several concrete call sites
        // (e.g. `tokio::spawn(` in a file full of spawns) should stay
        // strong because each match is localised — total matched bytes are
        // still a small fraction of source length.
        let eng = RegexEngine;
        let mut source = String::from("// a large module file with lots of unrelated code.\n");
        for _ in 0..200 {
            source.push_str("let v = 1; let w = 2; /* padding */\n");
        }
        for _ in 0..5 {
            source.push_str("tokio::spawn(async move { work().await });\n");
        }
        let result = eng
            .evaluate(&pred(r"tokio::spawn\("), source.as_bytes())
            .unwrap();
        assert!(result.passed);
        assert_eq!(result.match_count, 5);
        assert!(
            result.strength > 0.9,
            "specific pattern across many call sites should still score strong, got {}",
            result.strength
        );
    }
}
