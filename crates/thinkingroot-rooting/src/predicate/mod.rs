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
    /// Evidential strength of the match in `[0.0, 1.0]`. Measures how
    /// specific the predicate is relative to the source: a pattern that
    /// covers most of the source bytes (e.g. `.` or `\w+`) scores near
    /// `0.0`; a pattern with tight, localised matches scores near `1.0`.
    /// `0.0` when `passed = false`.
    pub strength: f32,
    /// Short description for the verdict's `detail` field.
    pub detail: String,
}

/// Compute coverage-based strength: `1 - clamp(matched_bytes / source_bytes, 0, 1)`.
/// Returns `0.0` when `source_bytes == 0` (no source → no evidence).
pub(crate) fn coverage_strength(matched_bytes: usize, source_bytes: usize) -> f32 {
    if source_bytes == 0 {
        return 0.0;
    }
    let ratio = (matched_bytes as f64 / source_bytes as f64).clamp(0.0, 1.0);
    (1.0 - ratio) as f32
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
