//! Witness anchor verification — the only surviving piece of the
//! grounding tribunal under the Witness Mesh design.
//!
//! In the LLM-extraction era, the four-judge tribunal scored LLM
//! paraphrases against source bytes (lexical overlap, span match,
//! embedding cosine, NLI entailment). The tribunal existed because
//! the LLM might hallucinate.
//!
//! Under the Witness Mesh, a Witness IS its byte span — there is no
//! paraphrase to fact-check. The single remaining verification is:
//! does the BLAKE3 of `source[byte_start..byte_end]` match the
//! Witness's stored `content_blake3`? If yes, the Witness anchor is
//! intact and downstream consumers can trust the row. If no, the
//! source bytes have been tampered with (or the row predates the
//! source rev) and the Witness is `Stale`.
//!
//! This module is intentionally tiny — verification is a 2-line
//! BLAKE3 comparison. Its job is to be a typed, named API surface
//! that pipeline phases and `tr-verify` both call into so the
//! semantics never drift.
//!
//! Cost: ~10µs per Witness on a modern CPU (BLAKE3 is the fastest
//! cryptographic hash for short inputs). CCC I-4 budget per query
//! is ~500µs for top-50 hits — well within budget.

use thiserror::Error;

/// Verdict for a single Witness anchor verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnchorVerdict {
    /// `BLAKE3(source_bytes[start..end]) == expected_content_blake3`.
    /// The Witness is anchored to bytes that match its stored hash.
    Verified,
    /// The hashes do not match — source bytes have changed (or were
    /// never what the Witness claimed). Consumer must treat the
    /// Witness as `Stale` and either re-derive or drop from results.
    Mismatch {
        expected_blake3: String,
        actual_blake3: String,
    },
}

impl AnchorVerdict {
    /// True iff this verdict says the anchor is intact.
    pub fn is_verified(&self) -> bool {
        matches!(self, Self::Verified)
    }
}

/// Reasons a verification call cannot produce a meaningful verdict.
/// Distinct from `AnchorVerdict::Mismatch` — these are caller
/// errors (out-of-range slice, malformed inputs) rather than
/// substrate errors.
#[derive(Debug, Error)]
pub enum WitnessAnchorError {
    #[error("anchor span [{start}, {end}) exceeds source length {source_len}")]
    SpanOutOfRange {
        start: u64,
        end: u64,
        source_len: u64,
    },
    #[error("anchor span end {end} is less than or equal to start {start}")]
    InvertedSpan { start: u64, end: u64 },
    #[error("expected_content_blake3 must be 64 lower-hex chars, got {0}")]
    MalformedExpectedHash(usize),
}

/// Verify that a Witness's stored `content_blake3` matches the actual
/// BLAKE3 of the source byte slice it points at.
///
/// `source_bytes` is the full file's bytes (typically read from
/// `source.tar.zst` at pack-verify time, or from the byte store at
/// query time). `byte_start` and `byte_end` are file-relative
/// offsets from the Witness's canonical anchor span (`spans[0]`).
/// `expected_content_blake3` is the 64-char lower-hex string the
/// Witness has on record.
///
/// Returns:
/// - `Ok(AnchorVerdict::Verified)` on success.
/// - `Ok(AnchorVerdict::Mismatch { … })` when the hashes diverge.
/// - `Err(WitnessAnchorError::*)` on out-of-range or malformed input.
pub fn verify_witness_anchor(
    byte_start: u64,
    byte_end: u64,
    source_bytes: &[u8],
    expected_content_blake3: &str,
) -> Result<AnchorVerdict, WitnessAnchorError> {
    if byte_end <= byte_start {
        return Err(WitnessAnchorError::InvertedSpan {
            start: byte_start,
            end: byte_end,
        });
    }
    if byte_end > source_bytes.len() as u64 {
        return Err(WitnessAnchorError::SpanOutOfRange {
            start: byte_start,
            end: byte_end,
            source_len: source_bytes.len() as u64,
        });
    }
    if expected_content_blake3.len() != 64 {
        return Err(WitnessAnchorError::MalformedExpectedHash(
            expected_content_blake3.len(),
        ));
    }

    let slice = &source_bytes[byte_start as usize..byte_end as usize];
    let actual = blake3::hash(slice).to_hex().to_string();

    if actual == expected_content_blake3 {
        Ok(AnchorVerdict::Verified)
    } else {
        Ok(AnchorVerdict::Mismatch {
            expected_blake3: expected_content_blake3.to_string(),
            actual_blake3: actual,
        })
    }
}

/// Convenience wrapper that returns a `bool` for callers that don't
/// care about the diagnostic detail. Returns `false` on any error
/// or mismatch — use `verify_witness_anchor` when you need to log
/// the reason.
pub fn is_witness_anchor_intact(
    byte_start: u64,
    byte_end: u64,
    source_bytes: &[u8],
    expected_content_blake3: &str,
) -> bool {
    matches!(
        verify_witness_anchor(byte_start, byte_end, source_bytes, expected_content_blake3),
        Ok(AnchorVerdict::Verified)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_bytes_verify() {
        let source = b"hello world";
        let expected = blake3::hash(&source[6..11]).to_hex().to_string();
        let v = verify_witness_anchor(6, 11, source, &expected).unwrap();
        assert!(v.is_verified());
    }

    #[test]
    fn mismatched_hash_returns_mismatch_verdict() {
        let source = b"hello world";
        let wrong = "0".repeat(64);
        let v = verify_witness_anchor(6, 11, source, &wrong).unwrap();
        assert!(!v.is_verified());
        match v {
            AnchorVerdict::Mismatch {
                expected_blake3,
                actual_blake3,
            } => {
                assert_eq!(expected_blake3, "0".repeat(64));
                assert_eq!(actual_blake3.len(), 64);
                assert_ne!(expected_blake3, actual_blake3);
            }
            _ => panic!("expected Mismatch verdict"),
        }
    }

    #[test]
    fn out_of_range_span_returns_error() {
        let source = b"abc";
        let err = verify_witness_anchor(0, 100, source, &"0".repeat(64)).unwrap_err();
        assert!(matches!(err, WitnessAnchorError::SpanOutOfRange { .. }));
    }

    #[test]
    fn inverted_span_returns_error() {
        let source = b"abc";
        let err = verify_witness_anchor(2, 1, source, &"0".repeat(64)).unwrap_err();
        assert!(matches!(err, WitnessAnchorError::InvertedSpan { .. }));
    }

    #[test]
    fn equal_start_and_end_returns_inverted_error() {
        // Zero-length spans are rejected — every Witness's anchor
        // points at at-least-one byte.
        let source = b"abc";
        let err = verify_witness_anchor(1, 1, source, &"0".repeat(64)).unwrap_err();
        assert!(matches!(err, WitnessAnchorError::InvertedSpan { .. }));
    }

    #[test]
    fn malformed_expected_hash_returns_error() {
        let source = b"abc";
        let err = verify_witness_anchor(0, 3, source, "short").unwrap_err();
        assert!(matches!(err, WitnessAnchorError::MalformedExpectedHash(5)));
    }

    #[test]
    fn is_witness_anchor_intact_collapses_to_bool() {
        let source = b"hello world";
        let good = blake3::hash(&source[0..5]).to_hex().to_string();
        let bad = "0".repeat(64);
        assert!(is_witness_anchor_intact(0, 5, source, &good));
        assert!(!is_witness_anchor_intact(0, 5, source, &bad));
        // Errors collapse to false too.
        assert!(!is_witness_anchor_intact(0, 999, source, &good));
    }

    #[test]
    fn full_file_span_verifies() {
        let source = b"the entire file";
        let expected = blake3::hash(source).to_hex().to_string();
        let v = verify_witness_anchor(0, source.len() as u64, source, &expected).unwrap();
        assert!(v.is_verified());
    }
}
