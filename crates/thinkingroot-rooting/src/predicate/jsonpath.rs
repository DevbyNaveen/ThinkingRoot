//! JSONPath predicate engine (RFC 9535 via `serde_json_path`).
//!
//! Compiles the predicate query as a JSONPath expression, parses the source
//! bytes as JSON (with a YAML-lite pre-pass left for future work), and counts
//! matching nodes. Any match → pass.
//!
//! Invalid JSONPath expressions raise `InvalidPredicate`. Non-JSON source
//! bytes return a graceful no-match with a descriptive reason.

use serde_json_path::JsonPath;
use thinkingroot_core::types::{Predicate, PredicateLanguage};

use super::{PredicateEngine, PredicateEvaluation};
use crate::{Result, RootingError};

pub(super) struct JsonPathEngine;

impl PredicateEngine for JsonPathEngine {
    fn language(&self) -> PredicateLanguage {
        PredicateLanguage::JsonPath
    }

    fn evaluate(&self, predicate: &Predicate, source_bytes: &[u8]) -> Result<PredicateEvaluation> {
        let path = JsonPath::parse(&predicate.query).map_err(|e| {
            RootingError::InvalidPredicate(format!("jsonpath parse: {e}"))
        })?;

        let text = match std::str::from_utf8(source_bytes) {
            Ok(t) => t,
            Err(_) => {
                return Ok(PredicateEvaluation {
                    passed: false,
                    match_count: 0,
                    detail: "source is non-UTF-8 — JSONPath engine requires text".into(),
                });
            }
        };

        let json: serde_json::Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => {
                return Ok(PredicateEvaluation {
                    passed: false,
                    match_count: 0,
                    detail: format!("source is not valid JSON: {e}"),
                });
            }
        };

        let matches = path.query(&json);
        let match_count = matches.all().len();
        let passed = match_count > 0;

        let detail = if passed {
            format!("JSONPath matched {match_count} node(s)")
        } else {
            format!("JSONPath '{}' did not match any nodes", predicate.query)
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
            language: PredicateLanguage::JsonPath,
            query: query.into(),
            scope: PredicateScope::empty(),
        }
    }

    #[test]
    fn matches_simple_property() {
        let source = br#"{"rate_limit": 100, "service": "payment"}"#;
        let eng = JsonPathEngine;
        let result = eng.evaluate(&pred("$.rate_limit"), source).unwrap();
        assert!(result.passed);
        assert_eq!(result.match_count, 1);
    }

    #[test]
    fn matches_filter_expression() {
        let source = br#"{"endpoints": [{"path": "/users", "auth": true}, {"path": "/health", "auth": false}]}"#;
        let eng = JsonPathEngine;
        let result = eng
            .evaluate(&pred("$.endpoints[?@.auth == true]"), source)
            .unwrap();
        assert!(result.passed);
        assert_eq!(result.match_count, 1);
    }

    #[test]
    fn no_match_when_path_absent() {
        let source = br#"{"a": 1, "b": 2}"#;
        let eng = JsonPathEngine;
        let result = eng.evaluate(&pred("$.nonexistent"), source).unwrap();
        assert!(!result.passed);
        assert_eq!(result.match_count, 0);
        assert!(result.detail.contains("did not match"));
    }

    #[test]
    fn invalid_jsonpath_returns_error() {
        let eng = JsonPathEngine;
        // Missing closing bracket — invalid JSONPath.
        let result = eng.evaluate(&pred("$.bad["), br#"{}"#);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            RootingError::InvalidPredicate(_)
        ));
    }

    #[test]
    fn handles_non_json_source_gracefully() {
        let eng = JsonPathEngine;
        let result = eng.evaluate(&pred("$.x"), b"this is not json").unwrap();
        assert!(!result.passed);
        assert!(result.detail.contains("not valid JSON"));
    }

    #[test]
    fn handles_non_utf8_source_gracefully() {
        let eng = JsonPathEngine;
        let binary = [0xFFu8, 0xFE, 0xFD];
        let result = eng.evaluate(&pred("$.x"), &binary).unwrap();
        assert!(!result.passed);
        assert!(result.detail.contains("non-UTF-8"));
    }
}
