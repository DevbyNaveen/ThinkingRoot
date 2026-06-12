// Hydrated Answers (§5 output stack #1) — engine-side rendering.
//
// The model emits an ID-dense skeleton ("the DB is [claim:c2]"); the engine
// renders the verbatim claim text from the graph in place ("the DB is on Azure
// Postgres 16 [claim:c2]"). The provider never generates the quoted bytes — we
// render them for free from the grounding set — so grounded answers cost ~3-4×
// less output. Provider-agnostic (works off our own `[claim:id]` markers, not
// any one vendor's citation API) and supersession-safe (re-rendered from the
// live graph).
//
// This module is the PURE transform; wiring + the flag live in `rest.rs`. It is
// double-render-safe by construction: a claim whose verbatim statement is
// already present anywhere in the answer is never inserted again, so running it
// on a normal verbose answer (or twice) is a no-op. That makes it safe to ship
// before the paired skeleton-emitting prompt exists.

/// Result of hydrating one answer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HydrationResult {
    /// The answer with claim statements rendered inline before their markers.
    pub text: String,
    /// How many `[claim:id]` markers were expanded with verbatim text.
    pub claims_rendered: usize,
    /// Approx output tokens the model did NOT have to generate (chars/4 of the
    /// rendered statements). The savings-meter "hydration" contribution.
    pub output_tokens_saved: usize,
}

const MARKER_OPEN: &str = "[claim:";

/// Render `[claim:<id>]` markers in `answer` into verbatim cited prose using
/// `grounding` (`(claim_id, statement)`). For each marker whose id resolves to
/// a statement that is NOT already present in the answer, the statement is
/// inserted immediately before the marker. Idempotent and duplication-free.
pub fn hydrate_answer(answer: &str, grounding: &[(String, String)]) -> HydrationResult {
    use std::collections::{HashMap, HashSet};

    let map: HashMap<&str, &str> = grounding
        .iter()
        .map(|(id, st)| (id.as_str(), st.as_str()))
        .collect();

    // Statements already verbatim in the answer must not be re-inserted.
    let mut present: HashSet<String> = HashSet::new();
    for (_id, st) in grounding {
        let t = st.trim();
        if !t.is_empty() && answer.contains(t) {
            present.insert(t.to_string());
        }
    }

    let mut out = String::with_capacity(answer.len());
    let mut rendered = 0usize;
    let mut saved_chars = 0usize;
    let mut idx = 0usize;

    while let Some(rel) = answer[idx..].find(MARKER_OPEN) {
        let mstart = idx + rel;
        // Locate the marker's closing ']'; an unterminated marker ends the scan.
        let Some(close_rel) = answer[mstart..].find(']') else {
            break;
        };
        let mend = mstart + close_rel + 1;
        let id = &answer[mstart + MARKER_OPEN.len()..mend - 1];

        out.push_str(&answer[idx..mstart]); // text before the marker
        if let Some(stmt) = map.get(id) {
            let t = stmt.trim();
            if !t.is_empty() && !present.contains(t) {
                out.push_str(t);
                out.push(' ');
                present.insert(t.to_string());
                rendered += 1;
                saved_chars += t.len() + 1;
            }
        }
        out.push_str(&answer[mstart..mend]); // the marker itself (kept)
        idx = mend;
    }
    out.push_str(&answer[idx..]);

    HydrationResult {
        text: out,
        claims_rendered: rendered,
        output_tokens_saved: saved_chars / 4,
    }
}

/// Streaming counterpart to [`hydrate_answer`]. Feed it LLM chunks as they
/// arrive; it returns the text safe to emit now — with completed `[claim:id]`
/// markers rendered to their verbatim statement — and withholds any trailing
/// PARTIAL marker until it completes (so a marker split across chunks is never
/// shown half-rendered). Same insert-before-marker + de-dup semantics as the
/// one-shot transform. Call [`finish`](Self::finish) once at end-of-stream.
pub struct StreamingHydrator {
    map: std::collections::HashMap<String, String>,
    present: std::collections::HashSet<String>,
    pending: String,
    pub claims_rendered: usize,
    pub output_tokens_saved: usize,
}

impl StreamingHydrator {
    pub fn new(grounding: &[(String, String)]) -> Self {
        Self {
            map: grounding.iter().cloned().collect(),
            present: std::collections::HashSet::new(),
            pending: String::new(),
            claims_rendered: 0,
            output_tokens_saved: 0,
        }
    }

    /// Push a streamed chunk; return the renderable text to emit now.
    pub fn push(&mut self, chunk: &str) -> String {
        self.pending.push_str(chunk);
        // Hold from a trailing unterminated '[' (a possible partial marker);
        // flush everything before it. A complete `[...]` anywhere earlier is
        // safe to render.
        let flush_to = match self.pending.rfind('[') {
            Some(open) if !self.pending[open..].contains(']') => open,
            _ => self.pending.len(),
        };
        if flush_to == 0 {
            return String::new();
        }
        let segment: String = self.pending.drain(..flush_to).collect();
        self.render(&segment)
    }

    /// Flush any withheld tail at end-of-stream.
    pub fn finish(&mut self) -> String {
        let rest = std::mem::take(&mut self.pending);
        self.render(&rest)
    }

    fn render(&mut self, segment: &str) -> String {
        let mut out = String::with_capacity(segment.len());
        let mut idx = 0usize;
        while let Some(rel) = segment[idx..].find(MARKER_OPEN) {
            let mstart = idx + rel;
            let Some(close_rel) = segment[mstart..].find(']') else {
                break; // push() withholds partial markers, so unreachable in practice
            };
            let mend = mstart + close_rel + 1;
            let id = &segment[mstart + MARKER_OPEN.len()..mend - 1];
            out.push_str(&segment[idx..mstart]);
            if let Some(stmt) = self.map.get(id) {
                let t = stmt.trim();
                if !t.is_empty() && !self.present.contains(t) {
                    out.push_str(t);
                    out.push(' ');
                    self.present.insert(t.to_string());
                    self.claims_rendered += 1;
                    self.output_tokens_saved += (t.len() + 1) / 4;
                }
            }
            out.push_str(&segment[mstart..mend]); // keep the marker
            idx = mend;
        }
        out.push_str(&segment[idx..]);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn renders_bare_marker_with_verbatim_statement() {
        let grounding = g(&[("c1", "Postgres 16 on Azure")]);
        let r = hydrate_answer("The database is [claim:c1].", &grounding);
        assert_eq!(r.text, "The database is Postgres 16 on Azure [claim:c1].");
        assert_eq!(r.claims_rendered, 1);
        assert!(r.output_tokens_saved > 0);
    }

    #[test]
    fn does_not_duplicate_when_statement_already_present() {
        // A verbose answer that already quotes the claim text — no double render.
        let grounding = g(&[("c1", "Postgres 16 on Azure")]);
        let input = "The database is Postgres 16 on Azure [claim:c1].";
        let r = hydrate_answer(input, &grounding);
        assert_eq!(r.text, input, "already-present statement is left untouched");
        assert_eq!(r.claims_rendered, 0);
        assert_eq!(r.output_tokens_saved, 0);
    }

    #[test]
    fn is_idempotent() {
        let grounding = g(&[("c1", "Postgres 16")]);
        let once = hydrate_answer("DB: [claim:c1]", &grounding);
        let twice = hydrate_answer(&once.text, &grounding);
        assert_eq!(twice.text, once.text, "second pass is a no-op");
        assert_eq!(twice.claims_rendered, 0);
    }

    #[test]
    fn repeated_marker_renders_once() {
        let grounding = g(&[("c1", "Acme Corp")]);
        let r = hydrate_answer("[claim:c1] and again [claim:c1]", &grounding);
        assert_eq!(r.claims_rendered, 1, "same claim text inserted only once");
        assert_eq!(r.text, "Acme Corp [claim:c1] and again [claim:c1]");
    }

    #[test]
    fn unknown_id_and_unterminated_marker_are_safe() {
        let grounding = g(&[("c1", "known")]);
        // Unknown id: marker kept, nothing inserted.
        let r1 = hydrate_answer("see [claim:missing] here", &grounding);
        assert_eq!(r1.text, "see [claim:missing] here");
        assert_eq!(r1.claims_rendered, 0);
        // Unterminated marker: no panic, tail preserved.
        let r2 = hydrate_answer("dangling [claim:c1", &grounding);
        assert_eq!(r2.text, "dangling [claim:c1");
    }

    #[test]
    fn no_markers_is_noop() {
        let grounding = g(&[("c1", "x")]);
        let r = hydrate_answer("a plain answer with no markers", &grounding);
        assert_eq!(r.text, "a plain answer with no markers");
        assert_eq!(r.claims_rendered, 0);
    }

    // ── StreamingHydrator ───────────────────────────────────────────────────

    fn drain(h: &mut StreamingHydrator, chunks: &[&str]) -> String {
        let mut out = String::new();
        for c in chunks {
            out.push_str(&h.push(c));
        }
        out.push_str(&h.finish());
        out
    }

    #[test]
    fn streaming_renders_marker_split_across_chunks() {
        let grounding = g(&[("c1", "Postgres 16 on Azure")]);
        let mut h = StreamingHydrator::new(&grounding);
        // The marker arrives in pieces; it must never be shown half-rendered.
        let out = drain(&mut h, &["The DB is ", "[clai", "m:c", "1]", "."]);
        assert_eq!(out, "The DB is Postgres 16 on Azure [claim:c1].");
        assert_eq!(h.claims_rendered, 1);
        assert!(h.output_tokens_saved > 0);
    }

    #[test]
    fn streaming_matches_one_shot_for_whole_answer() {
        let grounding = g(&[("c1", "Acme Corp"), ("c2", "founded 2026")]);
        let answer = "Acme [claim:c1] was [claim:c2] last year.";
        let one_shot = hydrate_answer(answer, &grounding).text;
        // Feed it one byte at a time — the streamed result must equal one-shot.
        let mut h = StreamingHydrator::new(&grounding);
        let chunks: Vec<String> = answer.chars().map(|c| c.to_string()).collect();
        let refs: Vec<&str> = chunks.iter().map(|s| s.as_str()).collect();
        assert_eq!(drain(&mut h, &refs), one_shot);
    }

    #[test]
    fn streaming_passes_non_claim_brackets_through() {
        let grounding = g(&[("c1", "x")]);
        let mut h = StreamingHydrator::new(&grounding);
        assert_eq!(drain(&mut h, &["see arr", "ay[0] and [1] done"]), "see array[0] and [1] done");
        assert_eq!(h.claims_rendered, 0);
    }
}
