//! Mechanical supersession-key extraction — zero LLM.
//!
//! Knowledge-update questions ("where do I live *now*?") need an older fact to
//! be marked superseded when a newer fact about the **same subject + attribute**
//! arrives with a **different value**. Detecting that in general requires an LLM
//! (Zep/Mem0 do) — rephrased contradictions are unbounded. This module does NOT
//! attempt the general case; it extracts a key ONLY for a small set of
//! **high-precision, unambiguous** first-person update patterns, so the caller
//! can supersede the clear cases and never wrongly invalidate a still-true fact.
//!
//! Two claims supersede iff `supersession_key().0` (subject|attribute) matches
//! and `.1` (the value) differs. Everything outside these patterns returns
//! `None` (no supersession) — the safe default. The remaining "current vs past"
//! resolution is handled at query time by recency ranking + temporal anchoring +
//! the LLM reader, so missing a supersession edge degrades gracefully.

/// Split a leading `"User: "` / `"Assistant: "` speaker prefix off a statement.
/// Returns `(subject, rest)` where subject is `"user"`, `"assistant"`, or `""`.
fn split_speaker(statement: &str) -> (&'static str, &str) {
    if let Some(r) = statement.strip_prefix("User: ") {
        ("user", r)
    } else if let Some(r) = statement.strip_prefix("Assistant: ") {
        ("assistant", r)
    } else {
        ("", statement)
    }
}

/// Lowercase, trim, strip leading articles, drop "now"/"currently" fillers, and
/// collapse internal whitespace — so "a Tesla" and "now a tesla" normalise equal.
fn normalize(s: &str) -> String {
    let mut t = s.trim().to_ascii_lowercase();
    for filler in ["now ", "currently ", "actually ", "still "] {
        if let Some(r) = t.strip_prefix(filler) {
            t = r.to_string();
        }
    }
    for article in ["a ", "an ", "the ", "my "] {
        if let Some(r) = t.strip_prefix(article) {
            t = r.to_string();
        }
    }
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// First-person verb/preposition patterns mapped to a stable attribute key.
/// Each entry is `(lowercased prefix, attribute)`. Different surface verbs that
/// mean the same attribute collapse to one key ("i live in" / "i moved to" →
/// `live_in`) so a relocation supersedes the old residence.
const I_PATTERNS: &[(&str, &str)] = &[
    ("i live in ", "live_in"),
    ("i am in ", "live_in"),
    ("i moved to ", "live_in"),
    ("i relocated to ", "live_in"),
    ("i now live in ", "live_in"),
    ("i work at ", "work_at"),
    ("i work for ", "work_at"),
    ("i now work at ", "work_at"),
    ("i joined ", "work_at"),
    ("i am a ", "occupation"),
    ("i am an ", "occupation"),
    ("i'm a ", "occupation"),
    ("i'm an ", "occupation"),
    ("i became a ", "occupation"),
    ("i use ", "primary_tool"),
    ("i drive a ", "car"),
    ("i drive ", "car"),
];

/// Extract a `(subject|attribute, value)` supersession key from a statement, or
/// `None` when the statement is not a clear, supersedable first-person update.
///
/// Recognised, high-precision forms (after an optional speaker prefix):
/// - `"my <attr> is|are [now] <value>"` → key `"{subj}|my:{attr}"`
/// - `"I live in / moved to <value>"` → key `"{subj}|live_in"`
/// - `"I work at / for <value>"`, `"I am a <value>"`, `"I drive a <value>"`, …
pub fn supersession_key(statement: &str) -> Option<(String, String)> {
    let (subject, rest) = split_speaker(statement);
    let lower = rest
        .trim()
        .trim_end_matches(|c| c == '.' || c == '!' || c == '?')
        .trim()
        .to_ascii_lowercase();

    // "my <attr> is|are [now] <value>"
    if let Some(after_my) = lower.strip_prefix("my ") {
        for sep in [" is ", " are "] {
            if let Some(idx) = after_my.find(sep) {
                let attr = normalize(&after_my[..idx]);
                let value = normalize(&after_my[idx + sep.len()..]);
                if !attr.is_empty() && !value.is_empty() {
                    return Some((format!("{subject}|my:{attr}"), value));
                }
            }
        }
    }

    // First-person verb/preposition patterns.
    for (prefix, attr) in I_PATTERNS {
        if let Some(value) = lower.strip_prefix(prefix) {
            let value = normalize(value);
            if !value.is_empty() {
                return Some((format!("{subject}|{attr}"), value));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn my_attribute_reassignment_shares_key() {
        let a = supersession_key("User: My car is a Toyota.").unwrap();
        let b = supersession_key("User: My car is now a Tesla.").unwrap();
        assert_eq!(a.0, b.0, "same subject+attribute key");
        assert_ne!(a.1, b.1, "different value");
        assert_eq!(a.0, "user|my:car");
        assert_eq!(a.1, "toyota");
        assert_eq!(b.1, "tesla");
    }

    #[test]
    fn relocation_collapses_to_live_in() {
        let a = supersession_key("User: I live in Tokyo.").unwrap();
        let b = supersession_key("User: I moved to Berlin.").unwrap();
        assert_eq!(a.0, b.0);
        assert_eq!(a.0, "user|live_in");
        assert_eq!(a.1, "tokyo");
        assert_eq!(b.1, "berlin");
    }

    #[test]
    fn speaker_scopes_the_key() {
        let u = supersession_key("User: I work at Acme.").unwrap();
        let a = supersession_key("Assistant: I work at Acme.").unwrap();
        assert_ne!(u.0, a.0, "different speakers must not supersede each other");
    }

    #[test]
    fn non_update_facts_return_none() {
        assert!(supersession_key("User: I prefer aisle seats.").is_none());
        assert!(supersession_key("Assistant: The flight departs at noon.").is_none());
        assert!(supersession_key("User: I had a great day.").is_none());
        assert!(supersession_key("Q: hi → A: hello").is_none());
    }
}
