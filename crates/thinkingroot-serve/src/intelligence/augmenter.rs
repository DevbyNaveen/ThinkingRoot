// crates/thinkingroot-serve/src/intelligence/augmenter.rs
//
// Source augmentation — loads raw session transcripts for the synthesis prompt.
//
// Two modes (proven in LongMemEval Round 5-6):
//
//   load_raw_sources()          — full transcript loading.
//     Used for: single-session-* (all 3 categories), temporal-reasoning,
//     knowledge-update, and multi-session with ≤3 answer sessions.
//     Rationale: full context is ground truth — claims miss ~15% of facts.
//
//   extract_relevant_snippets() — keyword-filtered paragraph extraction.
//     Used for: multi-session with >3 answer sessions.
//     Rationale: 5 sessions × 14KB = 70KB overwhelms LLM for counting tasks.
//     Keyword filtering keeps 3-5KB of signal, eliminates noise.

use std::collections::HashSet;
use std::path::Path;

// ---------------------------------------------------------------------------
// Keyword helpers
// ---------------------------------------------------------------------------

/// Extract content-bearing tokens from a question (non-stop-words, len ≥ 3).
pub fn question_keywords(question: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "how", "many", "what", "is", "the", "did", "do", "have", "has", "had",
        "a", "an", "in", "at", "on", "was", "were", "to", "from", "and", "or",
        "of", "for", "with", "that", "this", "it", "be", "been", "am", "are",
        "who", "when", "where", "which", "why", "will", "can", "could", "would",
        "should", "me", "we", "you", "your", "our", "their", "its", "his", "her",
        "total", "currently", "recently", "usually", "ever", "always", "never",
        "much", "more", "some", "any", "all", "both", "each", "few", "most",
        "own", "same", "different", "just", "now", "then", "here", "there",
        "during", "before", "after", "since", "while", "about", "between",
    ];
    let stop: HashSet<&str> = STOP.iter().copied().collect();

    question
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3 && !stop.contains(*w))
        .map(|w| w.to_string())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect()
}

// ---------------------------------------------------------------------------
// Full transcript loader
// ---------------------------------------------------------------------------

/// Load full session transcript files for the given answer session IDs.
///
/// Scans `sessions_dir` for files whose names contain each session ID, reads
/// them in sorted order, and concatenates up to `char_budget` characters.
/// Each file is prefixed with a `━━━ SESSION: <filename> ━━━` header.
pub fn load_raw_sources(
    sessions_dir: &Path,
    answer_sids: &[String],
    char_budget: usize,
) -> String {
    if !sessions_dir.is_dir() {
        return String::new();
    }

    let all_files: Vec<String> = match std::fs::read_dir(sessions_dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect(),
        Err(_) => return String::new(),
    };

    let mut combined = String::new();
    let mut total_chars = 0usize;

    'outer: for asid in answer_sids {
        let mut matches: Vec<&String> = all_files
            .iter()
            .filter(|f| f.contains(asid.as_str()))
            .collect();
        matches.sort();

        for fname in matches {
            let remaining = char_budget.saturating_sub(total_chars);
            if remaining == 0 {
                break 'outer;
            }
            let path = sessions_dir.join(fname);
            if let Ok(content) = std::fs::read_to_string(&path) {
                let to_add = if content.len() > remaining {
                    &content[..remaining]
                } else {
                    &content
                };
                combined.push_str("━━━ SESSION: ");
                combined.push_str(fname);
                combined.push_str(" ━━━\n");
                combined.push_str(to_add);
                combined.push_str("\n\n");
                total_chars += to_add.len() + fname.len() + 20;
            }
        }
    }

    combined
}

// ---------------------------------------------------------------------------
// Keyword-snippet extractor
// ---------------------------------------------------------------------------

/// Extract keyword-relevant paragraphs from session files.
///
/// For large multi-session questions (>3 answer sessions) loading full
/// transcripts produces too much noise. This function:
///   1. Splits each file into paragraphs separated by blank lines.
///   2. Scores each paragraph by number of question keyword hits.
///   3. Returns the top-scoring paragraphs up to `char_budget`.
pub fn extract_relevant_snippets(
    sessions_dir: &Path,
    answer_sids: &[String],
    question: &str,
    char_budget: usize,
) -> String {
    if !sessions_dir.is_dir() || char_budget == 0 {
        return String::new();
    }

    let keywords = question_keywords(question);
    if keywords.is_empty() {
        return String::new();
    }

    let all_files: Vec<String> = match std::fs::read_dir(sessions_dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect(),
        Err(_) => return String::new(),
    };

    let mut result = String::new();
    let mut total_chars = 0usize;

    'outer: for asid in answer_sids {
        let mut matching: Vec<&String> = all_files
            .iter()
            .filter(|f| f.contains(asid.as_str()))
            .collect();
        matching.sort();

        for fname in matching {
            if total_chars >= char_budget {
                break 'outer;
            }
            let path = sessions_dir.join(fname);
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            // Score paragraphs by keyword hit count, keep top-scoring ones
            let mut scored: Vec<(usize, &str)> = content
                .split("\n\n")
                .filter(|p| p.trim().len() > 20)
                .map(|p| {
                    let p_low = p.to_lowercase();
                    let score = keywords
                        .iter()
                        .filter(|kw| p_low.contains(kw.as_str()))
                        .count();
                    (score, p)
                })
                .filter(|(s, _)| *s > 0)
                .collect();

            if scored.is_empty() {
                continue;
            }
            scored.sort_by(|a, b| b.0.cmp(&a.0));

            let hdr = format!("━━━ SNIPPET: {} ━━━\n", fname);
            result.push_str(&hdr);
            total_chars += hdr.len();

            for (_, para) in scored.iter().take(10) {
                let remaining = char_budget.saturating_sub(total_chars);
                if remaining < 30 {
                    break;
                }
                let to_add = if para.len() > remaining {
                    &para[..remaining]
                } else {
                    para
                };
                result.push_str(to_add);
                result.push_str("\n\n");
                total_chars += to_add.len() + 2;
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn question_keywords_extracts_content_words() {
        let kws = question_keywords("How many books did I buy last year?");
        assert!(kws.contains(&"books".to_string()));
        assert!(kws.contains(&"buy".to_string()));
        // stop words should be absent
        assert!(!kws.contains(&"how".to_string()));
        assert!(!kws.contains(&"many".to_string()));
        assert!(!kws.contains(&"did".to_string()));
    }

    #[test]
    fn question_keywords_deduplicates() {
        let kws = question_keywords("coffee coffee latte");
        let coffee_count = kws.iter().filter(|k| k.as_str() == "coffee").count();
        assert_eq!(coffee_count, 1);
    }

    #[test]
    fn load_raw_sources_returns_empty_on_missing_dir() {
        let result = load_raw_sources(Path::new("/nonexistent/dir"), &["sid1".to_string()], 10000);
        assert!(result.is_empty());
    }

    #[test]
    fn extract_snippets_returns_empty_on_missing_dir() {
        let result = extract_relevant_snippets(
            Path::new("/nonexistent/dir"),
            &["sid1".to_string()],
            "what books did I read?",
            10000,
        );
        assert!(result.is_empty());
    }
}
