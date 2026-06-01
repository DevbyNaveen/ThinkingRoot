//! SRX-style deterministic sentence + clause segmentation — zero LLM.
//!
//! The Witness Mesh turns a prose paragraph into individually-retrievable
//! units. Retrieval quality (the Dense X Retrieval result) improves as those
//! units approach *atomic propositions*: one fact each. Two mechanical steps
//! get most of that benefit without an LLM:
//!
//! 1. **Sentence segmentation** that does NOT mis-split on abbreviations
//!    ("Dr. Smith"), decimals ("3.14"), initials ("J. K. Rowling"), or
//!    ellipses ("wait…") — the failures of naive `.`/`!`/`?` splitting. This
//!    mirrors what an SRX ruleset encodes, implemented as explicit guards.
//! 2. **Clause splitting** (ClausIE-lite): a compound sentence
//!    ("I live in Tokyo, and I work at Acme.") carries multiple facts; split
//!    at coordinating conjunctions / semicolons when *both* sides contain a
//!    finite verb, so each fact becomes its own retrievable unit.
//!
//! Both return **byte sub-ranges relative to the input** so callers preserve
//! the Witness Mesh's exact byte anchoring. Determinism is total: same input →
//! same spans, no model, no allocation of the source.

use crate::fact_quality::has_finite_verb;

/// Abbreviations that take a trailing period but do NOT end a sentence.
/// Stored lowercase, compared case-insensitively against the token preceding a
/// period. Kept deliberately small and high-frequency — over-inclusion only
/// risks under-splitting (two facts in one unit), which is safer for recall
/// than over-splitting an abbreviation into a bogus boundary.
const ABBREVIATIONS: &[&str] = &[
    "mr", "mrs", "ms", "dr", "prof", "sr", "jr", "st", "vs", "etc", "inc",
    "ltd", "co", "corp", "fig", "no", "vol", "al", "approx", "dept", "est",
    "gen", "gov", "rep", "sen", "rev", "hon", "messrs", "mt", "ave", "blvd",
    "e.g", "i.e", "a.m", "p.m", "u.s", "u.k", "ph.d", "b.a", "m.a", "p.s",
];

/// Returns byte sub-ranges (relative to `text`) for each sentence.
///
/// A terminator (`.`/`!`/`?`, including runs like `...`/`?!`) ends a sentence
/// only when it is followed by whitespace or end-of-text AND it is not part of
/// an abbreviation, decimal, or single-letter initial. Leading/trailing
/// whitespace is trimmed out of each returned span.
pub fn sentence_spans(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut spans: Vec<(usize, usize)> = Vec::new();

    // Advance past leading whitespace to the first sentence start.
    let mut seg_start = 0usize;
    while seg_start < n && bytes[seg_start].is_ascii_whitespace() {
        seg_start += 1;
    }

    let mut i = seg_start;
    while i < n {
        let b = bytes[i];
        let is_term = b == b'.' || b == b'!' || b == b'?';
        if !is_term {
            i += 1;
            continue;
        }
        // Consume a run of terminators (handles "..." and "?!").
        let mut j = i + 1;
        while j < n && (bytes[j] == b'.' || bytes[j] == b'!' || bytes[j] == b'?') {
            j += 1;
        }
        let at_text_boundary = j >= n || bytes[j].is_ascii_whitespace();
        // A run of 2+ dots ("…") is an ellipsis — mid-utterance continuation,
        // not a sentence end ("Well... I think so."). "?!" / "!" still break.
        let multi_dot = (j - i) >= 2 && bytes[i..j].iter().all(|&c| c == b'.');
        if at_text_boundary && !multi_dot && !suppress_boundary(text, bytes, i, seg_start) {
            // Sentence ends at j (terminators included). Trim trailing space.
            let mut end = j;
            while end > seg_start && bytes[end - 1].is_ascii_whitespace() {
                end -= 1;
            }
            if end > seg_start {
                spans.push((seg_start, end));
            }
            // Next sentence starts after the whitespace following the run.
            let mut k = j;
            while k < n && bytes[k].is_ascii_whitespace() {
                k += 1;
            }
            seg_start = k;
            i = k;
            continue;
        }
        i = j;
    }
    if seg_start < n {
        let mut end = n;
        while end > seg_start && bytes[end - 1].is_ascii_whitespace() {
            end -= 1;
        }
        if end > seg_start {
            spans.push((seg_start, end));
        }
    }
    spans
}

/// True when a period at byte `dot` should NOT be treated as a sentence end:
/// part of an abbreviation, a decimal number, or a single-letter initial.
/// Only meaningful for `.` (run-leader); `!`/`?` are always real boundaries.
fn suppress_boundary(text: &str, bytes: &[u8], dot: usize, seg_start: usize) -> bool {
    if bytes[dot] != b'.' {
        return false;
    }
    // Decimal: digit immediately before AND after the dot ("3.14").
    let after = bytes.get(dot + 1).copied();
    let before = if dot > 0 { Some(bytes[dot - 1]) } else { None };
    if matches!(before, Some(c) if c.is_ascii_digit())
        && matches!(after, Some(c) if c.is_ascii_digit())
    {
        return true;
    }
    // Token immediately preceding the dot (letters/dots), e.g. "Dr", "e.g".
    let tok_end = dot; // dot is exclusive end of the preceding token
    let mut tok_start = tok_end;
    while tok_start > seg_start {
        let c = bytes[tok_start - 1];
        if c.is_ascii_alphabetic() || c == b'.' {
            tok_start -= 1;
        } else {
            break;
        }
    }
    if tok_start >= tok_end {
        return false;
    }
    let tok = &text[tok_start..tok_end];
    // Single-letter initial: "J." (one uppercase letter standing alone).
    if tok.len() == 1 && tok.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
        return true;
    }
    let lower = tok.to_ascii_lowercase();
    let lower = lower.trim_matches('.');
    ABBREVIATIONS.contains(&lower)
}

/// Coordinating-conjunction / boundary markers a compound sentence may be split
/// at. Each is matched as `", <word> "` (comma + conjunction) or `"; "`.
const CLAUSE_CONJUNCTIONS: &[&str] = &["and", "but", "so", "or", "yet", "because", "while", "whereas"];

/// Minimum bytes for a clause to be worth emitting on its own. Kept low
/// (short facts like "I live in Tokyo" are valuable); the finite-verb guard,
/// not length, is the real quality gate.
const MIN_CLAUSE_BYTES: usize = 12;

/// Split one sentence (byte sub-ranges relative to `text`) at clause boundaries
/// when BOTH resulting sides carry a finite verb — ClausIE-lite. Returns the
/// original single span when no safe split exists (never over-fragments).
///
/// `span` is `(start, end)` into `text`. Returned spans are also into `text`.
pub fn clause_spans(text: &str, span: (usize, usize)) -> Vec<(usize, usize)> {
    let (start, end) = span;
    let sentence = &text[start..end];
    let bytes = sentence.as_bytes();
    let len = bytes.len();
    if len < 2 * MIN_CLAUSE_BYTES {
        return vec![span];
    }

    // Find candidate split points: "; " or ", <conj> ".
    let lower = sentence.to_ascii_lowercase();
    let mut best_split: Option<(usize, usize)> = None; // (left_end, right_start) rel to sentence
    let mut search_from = 0usize;
    while search_from < len {
        // Semicolon boundary.
        if let Some(rel) = lower[search_from..].find("; ") {
            let pos = search_from + rel;
            let left_end = pos; // exclude ';'
            let right_start = pos + 2;
            if clause_ok(sentence, left_end) && clause_ok_rhs(sentence, right_start) {
                best_split = Some((left_end, right_start));
                break;
            }
            search_from = pos + 2;
            continue;
        }
        break;
    }
    if best_split.is_none() {
        // ", <conj> " boundary — pick the first that yields two verbed clauses.
        'outer: for conj in CLAUSE_CONJUNCTIONS {
            let needle = format!(", {conj} ");
            let mut from = 0usize;
            while let Some(rel) = lower[from..].find(&needle) {
                let pos = from + rel;
                let left_end = pos; // exclude the comma
                let right_start = pos + needle.len();
                if clause_ok(sentence, left_end) && clause_ok_rhs(sentence, right_start) {
                    best_split = Some((left_end, right_start));
                    break 'outer;
                }
                from = pos + needle.len();
            }
        }
    }

    match best_split {
        Some((left_end, right_start)) => {
            // Trim whitespace at the inner edges, keep byte-accuracy.
            let mut le = left_end;
            while le > 0 && bytes[le - 1].is_ascii_whitespace() {
                le -= 1;
            }
            let mut rs = right_start;
            while rs < len && bytes[rs].is_ascii_whitespace() {
                rs += 1;
            }
            vec![(start, start + le), (start + rs, end)]
        }
        None => vec![span],
    }
}

/// A left clause is acceptable if it is long enough and has a finite verb.
fn clause_ok(sentence: &str, left_end: usize) -> bool {
    left_end >= MIN_CLAUSE_BYTES && has_finite_verb(&sentence[..left_end])
}
/// A right clause is acceptable if the remainder is long enough and has a verb.
fn clause_ok_rhs(sentence: &str, right_start: usize) -> bool {
    sentence.len().saturating_sub(right_start) >= MIN_CLAUSE_BYTES
        && has_finite_verb(&sentence[right_start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(text: &str, spans: &[(usize, usize)]) -> Vec<String> {
        spans.iter().map(|(s, e)| text[*s..*e].to_string()).collect()
    }

    #[test]
    fn splits_basic_sentences() {
        let t = "I like tea. I dislike coffee. Do you?";
        let got = texts(t, &sentence_spans(t));
        assert_eq!(got, vec!["I like tea.", "I dislike coffee.", "Do you?"]);
    }

    #[test]
    fn does_not_split_on_abbreviation() {
        let t = "Dr. Smith works here. He is kind.";
        let got = texts(t, &sentence_spans(t));
        assert_eq!(got, vec!["Dr. Smith works here.", "He is kind."]);
    }

    #[test]
    fn does_not_split_on_decimal_or_initial() {
        let t = "Pi is 3.14 today. J. K. Rowling writes books.";
        let got = texts(t, &sentence_spans(t));
        assert_eq!(
            got,
            vec!["Pi is 3.14 today.", "J. K. Rowling writes books."]
        );
    }

    #[test]
    fn handles_ellipsis_as_single_terminator() {
        let t = "Well... I think so. Maybe.";
        let got = texts(t, &sentence_spans(t));
        assert_eq!(got, vec!["Well... I think so.", "Maybe."]);
    }

    #[test]
    fn clause_split_on_comma_conjunction() {
        let t = "I live in Tokyo, and I work at Acme.";
        let span = (0, t.len());
        let got = texts(t, &clause_spans(t, span));
        assert_eq!(got, vec!["I live in Tokyo", "I work at Acme."]);
    }

    #[test]
    fn clause_no_split_without_two_verbs() {
        // RHS "aisle and window seats" has no finite verb → keep whole.
        let t = "I prefer aisle and window seats on flights.";
        let span = (0, t.len());
        let got = texts(t, &clause_spans(t, span));
        assert_eq!(got, vec![t.to_string()]);
    }

    #[test]
    fn clause_split_on_semicolon() {
        let t = "She left early; he stayed late at the office.";
        let span = (0, t.len());
        let got = texts(t, &clause_spans(t, span));
        assert_eq!(got, vec!["She left early", "he stayed late at the office."]);
    }
}
