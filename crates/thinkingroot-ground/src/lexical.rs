use unicode_segmentation::UnicodeSegmentation;

/// Judge 1: Lexical anchoring.
///
/// Checks what fraction of meaningful words in the claim appear in the source text.
/// Fast (< 1ms per claim), zero dependencies beyond unicode-segmentation.
pub struct LexicalJudge;

/// Words to ignore when computing overlap (too common to be meaningful).
const STOP_WORDS: &[&str] = &[
    "a", "an", "the", "is", "are", "was", "were", "be", "been", "being",
    "have", "has", "had", "do", "does", "did", "will", "would", "could",
    "should", "may", "might", "shall", "can", "need", "must",
    "and", "or", "but", "if", "then", "else", "when", "where", "how",
    "what", "which", "who", "whom", "this", "that", "these", "those",
    "it", "its", "of", "in", "to", "for", "with", "on", "at", "by",
    "from", "as", "into", "about", "not", "no", "so", "up", "out",
    "than", "too", "very", "just", "also", "all", "each", "every",
    "any", "some", "such", "only", "own", "same", "other", "new",
    "used", "using", "uses", "use",
];

impl LexicalJudge {
    /// Score how well a claim is lexically anchored in the source text.
    ///
    /// Returns a score in [0.0, 1.0]:
    /// - 1.0 = every meaningful word in the claim appears in the source
    /// - 0.0 = no meaningful words match
    pub fn score(claim: &str, source_text: &str) -> f64 {
        let source_words = Self::extract_words(source_text);
        let claim_words = Self::extract_words(claim);

        if claim_words.is_empty() {
            return 0.0;
        }

        let matches = claim_words
            .iter()
            .filter(|w| source_words.contains(w))
            .count();

        matches as f64 / claim_words.len() as f64
    }

    /// Extract meaningful lowercase words, filtering stop words and short tokens.
    fn extract_words(text: &str) -> Vec<String> {
        text.unicode_words()
            .map(|w| w.to_lowercase())
            .filter(|w| w.len() >= 2)
            .filter(|w| !STOP_WORDS.contains(&w.as_str()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perfect_overlap() {
        let source = "PostgreSQL stores user data in tables";
        let claim = "PostgreSQL stores user data";
        let score = LexicalJudge::score(claim, source);
        assert!(score > 0.99, "expected ~1.0, got {score}");
    }

    #[test]
    fn zero_overlap() {
        let source = "PostgreSQL stores user data";
        let claim = "Redis caches session tokens";
        let score = LexicalJudge::score(claim, source);
        assert!(score < 0.01, "expected ~0.0, got {score}");
    }

    #[test]
    fn partial_overlap() {
        let source = "PostgreSQL stores user data and handles transactions";
        let claim = "PostgreSQL handles authentication and sessions";
        let score = LexicalJudge::score(claim, source);
        // "postgresql" and "handles" match; "authentication" and "sessions" don't
        assert!(score > 0.2 && score < 0.8, "expected partial, got {score}");
    }

    #[test]
    fn empty_claim_returns_zero() {
        let score = LexicalJudge::score("", "some source text");
        assert_eq!(score, 0.0);
    }

    #[test]
    fn stop_words_are_ignored() {
        let source = "The system";
        let claim = "The system is very good and also fast";
        // After stop word removal, claim has: "system", "good", "fast"
        // Source has: "system"
        // Score = 1/3
        let score = LexicalJudge::score(claim, source);
        assert!(score > 0.3 && score < 0.4, "expected ~0.33, got {score}");
    }

    #[test]
    fn case_insensitive() {
        let source = "PostgreSQL is a database";
        let claim = "POSTGRESQL database";
        let score = LexicalJudge::score(claim, source);
        assert!(score > 0.99, "expected ~1.0, got {score}");
    }
}
