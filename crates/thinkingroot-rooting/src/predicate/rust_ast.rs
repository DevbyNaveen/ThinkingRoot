//! tree-sitter-rust AST predicate engine.
//!
//! Compiles the predicate query as a tree-sitter Query against the Rust
//! grammar, parses source bytes, and counts captures. Match count ≥ 1 → pass.
//!
//! Query syntax follows tree-sitter's S-expression pattern language. Examples:
//! - Find any function definition:   `(function_item)`
//! - Named function named `foo`:     `(function_item name: (identifier) @n (#eq? @n "foo"))`
//! - Any call expression:            `(call_expression)`
//!
//! Invalid queries (grammar errors) return an `InvalidPredicate` error so
//! callers can distinguish malformed predicates from "no match."

use thinkingroot_core::types::{Predicate, PredicateLanguage};
use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator};

use super::{PredicateEngine, PredicateEvaluation, coverage_strength};
use crate::{Result, RootingError};

pub(super) struct RustAstEngine;

impl PredicateEngine for RustAstEngine {
    fn language(&self) -> PredicateLanguage {
        PredicateLanguage::RustAst
    }

    fn evaluate(&self, predicate: &Predicate, source_bytes: &[u8]) -> Result<PredicateEvaluation> {
        let ts_language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();

        let query = Query::new(&ts_language, &predicate.query)
            .map_err(|e| RootingError::InvalidPredicate(format!("tree-sitter-rust query: {e}")))?;

        let mut parser = Parser::new();
        parser
            .set_language(&ts_language)
            .map_err(|e| RootingError::InvalidPredicate(format!("tree-sitter parser init: {e}")))?;

        let tree = match parser.parse(source_bytes, None) {
            Some(t) => t,
            None => {
                return Ok(PredicateEvaluation {
                    passed: false,
                    match_count: 0,
                    strength: 0.0,
                    detail: "tree-sitter-rust parse returned no tree".into(),
                });
            }
        };

        let mut cursor = QueryCursor::new();
        let mut matches_iter = cursor.matches(&query, tree.root_node(), source_bytes);
        let mut match_count = 0usize;
        // Sum byte-range coverage across captured nodes for strength scoring.
        // A query like `(source_file)` matches the whole tree → high coverage
        // → low strength. A tight query like a named function definition
        // matches a small subrange → low coverage → high strength.
        let mut matched_bytes = 0usize;
        while let Some(m) = matches_iter.next() {
            match_count += 1;
            for capture in m.captures {
                let span = capture.node.end_byte() - capture.node.start_byte();
                matched_bytes += span;
            }
        }

        let passed = match_count > 0;
        let strength = if !passed {
            0.0
        } else if matched_bytes > 0 {
            coverage_strength(matched_bytes, source_bytes.len())
        } else {
            // Pattern has no named captures (e.g. bare `(function_item)`).
            // Fall back to inverse match-count so a broad query matching many
            // nodes still signals low specificity.
            (1.0 / match_count as f32).min(1.0)
        };
        let detail = if passed {
            format!(
                "AST query captured {match_count} site(s) (strength {:.2})",
                strength
            )
        } else {
            format!("AST query '{}' did not match any nodes", predicate.query)
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
            language: PredicateLanguage::RustAst,
            query: query.into(),
            scope: PredicateScope::empty(),
        }
    }

    #[test]
    fn matches_function_definition() {
        let source = br#"fn validate_token(t: &str) -> bool { true }"#;
        let eng = RustAstEngine;
        let result = eng.evaluate(&pred("(function_item)"), source).unwrap();
        assert!(result.passed);
        assert!(result.match_count >= 1);
    }

    #[test]
    fn matches_named_function_with_predicate_constraint() {
        let source = br#"
fn foo() {}
fn bar() {}
fn validate_token() {}
"#;
        let eng = RustAstEngine;
        let q = r#"(function_item name: (identifier) @n (#eq? @n "validate_token"))"#;
        let result = eng.evaluate(&pred(q), source).unwrap();
        assert!(result.passed);
    }

    #[test]
    fn no_match_when_pattern_absent() {
        let source = br#"fn helper() { let x = 1; }"#;
        let eng = RustAstEngine;
        // `unsafe_block` doesn't exist in this source.
        let result = eng.evaluate(&pred("(unsafe_block)"), source).unwrap();
        assert!(!result.passed);
        assert_eq!(result.match_count, 0);
        assert!(result.detail.contains("did not match"));
    }

    #[test]
    fn invalid_query_returns_error() {
        let eng = RustAstEngine;
        // Unbalanced parens — invalid tree-sitter query.
        let result = eng.evaluate(&pred("(function_item"), b"fn x(){}");
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            RootingError::InvalidPredicate(_)
        ));
    }

    #[test]
    fn handles_malformed_source_gracefully() {
        // tree-sitter is error-tolerant and produces a partial tree for
        // malformed input; an (unsafe_block) query still yields zero matches.
        let eng = RustAstEngine;
        let result = eng
            .evaluate(
                &pred("(unsafe_block)"),
                b"this is not valid rust at all <<<",
            )
            .unwrap();
        assert!(!result.passed);
    }

    #[test]
    fn strength_is_high_for_tight_named_function_query() {
        // A query that pins a single named function via `#eq?` matches one
        // site in a multi-function source → small byte-coverage →
        // strength should stay near 1.0.
        let source = br#"
pub mod payment {
    pub fn charge() {}
    pub fn refund() {}
    pub fn validate_card(n: &str) -> bool { n.len() == 16 }
    pub fn tokenize(s: &str) -> String { s.into() }
}
"#;
        let eng = RustAstEngine;
        let q = r#"(function_item name: (identifier) @n (#eq? @n "validate_card"))"#;
        let result = eng.evaluate(&pred(q), source).unwrap();
        assert!(result.passed);
        assert!(
            result.strength > 0.85,
            "tight named-function query should score strong, got {}",
            result.strength
        );
    }

    #[test]
    fn strength_is_lower_for_broad_wildcard_query() {
        // A bare `(identifier)` matches every identifier in the source. In a
        // file with many identifiers, inverse-count strength should fall
        // clearly below the 0.60 threshold, catching the gaming pattern.
        let source = br#"
fn one(a: i32, b: i32) -> i32 { a + b }
fn two(c: i32, d: i32) -> i32 { c - d }
fn three(e: i32, f: i32) -> i32 { e * f }
fn four(g: i32, h: i32) -> i32 { g / h }
"#;
        let eng = RustAstEngine;
        let result = eng.evaluate(&pred("(identifier)"), source).unwrap();
        assert!(result.passed);
        assert!(
            result.strength < 0.6,
            "broad `(identifier)` query should be flagged weak, got {}",
            result.strength
        );
    }
}
