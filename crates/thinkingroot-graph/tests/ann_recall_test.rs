//! B4 — ANN recall proof. The HNSW fast path must return essentially the same
//! top-k as exact brute-force (recall@10 ≥ 0.9), or it would silently lose recall
//! (the senescence the aging research warns about). Deterministic; no model.

#![cfg(feature = "vector")]

use tempfile::tempdir;
use thinkingroot_graph::vector::VectorStore;

/// Tiny deterministic PRNG (no `rand` dep, reproducible across runs/machines).
fn lcg(seed: &mut u64) -> f32 {
    *seed = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    ((*seed >> 33) as f32 / (1u64 << 31) as f32) - 1.0 // ~[-1, 1]
}
fn rand_vec(seed: &mut u64, dim: usize) -> Vec<f32> {
    (0..dim).map(|_| lcg(seed)).collect()
}

#[tokio::test]
async fn ann_recall_matches_bruteforce_at_scale() {
    let dir = tempdir().unwrap();
    let mut vs = VectorStore::init(dir.path()).await.unwrap();

    let dim = 768;
    let n = 3000; // > ANN_THRESHOLD (1024) → search_by_vector_fast uses HNSW
    let mut seed = 0x9E3779B97F4A7C15u64;
    let items: Vec<(String, Vec<f32>, String)> = (0..n)
        .map(|i| (format!("id{i}"), rand_vec(&mut seed, dim), format!("claim|id{i}")))
        .collect();
    vs.upsert_raw_batch(items).unwrap();

    let top_k = 10;
    let queries = 25;
    let mut total_recall = 0.0f64;
    for _ in 0..queries {
        let q = rand_vec(&mut seed, dim);
        let brute: std::collections::BTreeSet<String> = vs
            .search_by_vector(&q, top_k)
            .into_iter()
            .map(|(id, _, _)| id)
            .collect();
        let ann: std::collections::BTreeSet<String> = vs
            .search_by_vector_fast(&q, top_k)
            .into_iter()
            .map(|(id, _, _)| id)
            .collect();
        let hit = brute.intersection(&ann).count();
        total_recall += hit as f64 / top_k as f64;
    }
    let recall = total_recall / queries as f64;
    eprintln!("ANN recall@{top_k} over {n} vectors = {recall:.3}");
    assert!(
        recall >= 0.9,
        "ANN recall@{top_k} = {recall:.3} (< 0.9) — would silently drop recall"
    );
}

#[tokio::test]
async fn small_index_is_exact() {
    // Below the threshold, search_by_vector_fast == exact brute-force.
    let dir = tempdir().unwrap();
    let mut vs = VectorStore::init(dir.path()).await.unwrap();
    let dim = 768;
    let mut seed = 42u64;
    let items: Vec<(String, Vec<f32>, String)> = (0..50)
        .map(|i| (format!("id{i}"), rand_vec(&mut seed, dim), format!("claim|id{i}")))
        .collect();
    vs.upsert_raw_batch(items).unwrap();
    let q = rand_vec(&mut seed, dim);
    let brute: Vec<String> = vs.search_by_vector(&q, 10).into_iter().map(|(id, _, _)| id).collect();
    let fast: Vec<String> = vs.search_by_vector_fast(&q, 10).into_iter().map(|(id, _, _)| id).collect();
    assert_eq!(brute, fast, "below threshold must be exact (identical order)");
}
