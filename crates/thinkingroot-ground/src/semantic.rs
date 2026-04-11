use thinkingroot_graph::vector::VectorStore;

/// Judge 3: Semantic similarity via embedding cosine distance.
///
/// Uses the existing fastembed model (AllMiniLM-L6-V2) already loaded for
/// vector search. Computes cosine similarity between claim and source text.
///
/// This catches claims that reuse real words but change the meaning:
/// - Source: "migrated FROM MySQL to PostgreSQL"
/// - Claim:  "The system uses MySQL" → low similarity with actual context
///
/// Feature-gated behind `vector` — disabled on low-end builds.
pub struct SemanticJudge;

impl SemanticJudge {
    /// Score semantic similarity between claim and source text.
    ///
    /// Returns a score in [0.0, 1.0]:
    /// - > 0.7: claim is semantically close to source content
    /// - 0.4-0.7: partially related
    /// - < 0.4: likely off-topic / hallucinated
    pub fn score(claim: &str, source_text: &str, vector_store: &mut VectorStore) -> f64 {
        // Embed both texts using the existing model.
        let texts = vec![claim, source_text];
        match vector_store.embed_texts(&texts) {
            Ok(embeddings) if embeddings.len() == 2 => {
                cosine_similarity(&embeddings[0], &embeddings[1])
            }
            Ok(_) => {
                tracing::warn!("semantic judge: unexpected embedding count");
                0.5 // neutral fallback
            }
            Err(e) => {
                tracing::warn!("semantic judge: embedding failed: {e}");
                0.5 // neutral fallback — don't reject on infra failure
            }
        }
    }
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    (dot / (norm_a * norm_b)) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_similarity_identical() {
        let a = vec![1.0, 2.0, 3.0];
        let sim = cosine_similarity(&a, &a);
        assert!((sim - 1.0).abs() < 0.001);
    }

    #[test]
    fn cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 0.001);
    }

    #[test]
    fn cosine_similarity_opposite() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![-1.0, -2.0, -3.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - (-1.0)).abs() < 0.001);
    }
}
