use std::collections::HashMap;

use thinkingroot_core::types::{Claim, SourceId};

/// Deduplicate claims within the same source.
///
/// When the same fact appears in multiple chunks of the same file,
/// the LLM extracts it multiple times. This inflates the graph and
/// distorts coverage scores.
///
/// Uses lexical similarity (Jaccard on word sets) as a lightweight
/// dedup signal. Claims with > 85% word overlap from the same source
/// are merged (higher-confidence version kept).
pub fn dedup_claims(claims: &mut Vec<Claim>) {
    // Group by source.
    let mut by_source: HashMap<SourceId, Vec<usize>> = HashMap::new();
    for (idx, claim) in claims.iter().enumerate() {
        by_source.entry(claim.source).or_default().push(idx);
    }

    let mut to_remove: Vec<usize> = Vec::new();

    for indices in by_source.values() {
        if indices.len() < 2 {
            continue;
        }

        for i in 0..indices.len() {
            if to_remove.contains(&indices[i]) {
                continue;
            }
            for j in (i + 1)..indices.len() {
                if to_remove.contains(&indices[j]) {
                    continue;
                }

                let a = &claims[indices[i]];
                let b = &claims[indices[j]];

                let similarity = jaccard_words(&a.statement, &b.statement);
                if similarity > 0.85 {
                    // Keep the one with higher confidence (or grounding score).
                    let keep_j = b.confidence.value() > a.confidence.value();
                    if keep_j {
                        to_remove.push(indices[i]);
                        break; // 'i' is removed, no point comparing further
                    } else {
                        to_remove.push(indices[j]);
                    }
                }
            }
        }
    }

    // Sort descending so removal doesn't shift indices.
    to_remove.sort_unstable();
    to_remove.dedup();
    for idx in to_remove.into_iter().rev() {
        claims.remove(idx);
    }
}

/// Jaccard similarity on word sets (case-insensitive).
fn jaccard_words(a: &str, b: &str) -> f64 {
    let words_a = word_set(a);
    let words_b = word_set(b);

    if words_a.is_empty() && words_b.is_empty() {
        return 1.0;
    }

    let intersection = words_a.intersection(&words_b).count();
    let union = words_a.union(&words_b).count();

    if union == 0 {
        return 0.0;
    }

    intersection as f64 / union as f64
}

fn word_set(text: &str) -> std::collections::HashSet<String> {
    use unicode_segmentation::UnicodeSegmentation;
    text.unicode_words()
        .map(|w| w.to_lowercase())
        .filter(|w| w.len() >= 2 && !is_stop_word(w))
        .collect()
}

/// Common English stop words to exclude from Jaccard comparison.
/// Filtering these out makes near-duplicates that differ only in
/// articles/determiners register as identical.
fn is_stop_word(word: &str) -> bool {
    matches!(
        word,
        "the" | "an" | "and" | "or" | "of" | "to" | "in" | "is" | "it" | "its"
            | "as" | "at" | "by" | "be" | "on" | "up" | "if" | "no" | "so"
            | "for" | "was" | "are" | "has" | "had" | "not" | "but" | "can"
            | "with" | "from" | "that" | "this" | "they" | "them" | "their"
            | "will" | "have" | "been" | "than" | "into" | "also" | "when"
            | "what" | "then" | "over" | "such"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use thinkingroot_core::types::{ClaimType, WorkspaceId};

    fn make_claim(statement: &str, source: SourceId, confidence: f64) -> Claim {
        Claim::new(statement, ClaimType::Fact, source, WorkspaceId::new())
            .with_confidence(confidence)
    }

    #[test]
    fn identical_claims_deduped() {
        let src = SourceId::new();
        let mut claims = vec![
            make_claim("PostgreSQL stores user data", src, 0.8),
            make_claim("PostgreSQL stores user data", src, 0.9),
        ];
        dedup_claims(&mut claims);
        assert_eq!(claims.len(), 1);
        // Higher confidence kept.
        assert!((claims[0].confidence.value() - 0.9).abs() < 0.01);
    }

    #[test]
    fn different_claims_not_deduped() {
        let src = SourceId::new();
        let mut claims = vec![
            make_claim("PostgreSQL stores user data", src, 0.8),
            make_claim("Redis caches session tokens", src, 0.9),
        ];
        dedup_claims(&mut claims);
        assert_eq!(claims.len(), 2);
    }

    #[test]
    fn cross_source_not_deduped() {
        let src_a = SourceId::new();
        let src_b = SourceId::new();
        let mut claims = vec![
            make_claim("PostgreSQL stores user data", src_a, 0.8),
            make_claim("PostgreSQL stores user data", src_b, 0.9),
        ];
        dedup_claims(&mut claims);
        // Same statement but different sources — keep both.
        assert_eq!(claims.len(), 2);
    }

    #[test]
    fn near_duplicate_deduped() {
        let src = SourceId::new();
        let mut claims = vec![
            make_claim("The PostgreSQL database stores user data", src, 0.8),
            make_claim("PostgreSQL database stores user data", src, 0.7),
        ];
        dedup_claims(&mut claims);
        assert_eq!(claims.len(), 1);
    }
}
