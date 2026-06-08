//! E5 — int8 vector-quantization primitives.
//!
//! The recall store keeps 768-dim f32 embeddings (≈3 KB/vector). Symmetric
//! int8 quantization shrinks that ~4× (768 bytes + one f32 scale) while the
//! **coarse→rescore** pattern preserves ranking: a fast int8 pass over-fetches
//! candidates, then an exact dequantized-cosine pass re-ranks the top
//! `k·4 (≥64)` so the final order matches f32 within quantization error.
//!
//! Symmetric per-vector scheme: `scale = max|v| / 127`, `q[i] = round(v[i] /
//! scale)` clamped to `[-127, 127]`; `dequant[i] = q[i] · scale`. Per-element
//! error ≤ `scale/2`, so cosine error is tiny (validated < 1e-3 in tests).
//!
//! Pure math — no ONNX, no I/O — so it is fully unit-testable locally. The
//! end-to-end recall check (LongMemEval-500 ≥ 91.2%, paraphrase 4/4) runs via
//! `scripts/eval_gate.sh` on the Azure VM, where the embedder is staged; this
//! module's tests guarantee the *ranking-preservation* property the gate then
//! confirms on real data.

/// A symmetrically-quantized vector: int8 codes + the scale that
/// reconstructs the original (`v ≈ q · scale`).
#[derive(Debug, Clone, PartialEq)]
pub struct QuantizedVec {
    pub codes: Vec<i8>,
    pub scale: f32,
    /// L2 norm of the ORIGINAL f32 vector — cached so cosine never has to
    /// reconstruct it (and so a zero vector is handled honestly).
    pub norm: f32,
}

/// Symmetric int8 quantization of an f32 vector. A zero vector maps to all-zero
/// codes with `scale = 0` (cosine against it returns 0, never NaN).
pub fn quantize_i8(v: &[f32]) -> QuantizedVec {
    let max_abs = v.iter().fold(0.0_f32, |m, &x| m.max(x.abs()));
    let scale = if max_abs == 0.0 { 0.0 } else { max_abs / 127.0 };
    let codes: Vec<i8> = if scale == 0.0 {
        vec![0; v.len()]
    } else {
        v.iter()
            .map(|&x| {
                let q = (x / scale).round();
                q.clamp(-127.0, 127.0) as i8
            })
            .collect()
    };
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    QuantizedVec { codes, scale, norm }
}

/// Reconstruct the approximate f32 vector from its quantized form.
pub fn dequantize(q: &QuantizedVec) -> Vec<f32> {
    q.codes.iter().map(|&c| c as f32 * q.scale).collect()
}

/// Cosine similarity between an f32 query and a quantized stored vector,
/// computed exactly over the dequantized codes. Uses the cached original norm
/// for the stored side (more faithful than the dequantized norm). Returns 0
/// when either side is degenerate (honest, never NaN).
pub fn cosine_query_to_i8(query: &[f32], stored: &QuantizedVec) -> f32 {
    if stored.scale == 0.0 || stored.norm == 0.0 || query.len() != stored.codes.len() {
        return 0.0;
    }
    let mut dot = 0.0_f32;
    let mut qnorm = 0.0_f32;
    for (i, &qv) in query.iter().enumerate() {
        let s = stored.codes[i] as f32 * stored.scale;
        dot += qv * s;
        qnorm += qv * qv;
    }
    let qnorm = qnorm.sqrt();
    if qnorm == 0.0 {
        return 0.0;
    }
    dot / (qnorm * stored.norm)
}

/// Cosine between two quantized vectors via the integer dot product — the fast
/// coarse-pass scorer. `dot(a,b) · scale_a · scale_b / (‖a‖·‖b‖)`.
pub fn cosine_i8(a: &QuantizedVec, b: &QuantizedVec) -> f32 {
    if a.scale == 0.0 || b.scale == 0.0 || a.norm == 0.0 || b.norm == 0.0 {
        return 0.0;
    }
    if a.codes.len() != b.codes.len() {
        return 0.0;
    }
    let mut idot: i64 = 0;
    for i in 0..a.codes.len() {
        idot += a.codes[i] as i64 * b.codes[i] as i64;
    }
    (idot as f32 * a.scale * b.scale) / (a.norm * b.norm)
}

/// The coarse→rescore search over a quantized index. `index` is `(id, qvec)`.
/// Phase 1: score every candidate with the fast `cosine_query_to_i8`. Phase 2:
/// take the top `max(top_k·4, 64)` and re-sort by the same (already-exact)
/// dequantized cosine — structured so a future cheaper coarse scorer can slot
/// into phase 1 without changing the contract. Returns the top_k `(id, score)`
/// in descending score, deterministic on ties (by id).
pub fn search_rescore(
    query: &[f32],
    index: &[(String, QuantizedVec)],
    top_k: usize,
) -> Vec<(String, f32)> {
    if top_k == 0 || index.is_empty() {
        return Vec::new();
    }
    let overfetch = (top_k * 4).max(64);
    // Phase 1 — coarse score over all candidates.
    let mut coarse: Vec<(usize, f32)> = index
        .iter()
        .enumerate()
        .map(|(i, (_, q))| (i, cosine_query_to_i8(query, q)))
        .collect();
    coarse.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| index[a.0].0.cmp(&index[b.0].0))
    });
    coarse.truncate(overfetch);
    // Phase 2 — exact dequantized-cosine rescore of the candidate set.
    let mut scored: Vec<(String, f32)> = coarse
        .iter()
        .map(|&(i, _)| {
            let (id, q) = &index[i];
            (id.clone(), cosine_query_to_i8(query, q))
        })
        .collect();
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    scored.truncate(top_k);
    scored
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cosine_f32(a: &[f32], b: &[f32]) -> f32 {
        let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
        let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        if na == 0.0 || nb == 0.0 {
            return 0.0;
        }
        dot / (na * nb)
    }

    /// Deterministic pseudo-vector (no Math.random in this env).
    fn vec_of(seed: u32, dim: usize) -> Vec<f32> {
        let mut s = seed.wrapping_mul(2654435761).wrapping_add(1);
        (0..dim)
            .map(|_| {
                s = s.wrapping_mul(1103515245).wrapping_add(12345);
                ((s >> 8) & 0xffff) as f32 / 32768.0 - 1.0
            })
            .collect()
    }

    #[test]
    fn quantize_roundtrip_bounded_error() {
        let v = vec_of(7, 768);
        let q = quantize_i8(&v);
        let dq = dequantize(&q);
        // Per-element error ≤ scale/2 + fp slack.
        for (orig, recon) in v.iter().zip(&dq) {
            assert!((orig - recon).abs() <= q.scale / 2.0 + 1e-6, "elem err too large");
        }
        // Cosine is preserved to high precision.
        let cos = cosine_f32(&v, &dq);
        assert!(cos > 0.999, "dequant cosine {cos} should be ~1");
    }

    #[test]
    fn cosine_query_to_i8_approximates_f32_cosine() {
        let a = vec_of(1, 768);
        let b = vec_of(2, 768);
        let exact = cosine_f32(&a, &b);
        let approx = cosine_query_to_i8(&a, &quantize_i8(&b));
        assert!((exact - approx).abs() < 1e-2, "exact={exact} approx={approx}");
    }

    #[test]
    fn cosine_i8_symmetric_approximation() {
        let a = vec_of(3, 768);
        let b = vec_of(4, 768);
        let exact = cosine_f32(&a, &b);
        let approx = cosine_i8(&quantize_i8(&a), &quantize_i8(&b));
        assert!((exact - approx).abs() < 2e-2, "exact={exact} approx={approx}");
    }

    #[test]
    fn int8_rescore_matches_f32_topk() {
        // Build a 200-vector index + a query; the int8 rescored top-10 ids must
        // equal the exact f32 top-10 ids (ranking preserved through quantization).
        let dim = 128;
        let query = vec_of(999, dim);
        let raw: Vec<(String, Vec<f32>)> =
            (0..200).map(|i| (format!("v{i}"), vec_of(i, dim))).collect();

        let mut f32_scored: Vec<(String, f32)> = raw
            .iter()
            .map(|(id, v)| (id.clone(), cosine_f32(&query, v)))
            .collect();
        f32_scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1).unwrap().then_with(|| a.0.cmp(&b.0))
        });
        let f32_top: Vec<&String> = f32_scored.iter().take(10).map(|(id, _)| id).collect();

        let index: Vec<(String, QuantizedVec)> =
            raw.iter().map(|(id, v)| (id.clone(), quantize_i8(v))).collect();
        let i8_top = search_rescore(&query, &index, 10);
        let i8_ids: Vec<&String> = i8_top.iter().map(|(id, _)| id).collect();

        assert_eq!(f32_top, i8_ids, "int8 rescore top-10 must match f32 top-10");
    }

    #[test]
    fn zero_vector_is_honest() {
        let z = quantize_i8(&[0.0; 16]);
        assert_eq!(z.scale, 0.0);
        assert_eq!(cosine_query_to_i8(&[1.0; 16], &z), 0.0);
        assert!(search_rescore(&[1.0; 16], &[("z".into(), z)], 5)[0].1 == 0.0);
    }
}
