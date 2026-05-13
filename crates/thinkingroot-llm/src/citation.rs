//! Slice 10 — streaming `[claim:<id>]` marker parser.
//!
//! Buffered parser for the LLM citation contract. The agent's standing
//! system prompt instructs it to emit `[claim:<id>]` immediately after
//! the relevant text whenever it cites a claim from the provided
//! context. This parser tails the streaming token output and emits a
//! [`Citation`] for every well-formed marker, dedupe'd per turn so the
//! same claim referenced twice fires only one citation.
//!
//! # Honesty contract
//!
//! - The parser only emits citations that **literally** appear in the
//!   stream. If the LLM ignores the system-prompt instruction, no
//!   citations fire — we never fabricate them from retrieval metadata.
//! - The buffer cap (`MAX_PENDING`) bounds memory at 64 bytes — enough
//!   to handle any well-formed marker even when it spans several SSE
//!   chunks. A would-be marker that runs longer than 64 bytes is
//!   discarded as malformed without emitting; the buffer is cleared up
//!   to the next non-overlapping `[`.
//! - The marker grammar is intentionally narrow: `[claim:<id>]` where
//!   `<id>` is `[A-Za-z0-9_-]{1,64}`. A claim id with spaces, slashes,
//!   or unicode would be rejected — same posture as the v3 manifest's
//!   `relative_path` allow-list (CLAUDE.md §honesty rule §1).

use std::collections::HashSet;

/// Maximum bytes retained in the buffer waiting for a marker to close.
/// 64 + the prefix + the closing bracket gives ~80 bytes upper bound
/// per chunk; anything longer than that wouldn't be a well-formed
/// marker anyway.
const MAX_PENDING: usize = 80;

/// Marker prefix. Public so test fixtures + the system-prompt string
/// stay in lockstep without manually-typed copies.
pub const CITATION_PREFIX: &str = "[claim:";

/// Maximum claim-id length the parser will accept. Defends against a
/// pathological prompt that emits unbounded text inside `[claim:...]`.
const MAX_ID_LEN: usize = 64;

/// One emitted citation. The `claim_id` is verbatim from the stream;
/// callers responsible for any further validation (e.g. checking it
/// exists in the workspace's substrate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Citation {
    /// Claim id as it appeared between `[claim:` and `]`.
    pub claim_id: String,
}

/// Streaming parser for `[claim:<id>]` markers.
#[derive(Debug, Default)]
pub struct CitationParser {
    /// Bytes accumulated since the last completed marker, holding any
    /// in-progress prefix ("[clai…").
    buf: String,
    /// Claim ids already emitted in this parser's lifetime.  Once a
    /// claim is cited it never fires twice for the same parser
    /// instance — chat handlers create a fresh parser per turn.
    seen: HashSet<String>,
}

impl CitationParser {
    /// Construct a fresh parser. Equivalent to `Default::default()`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of the LLM stream and return any citations that
    /// became fully visible during this call. Designed to be called
    /// from inside the SSE token handler — `&str` is taken (not
    /// `&[u8]`) because the upstream is already UTF-8 by contract.
    pub fn feed(&mut self, chunk: &str) -> Vec<Citation> {
        if chunk.is_empty() && self.buf.is_empty() {
            return Vec::new();
        }
        self.buf.push_str(chunk);

        let mut emitted = Vec::new();
        loop {
            let Some(prefix_at) = self.buf.find(CITATION_PREFIX) else {
                // No prefix in buffer at all.  Drop everything except
                // the tail that could be the start of the next marker
                // (up to MAX_PENDING bytes).
                self.compact_drop_all();
                break;
            };
            // Drop bytes before the prefix — they aren't part of any
            // marker.
            if prefix_at > 0 {
                self.buf.drain(..prefix_at);
            }
            let after = CITATION_PREFIX.len();
            // Search for the closing `]` after the prefix.
            let Some(close_rel) = self.buf[after..].find(']') else {
                // Marker is open but unclosed in this batch.  Cap the
                // pending buffer so a malformed `[claim:foo` without a
                // close doesn't grow without bound.
                if self.buf.len() > MAX_PENDING {
                    // Skip past this aborted prefix so the next
                    // legitimate marker can start matching.
                    self.buf.drain(..1);
                    continue;
                }
                break;
            };
            let id_start = after;
            let id_end = id_start + close_rel;
            let id_str = self.buf[id_start..id_end].to_string();
            let close_at = id_end + 1; // include the closing `]`
            if is_valid_claim_id(&id_str) {
                if self.seen.insert(id_str.clone()) {
                    emitted.push(Citation { claim_id: id_str });
                }
                self.buf.drain(..close_at);
                continue;
            }
            // Malformed marker — drop the opening bracket so we keep
            // scanning for a real one.
            self.buf.drain(..1);
        }
        emitted
    }

    /// Reset the parser's `seen` set + buffer. Useful when reusing a
    /// parser across a session boundary.
    pub fn reset(&mut self) {
        self.buf.clear();
        self.seen.clear();
    }

    /// Borrow the de-dup set for tests.
    #[cfg(test)]
    pub(crate) fn seen(&self) -> &HashSet<String> {
        &self.seen
    }

    fn compact_drop_all(&mut self) {
        if self.buf.len() <= MAX_PENDING {
            // Even without a prefix, retain the tail — it could be
            // "[clai…" mid-emission.
            return;
        }
        let drop_to = self.buf.len() - MAX_PENDING;
        self.buf.drain(..drop_to);
    }
}

fn is_valid_claim_id(id: &str) -> bool {
    if id.is_empty() || id.len() > MAX_ID_LEN {
        return false;
    }
    id.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The exact instruction string injected into the agent's system
/// prompt by `crate::llm::build_system_prompt`. Lifted out of that
/// function so a test can pin the wording — the parser's grammar
/// agrees with the prompt by literal string match.
pub const CITATION_PROMPT: &str = "When citing a claim from the provided context, append the marker `[claim:<id>]` immediately after the relevant text. The marker MUST appear verbatim in your output. Use the claim id verbatim from the context. Emit one marker per cited claim, even when re-referencing.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_chunk_marker_extracted() {
        let mut p = CitationParser::new();
        let cites = p.feed("Auth uses OAuth2 [claim:01HQAUTH] in production.");
        assert_eq!(cites.len(), 1);
        assert_eq!(cites[0].claim_id, "01HQAUTH");
    }

    #[test]
    fn marker_split_across_chunks_extracted() {
        let mut p = CitationParser::new();
        let mut all = Vec::new();
        all.extend(p.feed("Auth uses OAuth2 [cl"));
        all.extend(p.feed("aim:01HQ"));
        all.extend(p.feed("AUTH] in production."));
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].claim_id, "01HQAUTH");
    }

    #[test]
    fn duplicate_marker_in_same_turn_emitted_once() {
        let mut p = CitationParser::new();
        let mut all = Vec::new();
        all.extend(p.feed("[claim:X1] foo [claim:X2] bar [claim:X1]"));
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|c| c.claim_id == "X1"));
        assert!(all.iter().any(|c| c.claim_id == "X2"));
    }

    #[test]
    fn malformed_marker_left_unparsed() {
        let mut p = CitationParser::new();
        let cites = p.feed("Junk [claim:has space] more [claim:OK1] end");
        assert_eq!(cites.len(), 1);
        assert_eq!(cites[0].claim_id, "OK1");
    }

    #[test]
    fn empty_marker_is_rejected() {
        let mut p = CitationParser::new();
        let cites = p.feed("Junk [claim:] more");
        assert!(cites.is_empty(), "empty id must not emit");
    }

    #[test]
    fn marker_at_buffer_boundary_does_not_flush_prematurely() {
        let mut p = CitationParser::new();
        assert!(p.feed("…and then [claim").is_empty());
        assert!(p.feed(":AB").is_empty());
        let cites = p.feed("CD]");
        assert_eq!(cites.len(), 1);
        assert_eq!(cites[0].claim_id, "ABCD");
    }

    #[test]
    fn long_unclosed_marker_does_not_grow_unbounded() {
        let mut p = CitationParser::new();
        // Feed an open prefix and a runaway body without a close —
        // parser must cap the buffer.
        p.feed("[claim:");
        for _ in 0..10 {
            p.feed("xxxxxxxxxxxxxxxxxxxxxxxx");
        }
        // The buffer may keep at most MAX_PENDING + the original prefix
        // worth of bytes; either way we're nowhere near 200 bytes.
        assert!(
            p.buf.len() < 200,
            "buffer must be capped, got {} bytes",
            p.buf.len()
        );
    }

    #[test]
    fn id_with_underscore_and_dash_accepted() {
        let mut p = CitationParser::new();
        let cites = p.feed("[claim:01_HQ-Z]");
        assert_eq!(cites.len(), 1);
        assert_eq!(cites[0].claim_id, "01_HQ-Z");
    }

    #[test]
    fn reset_clears_seen_set() {
        let mut p = CitationParser::new();
        p.feed("[claim:A1]");
        assert!(p.seen().contains("A1"));
        p.reset();
        assert!(p.seen().is_empty());
        let cites = p.feed("[claim:A1]");
        assert_eq!(cites.len(), 1);
    }
}
