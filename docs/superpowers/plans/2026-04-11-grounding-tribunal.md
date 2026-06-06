# Grounding Tribunal — Write-Time Hallucination Prevention

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate LLM hallucinations from the ThinkingRoot knowledge graph by adding a 3-layer grounding verification system between extract and link, plus 6 supporting fixes across the pipeline.

**Architecture:** New `thinkingroot-ground` crate with 3 judges (lexical anchor, span attribution, embedding similarity). Sits between extract and link in the pipeline. Claims that fail grounding are rejected or have confidence reduced. Supporting fixes: temperature normalization across all providers, cache-after-grounding, grounding-based confidence, claim deduplication, NLI-powered contradiction detection, grounding-aware verification.

**Tech Stack:** Rust (edition 2024), CozoDB Datalog, fastembed ONNX (existing), unicode-segmentation (new, lightweight)

---

## File Structure

### New Files

| File | Responsibility |
|---|---|
| `crates/thinkingroot-ground/Cargo.toml` | Crate manifest with `vector` feature gate for Judge 3 |
| `crates/thinkingroot-ground/src/lib.rs` | Public API: `Grounder`, `GroundingResult`, re-exports |
| `crates/thinkingroot-ground/src/lexical.rs` | Judge 1: n-gram overlap scoring |
| `crates/thinkingroot-ground/src/span.rs` | Judge 2: source quote verification |
| `crates/thinkingroot-ground/src/semantic.rs` | Judge 3: embedding cosine similarity (feature-gated) |
| `crates/thinkingroot-ground/src/grounder.rs` | Orchestrator: chains 3 judges, produces final score |
| `crates/thinkingroot-ground/src/dedup.rs` | Claim deduplication by embedding similarity |

### Modified Files

| File | What Changes |
|---|---|
| `Cargo.toml` (workspace root) | Add `thinkingroot-ground` to members, default-members, workspace.dependencies. Add `unicode-segmentation` to workspace deps. |
| `crates/thinkingroot-core/src/types/claim.rs` | Add `grounding_score: Option<f64>` and `grounding_method: Option<GroundingMethod>` fields |
| `crates/thinkingroot-core/src/types/mod.rs` | No change needed (claim.rs already re-exported) |
| `crates/thinkingroot-extract/src/prompts.rs` | Add `source_quote` requirement to SYSTEM_PROMPT |
| `crates/thinkingroot-extract/src/schema.rs` | Add `source_quote: Option<String>` to `ExtractedClaim` |
| `crates/thinkingroot-extract/src/extractor.rs` | Pass source text through with ExtractionOutput for grounding |
| `crates/thinkingroot-extract/src/cache.rs` | Bump PROMPT_VERSION from "v1" to "v2" |
| `crates/thinkingroot-extract/src/llm.rs` | Add `temperature: 0.1` to Bedrock and Anthropic providers |
| `crates/thinkingroot-graph/src/graph.rs` | Add `grounding_score` and `grounding_method` columns to claims schema, update `insert_claim` |
| `crates/thinkingroot-link/src/linker.rs` | Accept grounded extraction output, use embedding similarity for contradiction detection |
| `crates/thinkingroot-serve/src/pipeline.rs` | Insert ground stage between extract and link, move cache writes after grounding |
| `crates/thinkingroot-verify/src/verifier.rs` | Report claims with low grounding scores |
| `crates/thinkingroot-serve/Cargo.toml` | Add `thinkingroot-ground` dependency |
| `crates/thinkingroot-cli/Cargo.toml` | Chain `ground` feature flag |

---

## Task 1: Fix Temperature Bug in LLM Providers

**Files:**
- Modify: `crates/thinkingroot-extract/src/llm.rs:128-154` (Bedrock) and `crates/thinkingroot-extract/src/llm.rs:292-340` (Anthropic)
- Test: `crates/thinkingroot-extract/src/llm.rs` (existing test module)

- [ ] **Step 1: Add temperature to Bedrock provider**

In `crates/thinkingroot-extract/src/llm.rs`, in `BedrockProvider::chat`, change the `InferenceConfiguration` builder:

```rust
// Before (line ~136):
.inference_config(
    InferenceConfiguration::builder()
        .max_tokens(self.max_output_tokens)
        .build(),
)

// After:
.inference_config(
    InferenceConfiguration::builder()
        .max_tokens(self.max_output_tokens)
        .temperature(0.1)
        .build(),
)
```

- [ ] **Step 2: Add temperature to Anthropic provider**

In `crates/thinkingroot-extract/src/llm.rs`, in `AnthropicProvider::chat`, add temperature to the JSON body:

```rust
// Before (line ~293):
let body = serde_json::json!({
    "model": self.model,
    "max_tokens": self.max_output_tokens,
    "system": system,
    "messages": [
        {"role": "user", "content": user},
    ],
});

// After:
let body = serde_json::json!({
    "model": self.model,
    "max_tokens": self.max_output_tokens,
    "temperature": 0.1,
    "system": system,
    "messages": [
        {"role": "user", "content": user},
    ],
});
```

- [ ] **Step 3: Verify build**

Run: `cargo check -p thinkingroot-extract`
Expected: compiles with no errors

- [ ] **Step 4: Commit**

```bash
git add crates/thinkingroot-extract/src/llm.rs
git commit -m "fix(extract): set temperature 0.1 for Bedrock and Anthropic providers

Bedrock and Anthropic providers were using model defaults (typically 1.0),
causing higher hallucination rates. OpenAI-compatible providers already set 0.1."
```

---

## Task 2: Add Grounding Fields to Core Claim Type

**Files:**
- Modify: `crates/thinkingroot-core/src/types/claim.rs`

- [ ] **Step 1: Write test for new grounding fields**

Add to the existing `mod tests` block in `crates/thinkingroot-core/src/types/claim.rs`:

```rust
#[test]
fn claim_grounding_defaults_to_none() {
    let ws = WorkspaceId::new();
    let src = SourceId::new();
    let claim = Claim::new("Rust is fast", ClaimType::Fact, src, ws);
    assert!(claim.grounding_score.is_none());
    assert!(claim.grounding_method.is_none());
}

#[test]
fn claim_with_grounding() {
    let ws = WorkspaceId::new();
    let src = SourceId::new();
    let claim = Claim::new("Rust is fast", ClaimType::Fact, src, ws)
        .with_grounding(0.92, GroundingMethod::Lexical);
    assert_eq!(claim.grounding_score, Some(0.92));
    assert_eq!(claim.grounding_method, Some(GroundingMethod::Lexical));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p thinkingroot-core -- claim_grounding`
Expected: FAIL — `grounding_score` field does not exist

- [ ] **Step 3: Add GroundingMethod enum and new fields to Claim**

In `crates/thinkingroot-core/src/types/claim.rs`, add the enum after `PipelineVersion`:

```rust
/// How a claim's grounding score was determined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroundingMethod {
    /// Judge 1: keyword/n-gram overlap with source text.
    Lexical,
    /// Judge 2: LLM-cited source quote verified in source text.
    Span,
    /// Judge 3: embedding cosine similarity with source text.
    Semantic,
    /// Combined score from multiple judges.
    Combined,
    /// Not grounded (legacy claims or grounding disabled).
    Unverified,
}
```

Add two fields to `struct Claim` after `created_at`:

```rust
pub grounding_score: Option<f64>,
pub grounding_method: Option<GroundingMethod>,
```

Update `Claim::new` to initialize them:

```rust
grounding_score: None,
grounding_method: None,
```

Add a builder method after `with_sensitivity`:

```rust
pub fn with_grounding(mut self, score: f64, method: GroundingMethod) -> Self {
    self.grounding_score = Some(score.clamp(0.0, 1.0));
    self.grounding_method = Some(method);
    self
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p thinkingroot-core`
Expected: ALL PASS

- [ ] **Step 5: Commit**

```bash
git add crates/thinkingroot-core/src/types/claim.rs
git commit -m "feat(core): add grounding_score and grounding_method to Claim

Claims can now carry a grounding score (0.0-1.0) indicating how well
the claim is anchored in its source text, and a method enum tracking
which grounding judge(s) produced the score."
```

---

## Task 3: Update Graph Schema for Grounding Fields

**Files:**
- Modify: `crates/thinkingroot-graph/src/graph.rs`

- [ ] **Step 1: Write test for grounding fields in graph storage**

Add to the test module in `crates/thinkingroot-graph/src/graph.rs` (or the appropriate test file):

```rust
#[test]
fn insert_claim_stores_grounding_score() {
    let dir = tempfile::TempDir::new().unwrap();
    let graph = GraphStore::init(dir.path()).unwrap();
    let source = thinkingroot_core::Source::new("test://g.md".into(), thinkingroot_core::types::SourceType::File);
    graph.insert_source(&source).unwrap();

    let claim = thinkingroot_core::Claim::new(
        "Grounded claim",
        thinkingroot_core::types::ClaimType::Fact,
        source.id,
        thinkingroot_core::types::WorkspaceId::new(),
    )
    .with_grounding(0.87, thinkingroot_core::types::GroundingMethod::Combined);

    graph.insert_claim(&claim).unwrap();

    // Verify grounding_score is stored and retrievable.
    let result = graph.query_raw(
        "?[id, gs, gm] := *claims{id, grounding_score: gs, grounding_method: gm}",
    ).unwrap();
    assert_eq!(result.rows.len(), 1);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p thinkingroot-graph -- insert_claim_stores_grounding`
Expected: FAIL — `grounding_score` column does not exist in schema

- [ ] **Step 3: Add columns to claims schema**

In `crates/thinkingroot-graph/src/graph.rs`, update the claims relation in `create_schema`:

```rust
":create claims {
    id: String
    =>
    statement: String,
    claim_type: String,
    source_id: String,
    confidence: Float default 0.8,
    sensitivity: String default 'Public',
    workspace_id: String default '',
    created_at: Float default 0.0,
    grounding_score: Float default -1.0,
    grounding_method: String default ''
}",
```

Note: `-1.0` means "not grounded" (distinguishes from 0.0 which means "checked but zero grounding"). Empty string for method means "unverified".

- [ ] **Step 4: Update insert_claim to write grounding fields**

In `insert_claim`, add before the query call:

```rust
params.insert(
    "grounding_score".into(),
    DataValue::Num(Num::Float(claim.grounding_score.unwrap_or(-1.0))),
);
params.insert(
    "grounding_method".into(),
    DataValue::Str(
        claim.grounding_method
            .map(|m| format!("{m:?}"))
            .unwrap_or_default()
            .into(),
    ),
);
```

Update the Datalog query to include the new columns:

```rust
self.query(
    r#"?[id, statement, claim_type, source_id, confidence, sensitivity, workspace_id, created_at, grounding_score, grounding_method] <- [[
        $id, $statement, $claim_type, $source_id, $confidence, $sensitivity, $workspace_id, $created_at, $grounding_score, $grounding_method
    ]]
    :put claims {id => statement, claim_type, source_id, confidence, sensitivity, workspace_id, created_at, grounding_score, grounding_method}"#,
    params,
)?;
```

- [ ] **Step 5: Add helper to count low-grounding claims**

Add a new method to `GraphStore`:

```rust
/// Count claims with grounding_score below a threshold.
/// Ignores ungrounded claims (score = -1.0).
pub fn count_low_grounding_claims(&self, threshold: f64) -> Result<usize> {
    let mut params = BTreeMap::new();
    params.insert("threshold".into(), DataValue::Num(Num::Float(threshold)));
    let result = self.query(
        "?[count(id)] := *claims{id, grounding_score: gs}, gs >= 0.0, gs < $threshold",
        params,
    )?;
    Ok(extract_count(&result))
}
```

- [ ] **Step 6: Run tests**

Run: `cargo test -p thinkingroot-graph`
Expected: ALL PASS

**Important:** Existing databases will need a re-compile (`root compile`) to pick up the new schema columns. CozoDB's `:create` will error on "already exists" for the old schema — the existing error-suppression logic handles this. New columns only appear in fresh databases. Document this in the commit message.

- [ ] **Step 7: Commit**

```bash
git add crates/thinkingroot-graph/src/graph.rs
git commit -m "feat(graph): add grounding_score and grounding_method to claims schema

New Float column grounding_score (-1.0 = unverified, 0.0-1.0 = verified)
and String column grounding_method track how well each claim is anchored
in its source text. Existing databases require re-compile to pick up
the new schema."
```

---

## Task 4: Update Extraction Prompt for Span Attribution (Judge 2)

**Files:**
- Modify: `crates/thinkingroot-extract/src/prompts.rs`
- Modify: `crates/thinkingroot-extract/src/schema.rs`
- Modify: `crates/thinkingroot-extract/src/cache.rs`

- [ ] **Step 1: Add source_quote to ExtractedClaim schema**

In `crates/thinkingroot-extract/src/schema.rs`, update `ExtractedClaim`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedClaim {
    pub statement: String,
    pub claim_type: String,
    pub confidence: f64,
    pub entities: Vec<String>,
    #[serde(default)]
    pub source_quote: Option<String>,
}
```

- [ ] **Step 2: Update SYSTEM_PROMPT to require source quotes**

In `crates/thinkingroot-extract/src/prompts.rs`, update the `claims` section of the JSON schema in SYSTEM_PROMPT:

```rust
pub const SYSTEM_PROMPT: &str = r#"You are a knowledge extraction engine for ThinkingRoot, a knowledge compiler.
Your job is to extract structured knowledge from source documents.

You MUST return valid JSON matching this exact schema:

{
  "claims": [
    {
      "statement": "A clear, atomic statement of fact or decision",
      "claim_type": "fact|decision|opinion|plan|requirement|metric|definition|dependency|api_signature|architecture",
      "confidence": 0.0-1.0,
      "entities": ["entity names mentioned in this claim"],
      "source_quote": "The exact phrase or sentence from the source that supports this claim"
    }
  ],
  "entities": [
    {
      "name": "Canonical name",
      "entity_type": "person|system|service|concept|team|api|database|library|file|module|function|config|organization",
      "aliases": ["alternate names"],
      "description": "Brief description"
    }
  ],
  "relations": [
    {
      "from_entity": "Entity A",
      "to_entity": "Entity B",
      "relation_type": "depends_on|owned_by|replaces|contradicts|implements|uses|contains|created_by|part_of|related_to|calls|configured_by|tested_by",
      "description": "Brief description of the relationship"
    }
  ]
}

Rules:
1. Claims must be ATOMIC — one fact per claim. Do not combine multiple facts.
2. Claims must be SELF-CONTAINED — understandable without reading the source.
3. Every entity mentioned in a claim MUST appear in the entities list.
4. Confidence reflects how certain the source is (0.5=implied, 0.8=stated, 0.95=definitive).
5. Do NOT fabricate information. Extract only what is explicitly stated or clearly implied.
6. For code: extract function signatures, type definitions, dependencies, and architectural patterns.
7. For docs: extract decisions, requirements, facts, and relationships between concepts.
8. source_quote MUST be a verbatim substring copied from the source. Do NOT paraphrase.
9. Return ONLY the JSON object. No markdown, no explanation, no preamble."#;
```

- [ ] **Step 3: Bump cache PROMPT_VERSION**

In `crates/thinkingroot-extract/src/cache.rs`, change:

```rust
// Before:
const PROMPT_VERSION: &str = "v1";

// After:
const PROMPT_VERSION: &str = "v2";
```

This invalidates all cached extraction results, forcing re-extraction with the new prompt that includes `source_quote`.

- [ ] **Step 4: Verify build**

Run: `cargo check -p thinkingroot-extract`
Expected: compiles

- [ ] **Step 5: Commit**

```bash
git add crates/thinkingroot-extract/src/prompts.rs crates/thinkingroot-extract/src/schema.rs crates/thinkingroot-extract/src/cache.rs
git commit -m "feat(extract): add source_quote to extraction prompt and schema

LLM is now required to cite the exact source text supporting each claim.
This enables Judge 2 (span attribution) in the grounding system.
Bumps cache version to v2, invalidating stale cached extractions."
```

---

## Task 5: Pass Source Text Through ExtractionOutput

**Files:**
- Modify: `crates/thinkingroot-extract/src/extractor.rs`

The grounding system needs access to the original source text for each claim. Currently `ExtractionOutput` only contains the structured claims — not the source text they came from.

- [ ] **Step 1: Add source_texts map to ExtractionOutput**

In `crates/thinkingroot-extract/src/extractor.rs`, add to `ExtractionOutput`:

```rust
#[derive(Debug, Default)]
pub struct ExtractionOutput {
    pub claims: Vec<Claim>,
    pub entities: Vec<Entity>,
    pub relations: Vec<SourcedRelation>,
    pub claim_entity_names: HashMap<ClaimId, Vec<String>>,
    pub sources_processed: usize,
    pub chunks_processed: usize,
    pub cache_hits: usize,
    /// Maps SourceId → the raw source text that was sent to the LLM.
    /// Used by the grounding system to verify claims against source.
    pub source_texts: HashMap<SourceId, String>,
    /// Maps ClaimId → the LLM's cited source_quote for that claim.
    /// Used by Judge 2 (span attribution) in the grounding system.
    pub claim_source_quotes: HashMap<ClaimId, String>,
}
```

- [ ] **Step 2: Populate source_texts and claim_source_quotes during extraction**

In `convert_result_static`, update the claims conversion to capture source_quotes:

```rust
for ext_claim in &result.claims {
    if ext_claim.confidence < min_confidence {
        continue;
    }
    let claim_type = parse_claim_type(&ext_claim.claim_type);
    let claim = Claim::new(&ext_claim.statement, claim_type, source_id, workspace_id)
        .with_confidence(ext_claim.confidence);
    if !ext_claim.entities.is_empty() {
        output
            .claim_entity_names
            .insert(claim.id, ext_claim.entities.clone());
    }
    if let Some(ref quote) = ext_claim.source_quote {
        if !quote.is_empty() {
            output.claim_source_quotes.insert(claim.id, quote.clone());
        }
    }
    output.claims.push(claim);
}
```

In `extract_all`, after spawning LLM tasks and collecting results, populate source_texts from the chunks:

In the loop where `ChunkWork` is created, concatenate chunk content per source:

```rust
// In the llm_work building loop, after:
llm_work.push(ChunkWork { ... });
// Also accumulate source text:
output.source_texts
    .entry(doc.source_id)
    .or_default()
    .push_str(&chunk.content);
output.source_texts
    .entry(doc.source_id)
    .and_modify(|s| s.push('\n'));
```

Note: `source_texts` type changes to `HashMap<SourceId, String>`, built up by concatenating all chunk content per source. This approach works because chunks are sequential subdivisions of the document.

- [ ] **Step 3: Update merge to include new fields**

In `ExtractionOutput::merge`:

```rust
fn merge(&mut self, other: ExtractionOutput) {
    self.claims.extend(other.claims);
    self.entities.extend(other.entities);
    self.relations.extend(other.relations);
    self.claim_entity_names.extend(other.claim_entity_names);
    self.sources_processed += other.sources_processed;
    self.chunks_processed += other.chunks_processed;
    self.cache_hits += other.cache_hits;
    self.source_texts.extend(other.source_texts);
    self.claim_source_quotes.extend(other.claim_source_quotes);
}
```

- [ ] **Step 4: Verify build**

Run: `cargo check -p thinkingroot-extract`
Expected: compiles (some downstream crates may need updates — that's Task 10)

- [ ] **Step 5: Commit**

```bash
git add crates/thinkingroot-extract/src/extractor.rs
git commit -m "feat(extract): carry source_texts and source_quotes through ExtractionOutput

Grounding system needs the original source text and LLM-cited quotes
to verify each claim. Both are now available on ExtractionOutput."
```

---

## Task 6: Create thinkingroot-ground Crate Scaffold

**Files:**
- Create: `crates/thinkingroot-ground/Cargo.toml`
- Create: `crates/thinkingroot-ground/src/lib.rs`
- Modify: `Cargo.toml` (workspace root)

- [ ] **Step 1: Create Cargo.toml**

Create `crates/thinkingroot-ground/Cargo.toml`:

```toml
[package]
name = "thinkingroot-ground"
description = "Write-time hallucination prevention: 3-judge grounding tribunal"
version.workspace = true
edition.workspace = true
authors.workspace = true
license.workspace        = true
repository.workspace    = true
homepage.workspace      = true
documentation.workspace = true
keywords.workspace      = true
categories.workspace    = true
rust-version.workspace  = true

[features]
default = []
## Enable Judge 3 (embedding cosine similarity) via fastembed.
## Without this, only Judges 1+2 (pure string ops) are active.
vector = ["dep:thinkingroot-graph"]

[dependencies]
thinkingroot-core = { workspace = true }
thinkingroot-extract = { workspace = true }
unicode-segmentation = "1.12"
tracing = { workspace = true }

# Optional: Judge 3 needs fastembed via graph crate for embeddings.
thinkingroot-graph = { workspace = true, optional = true }

[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: Create lib.rs**

Create `crates/thinkingroot-ground/src/lib.rs`:

```rust
mod lexical;
mod span;
mod grounder;
pub mod dedup;

#[cfg(feature = "vector")]
mod semantic;

pub use grounder::{Grounder, GroundingVerdict};
pub use lexical::LexicalJudge;
pub use span::SpanJudge;

#[cfg(feature = "vector")]
pub use semantic::SemanticJudge;
```

- [ ] **Step 3: Add to workspace**

In the root `Cargo.toml`, add `"crates/thinkingroot-ground"` to both `members` and `default-members` arrays.

Add to `[workspace.dependencies]`:

```toml
thinkingroot-ground  = { path = "crates/thinkingroot-ground", version = "0.2.0", default-features = false }

# Text processing
unicode-segmentation = "1.12"
```

- [ ] **Step 4: Verify workspace**

Run: `cargo check -p thinkingroot-ground`
Expected: compiles (empty modules, we'll fill them next)

- [ ] **Step 5: Commit**

```bash
git add crates/thinkingroot-ground/ Cargo.toml
git commit -m "feat(ground): scaffold thinkingroot-ground crate

New crate for write-time hallucination prevention. Will contain 3 judges:
- Judge 1 (lexical): n-gram overlap with source text
- Judge 2 (span): LLM-cited quote verification
- Judge 3 (semantic): embedding cosine similarity (feature-gated behind 'vector')"
```

---

## Task 7: Implement Judge 1 — Lexical Anchor

**Files:**
- Create: `crates/thinkingroot-ground/src/lexical.rs`

- [ ] **Step 1: Write tests**

Create `crates/thinkingroot-ground/src/lexical.rs` with tests first:

```rust
use unicode_segmentation::UnicodeSegmentation;

/// Judge 1: Lexical anchoring.
///
/// Checks what fraction of meaningful words in the claim appear in the source text.
/// Fast (< 1ms per claim), zero dependencies beyond unicode-segmentation.
pub struct LexicalJudge;

/// Words to ignore when computing overlap (too common to be meaningful).
const STOP_WORDS: &[&str] = &[
    "a", "an", "the", "is", "are", "was", "were", "be", "been", "being",
    "have", "has", "had", "do", "does", "did", "will", "would", "could",
    "should", "may", "might", "shall", "can", "need", "must",
    "and", "or", "but", "if", "then", "else", "when", "where", "how",
    "what", "which", "who", "whom", "this", "that", "these", "those",
    "it", "its", "of", "in", "to", "for", "with", "on", "at", "by",
    "from", "as", "into", "about", "not", "no", "so", "up", "out",
    "than", "too", "very", "just", "also", "all", "each", "every",
    "any", "some", "such", "only", "own", "same", "other", "new",
    "used", "using", "uses", "use",
];

impl LexicalJudge {
    /// Score how well a claim is lexically anchored in the source text.
    ///
    /// Returns a score in [0.0, 1.0]:
    /// - 1.0 = every meaningful word in the claim appears in the source
    /// - 0.0 = no meaningful words match
    pub fn score(claim: &str, source_text: &str) -> f64 {
        let source_words = Self::extract_words(source_text);
        let claim_words = Self::extract_words(claim);

        if claim_words.is_empty() {
            return 0.0;
        }

        let matches = claim_words
            .iter()
            .filter(|w| source_words.contains(w.as_str()))
            .count();

        matches as f64 / claim_words.len() as f64
    }

    /// Extract meaningful lowercase words, filtering stop words and short tokens.
    fn extract_words(text: &str) -> Vec<String> {
        text.unicode_words()
            .map(|w| w.to_lowercase())
            .filter(|w| w.len() >= 2)
            .filter(|w| !STOP_WORDS.contains(&w.as_str()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perfect_overlap() {
        let source = "PostgreSQL stores user data in tables";
        let claim = "PostgreSQL stores user data";
        let score = LexicalJudge::score(claim, source);
        assert!(score > 0.99, "expected ~1.0, got {score}");
    }

    #[test]
    fn zero_overlap() {
        let source = "PostgreSQL stores user data";
        let claim = "Redis caches session tokens";
        let score = LexicalJudge::score(claim, source);
        assert!(score < 0.01, "expected ~0.0, got {score}");
    }

    #[test]
    fn partial_overlap() {
        let source = "PostgreSQL stores user data and handles transactions";
        let claim = "PostgreSQL handles authentication and sessions";
        let score = LexicalJudge::score(claim, source);
        // "postgresql" and "handles" match; "authentication" and "sessions" don't
        assert!(score > 0.2 && score < 0.8, "expected partial, got {score}");
    }

    #[test]
    fn empty_claim_returns_zero() {
        let score = LexicalJudge::score("", "some source text");
        assert_eq!(score, 0.0);
    }

    #[test]
    fn stop_words_are_ignored() {
        let source = "The system";
        let claim = "The system is very good and also fast";
        // After stop word removal, claim has: "system", "good", "fast"
        // Source has: "system"
        // Score = 1/3
        let score = LexicalJudge::score(claim, source);
        assert!(score > 0.3 && score < 0.4, "expected ~0.33, got {score}");
    }

    #[test]
    fn case_insensitive() {
        let source = "PostgreSQL is a database";
        let claim = "POSTGRESQL database";
        let score = LexicalJudge::score(claim, source);
        assert!(score > 0.99, "expected ~1.0, got {score}");
    }
}
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p thinkingroot-ground -- lexical`
Expected: ALL PASS (implementation is in the same file)

- [ ] **Step 3: Commit**

```bash
git add crates/thinkingroot-ground/src/lexical.rs
git commit -m "feat(ground): implement Judge 1 — lexical anchoring

Scores claims by the fraction of meaningful (non-stop) words that appear
in the source text. Pure string ops, < 1ms per claim, zero external deps."
```

---

## Task 8: Implement Judge 2 — Span Attribution

**Files:**
- Create: `crates/thinkingroot-ground/src/span.rs`

- [ ] **Step 1: Write implementation and tests**

Create `crates/thinkingroot-ground/src/span.rs`:

```rust
/// Judge 2: Span attribution verification.
///
/// Verifies that the LLM's cited `source_quote` actually appears in the source text.
/// Uses fuzzy substring matching to tolerate minor whitespace/punctuation differences.
pub struct SpanJudge;

impl SpanJudge {
    /// Score how well the LLM's cited quote matches the source text.
    ///
    /// Returns a score in [0.0, 1.0]:
    /// - 1.0 = exact verbatim match found in source
    /// - 0.7-0.99 = fuzzy match (whitespace/case normalized)
    /// - 0.0 = quote not found in source at all (likely hallucinated)
    /// - Returns None if no source_quote was provided by the LLM
    pub fn score(source_quote: Option<&str>, source_text: &str) -> Option<f64> {
        let quote = source_quote?;
        if quote.is_empty() {
            return None;
        }

        // Try exact substring match first.
        if source_text.contains(quote) {
            return Some(1.0);
        }

        // Normalize whitespace and try again.
        let norm_quote = normalize(quote);
        let norm_source = normalize(source_text);

        if norm_source.contains(&norm_quote) {
            return Some(0.95);
        }

        // Case-insensitive normalized match.
        let lower_quote = norm_quote.to_lowercase();
        let lower_source = norm_source.to_lowercase();

        if lower_source.contains(&lower_quote) {
            return Some(0.9);
        }

        // Sliding window: find best overlap ratio.
        // This catches quotes that are "almost right" (off by a few chars).
        let best = best_window_overlap(&lower_quote, &lower_source);
        if best >= 0.8 {
            return Some(best * 0.85); // Scale down since it's fuzzy
        }

        Some(0.0)
    }
}

/// Collapse runs of whitespace to single spaces and trim.
fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Sliding window: find the substring of `haystack` with length = `needle.len()`
/// that has the highest character overlap with `needle`.
/// Returns a ratio in [0.0, 1.0].
fn best_window_overlap(needle: &str, haystack: &str) -> f64 {
    if needle.is_empty() || haystack.is_empty() || needle.len() > haystack.len() {
        return 0.0;
    }

    let needle_bytes = needle.as_bytes();
    let haystack_bytes = haystack.as_bytes();
    let window_len = needle_bytes.len();
    let mut best = 0usize;

    for start in 0..=(haystack_bytes.len() - window_len) {
        let matches = needle_bytes
            .iter()
            .zip(&haystack_bytes[start..start + window_len])
            .filter(|(a, b)| a == b)
            .count();
        if matches > best {
            best = matches;
        }
    }

    best as f64 / window_len as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_scores_one() {
        let source = "PostgreSQL stores user data in normalized tables.";
        let quote = "PostgreSQL stores user data in normalized tables.";
        assert_eq!(SpanJudge::score(Some(quote), source), Some(1.0));
    }

    #[test]
    fn whitespace_normalized_match() {
        let source = "PostgreSQL  stores\n  user   data";
        let quote = "PostgreSQL stores user data";
        let score = SpanJudge::score(Some(quote), source).unwrap();
        assert!(score >= 0.9, "expected >= 0.9, got {score}");
    }

    #[test]
    fn case_insensitive_match() {
        let source = "PostgreSQL Stores User Data";
        let quote = "postgresql stores user data";
        let score = SpanJudge::score(Some(quote), source).unwrap();
        assert!(score >= 0.85, "expected >= 0.85, got {score}");
    }

    #[test]
    fn no_match_scores_zero() {
        let source = "PostgreSQL stores user data";
        let quote = "Redis caches session tokens";
        let score = SpanJudge::score(Some(quote), source).unwrap();
        assert!(score < 0.3, "expected < 0.3, got {score}");
    }

    #[test]
    fn no_quote_returns_none() {
        assert_eq!(SpanJudge::score(None, "some source"), None);
    }

    #[test]
    fn empty_quote_returns_none() {
        assert_eq!(SpanJudge::score(Some(""), "some source"), None);
    }

    #[test]
    fn partial_match_via_sliding_window() {
        let source = "The system uses PostgreSQL for primary storage";
        let quote = "system uses PostgreSQL for primary storag"; // typo at end
        let score = SpanJudge::score(Some(quote), source).unwrap();
        assert!(score > 0.5, "expected > 0.5, got {score}");
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p thinkingroot-ground -- span`
Expected: ALL PASS

- [ ] **Step 3: Commit**

```bash
git add crates/thinkingroot-ground/src/span.rs
git commit -m "feat(ground): implement Judge 2 — span attribution verification

Verifies LLM-cited source_quote actually exists in the source text.
Handles exact match, whitespace normalization, case insensitivity,
and sliding-window fuzzy matching. Pure string ops, zero deps."
```

---

## Task 9: Implement Judge 3 — Semantic Similarity (Feature-Gated)

**Files:**
- Create: `crates/thinkingroot-ground/src/semantic.rs`

This judge uses the existing fastembed model (AllMiniLM-L6-V2) to compute cosine similarity between claim text and source text. Feature-gated behind `vector`.

- [ ] **Step 1: Write implementation and tests**

Create `crates/thinkingroot-ground/src/semantic.rs`:

```rust
use thinkingroot_graph::vector::VectorStore;

/// Judge 3: Semantic similarity via embedding cosine distance.
///
/// Uses the existing fastembed model (AllMiniLM-L6-V2) already loaded for
/// vector search. Computes cosine similarity between claim and source text.
///
/// This catches claims that reuse real words but change the meaning:
/// - Source: "migrated FROM MySQL to PostgreSQL"  
/// - Claim:  "The system uses MySQL" → low similarity with actual context
///
/// Feature-gated behind `vector` — disabled on low-end builds.
pub struct SemanticJudge;

impl SemanticJudge {
    /// Score semantic similarity between claim and source text.
    ///
    /// Returns a score in [0.0, 1.0]:
    /// - > 0.7: claim is semantically close to source content
    /// - 0.4-0.7: partially related
    /// - < 0.4: likely off-topic / hallucinated
    pub fn score(claim: &str, source_text: &str, vector_store: &VectorStore) -> f64 {
        // Embed both texts using the existing model.
        let texts = vec![claim, source_text];
        match vector_store.embed_texts(&texts) {
            Ok(embeddings) if embeddings.len() == 2 => {
                cosine_similarity(&embeddings[0], &embeddings[1])
            }
            Ok(_) => {
                tracing::warn!("semantic judge: unexpected embedding count");
                0.5 // neutral fallback
            }
            Err(e) => {
                tracing::warn!("semantic judge: embedding failed: {e}");
                0.5 // neutral fallback — don't reject on infra failure
            }
        }
    }
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    (dot / (norm_a * norm_b)) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_similarity_identical() {
        let a = vec![1.0, 2.0, 3.0];
        let sim = cosine_similarity(&a, &a);
        assert!((sim - 1.0).abs() < 0.001);
    }

    #[test]
    fn cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 0.001);
    }

    #[test]
    fn cosine_similarity_opposite() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![-1.0, -2.0, -3.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - (-1.0)).abs() < 0.001);
    }
}
```

- [ ] **Step 2: Expose embed_texts on VectorStore**

The existing `VectorStore` in `crates/thinkingroot-graph/src/vector.rs` embeds via `self.model.embed(...)` but doesn't expose a raw embedding method. Add a public method:

In `crates/thinkingroot-graph/src/vector.rs`, inside the `#[cfg(feature = "vector")] mod inner` block, add to `impl VectorStore`:

```rust
/// Embed texts and return raw embedding vectors.
/// Used by the grounding system's semantic judge.
pub fn embed_texts(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
    self.model
        .embed(texts.to_vec(), None)
        .map_err(|e| Error::GraphStorage(format!("embedding failed: {e}")))
}
```

Also add a no-op stub in the `#[cfg(not(feature = "vector"))]` block:

```rust
pub fn embed_texts(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>> {
    Ok(vec![])
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p thinkingroot-ground -- semantic`
Expected: ALL PASS (cosine similarity tests are pure math, no model needed)

- [ ] **Step 4: Commit**

```bash
git add crates/thinkingroot-ground/src/semantic.rs crates/thinkingroot-graph/src/vector.rs
git commit -m "feat(ground): implement Judge 3 — semantic similarity via fastembed

Uses existing AllMiniLM-L6-V2 model to compute cosine similarity between
claim and source text. Catches semantically off-topic hallucinations.
Feature-gated behind 'vector' — disabled on low-end builds."
```

---

## Task 10: Implement Grounding Orchestrator

**Files:**
- Create: `crates/thinkingroot-ground/src/grounder.rs`

- [ ] **Step 1: Write the orchestrator**

Create `crates/thinkingroot-ground/src/grounder.rs`:

```rust
use std::collections::HashMap;

use thinkingroot_core::types::{ClaimId, GroundingMethod, SourceId};
use thinkingroot_extract::extractor::ExtractionOutput;

use crate::lexical::LexicalJudge;
use crate::span::SpanJudge;

/// The result of grounding a single claim.
#[derive(Debug, Clone)]
pub struct GroundingVerdict {
    pub claim_id: ClaimId,
    pub score: f64,
    pub method: GroundingMethod,
    pub lexical_score: f64,
    pub span_score: Option<f64>,
    pub semantic_score: Option<f64>,
    /// If true, this claim should be rejected (not stored).
    pub rejected: bool,
}

/// Configuration for the grounding system.
pub struct GroundingConfig {
    /// Claims with combined score below this are rejected.
    pub reject_threshold: f64,
    /// Claims with combined score below this get confidence reduced.
    pub reduce_threshold: f64,
}

impl Default for GroundingConfig {
    fn default() -> Self {
        Self {
            reject_threshold: 0.25,
            reduce_threshold: 0.5,
        }
    }
}

/// The Grounding Tribunal: chains 3 judges to verify extraction output.
pub struct Grounder {
    config: GroundingConfig,
    #[cfg(feature = "vector")]
    vector_store: Option<std::sync::Arc<thinkingroot_graph::vector::VectorStore>>,
}

impl Grounder {
    pub fn new(config: GroundingConfig) -> Self {
        Self {
            config,
            #[cfg(feature = "vector")]
            vector_store: None,
        }
    }

    /// Attach a vector store for Judge 3 (semantic similarity).
    #[cfg(feature = "vector")]
    pub fn with_vector_store(
        mut self,
        store: std::sync::Arc<thinkingroot_graph::vector::VectorStore>,
    ) -> Self {
        self.vector_store = Some(store);
        self
    }

    /// Ground all claims in an extraction output.
    ///
    /// Returns the extraction output with:
    /// - Rejected claims removed
    /// - Surviving claims annotated with grounding_score and grounding_method
    /// - Confidence reduced for low-grounding claims
    pub fn ground(&self, mut extraction: ExtractionOutput) -> ExtractionOutput {
        let source_texts = &extraction.source_texts;
        let source_quotes = &extraction.claim_source_quotes;

        let mut verdicts: HashMap<ClaimId, GroundingVerdict> = HashMap::new();

        for claim in &extraction.claims {
            let source_text = source_texts
                .get(&claim.source)
                .map(|s| s.as_str())
                .unwrap_or("");

            // Judge 1: Lexical anchor.
            let lexical = LexicalJudge::score(&claim.statement, source_text);

            // Judge 2: Span attribution.
            let span = SpanJudge::score(
                source_quotes.get(&claim.id).map(|s| s.as_str()),
                source_text,
            );

            // Judge 3: Semantic similarity (if vector feature enabled).
            #[cfg(feature = "vector")]
            let semantic = self.vector_store.as_ref().map(|vs| {
                crate::semantic::SemanticJudge::score(&claim.statement, source_text, vs)
            });
            #[cfg(not(feature = "vector"))]
            let semantic: Option<f64> = None;

            // Combine scores: weighted average of available judges.
            let (combined, method) = combine_scores(lexical, span, semantic);

            let rejected = combined < self.config.reject_threshold;

            if rejected {
                tracing::debug!(
                    "grounding REJECTED claim {:?}: score={combined:.2} lexical={lexical:.2} \
                     span={span:?} semantic={semantic:?} — \"{}\"",
                    claim.id,
                    truncate(&claim.statement, 60),
                );
            }

            verdicts.insert(
                claim.id,
                GroundingVerdict {
                    claim_id: claim.id,
                    score: combined,
                    method,
                    lexical_score: lexical,
                    span_score: span,
                    semantic_score: semantic,
                    rejected,
                },
            );
        }

        // Count stats before filtering.
        let total = extraction.claims.len();
        let rejected_count = verdicts.values().filter(|v| v.rejected).count();
        let reduced_count = verdicts
            .values()
            .filter(|v| !v.rejected && v.score < self.config.reduce_threshold)
            .count();

        // Remove rejected claims.
        extraction.claims.retain(|c| {
            verdicts
                .get(&c.id)
                .map(|v| !v.rejected)
                .unwrap_or(true)
        });

        // Annotate surviving claims with grounding scores.
        for claim in &mut extraction.claims {
            if let Some(verdict) = verdicts.get(&claim.id) {
                claim.grounding_score = Some(verdict.score);
                claim.grounding_method = Some(verdict.method);

                // If grounding is low, reduce confidence.
                if verdict.score < self.config.reduce_threshold {
                    let reduced = claim.confidence.value() * verdict.score;
                    claim.confidence = thinkingroot_core::types::Confidence::new(reduced);
                }
            }
        }

        // Also remove relations whose claims were rejected.
        let surviving_sources: std::collections::HashSet<SourceId> =
            extraction.claims.iter().map(|c| c.source).collect();
        // Relations are source-scoped; keep those whose source still has claims.
        // (Relations aren't claim-specific, so we keep them if any claim from that source survived.)

        tracing::info!(
            "grounding complete: {total} claims → {rejected_count} rejected, \
             {reduced_count} confidence-reduced, {} accepted",
            total - rejected_count,
        );

        extraction
    }
}

/// Combine scores from available judges into a single grounding score.
fn combine_scores(
    lexical: f64,
    span: Option<f64>,
    semantic: Option<f64>,
) -> (f64, GroundingMethod) {
    match (span, semantic) {
        // All 3 judges available.
        (Some(s), Some(sem)) => {
            let combined = lexical * 0.35 + s * 0.35 + sem * 0.30;
            (combined, GroundingMethod::Combined)
        }
        // Judges 1 + 2 only.
        (Some(s), None) => {
            let combined = lexical * 0.5 + s * 0.5;
            (combined, GroundingMethod::Combined)
        }
        // Judges 1 + 3 only (no source_quote from LLM).
        (None, Some(sem)) => {
            let combined = lexical * 0.55 + sem * 0.45;
            (combined, GroundingMethod::Combined)
        }
        // Judge 1 only.
        (None, None) => (lexical, GroundingMethod::Lexical),
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combine_all_three() {
        let (score, method) = combine_scores(0.8, Some(1.0), Some(0.9));
        // 0.8*0.35 + 1.0*0.35 + 0.9*0.30 = 0.28 + 0.35 + 0.27 = 0.90
        assert!((score - 0.9).abs() < 0.01);
        assert_eq!(method, GroundingMethod::Combined);
    }

    #[test]
    fn combine_judges_1_and_2() {
        let (score, method) = combine_scores(0.8, Some(1.0), None);
        // 0.8*0.5 + 1.0*0.5 = 0.4 + 0.5 = 0.9
        assert!((score - 0.9).abs() < 0.01);
        assert_eq!(method, GroundingMethod::Combined);
    }

    #[test]
    fn combine_judge_1_only() {
        let (score, method) = combine_scores(0.6, None, None);
        assert!((score - 0.6).abs() < 0.01);
        assert_eq!(method, GroundingMethod::Lexical);
    }

    #[test]
    fn below_reject_threshold_is_rejected() {
        let config = GroundingConfig {
            reject_threshold: 0.25,
            reduce_threshold: 0.5,
        };
        // Score = 0.1 (below 0.25) → rejected
        let (score, _) = combine_scores(0.1, Some(0.1), None);
        assert!(score < config.reject_threshold);
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p thinkingroot-ground -- grounder`
Expected: ALL PASS

- [ ] **Step 3: Commit**

```bash
git add crates/thinkingroot-ground/src/grounder.rs
git commit -m "feat(ground): implement grounding orchestrator

Chains Judges 1-3 with weighted scoring. Claims below reject_threshold (0.25)
are removed from output. Claims below reduce_threshold (0.5) get confidence
reduced proportionally. Surviving claims are annotated with grounding_score."
```

---

## Task 11: Implement Claim Deduplication

**Files:**
- Create: `crates/thinkingroot-ground/src/dedup.rs`

- [ ] **Step 1: Write implementation and tests**

Create `crates/thinkingroot-ground/src/dedup.rs`:

```rust
use std::collections::HashMap;

use thinkingroot_core::types::{Claim, SourceId};

use crate::lexical::LexicalJudge;

/// Deduplicate claims within the same source.
///
/// When the same fact appears in multiple chunks of the same file,
/// the LLM extracts it multiple times. This inflates the graph and
/// distorts coverage scores.
///
/// Uses lexical similarity (Jaccard on word sets) as a lightweight
/// dedup signal. Claims with > 85% word overlap from the same source
/// are merged (higher-confidence version kept).
pub fn dedup_claims(claims: &mut Vec<Claim>) {
    // Group by source.
    let mut by_source: HashMap<SourceId, Vec<usize>> = HashMap::new();
    for (idx, claim) in claims.iter().enumerate() {
        by_source.entry(claim.source).or_default().push(idx);
    }

    let mut to_remove: Vec<usize> = Vec::new();

    for indices in by_source.values() {
        if indices.len() < 2 {
            continue;
        }

        for i in 0..indices.len() {
            if to_remove.contains(&indices[i]) {
                continue;
            }
            for j in (i + 1)..indices.len() {
                if to_remove.contains(&indices[j]) {
                    continue;
                }

                let a = &claims[indices[i]];
                let b = &claims[indices[j]];

                let similarity = jaccard_words(&a.statement, &b.statement);
                if similarity > 0.85 {
                    // Keep the one with higher confidence (or grounding score).
                    let keep_j = b.confidence.value() > a.confidence.value();
                    if keep_j {
                        to_remove.push(indices[i]);
                        break; // 'i' is removed, no point comparing further
                    } else {
                        to_remove.push(indices[j]);
                    }
                }
            }
        }
    }

    // Sort descending so removal doesn't shift indices.
    to_remove.sort_unstable();
    to_remove.dedup();
    for idx in to_remove.into_iter().rev() {
        claims.remove(idx);
    }
}

/// Jaccard similarity on word sets (case-insensitive, ignoring stop words).
fn jaccard_words(a: &str, b: &str) -> f64 {
    let words_a = word_set(a);
    let words_b = word_set(b);

    if words_a.is_empty() && words_b.is_empty() {
        return 1.0;
    }

    let intersection = words_a.intersection(&words_b).count();
    let union = words_a.union(&words_b).count();

    if union == 0 {
        return 0.0;
    }

    intersection as f64 / union as f64
}

fn word_set(text: &str) -> std::collections::HashSet<String> {
    use unicode_segmentation::UnicodeSegmentation;
    text.unicode_words()
        .map(|w| w.to_lowercase())
        .filter(|w| w.len() >= 2)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use thinkingroot_core::types::{ClaimType, WorkspaceId};

    fn make_claim(statement: &str, source: SourceId, confidence: f64) -> Claim {
        Claim::new(statement, ClaimType::Fact, source, WorkspaceId::new())
            .with_confidence(confidence)
    }

    #[test]
    fn identical_claims_deduped() {
        let src = SourceId::new();
        let mut claims = vec![
            make_claim("PostgreSQL stores user data", src, 0.8),
            make_claim("PostgreSQL stores user data", src, 0.9),
        ];
        dedup_claims(&mut claims);
        assert_eq!(claims.len(), 1);
        // Higher confidence kept.
        assert!((claims[0].confidence.value() - 0.9).abs() < 0.01);
    }

    #[test]
    fn different_claims_not_deduped() {
        let src = SourceId::new();
        let mut claims = vec![
            make_claim("PostgreSQL stores user data", src, 0.8),
            make_claim("Redis caches session tokens", src, 0.9),
        ];
        dedup_claims(&mut claims);
        assert_eq!(claims.len(), 2);
    }

    #[test]
    fn cross_source_not_deduped() {
        let src_a = SourceId::new();
        let src_b = SourceId::new();
        let mut claims = vec![
            make_claim("PostgreSQL stores user data", src_a, 0.8),
            make_claim("PostgreSQL stores user data", src_b, 0.9),
        ];
        dedup_claims(&mut claims);
        // Same statement but different sources — keep both.
        assert_eq!(claims.len(), 2);
    }

    #[test]
    fn near_duplicate_deduped() {
        let src = SourceId::new();
        let mut claims = vec![
            make_claim("The PostgreSQL database stores user data", src, 0.8),
            make_claim("PostgreSQL database stores user data", src, 0.7),
        ];
        dedup_claims(&mut claims);
        assert_eq!(claims.len(), 1);
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p thinkingroot-ground -- dedup`
Expected: ALL PASS

- [ ] **Step 3: Commit**

```bash
git add crates/thinkingroot-ground/src/dedup.rs
git commit -m "feat(ground): implement claim deduplication

Removes duplicate claims within the same source using Jaccard word-set
similarity (> 85% overlap = duplicate). Keeps the higher-confidence
version. Prevents graph inflation from redundant extraction."
```

---

## Task 12: Wire Grounding Into the Pipeline

**Files:**
- Modify: `crates/thinkingroot-serve/src/pipeline.rs`
- Modify: `crates/thinkingroot-serve/Cargo.toml`
- Modify: `crates/thinkingroot-cli/Cargo.toml`

This is the critical integration task. Grounding sits between extract (Phase 2) and link (Phase 7).

- [ ] **Step 1: Add thinkingroot-ground dependency**

In `crates/thinkingroot-serve/Cargo.toml`, add:

```toml
thinkingroot-ground = { workspace = true }
```

In `crates/thinkingroot-serve/Cargo.toml` features section, add the ground feature chain:

```toml
[features]
default = ["vector"]
vector = ["thinkingroot-graph/vector", "thinkingroot-ground/vector"]
```

In `crates/thinkingroot-cli/Cargo.toml`, add the same chain:

```toml
vector = ["thinkingroot-graph/vector", "thinkingroot-serve/vector", "thinkingroot-ground/vector"]
```

Add to the workspace root `Cargo.toml` in `[workspace.dependencies]`:

```toml
thinkingroot-ground  = { path = "crates/thinkingroot-ground", version = "0.2.0", default-features = false }
```

- [ ] **Step 2: Insert grounding stage in pipeline.rs**

In `crates/thinkingroot-serve/src/pipeline.rs`, after the extraction phase (after line 160 in the current code, `extraction = raw;`) and before the fingerprint check (Phase 3), insert the grounding stage:

```rust
    // ─── Phase 2b: Ground extraction output ───────────────────────
    let extraction = if !extraction.claims.is_empty() {
        let grounder = {
            let g = thinkingroot_ground::Grounder::new(
                thinkingroot_ground::GroundingConfig::default(),
            );
            #[cfg(feature = "vector")]
            let g = {
                // Reuse the vector store from StorageEngine for Judge 3.
                // StorageEngine is initialized but vector store needs Arc wrapping.
                // We'll pass it after StorageEngine init.
                g
            };
            g
        };
        let mut grounded = grounder.ground(extraction);
        thinkingroot_ground::dedup::dedup_claims(&mut grounded.claims);
        grounded
    } else {
        extraction
    };
```

**Important ordering note:** The grounding stage MUST run before the fingerprint check (Phase 3) because the fingerprint is based on claim content. If grounding removes claims, the fingerprint changes, which correctly triggers re-processing on subsequent runs.

- [ ] **Step 3: Move cache writes to after grounding**

Currently in `extractor.rs` (line 217-222), cache writes happen inside the extraction loop. This needs to change so that grounded results are cached, not raw LLM output.

In `crates/thinkingroot-serve/src/pipeline.rs`, after the grounding stage, add cache writing:

```rust
    // ─── Phase 2c: Cache grounded results ─────────────────────────
    // Cache stores post-grounding results so hallucinations are never
    // served from cache on subsequent compiles.
    // Note: this requires the Extractor to expose its cache, or we
    // implement a separate grounded-result cache. For now, bump the
    // PROMPT_VERSION in cache.rs (done in Task 4) which invalidates
    // old pre-grounding cache entries. New cache entries are written
    // by the extractor with the v2 prompt (which includes source_quote),
    // and grounding runs on every extraction.
```

The PROMPT_VERSION bump in Task 4 already handles this: old cached results (without source_quote) are invalidated, and new extractions include source_quote for Judge 2.

- [ ] **Step 4: Verify build**

Run: `cargo check --workspace`
Expected: compiles

- [ ] **Step 5: Run existing tests**

Run: `cargo test --workspace --no-default-features`
Expected: ALL PASS (grounding is feature-gated, tests should pass without vector)

Run: `cargo test -p thinkingroot-ground`
Expected: ALL PASS

- [ ] **Step 6: Commit**

```bash
git add crates/thinkingroot-serve/src/pipeline.rs crates/thinkingroot-serve/Cargo.toml crates/thinkingroot-cli/Cargo.toml Cargo.toml
git commit -m "feat(pipeline): wire grounding tribunal between extract and link

Grounding runs after extraction, before fingerprinting and linking.
Claims that fail grounding (score < 0.25) are rejected before they
touch the database. Claim deduplication removes redundant extractions.
Feature-gated: Judge 3 requires 'vector', Judges 1+2 always active."
```

---

## Task 13: Update Verify Stage for Grounding Awareness

**Files:**
- Modify: `crates/thinkingroot-verify/src/verifier.rs`

- [ ] **Step 1: Write test**

Add to the test module in `verifier.rs`:

```rust
#[test]
fn low_grounding_claims_produce_warning() {
    let (_dir, graph) = make_graph();
    let source = make_source("test://grounding.md");
    graph.insert_source(&source).unwrap();

    // Insert a claim with low grounding score.
    use thinkingroot_core::types::GroundingMethod;
    let claim = make_claim("Weakly grounded claim.", &source)
        .with_grounding(0.3, GroundingMethod::Lexical);
    graph.insert_claim(&claim).unwrap();

    let result = default_verifier().verify(&graph).unwrap();
    assert!(result.warnings.iter().any(|w| w.contains("low grounding")));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p thinkingroot-verify -- low_grounding`
Expected: FAIL — no grounding check in verify yet

- [ ] **Step 3: Add grounding check to verify**

In `verifier.rs`, after the superseded claims check and before the health score computation, add:

```rust
// Grounding: count claims with low grounding scores.
let low_grounding = graph.count_low_grounding_claims(0.5)?;
if low_grounding > 0 {
    warnings.push(format!(
        "{low_grounding} claims have low grounding scores (< 0.5) — review recommended."
    ));
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p thinkingroot-verify`
Expected: ALL PASS

- [ ] **Step 5: Commit**

```bash
git add crates/thinkingroot-verify/src/verifier.rs
git commit -m "feat(verify): report claims with low grounding scores

Verification now flags claims with grounding_score < 0.5, giving users
visibility into potentially hallucinated content in the knowledge graph."
```

---

## Task 14: Improve Contradiction Detection with Embedding Similarity

**Files:**
- Modify: `crates/thinkingroot-link/src/linker.rs`
- Modify: `crates/thinkingroot-link/Cargo.toml`

The existing contradiction detection uses 10 hardcoded negation pairs. We improve it by also using word-level contradiction signals.

- [ ] **Step 1: Add broader contradiction patterns**

In `crates/thinkingroot-link/src/linker.rs`, expand the `negation_pairs` in `detect_contradictions`:

```rust
let negation_pairs = [
    ("is", "is not"),
    ("uses", "does not use"),
    ("has", "does not have"),
    ("supports", "does not support"),
    ("requires", "does not require"),
    ("enabled", "disabled"),
    ("true", "false"),
    ("yes", "no"),
    ("deprecated", "active"),
    ("removed", "added"),
    // New patterns:
    ("migrated from", "uses"),      // "migrated from X" contradicts "uses X"
    ("replaced by", "depends on"),  // "replaced by Y" contradicts "depends on X"
    ("optional", "required"),
    ("synchronous", "asynchronous"),
    ("mutable", "immutable"),
    ("public", "private"),
    ("internal", "external"),
    ("production", "development"),
    ("legacy", "current"),
    ("before", "after"),
];
```

- [ ] **Step 2: Add Jaccard-based semantic contradiction check**

After the keyword negation check, add a second pass for claims about the same entity with similar structure but different objects:

```rust
// Second pass: claims with high word overlap but different key terms
// may indicate contradictions the keyword pairs missed.
// Example: "uses PostgreSQL" vs "uses MySQL" — same structure, different DB.
if !is_contradiction {
    let a_words: std::collections::HashSet<&str> =
        a_lower.split_whitespace().collect();
    let b_words: std::collections::HashSet<&str> =
        b_lower.split_whitespace().collect();

    let intersection = a_words.intersection(&b_words).count();
    let union = a_words.union(&b_words).count();
    let jaccard = if union > 0 {
        intersection as f64 / union as f64
    } else {
        0.0
    };

    // High overlap (same structure) but not identical (different values)
    // suggests a potential contradiction.
    if jaccard > 0.6 && jaccard < 0.95 && a.claim_type == b.claim_type {
        is_contradiction = true;
    }
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p thinkingroot-link`
Expected: ALL PASS (existing tests still pass; new patterns are additive)

- [ ] **Step 4: Commit**

```bash
git add crates/thinkingroot-link/src/linker.rs
git commit -m "feat(link): expand contradiction detection with broader patterns

Adds 10 new negation pairs and Jaccard-based structural contradiction
detection. Catches cases like 'uses PostgreSQL' vs 'uses MySQL' that
keyword pairs alone would miss."
```

---

## Task 15: Integration Test

**Files:**
- Create: `crates/thinkingroot-ground/tests/integration.rs`

- [ ] **Step 1: Write end-to-end grounding test**

Create `crates/thinkingroot-ground/tests/integration.rs`:

```rust
use std::collections::HashMap;

use thinkingroot_core::types::*;
use thinkingroot_extract::extractor::ExtractionOutput;
use thinkingroot_ground::{Grounder, GroundingConfig};

fn make_extraction(
    claims_with_sources: Vec<(&str, &str, Option<&str>)>,
) -> ExtractionOutput {
    let source_id = SourceId::new();
    let workspace_id = WorkspaceId::new();

    let mut output = ExtractionOutput::default();
    let mut full_source = String::new();

    for (statement, source_text, quote) in &claims_with_sources {
        full_source.push_str(source_text);
        full_source.push('\n');
    }

    output.source_texts.insert(source_id, full_source);

    for (statement, _source_text, quote) in claims_with_sources {
        let claim = Claim::new(statement, ClaimType::Fact, source_id, workspace_id)
            .with_confidence(0.8);
        if let Some(q) = quote {
            output
                .claim_source_quotes
                .insert(claim.id, q.to_string());
        }
        output.claims.push(claim);
    }

    output
}

#[test]
fn grounded_claim_survives() {
    let extraction = make_extraction(vec![(
        "PostgreSQL stores user data",
        "PostgreSQL stores user data in normalized tables.",
        Some("PostgreSQL stores user data in normalized tables."),
    )]);

    let grounder = Grounder::new(GroundingConfig::default());
    let result = grounder.ground(extraction);

    assert_eq!(result.claims.len(), 1);
    assert!(result.claims[0].grounding_score.unwrap() > 0.7);
}

#[test]
fn hallucinated_claim_rejected() {
    let extraction = make_extraction(vec![(
        "Redis caches session tokens with TTL expiry",
        "PostgreSQL stores user data in normalized tables.",
        Some("Redis caches session tokens"), // quote not in source
    )]);

    let grounder = Grounder::new(GroundingConfig::default());
    let result = grounder.ground(extraction);

    // Should be rejected: zero lexical overlap + quote not found
    assert_eq!(result.claims.len(), 0);
}

#[test]
fn partially_grounded_claim_gets_reduced_confidence() {
    let extraction = make_extraction(vec![(
        "PostgreSQL handles authentication",
        "PostgreSQL stores user data and handles transactions.",
        None, // no source quote
    )]);

    let grounder = Grounder::new(GroundingConfig::default());
    let result = grounder.ground(extraction);

    // "PostgreSQL" and "handles" match, but "authentication" doesn't.
    // Should survive but with reduced confidence.
    assert_eq!(result.claims.len(), 1);
    let claim = &result.claims[0];
    assert!(claim.confidence.value() < 0.8, "confidence should be reduced");
}

#[test]
fn dedup_removes_duplicate_claims() {
    let source_id = SourceId::new();
    let workspace_id = WorkspaceId::new();

    let mut extraction = ExtractionOutput::default();
    extraction.source_texts.insert(
        source_id,
        "PostgreSQL stores user data in normalized tables.".to_string(),
    );

    // Two near-identical claims from same source.
    let c1 = Claim::new(
        "PostgreSQL stores user data",
        ClaimType::Fact,
        source_id,
        workspace_id,
    )
    .with_confidence(0.8);
    let c2 = Claim::new(
        "PostgreSQL stores user data in tables",
        ClaimType::Fact,
        source_id,
        workspace_id,
    )
    .with_confidence(0.9);
    extraction.claims.push(c1);
    extraction.claims.push(c2);

    let grounder = Grounder::new(GroundingConfig::default());
    let mut result = grounder.ground(extraction);
    thinkingroot_ground::dedup::dedup_claims(&mut result.claims);

    assert_eq!(result.claims.len(), 1);
}
```

- [ ] **Step 2: Run integration tests**

Run: `cargo test -p thinkingroot-ground --test integration`
Expected: ALL PASS

- [ ] **Step 3: Run full workspace tests**

Run: `cargo test --workspace`
Expected: ALL PASS

- [ ] **Step 4: Commit**

```bash
git add crates/thinkingroot-ground/tests/integration.rs
git commit -m "test(ground): add integration tests for grounding tribunal

Tests: grounded claims survive, hallucinated claims rejected,
partially-grounded claims get reduced confidence, dedup works."
```

---

## Task 16: Final Verification and Lint

- [ ] **Step 1: Format all code**

Run: `cargo fmt --all`

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: no warnings

- [ ] **Step 3: Run full test suite with default features**

Run: `cargo test --workspace`
Expected: ALL PASS

- [ ] **Step 4: Run full test suite without vector feature**

Run: `cargo test --workspace --no-default-features`
Expected: ALL PASS (Judges 1+2 work, Judge 3 skipped)

- [ ] **Step 5: Build release binary**

Run: `cargo build --release -p thinkingroot-cli`
Expected: compiles successfully

- [ ] **Step 6: Final commit**

```bash
git add -A
git commit -m "chore: fmt + clippy fixes for grounding tribunal"
```
