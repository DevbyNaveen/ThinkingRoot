//! Predicate evaluation engines.
//!
//! Each engine evaluates a [`Predicate`] against source bytes deterministically.
//! No LLM is involved in evaluation — the LLM only generates the predicate
//! at extraction time.

use thinkingroot_core::types::{Predicate, PredicateLanguage};

use crate::Result;

mod jsonpath;
mod regex;
mod rust_ast;

/// Outcome of evaluating a predicate against source bytes.
#[derive(Debug, Clone)]
pub struct PredicateEvaluation {
    /// Whether the predicate matched at least once.
    pub passed: bool,
    /// Number of distinct matches found. `0` when `passed = false`.
    pub match_count: usize,
    /// Short description for the verdict's `detail` field.
    pub detail: String,
}

/// Evaluator for a single predicate language.
pub trait PredicateEngine: Send + Sync {
    /// Which language this engine handles.
    fn language(&self) -> PredicateLanguage;

    /// Evaluate the predicate against the given source bytes.
    fn evaluate(&self, predicate: &Predicate, source_bytes: &[u8]) -> Result<PredicateEvaluation>;
}

/// Return the right engine for a predicate's language. Week 1 returns stubs;
/// Weeks 3–5 replace these with real implementations.
pub fn engine_for(language: PredicateLanguage) -> Box<dyn PredicateEngine> {
    match language {
        PredicateLanguage::Regex => Box::new(regex::RegexEngine),
        PredicateLanguage::RustAst => Box::new(rust_ast::RustAstEngine),
        PredicateLanguage::JsonPath => Box::new(jsonpath::JsonPathEngine),
    }
}
