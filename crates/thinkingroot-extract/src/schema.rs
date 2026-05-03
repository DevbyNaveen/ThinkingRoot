use serde::{Deserialize, Serialize};
pub use thinkingroot_core::types::{ExpirationSignal, ExtractionTier, RecurringPattern, Sensitivity};

/// The structured output schema that the LLM must return.
/// This is what we parse from the LLM response for each chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionResult {
    #[serde(default)]
    pub claims: Vec<ExtractedClaim>,
    #[serde(default)]
    pub entities: Vec<ExtractedEntity>,
    #[serde(default)]
    pub relations: Vec<ExtractedRelation>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtractedClaim {
    pub statement: String,
    pub claim_type: String,
    pub confidence: f64,
    pub entities: Vec<String>,
    /// POSIX path of the source file inside the workspace / pack source
    /// bundle. Required by the v3 wire format for `claims.jsonl` per spec
    /// §3.3 (serialized as `"file"`). Empty string when the parser has not
    /// been upgraded to populate it yet — the downstream pipeline backfills
    /// from the chunk's source_uri at insert time.
    #[serde(default, rename = "file")]
    pub source_path: String,
    /// Byte offset (inclusive) within `source_path`. v3 wire field `"start"`
    /// per spec §3.3. Tree-sitter parsers populate from `node.byte_range()`;
    /// LLM extractor emits relative to the chunk's authoritative origin.
    #[serde(default, rename = "start")]
    pub byte_start: u64,
    /// Byte offset (exclusive) within `source_path`. v3 wire field `"end"`
    /// per spec §3.3.
    #[serde(default, rename = "end")]
    pub byte_end: u64,
    /// Verbatim source text that justifies this claim. Pre-v3 the only
    /// citation mechanism; under v3 still emitted by structural and LLM
    /// extractors as a debug aid but the authoritative citation is the
    /// `(source_path, byte_start, byte_end)` triple above. Removable once
    /// every consumer has migrated to the byte-range contract.
    #[serde(default)]
    pub source_quote: Option<String>,
    #[serde(default)]
    pub extraction_tier: ExtractionTier,
    /// ISO date (YYYY-MM-DD) of when the described event actually occurred.
    /// Null/absent when the claim has no specific associated event date.
    #[serde(default)]
    pub event_date: Option<String>,
    /// Optional executable predicate attached by the LLM at extraction time.
    /// When present, the Rooting gate re-executes it against the original
    /// source bytes before admission (Phase 6.5) and periodically thereafter.
    /// Null/absent when the LLM cannot generate an unambiguous predicate —
    /// the claim stays in the `Attested` tier rather than being quarantined.
    #[serde(default)]
    pub predicate: Option<ExtractedPredicate>,
    // ─── Compile Completeness Contract §5 — three new fields + symbol ────
    /// §5.1 — sensitivity tier for branch T2.6 PII redaction and the
    /// v3 mount-time grant model. Populated by the sensitivity classifier
    /// (`crates/thinkingroot-extract/src/sensitivity.rs`); falls through
    /// to `Sensitivity::Public` on the storage side when `None`.
    #[serde(default)]
    pub sensitivity: Option<Sensitivity>,
    /// §5.2 — ISO-8601 absolute expiration date derived from
    /// `expiration_signal` (when the signal is `HardDate`, `RelativeWindow`,
    /// or `Recurring`). Populates `claim_temporal.valid_until` so the AEP
    /// `rule_temporal_collapse` rule can filter expired claims.
    #[serde(default)]
    pub valid_until: Option<String>,
    /// §5.2 — typed expiration signal preserved for AEP caveats.
    #[serde(default)]
    pub expiration_signal: Option<ExpirationSignal>,
    /// §5.3 — numeric values mentioned in the claim. Multiple per claim
    /// because a single statement can mention several metrics
    /// ("p99=120ms at 50K rps"). Populates the `quantities` table.
    #[serde(default)]
    pub quantities: Vec<ExtractedQuantity>,
    /// §4.1 + §4.4 — function/type identifier for Phase 7e callee
    /// resolution + code_signatures lookup. The structural extractor
    /// (`structural.rs:113-229`) populates this from
    /// `chunk.metadata.function_name` / `type_name`. Empty for non-code
    /// claims.
    #[serde(default)]
    pub symbol: Option<String>,
}

/// Compile Completeness Contract §5.3 — a single numeric value extracted
/// from a claim's statement or chunk text. Populates the `quantities`
/// table during Phase 6.7.
///
/// `byte_start`/`byte_end` are **absolute file-local bytes**, not
/// chunk-local — they let the row link to a precise source location even
/// when the surrounding claim covers a longer span.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtractedQuantity {
    /// Classified metric (latency, throughput, price, cost, share, demand,
    /// count). Empty when the classifier cannot decide — the row still
    /// lands so AEP can surface untyped numerics rather than dropping them.
    #[serde(default)]
    pub metric_name: String,
    pub value: f64,
    pub unit: String,
    /// "p99" | "p95" | "max" | "avg" | "monthly" | "" — the modifier
    /// adjacent to the value in source.
    #[serde(default)]
    pub qualifier: String,
    /// True when the source phrase implies dynamism (rate, throughput,
    /// current load, live demand). Powers AEP's "live values shouldn't
    /// be treated as static facts" caveat.
    #[serde(default)]
    pub is_live: bool,
    #[serde(default)]
    pub byte_start: u64,
    #[serde(default)]
    pub byte_end: u64,
}

/// Predicate attached to an extracted claim. Serialized shape is the contract
/// between the LLM and the pipeline — keep stable or version it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedPredicate {
    /// Engine language: `"regex"`, `"rust_ast"`, or `"jsonpath"`.
    pub language: String,
    /// The query string itself (regex pattern, tree-sitter query, or JSONPath).
    pub query: String,
    /// Optional source URI globs scoping where this predicate runs.
    /// Empty / absent = use the owning claim's own source.
    #[serde(default)]
    pub scope_globs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedEntity {
    pub name: String,
    pub entity_type: String,
    pub aliases: Vec<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedRelation {
    pub from_entity: String,
    pub to_entity: String,
    pub relation_type: String,
    pub description: Option<String>,
    /// LLM-assigned confidence for this relation [0.0, 1.0].
    /// Relations below 0.3 are discarded during conversion.
    #[serde(default = "default_relation_confidence")]
    pub confidence: f64,
}

fn default_relation_confidence() -> f64 {
    0.8
}

impl ExtractionResult {
    pub fn empty() -> Self {
        Self {
            claims: Vec::new(),
            entities: Vec::new(),
            relations: Vec::new(),
        }
    }

    pub fn merge(&mut self, other: ExtractionResult) {
        self.claims.extend(other.claims);
        self.entities.extend(other.entities);
        self.relations.extend(other.relations);
    }
}
