//! Citation-marker extraction — Phase β.2.
//!
//! AI replies that ground citations use the canonical marker
//! `[[witness:<64-hex>]]` inline in markdown. The Living Paper, the
//! Playground citation chips, and (now) the cognition-commit
//! auto-recorder all need to scan the same prose for the same
//! markers — so the extractor lives in one place to avoid the rules
//! drifting across consumers.
//!
//! Identity rule: a marker is `[[witness:` followed by exactly 64
//! lower-hex characters followed by `]]`. Upper-case, partial, or
//! malformed inputs are silently skipped (matches the CitationChip
//! tolerant projection — see `apps/.../CitationChip.tsx`). Order is
//! preserved; duplicates are de-duplicated (returns the first
//! occurrence of each id).

use thinkingroot_core::types::WitnessId;

/// Parse every `[[witness:<id>]]` marker from `text` into typed
/// `WitnessId`s. De-duplicates while preserving first-occurrence
/// order. Pure function — no I/O, no allocation beyond the result
/// vector + an HashSet to dedup. Safe to call inline in the
/// agent-streaming hot path.
pub fn extract_witness_citations(text: &str) -> Vec<WitnessId> {
    use std::collections::HashSet;

    const PREFIX: &str = "[[witness:";
    const SUFFIX: &str = "]]";
    const HEX_LEN: usize = 64;

    let mut out: Vec<WitnessId> = Vec::new();
    let mut seen: HashSet<[u8; 32]> = HashSet::new();
    let bytes = text.as_bytes();

    let mut cursor = 0usize;
    while cursor < bytes.len() {
        // Find the next prefix.
        let Some(rel) = text[cursor..].find(PREFIX) else {
            break;
        };
        let start = cursor + rel + PREFIX.len();
        let candidate_end = start + HEX_LEN;
        if candidate_end + SUFFIX.len() > bytes.len() {
            // Not enough room for a valid id + closing marker.
            break;
        }
        // The id slice must be exactly 64 lower-hex chars and be
        // followed immediately by `]]`.
        let id_slice = &text[start..candidate_end];
        let suffix_slice = &text[candidate_end..candidate_end + SUFFIX.len()];
        if suffix_slice == SUFFIX
            && let Ok(id) = WitnessId::from_hex(id_slice)
            && seen.insert(id.0)
        {
            out.push(id);
        }
        // Advance past this attempt regardless of validity so a
        // malformed marker doesn't trap us in a loop.
        cursor = start;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_hex(byte: u8) -> String {
        format!("{:02x}", byte).repeat(32)
    }

    #[test]
    fn empty_text_returns_empty() {
        assert!(extract_witness_citations("").is_empty());
        assert!(extract_witness_citations("no markers here").is_empty());
    }

    #[test]
    fn parses_single_marker() {
        let id_hex = fake_hex(0xab);
        let text = format!("see [[witness:{id_hex}]] for details");
        let out = extract_witness_citations(&text);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].to_hex(), id_hex);
    }

    #[test]
    fn parses_multiple_markers_preserving_order() {
        let a = fake_hex(0x01);
        let b = fake_hex(0x02);
        let c = fake_hex(0x03);
        let text = format!(
            "first [[witness:{a}]], then [[witness:{b}]] and finally [[witness:{c}]]."
        );
        let out = extract_witness_citations(&text);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].to_hex(), a);
        assert_eq!(out[1].to_hex(), b);
        assert_eq!(out[2].to_hex(), c);
    }

    #[test]
    fn deduplicates_while_preserving_first_occurrence() {
        let a = fake_hex(0x10);
        let b = fake_hex(0x20);
        let text = format!(
            "[[witness:{a}]] and [[witness:{b}]] and again [[witness:{a}]] and [[witness:{b}]]"
        );
        let out = extract_witness_citations(&text);
        assert_eq!(out.len(), 2, "duplicates collapse");
        assert_eq!(out[0].to_hex(), a);
        assert_eq!(out[1].to_hex(), b);
    }

    #[test]
    fn skips_uppercase_hex() {
        let upper = "F".repeat(64);
        let text = format!("ignore [[witness:{upper}]] please");
        assert!(extract_witness_citations(&text).is_empty());
    }

    #[test]
    fn skips_wrong_length() {
        let too_short = "ab".repeat(20); // 40 chars
        let too_long = "ab".repeat(40); // 80 chars
        let text = format!(
            "neither [[witness:{too_short}]] nor [[witness:{too_long}]] count"
        );
        assert!(extract_witness_citations(&text).is_empty());
    }

    #[test]
    fn skips_non_hex_characters() {
        let bad = "g".repeat(64);
        let text = format!("nope [[witness:{bad}]]");
        assert!(extract_witness_citations(&text).is_empty());
    }

    #[test]
    fn requires_closing_double_bracket() {
        let id = fake_hex(0x55);
        // Missing one `]`.
        let text = format!("partial [[witness:{id}] more");
        assert!(extract_witness_citations(&text).is_empty());
    }

    #[test]
    fn parses_adjacent_markers_without_separator() {
        let a = fake_hex(0xaa);
        let b = fake_hex(0xbb);
        let text = format!("[[witness:{a}]][[witness:{b}]]");
        let out = extract_witness_citations(&text);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].to_hex(), a);
        assert_eq!(out[1].to_hex(), b);
    }

    #[test]
    fn parses_marker_at_text_boundaries() {
        let id = fake_hex(0x7f);
        // At start.
        let start = format!("[[witness:{id}]] then prose");
        assert_eq!(extract_witness_citations(&start).len(), 1);
        // At end.
        let end = format!("prose then [[witness:{id}]]");
        assert_eq!(extract_witness_citations(&end).len(), 1);
        // Exact full match.
        let only = format!("[[witness:{id}]]");
        assert_eq!(extract_witness_citations(&only).len(), 1);
    }

    #[test]
    fn handles_truncated_prefix_at_eof() {
        // Common adversarial case: a streaming reply that got cut off
        // mid-marker. Must not panic or infinite-loop.
        let truncated = "[[witness:ab";
        assert!(extract_witness_citations(truncated).is_empty());
    }
}
