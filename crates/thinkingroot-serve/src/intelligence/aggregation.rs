// Aggregation route (§3 #4) — count/list questions answered by an EXACT
// Datalog aggregate over the graph organ, not a probabilistic read of a few
// retrieved snippets. "How many X" is the category every vector-only memory
// system is worst at (~83% in LongMemEval) because top-k retrieval structurally
// cannot see rows it didn't rank; our graph counts them all exactly.
//
// This module is the (pure, sub-µs, unit-tested) front half: classify the
// intent and extract the subject keyword. The exact count itself lives in
// `graph::aggregate_claims_for_keyword`; the wiring is in `synthesizer::ask`,
// gated on `TR_AGGREGATION_ROUTE`. Honest by construction: when the subject
// resolves to no entity we return None and the normal reader path runs — we
// never fabricate a count.

/// What kind of aggregate the question asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregationKind {
    /// "how many…", "number of…" → an exact integer.
    Count,
    /// "list all…", "what are all…" → the full enumerated set.
    List,
    /// "how much did I spend…", "total hours…" → an exact per-unit total
    /// over the `quantities` table (memory-SOTA Phase 5).
    Sum,
}

/// Leading/embedded phrases that signal a count question. Ordered longest-first
/// so the most specific trigger is stripped when extracting the subject.
const COUNT_TRIGGERS: &[&str] = &[
    "how many",
    "how much",
    "what is the number of",
    "what's the number of",
    "total number of",
    "the number of",
    "number of",
    "count of",
    "count the",
    "count all",
    "how often",
];

/// Phrases that signal a quantity-SUM question. Checked BEFORE the count
/// triggers because "how much did I spend" / "how many hours" are totals
/// over parsed quantities, not row counts. Deliberately explicit — a
/// mis-routed Sum falls through honestly (no quantities → normal reader),
/// but the phrasing set still errs narrow.
const SUM_TRIGGERS: &[&str] = &[
    "how much did",
    "how much money",
    "how much time",
    "how much have i spent",
    "how many hours",
    "how many minutes",
    "how many days did",
    "total cost",
    "total amount",
    "total money",
    "total time",
    "total hours",
    "total spent",
    "in total",
    "altogether",
    "sum of",
];

/// Phrases that signal a list/enumeration question.
const LIST_TRIGGERS: &[&str] = &[
    "list all",
    "list every",
    "list the",
    "list out",
    "show me all",
    "show all",
    "give me all",
    "what are all",
    "which all",
    "enumerate the",
    "enumerate all",
    "enumerate",
];

/// Classify a query as a count/list aggregate, or `None` for a normal question.
/// Case-insensitive, whitespace-tolerant; matches a trigger anywhere in the
/// first clause so "Quickly, how many projects?" still routes. Count is checked
/// before List only matters when both could match — they are disjoint in
/// practice. Sub-microsecond: a handful of substring scans.
pub fn classify_aggregation(query: &str) -> Option<AggregationKind> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return None;
    }
    // Sum first: "how much did I spend" / "how many hours" would otherwise
    // false-match the broader count triggers ("how much", "how many").
    if SUM_TRIGGERS.iter().any(|t| q.contains(t)) {
        return Some(AggregationKind::Sum);
    }
    if COUNT_TRIGGERS.iter().any(|t| q.contains(t)) {
        return Some(AggregationKind::Count);
    }
    if LIST_TRIGGERS.iter().any(|t| q.contains(t)) {
        return Some(AggregationKind::List);
    }
    None
}

/// Function words stripped from the subject so "how many projects do I have"
/// reduces to "projects". Conservative: only truly generic glue, never
/// content nouns.
const SUBJECT_STOPWORDS: &[&str] = &[
    "do", "does", "did", "i", "we", "you", "have", "has", "had", "are", "is",
    "was", "were", "there", "my", "our", "your", "the", "a", "an", "of", "in",
    "on", "at", "to", "for", "with", "about", "that", "this", "these", "those",
    "all", "any", "total", "currently", "now", "so", "far", "right", "ever",
    "me", "us", "and", "or",
];

/// Extract the subject keyword to resolve against the entity graph. Strips the
/// matched trigger phrase and surrounding glue words, returns the remaining
/// content words joined by a space (lowercased). Returns `None` when nothing
/// contentful remains (e.g. "how many?") — the caller then falls through to the
/// normal reader rather than counting "everything".
pub fn extract_subject(query: &str, kind: AggregationKind) -> Option<String> {
    let mut q = query.trim().to_lowercase();
    // Drop trailing punctuation that would cling to the last word.
    q = q
        .trim_end_matches(|c: char| matches!(c, '?' | '.' | '!' | ',' | ';' | ':'))
        .to_string();

    let triggers = match kind {
        AggregationKind::Count => COUNT_TRIGGERS,
        AggregationKind::List => LIST_TRIGGERS,
        AggregationKind::Sum => SUM_TRIGGERS,
    };
    // Remove the (longest matching) trigger phrase.
    if let Some(t) = triggers.iter().find(|t| q.contains(**t)) {
        q = q.replacen(t, " ", 1);
    }

    let subject: Vec<&str> = q
        .split_whitespace()
        .filter(|w| {
            let w = w.trim_matches(|c: char| !c.is_alphanumeric());
            !w.is_empty() && !SUBJECT_STOPWORDS.contains(&w)
        })
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| !w.is_empty())
        .collect();

    if subject.is_empty() {
        return None;
    }
    Some(subject.join(" "))
}

/// Canonical dedup key for COUNT DISTINCT (memory-SOTA Phase 3): the same
/// real-world item restated across sessions ("Bought a black jacket." /
/// "bought the black jacket") must count ONCE. Lowercases, strips punctuation
/// and pure-glue articles, keeps content words IN ORDER.
///
/// Deliberately conservative: different verbs / different objects stay
/// distinct keys. Under-dedup degrades to today's behaviour (a duplicate
/// counted twice); over-dedup would FABRICATE a lower count — the worse
/// failure, so we never sort or stem.
pub fn canonical_statement_key(statement: &str) -> String {
    // Articles + trivial glue only. NOT the full SUBJECT_STOPWORDS list —
    // "I" vs "we", "my" vs "your" can distinguish real items.
    const GLUE: &[&str] = &["a", "an", "the"];
    statement
        .to_lowercase()
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| !w.is_empty() && !GLUE.contains(w))
        .collect::<Vec<_>>()
        .join(" ")
}

/// COUNT DISTINCT over `(claim_id, statement)` rows: collapse rows sharing a
/// canonical statement key, keeping the first row of each group. Returns the
/// deduplicated rows in input order — `out.len()` is the distinct-item count.
pub fn dedup_by_canonical_key(claims: &[(String, String)]) -> Vec<(String, String)> {
    let mut seen = std::collections::HashSet::new();
    claims
        .iter()
        .filter(|(_, statement)| seen.insert(canonical_statement_key(statement)))
        .cloned()
        .collect()
}

/// One per-unit total from [`sum_quantities_by_unit`].
#[derive(Debug, Clone, PartialEq)]
pub struct UnitTotal {
    /// Normalized unit key ("usd", "hour", "km"…).
    pub unit: String,
    pub total: f64,
    /// How many quantity rows contributed.
    pub rows: usize,
}

/// Normalize a stored unit for sum-grouping: lowercase, and merge the
/// singular/plural forms of the extractor's word-units ("hours"/"hour").
/// Symbol units ("%", "ms", "usd") pass through lowercased.
fn unit_key(unit: &str) -> String {
    let u = unit.trim().to_lowercase();
    const WORDY: &[&str] = &["second", "minute", "hour", "day", "user", "req", "core"];
    if let Some(stem) = u.strip_suffix('s') {
        if WORDY.contains(&stem) {
            return stem.to_string();
        }
    }
    u
}

/// SUM by unit (memory-SOTA Phase 5): total `(value, unit)` quantity rows
/// per normalized unit. NEVER sums across units — "3 hours + 40 USD" stays
/// two totals; collapsing them would be a fabricated number. Output sorted
/// by row-count descending (dominant unit first), then unit for stability.
pub fn sum_quantities_by_unit(rows: &[(f64, String)]) -> Vec<UnitTotal> {
    let mut acc: std::collections::BTreeMap<String, (f64, usize)> =
        std::collections::BTreeMap::new();
    for (value, unit) in rows {
        let key = unit_key(unit);
        if key.is_empty() {
            continue;
        }
        let e = acc.entry(key).or_insert((0.0, 0));
        e.0 += value;
        e.1 += 1;
    }
    let mut out: Vec<UnitTotal> = acc
        .into_iter()
        .map(|(unit, (total, rows))| UnitTotal { unit, total, rows })
        .collect();
    out.sort_by(|a, b| b.rows.cmp(&a.rows).then(a.unit.cmp(&b.unit)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_count_questions() {
        for q in [
            "how many projects do I have?",
            "How much memory have I stored about Alice",
            "what is the number of open tasks",
            "total number of meetings last week",
            "count all the bugs",
        ] {
            assert_eq!(classify_aggregation(q), Some(AggregationKind::Count), "{q}");
        }
    }

    #[test]
    fn classifies_list_questions() {
        for q in [
            "list all my projects",
            "what are all the services I deployed",
            "show me all the people I met",
            "enumerate the open issues",
        ] {
            assert_eq!(classify_aggregation(q), Some(AggregationKind::List), "{q}");
        }
    }

    #[test]
    fn normal_questions_do_not_route() {
        for q in [
            "what is the capital of France",
            "who is Alice",
            "summarize the meeting",
            "why did the build fail",
            "",
        ] {
            assert_eq!(classify_aggregation(q), None, "{q}");
        }
    }

    #[test]
    fn extracts_the_subject_keyword() {
        assert_eq!(
            extract_subject("how many projects do I have?", AggregationKind::Count).as_deref(),
            Some("projects")
        );
        assert_eq!(
            extract_subject("list all the services I deployed", AggregationKind::List).as_deref(),
            Some("services deployed")
        );
        assert_eq!(
            extract_subject(
                "how many memories about Alice are there",
                AggregationKind::Count
            )
            .as_deref(),
            Some("memories alice")
        );
    }

    #[test]
    fn subjectless_aggregate_returns_none() {
        // "how many?" with no subject must not be turned into "count everything".
        assert_eq!(extract_subject("how many?", AggregationKind::Count), None);
        assert_eq!(extract_subject("list all", AggregationKind::List), None);
    }

    #[test]
    fn classifies_sum_questions_before_count() {
        for q in [
            "how much did I spend on bikes?",
            "how many hours did I drive last month",
            "what's the total cost of my trips",
            "how much money went to subscriptions",
            "total hours spent studying",
        ] {
            assert_eq!(classify_aggregation(q), Some(AggregationKind::Sum), "{q}");
        }
        // The pre-existing count phrasings must NOT reroute to Sum.
        assert_eq!(
            classify_aggregation("How much memory have I stored about Alice"),
            Some(AggregationKind::Count)
        );
        assert_eq!(
            classify_aggregation("total number of meetings last week"),
            Some(AggregationKind::Count)
        );
    }

    #[test]
    fn sum_groups_by_unit_never_across() {
        let rows = vec![
            (120.0, "USD".to_string()),
            (80.5, "usd".to_string()),   // case-merged
            (3.0, "hours".to_string()),
            (2.0, "hour".to_string()),   // plural-merged
        ];
        let totals = sum_quantities_by_unit(&rows);
        assert_eq!(totals.len(), 2, "USD and hours never collapse: {totals:?}");
        let usd = totals.iter().find(|t| t.unit == "usd").unwrap();
        assert!((usd.total - 200.5).abs() < 1e-9);
        assert_eq!(usd.rows, 2);
        let hours = totals.iter().find(|t| t.unit == "hour").unwrap();
        assert!((hours.total - 5.0).abs() < 1e-9);
        // Empty input → empty output (honest: no fabricated zero-total).
        assert!(sum_quantities_by_unit(&[]).is_empty());
    }

    #[test]
    fn canonical_key_collapses_restatements_keeps_distinct_items() {
        // Same item restated (article/punct/case noise) → same key.
        assert_eq!(
            canonical_statement_key("Bought a black jacket."),
            canonical_statement_key("bought the black jacket")
        );
        // Different object → distinct key (never over-collapse).
        assert_ne!(
            canonical_statement_key("bought a black jacket"),
            canonical_statement_key("bought a blue jacket")
        );
        // Different verb → distinct key (conservative: no stemming/synonyms).
        assert_ne!(
            canonical_statement_key("bought a jacket"),
            canonical_statement_key("returned a jacket")
        );
        // Word order preserved — never sorted.
        assert_ne!(
            canonical_statement_key("alice emailed bob"),
            canonical_statement_key("bob emailed alice")
        );
    }

    #[test]
    fn dedup_by_canonical_key_counts_distinct_items() {
        let claims = vec![
            ("c1".to_string(), "Bought a black jacket.".to_string()),
            ("c2".to_string(), "bought the black jacket".to_string()), // dup of c1
            ("c3".to_string(), "bought a blue scarf".to_string()),
            ("c4".to_string(), "returned the blue scarf".to_string()),
        ];
        let distinct = dedup_by_canonical_key(&claims);
        assert_eq!(distinct.len(), 3, "off-by-1 fixed: dup collapses");
        // First occurrence wins; input order preserved.
        assert_eq!(distinct[0].0, "c1");
        assert_eq!(distinct[1].0, "c3");
        assert_eq!(distinct[2].0, "c4");
    }
}
