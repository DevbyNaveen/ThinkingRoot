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

use super::{PredicateEngine, PredicateEvaluation};
use crate::{Result, RootingError};

pub(super) struct RustAstEngine;

impl PredicateEngine for RustAstEngine {
    fn language(&self) -> PredicateLanguage {
        PredicateLanguage::RustAst
    }

    fn evaluate(&self, predicate: &Predicate, source_bytes: &[u8]) -> Result<PredicateEvaluation> {
        let ts_language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();

        let query = Query::new(&ts_language, &predicate.query).map_err(|e| {
            RootingError::InvalidPredicate(format!("tree-sitter-rust query: {e}"))
        })?;

        let mut parser = Parser::new();
        parser.set_language(&ts_language).map_err(|e| {
            RootingError::InvalidPredicate(format!("tree-sitter parser init: {e}"))
        })?;

        let tree = match parser.parse(source_bytes, None) {
            Some(t) => t,
            None => {
                return Ok(PredicateEvaluation {
                    passed: false,
                    match_count: 0,
                    detail: "tree-sitter-rust parse returned no tree".into(),
                });
            }
        };

        let mut cursor = QueryCursor::new();
        let mut matches_iter = cursor.matches(&query, tree.root_node(), source_bytes);
        let mut match_count = 0usize;
        while matches_iter.next().is_some() {
            match_count += 1;
        }

        let passed = match_count > 0;
        let detail = if passed {
            format!("AST query captured {match_count} site(s)")
        } else {
            format!("AST query '{}' did not match any nodes", predicate.query)
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
            .evaluate(&pred("(unsafe_block)"), b"this is not valid rust at all <<<")
            .unwrap();
        assert!(!result.passed);
    }
}
