// crates/thinkingroot-serve/src/intelligence/sandbox_classifier.rs
//
// Sandbox-by-default risk classifier (Task 17, plan 2026-05-09).
//
// Reads the user's question and returns a `SandboxIntent` describing
// whether the agent should fork an Ephemeral sandbox branch before any
// write. The signal flows into the reactive `<system-reminder>` bus
// (intelligence/reminder_bus.rs::render_sandbox_state_block) so the
// model sees an ambient "we recommend forking before writing" hint.
//
// The classifier never enforces a fork — it only nudges. The model
// decides whether to actually call `create_branch` based on the
// system prompt's guidance. Two reasons we don't auto-fork:
//
//   1. The prompt-level guidance is reversible: a future user can
//      override "no, change main directly" without us having to
//      escape an enforced gate. An auto-fork would either need a
//      separate "no really, edit main" tool or break that affordance.
//
//   2. Auto-forking on read-only questions ("how does X work?") would
//      pollute the branch list. The classifier's confidence floor
//      keeps it conservative — only fires on clear write-intent
//      signals like "refactor", "fix", "migrate", "change".
//
// The classifier is **pure** — no I/O, no async — so it's trivially
// unit-testable and runs in <1µs per query (a handful of regex-style
// substring matches against a normalised lowercased query).

/// What the classifier recommends. The reminder bus emits an ambient
/// hint when this is `RecommendSandbox`; otherwise the bus stays silent
/// on the topic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxIntent {
    /// Question is read-only or unclear. The agent should answer
    /// without forking; the bus emits no sandbox-related reminder.
    NoAction,
    /// Question has clear write intent. The bus emits a
    /// `<sandbox_alert>` block recommending the agent open an
    /// Ephemeral sandbox before any contribution.
    RecommendSandbox {
        /// One short phrase the bus surfaces verbatim. Useful for the
        /// trust-receipt UI's hover tooltip and for telemetry on
        /// classifier-trigger frequency.
        reason: &'static str,
    },
}

/// Classify a user question for sandbox-fork intent. Pure function;
/// always returns deterministically given the same input.
///
/// The signal hierarchy (most-confident first):
///
///   * **Imperative write verbs** — "refactor", "rewrite", "migrate",
///     "rename", "delete", "remove", "drop". These are unambiguous
///     change-intent words; we recommend a sandbox even when the
///     object is unstated ("refactor this" → sandbox).
///
///   * **Imperative fix verbs** — "fix the …", "patch the …",
///     "correct the …". Less direct than refactor but still
///     write-intent in practice.
///
///   * **"Change" + object** — "change the foo", "update the bar".
///     The bare verb "change" alone is too soft to fire (e.g.
///     "what would change if?" is a hypothetical question, not a
///     directive); we require a definite-article-led object phrase.
///
/// Read-only signals always win when ambiguous: a question containing
/// both "explain" and "fix" prefers the explanatory reading and stays
/// `NoAction`. The bias is to under-recommend rather than
/// over-recommend.
pub fn classify(question: &str) -> SandboxIntent {
    let lower = question.to_lowercase();
    let lower = lower.trim();

    // Read-only beat — if any of these are present, the question is
    // explanatory and the classifier shouldn't fire even if a write
    // verb appears later in the same sentence.
    const READ_ONLY_BEATS: &[&str] = &[
        "explain ",
        "what is ",
        "what are ",
        "how does ",
        "how do ",
        "why does ",
        "why do ",
        "show me ",
        "tell me ",
        "describe ",
        "summarize ",
        "summarise ",
        "list ",
    ];
    for marker in READ_ONLY_BEATS {
        if lower.starts_with(marker) || lower.contains(&format!(" {marker}")) {
            return SandboxIntent::NoAction;
        }
    }

    // Imperative write verbs — fire unconditionally.
    const HARD_WRITE_VERBS: &[(&str, &str)] = &[
        ("refactor", "refactor intent"),
        ("rewrite", "rewrite intent"),
        ("migrate", "migration intent"),
        ("rename", "rename intent"),
        ("delete", "delete intent"),
        ("remove", "remove intent"),
        ("drop", "drop intent"),
    ];
    for (verb, reason) in HARD_WRITE_VERBS {
        if word_present(&lower, verb) {
            return SandboxIntent::RecommendSandbox { reason };
        }
    }

    // Fix-class verbs — require an object to fire (avoids matching
    // "fix that" as a meta-comment about the assistant's last reply).
    const FIX_VERBS: &[&str] = &["fix the", "patch the", "correct the", "repair the"];
    for marker in FIX_VERBS {
        if lower.contains(marker) {
            return SandboxIntent::RecommendSandbox {
                reason: "fix intent",
            };
        }
    }

    // "Change" / "update" — require definite-article-led object
    // phrase to avoid soft hypotheticals.
    const SOFT_CHANGE_VERBS: &[&str] = &[
        "change the",
        "update the",
        "modify the",
        "edit the",
        "replace the",
    ];
    for marker in SOFT_CHANGE_VERBS {
        if lower.contains(marker) {
            return SandboxIntent::RecommendSandbox {
                reason: "modify intent",
            };
        }
    }

    SandboxIntent::NoAction
}

/// True when `needle` appears in `haystack` flanked by ASCII
/// non-alphanumerics (start-of-string + end-of-string count). Avoids
/// false-positives like "removeAll" matching `remove` or "drophead"
/// matching `drop`.
fn word_present(haystack: &str, needle: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let abs = start + pos;
        let end = abs + needle.len();
        let before_ok = abs == 0
            || haystack
                .as_bytes()
                .get(abs - 1)
                .map(|b| !b.is_ascii_alphanumeric())
                .unwrap_or(true);
        let after_ok = end == haystack.len()
            || haystack
                .as_bytes()
                .get(end)
                .map(|b| !b.is_ascii_alphanumeric())
                .unwrap_or(true);
        if before_ok && after_ok {
            return true;
        }
        start = end;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recommends(q: &str) -> bool {
        matches!(classify(q), SandboxIntent::RecommendSandbox { .. })
    }

    #[test]
    fn empty_question_does_not_fire() {
        assert_eq!(classify(""), SandboxIntent::NoAction);
    }

    #[test]
    fn explanatory_question_does_not_fire() {
        assert!(!recommends("explain how the auth flow works"));
        assert!(!recommends("how does the cache reload?"));
        assert!(!recommends("what is the rooting tier?"));
        assert!(!recommends("show me the open contradictions"));
    }

    #[test]
    fn refactor_question_recommends_sandbox() {
        let v = classify("refactor the webhook handler to use async/await");
        match v {
            SandboxIntent::RecommendSandbox { reason } => {
                assert_eq!(reason, "refactor intent");
            }
            other => panic!("expected refactor recommendation, got {other:?}"),
        }
    }

    #[test]
    fn migrate_question_recommends_sandbox() {
        assert!(recommends("migrate this from JWT to OAuth"));
    }

    #[test]
    fn rewrite_question_recommends_sandbox() {
        assert!(recommends("rewrite the auth middleware in Rust"));
    }

    #[test]
    fn rename_question_recommends_sandbox() {
        assert!(recommends("rename WebhookHandler → EventHandler"));
    }

    #[test]
    fn delete_remove_drop_each_fire() {
        assert!(recommends("delete the legacy fallback"));
        assert!(recommends("remove the unused middleware"));
        assert!(recommends("drop the deprecated table"));
    }

    #[test]
    fn fix_intent_with_object_recommends_sandbox() {
        assert!(recommends("fix the stripe bug"));
        assert!(recommends("patch the race condition"));
        assert!(recommends("correct the off-by-one in pagination"));
    }

    #[test]
    fn fix_without_object_does_not_fire() {
        // "fix that" with no object would be a meta-comment to the
        // assistant. Conservative classifier stays silent.
        assert!(!recommends("can you fix that"));
    }

    #[test]
    fn change_with_definite_article_fires() {
        assert!(recommends("change the database adapter to postgres"));
        assert!(recommends("update the auth provider"));
        assert!(recommends("modify the webhook timeout"));
    }

    #[test]
    fn change_alone_does_not_fire() {
        assert!(!recommends("what would change if we used redis?"));
        assert!(!recommends("describe the change"));
    }

    #[test]
    fn read_only_beat_wins_over_write_verb() {
        // "explain how to refactor" is asking for guidance, not
        // requesting a refactor. Read-only beat wins.
        assert!(!recommends("explain how to refactor the cache layer"));
        assert!(!recommends("describe the refactor that just shipped"));
    }

    #[test]
    fn word_boundary_prevents_substring_false_positives() {
        // "removeAll" is a method name in JS — not a write directive.
        // The classifier treats it as a code reference, not intent.
        assert!(!recommends("what does removeAll do?"));
        // "drophead" + similar concatenations don't trigger.
        assert!(!recommends("how does drophead work?"));
    }

    #[test]
    fn case_insensitive_matching() {
        assert!(recommends("REFACTOR the auth module"));
        assert!(recommends("Migrate from sqlite to postgres"));
    }

    #[test]
    fn classifier_is_deterministic() {
        let q = "refactor the webhook handler";
        let a = classify(q);
        let b = classify(q);
        assert_eq!(a, b);
    }
}
