//! ByteSpan coalescing — pure helper consumed by `hybrid::ByteSpanStitcher`.
//!
//! Spans that overlap or sit within `max_gap_bytes` of each other merge into
//! a single span; the contributing-table tags accumulate on the merged span
//! so the desktop UI can show "this byte range came from claims +
//! code_signatures + function_calls".
//!
//! Spec: `docs/2026-05-02-hybrid-retrieval-spec.md` §7.

use super::hybrid_types::ByteSpan;

/// Coalesce a set of byte spans into the minimum number of non-overlapping
/// spans, merging any pair within `max_gap_bytes` of each other. Returns the
/// coalesced spans sorted by `(byte_start, byte_end)`.
pub fn coalesce(mut spans: Vec<ByteSpan>, max_gap_bytes: u64) -> Vec<ByteSpan> {
    if spans.len() <= 1 {
        return spans;
    }
    spans.sort_by_key(|s| (s.byte_start, s.byte_end));
    let mut out: Vec<ByteSpan> = Vec::with_capacity(spans.len());
    for s in spans {
        match out.last_mut() {
            Some(prev) if prev.byte_end + max_gap_bytes >= s.byte_start => {
                prev.byte_end = prev.byte_end.max(s.byte_end);
                for tag in s.contributed_by {
                    if !prev.contributed_by.contains(&tag) {
                        prev.contributed_by.push(tag);
                    }
                }
            }
            _ => out.push(s),
        }
    }
    out
}

/// Default coalescing gap. Spans within this many bytes of each other merge.
/// 8 bytes covers typical inter-row whitespace + token boundary cases without
/// stitching unrelated regions together.
pub const DEFAULT_MAX_GAP_BYTES: u64 = 8;

#[cfg(test)]
mod tests {
    use super::*;

    fn span(start: u64, end: u64, tag: &str) -> ByteSpan {
        ByteSpan {
            byte_start: start,
            byte_end: end,
            contributed_by: vec![tag.into()],
        }
    }

    #[test]
    fn coalesce_merges_overlapping_spans() {
        let input = vec![span(0, 50, "claims"), span(40, 80, "code_signatures")];
        let out = coalesce(input, DEFAULT_MAX_GAP_BYTES);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].byte_start, 0);
        assert_eq!(out[0].byte_end, 80);
        assert!(out[0].contributed_by.contains(&"claims".to_string()));
        assert!(out[0].contributed_by.contains(&"code_signatures".to_string()));
    }

    #[test]
    fn coalesce_merges_at_max_gap_8() {
        // gap is exactly 8 bytes (50..58)
        let input = vec![span(0, 50, "a"), span(58, 100, "b")];
        let out = coalesce(input, 8);
        assert_eq!(out.len(), 1, "gap = max_gap should merge");
        assert_eq!(out[0].byte_end, 100);
    }

    #[test]
    fn coalesce_does_not_merge_at_gap_9() {
        // gap is 9 bytes (50..59)
        let input = vec![span(0, 50, "a"), span(59, 100, "b")];
        let out = coalesce(input, 8);
        assert_eq!(out.len(), 2, "gap > max_gap should NOT merge");
    }

    #[test]
    fn coalesce_is_order_independent() {
        let forward = vec![span(0, 10, "a"), span(20, 30, "b"), span(50, 60, "c")];
        let reversed = vec![span(50, 60, "c"), span(20, 30, "b"), span(0, 10, "a")];
        assert_eq!(
            coalesce(forward, DEFAULT_MAX_GAP_BYTES),
            coalesce(reversed, DEFAULT_MAX_GAP_BYTES)
        );
    }

    #[test]
    fn coalesce_empty_returns_empty() {
        let out = coalesce(vec![], DEFAULT_MAX_GAP_BYTES);
        assert!(out.is_empty());
    }

    #[test]
    fn coalesce_single_returns_single() {
        let out = coalesce(vec![span(0, 10, "a")], DEFAULT_MAX_GAP_BYTES);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn coalesce_dedupes_contributed_by_tags() {
        let input = vec![span(0, 50, "claims"), span(40, 80, "claims")];
        let out = coalesce(input, DEFAULT_MAX_GAP_BYTES);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].contributed_by, vec!["claims".to_string()]);
    }
}
