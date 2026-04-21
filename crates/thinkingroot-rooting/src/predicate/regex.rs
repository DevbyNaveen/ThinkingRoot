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

use super::{PredicateEngine, PredicateEvaluation};
use crate::{Result, RootingError};

pub(super) struct RegexEngine;

impl PredicateEngine for RegexEngine {
    fn language(&self) -> PredicateLanguage {
        PredicateLanguage::Regex
    }

    fn evaluate(&self, predicate: &Predicate, source_bytes: &[u8]) -> Result<PredicateEvaluation> {
        let pattern = Regex::new(&predicate.query)
            .map_err(|e| RootingError::InvalidPredicate(format!("regex compile: {e}")))?;

        // Treat non-UTF-8 bytes as a no-match rather than a hard error. Rooting
        // v1 only supports text sources for the regex engine.
        let text = match std::str::from_utf8(source_bytes) {
            Ok(t) => t,
            Err(_) => {
                return Ok(PredicateEvaluation {
                    passed: false,
                    match_count: 0,
                    detail: "source is non-UTF-8 — regex engine requires text".into(),
                });
            }
        };

        let match_count = pattern.find_iter(text).count();
        let passed = match_count > 0;

        let detail = if passed {
            format!("regex matched {match_count} site(s)")
        } else {
            format!("regex '{}' did not match", predicate.query)
        };

        Ok(PredicateEvaluation {
            passed,
            match_count,
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
        let result = eng.evaluate(&pred(r"Stripe"), b"PaymentService uses Stripe").unwrap();
        assert!(result.passed);
        assert_eq!(result.match_count, 1);
    }

    #[test]
    fn counts_all_matches() {
        let eng = RegexEngine;
        let result = eng.evaluate(&pred(r"fn\s+\w+"), b"fn a() {}\nfn b() {}\nfn c() {}").unwrap();
        assert!(result.passed);
        assert_eq!(result.match_count, 3);
    }

    #[test]
    fn returns_no_match_for_absent_pattern() {
        let eng = RegexEngine;
        let result = eng.evaluate(&pred(r"Adyen"), b"PaymentService uses Stripe").unwrap();
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
}
