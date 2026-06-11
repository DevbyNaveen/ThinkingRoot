//! §11 A7-SEC ⑤ — write-time anomaly detection (AgentPoison defense).
//!
//! Memory-poisoning attacks (AgentPoison and kin) inject a set of claims whose
//! embeddings deliberately CLUSTER tightly in vector space — a compact "trigger
//! cluster" engineered so the poison reliably surfaces for the attacker's
//! trigger queries. Ordinary, heterogeneous writes don't look like that: real
//! claims about real things spread out.
//!
//! So at index time we look at the batch of embeddings being written and flag an
//! anomalously tight, mutually-near cluster — a statistical signature genuine
//! writes rarely produce. This is the index-time half of the A7-SEC stack
//! (trust-aware retrieval ② is the recall-time half).
//!
//! Pure (operates on the embedding vectors) so it is fully unit-testable with no
//! model. The caller decides the response (warn / lower trust / quarantine);
//! this module only DETECTS, and never blocks a write by itself.

/// What the detector found for one write batch.
#[derive(Debug, Clone, PartialEq)]
pub struct AnomalyReport {
    /// True when a tight cluster of size ≥ `min_cluster` was found.
    pub anomalous: bool,
    /// Indices (into the batch) of the members of the largest tight cluster.
    pub cluster: Vec<usize>,
    /// Mean pairwise cosine across the whole batch (diagnostic).
    pub mean_pairwise_sim: f32,
}

impl Default for AnomalyReport {
    fn default() -> Self {
        Self { anomalous: false, cluster: Vec::new(), mean_pairwise_sim: 0.0 }
    }
}

/// Cosine similarity of two equal-length f32 vectors. Degenerate inputs
/// (length mismatch or a zero vector) score 0 — never NaN.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na <= 0.0 || nb <= 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Detect an anomalously tight embedding cluster in a write batch.
///
/// `sim_threshold` — pairs at/above this cosine are "near-duplicate" (e.g.
/// 0.97). `min_cluster` — how many mutually-near vectors constitute a
/// poison-style cluster (e.g. 3; one or two paraphrases are normal). A batch is
/// flagged when some vector has ≥ `min_cluster − 1` near neighbours; the
/// returned `cluster` is every index participating at that density.
pub fn detect_write_anomaly(
    embeddings: &[Vec<f32>],
    sim_threshold: f32,
    min_cluster: usize,
) -> AnomalyReport {
    let n = embeddings.len();
    if n < min_cluster || min_cluster < 2 {
        return AnomalyReport::default();
    }

    // Per-vector count of near neighbours + accumulate the global mean.
    let mut near: Vec<usize> = vec![0; n];
    let mut sum_sim = 0.0f64;
    let mut pairs = 0u64;
    for i in 0..n {
        for j in (i + 1)..n {
            let s = cosine(&embeddings[i], &embeddings[j]);
            sum_sim += s as f64;
            pairs += 1;
            if s >= sim_threshold {
                near[i] += 1;
                near[j] += 1;
            }
        }
    }
    let mean_pairwise_sim = if pairs > 0 { (sum_sim / pairs as f64) as f32 } else { 0.0 };

    // A member of a tight cluster of size k has (k−1) near neighbours. So
    // anyone with ≥ (min_cluster−1) near neighbours is in a poison-grade cluster.
    let need = min_cluster - 1;
    let cluster: Vec<usize> = (0..n).filter(|&i| near[i] >= need).collect();

    AnomalyReport { anomalous: !cluster.is_empty(), cluster, mean_pairwise_sim }
}

/// §11 A7-SEC ③ — use-time consensus (A-MemGuard). Among a recalled cohort,
/// find the claims that DON'T corroborate the consensus. When a MAJORITY of the
/// cohort is mutually similar (a consensus topic exists), a claim similar to
/// (almost) none of them is an outlier — the signature of context-activated
/// poison that rode a trigger query into the result set. The caller demotes
/// flagged-AND-low-trust claims (legit rare-but-true facts from trusted sources
/// are kept).
///
/// Conservative by construction: if there is NO majority consensus (a genuinely
/// diverse recall), it flags NOTHING — diversity is normal, only an isolated
/// dissenter amid a clear majority is suspect. Operates on PRE-FETCHED stored
/// vectors (no embedding) so it is latency-safe for the read path.
pub fn consensus_outliers(embeddings: &[Vec<f32>], sim_threshold: f32) -> Vec<usize> {
    let n = embeddings.len();
    if n < 4 {
        return Vec::new(); // too small for a meaningful consensus
    }
    let mut near: Vec<usize> = vec![0; n];
    for i in 0..n {
        for j in (i + 1)..n {
            if cosine(&embeddings[i], &embeddings[j]) >= sim_threshold {
                near[i] += 1;
                near[j] += 1;
            }
        }
    }
    // Largest mutually-similar group ≈ max near-count + 1. Require it to be a
    // strict majority before trusting it as "the consensus".
    let consensus = near.iter().copied().max().unwrap_or(0) + 1;
    if consensus * 2 <= n {
        return Vec::new(); // no majority consensus → diverse recall, flag nothing
    }
    // Outliers: corroborated by nobody while a majority consensus exists.
    (0..n).filter(|&i| near[i] == 0).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Build a vector that points mostly along axis `k` with a little noise, so
    // distinct k's are well-separated and same-k's are near-identical.
    fn axis(dim: usize, k: usize, jitter: f32) -> Vec<f32> {
        let mut v = vec![0.0f32; dim];
        v[k % dim] = 1.0;
        if dim > 1 {
            v[(k + 1) % dim] = jitter; // tiny tilt
        }
        v
    }

    #[test]
    fn heterogeneous_batch_is_not_anomalous() {
        // 5 claims, each pointing a different way → spread out.
        let emb: Vec<Vec<f32>> = (0..5).map(|k| axis(8, k, 0.0)).collect();
        let r = detect_write_anomaly(&emb, 0.97, 3);
        assert!(!r.anomalous, "distinct claims must not be flagged: {r:?}");
        assert!(r.cluster.is_empty());
    }

    #[test]
    fn tight_poison_cluster_is_flagged() {
        // 4 near-identical (the poison trigger cluster) + 3 distinct.
        let mut emb: Vec<Vec<f32>> = (0..4).map(|i| axis(8, 0, 0.001 * i as f32)).collect();
        emb.push(axis(8, 2, 0.0));
        emb.push(axis(8, 4, 0.0));
        emb.push(axis(8, 6, 0.0));
        let r = detect_write_anomaly(&emb, 0.97, 3);
        assert!(r.anomalous, "a tight 4-cluster must be flagged: {r:?}");
        // The 4 near-identical indices (0..4) are the cluster.
        for i in 0..4 {
            assert!(r.cluster.contains(&i), "index {i} should be in the cluster: {r:?}");
        }
        assert!(!r.cluster.contains(&4), "a distinct claim must not be in the cluster");
    }

    #[test]
    fn two_near_duplicates_below_min_cluster_are_ok() {
        // Normal: a claim + one paraphrase. min_cluster=3 → not poison-grade.
        let emb = vec![axis(8, 0, 0.0), axis(8, 0, 0.001), axis(8, 3, 0.0)];
        let r = detect_write_anomaly(&emb, 0.97, 3);
        assert!(!r.anomalous, "a single paraphrase pair is normal: {r:?}");
    }

    #[test]
    fn degenerate_inputs_are_safe() {
        assert!(!detect_write_anomaly(&[], 0.97, 3).anomalous);
        assert!(!detect_write_anomaly(&[vec![1.0, 0.0]], 0.97, 3).anomalous);
        // zero vectors → cosine 0, never NaN, never flagged
        let r = detect_write_anomaly(&[vec![0.0; 4], vec![0.0; 4], vec![0.0; 4]], 0.97, 3);
        assert!(!r.anomalous);
        assert!(r.mean_pairwise_sim.is_finite());
    }

    // ── ③ use-time consensus ──────────────────────────────────────────

    #[test]
    fn consensus_flags_the_lone_dissenter_amid_a_majority() {
        // 4 claims about the same topic (the consensus) + 1 unrelated poison.
        let mut emb: Vec<Vec<f32>> = (0..4).map(|i| axis(8, 0, 0.001 * i as f32)).collect();
        emb.push(axis(8, 5, 0.0)); // isolated outlier
        let out = consensus_outliers(&emb, 0.9);
        assert_eq!(out, vec![4], "the lone unrelated claim is the consensus outlier");
    }

    #[test]
    fn consensus_flags_nothing_when_recall_is_diverse() {
        // No majority consensus — 5 distinct claims. Diversity is normal.
        let emb: Vec<Vec<f32>> = (0..5).map(|k| axis(8, k, 0.0)).collect();
        assert!(consensus_outliers(&emb, 0.9).is_empty());
    }

    #[test]
    fn consensus_flags_nothing_when_all_corroborate() {
        let emb: Vec<Vec<f32>> = (0..5).map(|i| axis(8, 0, 0.001 * i as f32)).collect();
        assert!(consensus_outliers(&emb, 0.9).is_empty());
    }

    #[test]
    fn consensus_needs_a_cohort() {
        let emb = vec![axis(8, 0, 0.0), axis(8, 0, 0.001), axis(8, 5, 0.0)];
        assert!(consensus_outliers(&emb, 0.9).is_empty(), "n<4 → no consensus call");
    }
}
