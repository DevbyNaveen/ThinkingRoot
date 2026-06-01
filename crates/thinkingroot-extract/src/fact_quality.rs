//! Mechanical "is this a useful fact?" gate — zero LLM, zero model.
//!
//! The Witness-Mesh structural extractor is deterministic and conservative,
//! but it historically emitted *every* heading and *every* prose segment as a
//! retrievable unit — including hollow fragments like markdown headings
//! ("Additional Tips:", "Best Practices"), bare label lines, and single-token
//! noun phrases. Those fragments carry no domain knowledge yet crowd the
//! vector index and dilute `recall@k` on real questions.
//!
//! This module is the mechanical gate that distinguishes a *fact* (a clause
//! with a finite verb that asserts something) from a *fragment* (a heading,
//! label, or bare noun phrase). It uses only cheap, deterministic signals —
//! terminal punctuation, token count, a finite-verb lexicon with light
//! morphology, and Title-Case detection — so it stays inside the
//! "structural only at compile time, no LLM" contract.
//!
//! Design bias: **high precision on rejection.** A real sentence (has a finite
//! verb AND terminal punctuation) is *never* dropped. Only verbless,
//! unpunctuated, short, or label-shaped spans are rejected, so the gate
//! removes pollution without risking real-fact recall.

/// High-frequency English finite-verb lemmas, copulas, auxiliaries and modals.
/// Membership is checked against the lowercased token AND against a few
/// suffix-stripped stems (see [`has_finite_verb`]). This is intentionally a
/// *finite* curated set rather than a full lexicon: its only job is to confirm
/// "this span contains a verb, therefore it can be a fact." A miss only matters
/// when the span is *also* short/unpunctuated/Title-Case, where the other
/// signals already flag a fragment.
const VERB_LEMMAS: &[&str] = &[
    // copulas / auxiliaries / modals
    "is", "are", "was", "were", "be", "been", "being", "am", "has", "have",
    "had", "do", "does", "did", "done", "doing", "can", "could", "will",
    "would", "shall", "should", "may", "might", "must", "ought", "need",
    // high-frequency content verbs (base form; morphology handled separately)
    "make", "use", "run", "get", "go", "see", "say", "know", "think", "take",
    "come", "want", "give", "find", "tell", "work", "call", "try", "ask",
    "feel", "become", "leave", "put", "mean", "keep", "let", "begin", "seem",
    "help", "show", "hear", "play", "move", "like", "live", "believe", "bring",
    "happen", "write", "provide", "sit", "stand", "lose", "pay", "meet",
    "include", "continue", "set", "learn", "change", "lead", "understand",
    "watch", "follow", "stop", "create", "speak", "read", "allow", "add",
    "spend", "grow", "open", "walk", "win", "offer", "remember", "love",
    "consider", "appear", "buy", "wait", "serve", "die", "send", "expect",
    "build", "stay", "fall", "cut", "reach", "kill", "remain", "suggest",
    "raise", "pass", "sell", "require", "report", "decide", "pull", "return",
    "explain", "hope", "develop", "carry", "break", "receive", "agree",
    "support", "hit", "produce", "eat", "cover", "catch", "draw", "choose",
    "cause", "enable", "contain", "prefer", "enjoy", "describe", "define",
    "store", "compile", "merge", "link", "ground", "extract", "retrieve",
    "depend", "implement", "configure", "install", "deploy", "fix", "fail",
    "improve", "reduce", "increase", "replace", "update", "add", "remove",
    "save", "load", "split", "join", "match", "rank", "filter", "process",
    "want", "wish", "plan", "visit", "travel", "study", "teach", "drive",
    "drink", "sleep", "wake", "feel", "owns", "own", "belong", "consist",
    // High-frequency IRREGULAR past / participle forms that simple suffix
    // stripping cannot derive (e.g. "left" ↛ "leave"). Without these, common
    // narrated facts ("She left early", "I bought a car") read as verbless.
    "left", "went", "made", "took", "came", "got", "saw", "said", "knew",
    "thought", "found", "told", "gave", "felt", "became", "kept", "meant",
    "began", "held", "brought", "wrote", "written", "sat", "stood", "lost",
    "paid", "met", "led", "read", "spoke", "spoken", "won", "bought", "sent",
    "built", "fell", "fallen", "ran", "sold", "drew", "drawn", "chose",
    "chosen", "ate", "eaten", "drove", "driven", "drank", "drunk", "slept",
    "woke", "woken", "wore", "worn", "broke", "broken", "spent", "understood",
    "heard", "gone", "done", "seen", "taken", "given", "known", "grown",
    "flew", "flown", "threw", "thrown", "bit", "hid", "rode", "rose", "shot",
    "shut", "split", "spread", "swore", "told", "won", "moved", "lived",
];

/// Common verb suffixes stripped before a lemma lookup (light morphology).
/// Order matters — longest first so "running" → "runn" → fallback "run".
fn verb_stem_matches(token: &str) -> bool {
    if VERB_LEMMAS.contains(&token) {
        return true;
    }
    // Past / gerund / 3rd-person inflections → base form.
    // "prefers"→"prefer", "preferred"→"prefer", "preferring"→"prefer",
    // "uses"→"use", "stored"→"store", "running"→"run" (double consonant).
    let candidates: [Option<String>; 5] = [
        token.strip_suffix("ing").map(|s| s.to_string()),
        token.strip_suffix("ed").map(|s| s.to_string()),
        token.strip_suffix("es").map(|s| s.to_string()),
        token.strip_suffix('s').map(|s| s.to_string()),
        // doubled-consonant gerund/past: "running"→"run", "preferred"→"prefer"
        token
            .strip_suffix("ing")
            .or_else(|| token.strip_suffix("ed"))
            .and_then(|s| {
                let b = s.as_bytes();
                if b.len() >= 2 && b[b.len() - 1] == b[b.len() - 2] {
                    Some(s[..s.len() - 1].to_string())
                } else {
                    None
                }
            }),
    ];
    for c in candidates.into_iter().flatten() {
        if c.len() >= 2 && (VERB_LEMMAS.contains(&c.as_str()) || {
            // "stored"→"stor"→add 'e'→"store"
            let with_e = format!("{c}e");
            VERB_LEMMAS.contains(&with_e.as_str())
        }) {
            return true;
        }
    }
    false
}

/// Returns true if `text` contains at least one token that looks like a finite
/// verb (copula, auxiliary, modal, or content verb / inflected form).
pub fn has_finite_verb(text: &str) -> bool {
    text.split(|c: char| !c.is_alphabetic() && c != '\'')
        .filter(|t| !t.is_empty())
        .any(|raw| {
            let lower = raw.to_ascii_lowercase();
            verb_stem_matches(&lower)
        })
}

/// Function words that may stay lowercase inside an otherwise Title-Cased
/// heading ("Best Practices **for** Caching"). Used only to avoid
/// mis-classifying such headings as sentences.
const TITLE_FUNCTION_WORDS: &[&str] = &[
    "a", "an", "the", "of", "in", "on", "for", "to", "and", "or", "but",
    "with", "at", "by", "from", "as", "into", "over", "per", "via",
];

/// True when most alphabetic tokens are Title-Cased (heading style).
fn is_title_case_heading(tokens: &[&str]) -> bool {
    let mut alpha = 0usize;
    let mut capish = 0usize;
    for tok in tokens {
        let first = tok.chars().find(|c| c.is_alphabetic());
        let Some(first) = first else { continue };
        alpha += 1;
        let lower = tok.to_ascii_lowercase();
        if first.is_uppercase() || TITLE_FUNCTION_WORDS.contains(&lower.as_str()) {
            capish += 1;
        }
    }
    // Need at least 2 alphabetic tokens and ≥80% Title-Cased to call it a heading.
    alpha >= 2 && (capish * 5) >= (alpha * 4)
}

/// Strip surrounding markdown emphasis / list markers so the gate analyses the
/// underlying text ("**Additional Tips:**" → "Additional Tips:").
fn strip_markdown(text: &str) -> &str {
    text.trim()
        .trim_matches(|c: char| {
            c == '*' || c == '#' || c == '_' || c == '`' || c == '~'
                || c == '-' || c == '>' || c == ' '
        })
        .trim()
}

/// Returns `true` when `text` is a low-value structural fragment that should
/// NOT be surfaced as a standalone retrievable claim — a heading, a "Label:"
/// line, or a bare noun phrase. Conservative: anything that reads like a real
/// sentence (finite verb + terminal punctuation) is never flagged.
pub fn is_low_value_fragment(text: &str) -> bool {
    let core = strip_markdown(text);
    if core.is_empty() {
        return true;
    }

    let tokens: Vec<&str> = core.split_whitespace().collect();
    let n = tokens.len();
    let last = core.chars().next_back().unwrap_or(' ');
    let has_terminal = matches!(last, '.' | '!' | '?');
    let has_verb = has_finite_verb(core);

    // 1. Label / section line ending in ':' that is not itself a sentence.
    //    "Additional Tips:", "Best Practices:", "Meditation and sleep:".
    if core.ends_with(':') && !has_terminal {
        return true;
    }
    // 2. Bare single token with no sentence punctuation ("Introduction").
    if n <= 1 && !has_terminal {
        return true;
    }
    // 3. Short span with no finite verb and no terminal punctuation — a
    //    noun-phrase heading ("Best Practices", "Caching Strategies").
    if !has_verb && n <= 6 && !has_terminal {
        return true;
    }
    // 4. Title-Cased heading with no finite verb and no terminal punctuation,
    //    regardless of length ("Best Practices For Visualizing High-Dim Data").
    if !has_terminal && !has_verb && is_title_case_heading(&tokens) {
        return true;
    }
    false
}

/// Inverse of [`is_low_value_fragment`] — `true` when `text` looks like a real,
/// retrievable fact worth indexing as a claim.
pub fn is_useful_fact(text: &str) -> bool {
    !is_low_value_fragment(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_label_lines() {
        assert!(is_low_value_fragment("Additional Tips:"));
        assert!(is_low_value_fragment("**Best Practices:**"));
        assert!(is_low_value_fragment("Meditation and sleep:"));
        assert!(is_low_value_fragment("## Setup:"));
    }

    #[test]
    fn rejects_bare_headings() {
        assert!(is_low_value_fragment("Introduction"));
        assert!(is_low_value_fragment("# Best Practices"));
        assert!(is_low_value_fragment("Caching Strategies"));
        assert!(is_low_value_fragment("High-Dimensional Data Visualization"));
    }

    #[test]
    fn keeps_real_facts() {
        assert!(is_useful_fact("The user prefers aisle seats."));
        assert!(is_useful_fact("I will be in Tokyo next March."));
        assert!(is_useful_fact("Bell is a telecommunication company."));
        assert!(is_useful_fact("My dog is named Rex."));
        assert!(is_useful_fact("We migrated the database to Postgres."));
        // Question-form facts are real content.
        assert!(is_useful_fact("What time does the meeting start?"));
    }

    #[test]
    fn keeps_verb_bearing_heading_like_lines() {
        // A heading that is actually a statement should survive.
        assert!(is_useful_fact("Streaming branches ship in the OSS engine."));
    }

    #[test]
    fn finite_verb_detection() {
        assert!(has_finite_verb("the user prefers tea"));
        assert!(has_finite_verb("she stored the file"));
        assert!(has_finite_verb("they are running"));
        assert!(has_finite_verb("it contains data"));
        assert!(!has_finite_verb("additional tips"));
        assert!(!has_finite_verb("best practices"));
    }
}
