# Graph Quality — Six Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate the six structural quality problems in the ThinkingRoot knowledge graph: RelatedTo overuse, duplicate relations, weak structural extraction, uniform strength, missing relation context injection, and cross-file incremental staleness.

**Architecture:** Six independent tasks executed in dependency order. Tasks 1–3 fix the extraction layer (prompt, strength model, relation dedup). Tasks 4–5 fix the structural extractor and graph-primed context. Task 6 fixes incremental pipeline staleness. Each task produces tests and commits independently.

**Tech Stack:** Rust 2024 edition, tree-sitter (code parsing), CozoDB Datalog (graph storage), tokio (async), serde_json (LLM output parsing)

---

## File Map

| File | Change |
|---|---|
| `crates/thinkingroot-extract/src/schema.rs` | Add `confidence: f64` to `ExtractedRelation` |
| `crates/thinkingroot-extract/src/prompts.rs` | New SYSTEM_PROMPT: type definitions, ban RelatedTo, add confidence field |
| `crates/thinkingroot-extract/src/extractor.rs` | `parse_relation_type` returns `Option<RelationType>`, filter None + low-confidence |
| `crates/thinkingroot-graph/src/graph.rs` | Noisy-OR in `update_entity_relations_for_triples`, `get_known_relations`, `get_all_triples_involving_entities` |
| `crates/thinkingroot-link/src/relation_dedup.rs` | **NEW** — subsumption DAG + dedup logic |
| `crates/thinkingroot-link/src/lib.rs` | Export `relation_dedup` module |
| `crates/thinkingroot-link/src/linker.rs` | Call `relation_dedup::dedup_relations` in Phase 3 |
| `crates/thinkingroot-core/src/ir.rs` | Add `trait_name: Option<String>` and `field_types: Vec<String>` to `ChunkMetadata` |
| `crates/thinkingroot-parse/src/code.rs` | Populate `trait_name` for `impl_item`, `field_types` for `struct_item` |
| `crates/thinkingroot-extract/src/structural.rs` | Emit `implements` from `trait_name`, `depends_on` from `field_types` |
| `crates/thinkingroot-extract/src/graph_context.rs` | Add `KnownRelation`, extend `GraphPrimedContext`, update `prompt_section()` |
| `crates/thinkingroot-serve/src/pipeline.rs` | Load `known_relations`, pass to extractor; extend Phase 4 with cross-file triples |

---

## Task 1: Harden the Extraction Prompt — Ban RelatedTo Fallback

**Problem:** The LLM defaults to `related_to` when uncertain. Unknown relation type strings silently map to `RelatedTo` via the fallback arm in `parse_relation_type`. No confidence field exists on relations.

**Fix:** Add `confidence` to `ExtractedRelation`, rewrite `SYSTEM_PROMPT` with explicit type definitions + ban on `related_to`, change `parse_relation_type` to return `Option<RelationType>` (returning `None` for unknowns), filter `None` and low-confidence relations at conversion time.

**Files:**
- Modify: `crates/thinkingroot-extract/src/schema.rs`
- Modify: `crates/thinkingroot-extract/src/prompts.rs`
- Modify: `crates/thinkingroot-extract/src/extractor.rs`

- [ ] **Step 1: Write the failing tests**

Add to `crates/thinkingroot-extract/src/extractor.rs` in the `#[cfg(test)]` mod:

```rust
#[test]
fn unknown_relation_type_is_rejected_not_mapped_to_related_to() {
    // Previously "blah_relation" would silently become RelatedTo.
    // Now it must return None and be filtered out.
    let result = parse_relation_type("blah_relation");
    assert!(result.is_none(), "unknown types must be rejected, not silently mapped");
}

#[test]
fn skip_relation_is_rejected() {
    assert!(parse_relation_type("skip_relation").is_none());
    assert!(parse_relation_type("SKIP_RELATION").is_none());
    assert!(parse_relation_type("").is_none());
}

#[test]
fn known_types_still_parse() {
    assert_eq!(parse_relation_type("depends_on"), Some(RelationType::DependsOn));
    assert_eq!(parse_relation_type("calls"), Some(RelationType::Calls));
    assert_eq!(parse_relation_type("implements"), Some(RelationType::Implements));
    // related_to is still valid when LLM explicitly chooses it
    assert_eq!(parse_relation_type("related_to"), Some(RelationType::RelatedTo));
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cd /Users/naveen/Desktop/thinkingroot
cargo test -p thinkingroot-extract unknown_relation_type_is_rejected --no-default-features 2>&1 | tail -5
```
Expected: FAIL — `parse_relation_type` still returns `RelationType` not `Option<RelationType>`

- [ ] **Step 3: Add `confidence` to `ExtractedRelation` in schema.rs**

Replace the `ExtractedRelation` struct in `crates/thinkingroot-extract/src/schema.rs`:

```rust
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
```

- [ ] **Step 4: Rewrite `SYSTEM_PROMPT` in prompts.rs**

Replace the entire `SYSTEM_PROMPT` constant in `crates/thinkingroot-extract/src/prompts.rs`:

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
      "relation_type": "<see allowed types below>",
      "confidence": 0.0-1.0,
      "description": "One sentence describing why this relation exists"
    }
  ]
}

## Allowed relation_type values (use EXACTLY one, no other values):

- depends_on    — A cannot function without B (hard runtime dependency)
- calls         — A invokes B as a function or API at runtime
- implements    — A implements interface/trait/protocol B
- uses          — A uses B as a tool or library (soft dependency)
- contains      — A is a container that includes B as a sub-component
- part_of       — A is a sub-component of B (inverse of contains)
- owned_by      — A is maintained or owned by person/team B
- created_by    — A was originally authored by B
- configured_by — A's behaviour is controlled by configuration B
- tested_by     — A has test coverage provided by test suite B
- replaces      — A supersedes or replaces B
- contradicts   — A and B make conflicting assertions
- related_to    — use ONLY when none of the above apply AND you are confident a relationship exists

## Critical rules:
1. NEVER output related_to as a lazy default. If you are uncertain what the relation is, output "skip_relation" instead.
2. If you output "skip_relation" for relation_type, the relation will be discarded — this is correct behaviour for uncertain relations.
3. confidence for relations: 0.9=directly stated in code/text, 0.7=strongly implied, 0.5=inferred, below 0.3=output skip_relation instead.
4. Claims must be ATOMIC — one fact per claim.
5. Claims must be SELF-CONTAINED — understandable without the source.
6. Every entity in a claim MUST appear in the entities list.
7. source_quote MUST be a verbatim substring copied from the source. Do NOT paraphrase.
8. Return ONLY the JSON object. No markdown, no explanation, no preamble."#;
```

- [ ] **Step 5: Change `parse_relation_type` to return `Option<RelationType>`**

Replace the function in `crates/thinkingroot-extract/src/extractor.rs` (currently at line ~576):

```rust
fn parse_relation_type(s: &str) -> Option<RelationType> {
    match s.to_lowercase().trim() {
        "depends_on"    => Some(RelationType::DependsOn),
        "owned_by"      => Some(RelationType::OwnedBy),
        "replaces"      => Some(RelationType::Replaces),
        "contradicts"   => Some(RelationType::Contradicts),
        "implements"    => Some(RelationType::Implements),
        "uses"          => Some(RelationType::Uses),
        "contains"      => Some(RelationType::Contains),
        "created_by"    => Some(RelationType::CreatedBy),
        "part_of"       => Some(RelationType::PartOf),
        "related_to"    => Some(RelationType::RelatedTo),
        "calls"         => Some(RelationType::Calls),
        "configured_by" => Some(RelationType::ConfiguredBy),
        "tested_by"     => Some(RelationType::TestedBy),
        // Explicit skip signal or unknown type: discard the relation entirely.
        // Previously these silently became RelatedTo — that caused graph noise.
        "skip_relation" | "" => None,
        _               => None,
    }
}
```

- [ ] **Step 6: Update `convert_result_static` to filter None and low-confidence**

In `crates/thinkingroot-extract/src/extractor.rs`, replace the relation conversion block inside `convert_result_static` (around line ~396):

```rust
// Convert relations — filter unknown types and low-confidence ones.
for ext_rel in &result.relations {
    let from_id = entity_map.get(&ext_rel.from_entity.to_lowercase());
    let to_id = entity_map.get(&ext_rel.to_entity.to_lowercase());

    if let (Some(&from), Some(&to)) = (from_id, to_id) {
        // Reject unknown relation types (returns None) and explicit SKIP.
        let Some(rel_type) = parse_relation_type(&ext_rel.relation_type) else {
            tracing::debug!(
                "discarded relation '{}' → '{}' with unknown type '{}'",
                ext_rel.from_entity, ext_rel.to_entity, ext_rel.relation_type
            );
            continue;
        };

        // Reject low-confidence relations (LLM was too uncertain).
        let confidence = ext_rel.confidence.clamp(0.0, 1.0);
        if confidence < 0.3 {
            tracing::debug!(
                "discarded low-confidence relation '{}' → '{}' ({:.2})",
                ext_rel.from_entity, ext_rel.to_entity, confidence
            );
            continue;
        }

        let rel = Relation::new(from, to, rel_type)
            .with_strength(confidence)
            .with_description(ext_rel.description.clone().unwrap_or_default());
        output.relations.push(SourcedRelation {
            source: source_id,
            relation: rel,
        });
    }
}
```

- [ ] **Step 7: Run tests to verify they pass**

```bash
cargo test -p thinkingroot-extract --no-default-features 2>&1 | grep -E "^test result|FAILED"
```
Expected: `test result: ok` — all tests pass including the three new ones.

- [ ] **Step 8: Commit**

```bash
git add crates/thinkingroot-extract/src/schema.rs \
        crates/thinkingroot-extract/src/prompts.rs \
        crates/thinkingroot-extract/src/extractor.rs
git commit -m "feat(extract): harden relation extraction — confidence field, ban RelatedTo fallback, reject unknown types"
```

---

## Task 2: Noisy-OR Strength Aggregation

**Problem:** `update_entity_relations_for_triples` uses `max(strength)` across sources. This means one high-confidence source dominates and multiple corroborating sources provide no extra signal. Noisy-OR is the correct formula: each independent source adds evidence, with diminishing returns.

**Formula:** `strength = 1 − ∏(1 − s_i)` where `s_i` are per-source strengths for the triple.

**Files:**
- Modify: `crates/thinkingroot-graph/src/graph.rs`

- [ ] **Step 1: Write the failing test**

Add to `crates/thinkingroot-graph/src/graph.rs` in the `#[cfg(test)]` mod:

```rust
#[test]
fn noisy_or_combines_multiple_sources_stronger_than_max() {
    let store = mem_store();

    let e1 = thinkingroot_core::Entity::new("A", thinkingroot_core::types::EntityType::Service);
    let e2 = thinkingroot_core::Entity::new("B", thinkingroot_core::types::EntityType::Service);
    store.insert_entity(&e1).unwrap();
    store.insert_entity(&e2).unwrap();

    let eid1 = e1.id.to_string();
    let eid2 = e2.id.to_string();

    let src_a = thinkingroot_core::Source::new("test://a.rs".into(), thinkingroot_core::types::SourceType::File);
    let src_b = thinkingroot_core::Source::new("test://b.rs".into(), thinkingroot_core::types::SourceType::File);
    let src_c = thinkingroot_core::Source::new("test://c.rs".into(), thinkingroot_core::types::SourceType::File);
    store.insert_source(&src_a).unwrap();
    store.insert_source(&src_b).unwrap();
    store.insert_source(&src_c).unwrap();

    // Three sources each with strength 0.5.
    // MAX would give 0.5.
    // Noisy-OR gives 1 - (1-0.5)^3 = 1 - 0.125 = 0.875.
    store.link_entities_for_source(&src_a.id.to_string(), &eid1, &eid2, "Uses", 0.5).unwrap();
    store.link_entities_for_source(&src_b.id.to_string(), &eid1, &eid2, "Uses", 0.5).unwrap();
    store.link_entities_for_source(&src_c.id.to_string(), &eid1, &eid2, "Uses", 0.5).unwrap();

    // Trigger aggregation.
    let triples = vec![
        (eid1.clone(), eid2.clone(), "Uses".to_string()),
    ];
    store.update_entity_relations_for_triples(&triples).unwrap();

    let relations = store.get_all_relations().unwrap();
    assert_eq!(relations.len(), 1);
    let (_, _, _, _, _, strength) = &relations[0];
    // Must be greater than 0.5 (the max) — noisy-OR gives ~0.875
    assert!(
        *strength > 0.5,
        "noisy-OR with 3 sources of 0.5 should produce strength > 0.5, got {strength}"
    );
    assert!(
        (*strength - 0.875).abs() < 0.01,
        "expected ~0.875 from noisy-OR, got {strength}"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test -p thinkingroot-graph noisy_or_combines_multiple_sources --no-default-features 2>&1 | tail -5
```
Expected: FAIL — current code uses `max(strength)` which gives 0.5, not 0.875.

- [ ] **Step 3: Replace MAX with noisy-OR in `update_entity_relations_for_triples`**

In `crates/thinkingroot-graph/src/graph.rs`, replace the `update_entity_relations_for_triples` method body. Find the section that does re-aggregation after removing the stale edge (around line 572–596) and replace the entire per-triple loop body:

```rust
pub fn update_entity_relations_for_triples(
    &self,
    triples: &[(String, String, String)],
) -> Result<()> {
    for (from_id, to_id, relation_type) in triples {
        // Remove stale aggregated edge.
        let mut params = BTreeMap::new();
        params.insert("fid".into(), DataValue::Str(from_id.clone().into()));
        params.insert("tid".into(), DataValue::Str(to_id.clone().into()));
        params.insert("rtype".into(), DataValue::Str(relation_type.clone().into()));
        self.query(
            r#"?[from_id, to_id, relation_type] <- [[$fid, $tid, $rtype]]
            :rm entity_relations {from_id, to_id, relation_type}"#,
            params.clone(),
        )?;

        // Re-aggregate using noisy-OR: 1 − ∏(1 − s_i)
        // Fetch all per-source strengths for this triple.
        let result = self
            .db
            .run_script(
                "?[strength] := *source_entity_relations{from_id: $fid, to_id: $tid, relation_type: $rtype, strength}",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))?;

        if result.rows.is_empty() {
            // No sources remain — edge stays deleted.
            continue;
        }

        // Compute noisy-OR across all source strengths.
        let complement_product = result.rows.iter().fold(1.0_f64, |acc, row| {
            let s = match &row[0] {
                DataValue::Num(Num::Float(f)) => f.clamp(0.0, 1.0),
                DataValue::Num(Num::Int(i)) => (*i as f64).clamp(0.0, 1.0),
                _ => 0.0,
            };
            acc * (1.0 - s)
        });
        let noisy_or_strength = (1.0 - complement_product).clamp(0.0, 1.0);

        self.link_entities(from_id, to_id, relation_type, noisy_or_strength)?;
    }
    Ok(())
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p thinkingroot-graph --no-default-features 2>&1 | grep -E "^test result|FAILED"
```
Expected: `test result: ok`

- [ ] **Step 5: Commit**

```bash
git add crates/thinkingroot-graph/src/graph.rs
git commit -m "feat(graph): noisy-OR strength aggregation — multiple corroborating sources now combine correctly"
```

---

## Task 3: Relation Type Subsumption Deduplication

**Problem:** The linker writes both `Uses` and `DependsOn` for the same `(A, B)` entity pair when the LLM extracts both. `DependsOn` is strictly more specific than `Uses`. The graph should keep only the most specific relation type per pair.

**Subsumption hierarchy (most-specific wins):**
```
RelatedTo
  ├── Uses
  │     ├── DependsOn
  │     └── Calls
  ├── Contains
  │     └── PartOf
  └── CreatedBy
        └── OwnedBy
```

**Files:**
- Create: `crates/thinkingroot-link/src/relation_dedup.rs`
- Modify: `crates/thinkingroot-link/src/lib.rs`
- Modify: `crates/thinkingroot-link/src/linker.rs`

- [ ] **Step 1: Write the failing test (in the new file)**

Create `crates/thinkingroot-link/src/relation_dedup.rs` with the test first:

```rust
use thinkingroot_core::types::{EntityId, RelationType, Relation};
use thinkingroot_extract::extractor::SourcedRelation;
use thinkingroot_core::types::SourceId;

/// Specificity rank: higher = more specific.
/// Two relations with the same rank in different subtrees are orthogonal (both kept).
pub fn specificity_rank(r: RelationType) -> u8 {
    match r {
        RelationType::RelatedTo  => 0,
        RelationType::Uses       => 1,
        RelationType::Contains   => 1,
        RelationType::CreatedBy  => 1,
        RelationType::OwnedBy    => 2,  // more specific than CreatedBy
        RelationType::DependsOn  => 2,  // more specific than Uses
        RelationType::Calls      => 2,  // more specific than Uses
        RelationType::PartOf     => 2,  // more specific than Contains
        RelationType::Implements => 2,
        RelationType::TestedBy   => 2,
        RelationType::ConfiguredBy => 2,
        RelationType::Replaces   => 2,
        RelationType::Contradicts => 2,
    }
}

/// Returns true if `general` subsumes `specific` — meaning both describe the
/// same semantic concept but `specific` is more precise.
/// Only true within the same subtree (Uses→DependsOn, not Uses→PartOf).
pub fn subsumes(general: RelationType, specific: RelationType) -> bool {
    matches!(
        (general, specific),
        (RelationType::RelatedTo, _)
            | (RelationType::Uses, RelationType::DependsOn)
            | (RelationType::Uses, RelationType::Calls)
            | (RelationType::Contains, RelationType::PartOf)
            | (RelationType::CreatedBy, RelationType::OwnedBy)
    )
}

/// Deduplicate a list of sourced relations:
/// - For any `(from, to)` pair with multiple relation types, keep only the
///   most specific type (per subsumption hierarchy).
/// - If two types are orthogonal (different subtrees), both are kept.
/// - If two entries have the same type and same (from, to), keep the higher-strength one.
pub fn dedup_relations(relations: &mut Vec<SourcedRelation>) {
    use std::collections::HashMap;

    // Key: (from_entity_id, to_entity_id) → Vec<index into relations>
    let mut pair_map: HashMap<(String, String), Vec<usize>> = HashMap::new();
    for (i, sr) in relations.iter().enumerate() {
        let key = (sr.relation.from.to_string(), sr.relation.to.to_string());
        pair_map.entry(key).or_default().push(i);
    }

    let mut to_remove: Vec<usize> = Vec::new();

    for indices in pair_map.values() {
        if indices.len() < 2 {
            continue;
        }
        // For each pair of entries with the same (from, to), check subsumption.
        for i in 0..indices.len() {
            for j in 0..indices.len() {
                if i == j { continue; }
                let idx_i = indices[i];
                let idx_j = indices[j];
                if to_remove.contains(&idx_i) || to_remove.contains(&idx_j) { continue; }

                let type_i = relations[idx_i].relation.relation_type;
                let type_j = relations[idx_j].relation.relation_type;

                // If i subsumes j (j is more specific), remove i (keep the specific one).
                if subsumes(type_i, type_j) {
                    to_remove.push(idx_i);
                }
            }
        }
    }

    to_remove.sort_unstable();
    to_remove.dedup();
    // Remove in reverse order to preserve indices.
    for idx in to_remove.into_iter().rev() {
        relations.remove(idx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thinkingroot_core::types::{Relation, RelationType, EntityId, SourceId};
    use thinkingroot_extract::extractor::SourcedRelation;

    fn make_relation(from: EntityId, to: EntityId, rel: RelationType, strength: f64) -> SourcedRelation {
        SourcedRelation {
            source: SourceId::new(),
            relation: Relation::new(from, to, rel).with_strength(strength),
        }
    }

    #[test]
    fn dedup_removes_uses_when_depends_on_exists_for_same_pair() {
        let e1 = EntityId::new();
        let e2 = EntityId::new();
        let mut relations = vec![
            make_relation(e1, e2, RelationType::Uses, 0.8),
            make_relation(e1, e2, RelationType::DependsOn, 0.9),
        ];
        dedup_relations(&mut relations);
        assert_eq!(relations.len(), 1, "should keep only DependsOn");
        assert_eq!(relations[0].relation.relation_type, RelationType::DependsOn);
    }

    #[test]
    fn dedup_keeps_orthogonal_types_for_same_pair() {
        let e1 = EntityId::new();
        let e2 = EntityId::new();
        let mut relations = vec![
            make_relation(e1, e2, RelationType::DependsOn, 0.9),
            make_relation(e1, e2, RelationType::TestedBy, 0.8),
        ];
        dedup_relations(&mut relations);
        assert_eq!(relations.len(), 2, "orthogonal types for same pair must both survive");
    }

    #[test]
    fn dedup_removes_related_to_when_specific_type_exists() {
        let e1 = EntityId::new();
        let e2 = EntityId::new();
        let mut relations = vec![
            make_relation(e1, e2, RelationType::RelatedTo, 0.5),
            make_relation(e1, e2, RelationType::Calls, 0.9),
        ];
        dedup_relations(&mut relations);
        assert_eq!(relations.len(), 1);
        assert_eq!(relations[0].relation.relation_type, RelationType::Calls);
    }

    #[test]
    fn dedup_does_not_touch_different_pairs() {
        let e1 = EntityId::new();
        let e2 = EntityId::new();
        let e3 = EntityId::new();
        let mut relations = vec![
            make_relation(e1, e2, RelationType::Uses, 0.8),
            make_relation(e1, e3, RelationType::DependsOn, 0.9),
        ];
        dedup_relations(&mut relations);
        assert_eq!(relations.len(), 2, "different pairs must not affect each other");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p thinkingroot-link --no-default-features 2>&1 | tail -5
```
Expected: compilation error — `relation_dedup` module doesn't exist yet.

- [ ] **Step 3: Export the module in lib.rs**

Add to `crates/thinkingroot-link/src/lib.rs`:

```rust
pub mod linker;
pub mod relation_dedup;
pub mod resolution;

pub use linker::{EntityProgressFn, LinkOutput, Linker};
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p thinkingroot-link --no-default-features 2>&1 | grep -E "^test result|FAILED"
```
Expected: `test result: ok`

- [ ] **Step 5: Call `dedup_relations` in the linker Phase 3**

In `crates/thinkingroot-link/src/linker.rs`, at the start of Phase 3 (before the `for sourced_relation in &extraction.relations` loop at line ~124), add:

```rust
// Phase 3: Link relations (with resolved entity IDs).
// Deduplicate first: keep most-specific type per (from, to) pair.
let mut deduped_relations = extraction.relations.clone();
crate::relation_dedup::dedup_relations(&mut deduped_relations);
let removed = extraction.relations.len().saturating_sub(deduped_relations.len());
if removed > 0 {
    tracing::debug!("relation subsumption dedup: removed {} redundant relations", removed);
}

for sourced_relation in &deduped_relations {
    // ... (rest of existing Phase 3 loop unchanged)
```

Also update `output.relations_linked` to count from `deduped_relations`:
```rust
output.relations_linked += 1;  // this is inside the loop, already correct
```

- [ ] **Step 6: Run all link tests**

```bash
cargo test -p thinkingroot-link --no-default-features 2>&1 | grep -E "^test result|FAILED"
```
Expected: `test result: ok`

- [ ] **Step 7: Commit**

```bash
git add crates/thinkingroot-link/src/relation_dedup.rs \
        crates/thinkingroot-link/src/lib.rs \
        crates/thinkingroot-link/src/linker.rs
git commit -m "feat(link): relation type subsumption dedup — DependsOn beats Uses, Calls beats Uses, PartOf beats Contains"
```

---

## Task 4: Richer Structural Extraction — impl→implements, struct fields→depends_on

**Problem:** The structural extractor only emits `contains` and `uses`. Tree-sitter already parses `impl Trait for Struct` blocks and struct field declarations — we just don't read the trait name or field types. Emitting `implements` and `depends_on` from AST is zero-LLM and 0.99 confidence.

**Files:**
- Modify: `crates/thinkingroot-core/src/ir.rs`
- Modify: `crates/thinkingroot-parse/src/code.rs`
- Modify: `crates/thinkingroot-extract/src/structural.rs`

- [ ] **Step 1: Write failing tests**

Add to `crates/thinkingroot-extract/src/structural.rs` in `#[cfg(test)]` mod:

```rust
#[test]
fn impl_with_trait_produces_implements_relation() {
    use thinkingroot_core::ir::{Chunk, ChunkMetadata, ChunkType};

    let mut chunk = Chunk::new(
        "impl Serialize for MyStruct {}",
        ChunkType::TypeDef,
        1, 1,
    );
    chunk.metadata = ChunkMetadata {
        type_name: Some("MyStruct".to_string()),
        trait_name: Some("Serialize".to_string()),
        ..Default::default()
    };

    let result = extract_structural(&chunk, "src/models.rs");

    let implements = result.relations.iter()
        .find(|r| r.relation_type == "implements");
    assert!(
        implements.is_some(),
        "impl Trait for Struct must produce an implements relation"
    );
    let rel = implements.unwrap();
    assert_eq!(rel.from_entity, "MyStruct");
    assert_eq!(rel.to_entity, "Serialize");
}

#[test]
fn struct_with_field_types_produces_depends_on_relations() {
    use thinkingroot_core::ir::{Chunk, ChunkMetadata, ChunkType};

    let mut chunk = Chunk::new(
        "struct Engine { storage: StorageBackend, config: EngineConfig }",
        ChunkType::TypeDef,
        1, 3,
    );
    chunk.metadata = ChunkMetadata {
        type_name: Some("Engine".to_string()),
        field_types: vec!["StorageBackend".to_string(), "EngineConfig".to_string()],
        ..Default::default()
    };

    let result = extract_structural(&chunk, "src/engine.rs");

    let deps: Vec<_> = result.relations.iter()
        .filter(|r| r.relation_type == "depends_on")
        .collect();
    assert_eq!(deps.len(), 2, "two field types → two depends_on relations");
    assert!(deps.iter().any(|r| r.to_entity == "StorageBackend"));
    assert!(deps.iter().any(|r| r.to_entity == "EngineConfig"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p thinkingroot-extract impl_with_trait_produces_implements --no-default-features 2>&1 | tail -5
```
Expected: FAIL — `ChunkMetadata` has no `trait_name` or `field_types` fields yet.

- [ ] **Step 3: Add `trait_name` and `field_types` to `ChunkMetadata` in ir.rs**

In `crates/thinkingroot-core/src/ir.rs`, extend `ChunkMetadata`:

```rust
pub struct ChunkMetadata {
    /// For FunctionDef: the function name.
    pub function_name: Option<String>,
    /// For TypeDef: the type name.
    pub type_name: Option<String>,
    /// For FunctionDef: parameter signatures.
    pub parameters: Option<Vec<String>>,
    /// For FunctionDef: return type.
    pub return_type: Option<String>,
    /// For Import: the imported module/path.
    pub import_path: Option<String>,
    /// Visibility (pub, pub(crate), private).
    pub visibility: Option<String>,
    /// Parent scope name (e.g., the struct a method belongs to).
    pub parent: Option<String>,
    /// For TypeDef (impl_item): the trait being implemented, if any.
    /// Set when the chunk is `impl Trait for Type`.
    pub trait_name: Option<String>,
    /// For TypeDef (struct_item): the non-primitive field types.
    /// Each entry is the base type name (generics stripped).
    pub field_types: Vec<String>,
}
```

Also update `Default` — since the struct already derives `Default`, adding `Vec<String>` for `field_types` works automatically (empty vec). Verify the impl block if it's manual:

If `ChunkMetadata` has a manual `Default` impl, add:
```rust
trait_name: None,
field_types: Vec::new(),
```

- [ ] **Step 4: Populate `trait_name` in the parser for `impl_item`**

In `crates/thinkingroot-parse/src/code.rs`, replace the `impl_item` arm inside `extract_chunks`. Find the existing combined match arm that handles struct/enum/type/trait/impl (around line 96):

```rust
// Split impl_item handling from the rest of TypeDef patterns:

// impl blocks — extract implementing type AND trait name
"impl_item" => {
    // For `impl Foo` or `impl Trait for Foo`:
    // tree-sitter-rust: type field = implementing type, trait field = trait (optional)
    let type_name = child.child_by_field_name("type")
        .map(|n| source[n.byte_range()].to_string());
    let trait_name = child.child_by_field_name("trait")
        .map(|n| source[n.byte_range()].to_string());

    let mut chunk = Chunk::new(text, ChunkType::TypeDef, start_line, end_line)
        .with_language(language);
    chunk.metadata = ChunkMetadata {
        type_name,
        trait_name,
        visibility: extract_visibility(source, &child),
        ..Default::default()
    };
    doc.add_chunk(chunk);
}

// Struct, enum, type alias, trait definitions — extract name and field types
"struct_item"
| "enum_item"
| "type_item"
| "trait_item"
| "class_definition"
| "class_declaration"
| "interface_declaration"
| "type_alias_declaration"
| "type_spec" => {
    let name = find_child_by_field(&child, "name")
        .map(|n| source[n.byte_range()].to_string());
    let field_types = extract_field_types(source, &child);

    let mut chunk = Chunk::new(text, ChunkType::TypeDef, start_line, end_line)
        .with_language(language);
    chunk.metadata = ChunkMetadata {
        type_name: name,
        field_types,
        visibility: extract_visibility(source, &child),
        ..Default::default()
    };
    doc.add_chunk(chunk);
}
```

- [ ] **Step 5: Add the `extract_field_types` helper in code.rs**

Add this function after `extract_visibility` in `crates/thinkingroot-parse/src/code.rs`:

```rust
/// Walk struct/class body and collect non-primitive field type names.
/// Returns base type names with generics stripped (e.g., `Vec<String>` → `Vec`).
fn extract_field_types(source: &str, node: &tree_sitter::Node) -> Vec<String> {
    let mut types = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        // Rust: field_declaration_list; Python/TS: class_body / declaration_list
        if matches!(child.kind(), "field_declaration_list" | "declaration_list" | "class_body") {
            let mut inner = child.walk();
            for field in child.children(&mut inner) {
                if matches!(field.kind(), "field_declaration" | "typed_parameter" | "public_field_definition") {
                    if let Some(type_node) = field.child_by_field_name("type") {
                        let raw = source[type_node.byte_range()].trim().to_string();
                        let base = raw
                            .trim_start_matches('&')
                            .trim_start_matches("mut ")
                            .trim_start_matches("Option<")
                            .trim_start_matches("Vec<")
                            .trim_start_matches("Arc<")
                            .trim_start_matches("Box<")
                            .split('<').next()
                            .unwrap_or(&raw)
                            .trim_end_matches('>')
                            .trim()
                            .to_string();
                        if !base.is_empty() && !is_primitive_type(&base) {
                            types.push(base);
                        }
                    }
                }
            }
        }
    }
    types
}

fn is_primitive_type(s: &str) -> bool {
    matches!(
        s,
        "bool" | "i8" | "i16" | "i32" | "i64" | "i128" | "isize"
            | "u8" | "u16" | "u32" | "u64" | "u128" | "usize"
            | "f32" | "f64" | "char" | "str" | "String" | "()"
            | "Vec" | "Option" | "Arc" | "Box" | "HashMap" | "BTreeMap"
            | "HashSet" | "BTreeSet" | "Rc" | "Cell" | "RefCell"
    )
}
```

- [ ] **Step 6: Emit `implements` and `depends_on` in the structural extractor**

In `crates/thinkingroot-extract/src/structural.rs`, update `extract_type_def` to use the new metadata:

```rust
fn extract_type_def(chunk: &Chunk, source_uri: &str) -> ExtractionResult {
    let name = match &chunk.metadata.type_name {
        Some(n) if !n.is_empty() => n.clone(),
        _ => return ExtractionResult::empty(),
    };

    let file_name = file_name_from_uri(source_uri);
    let entity_type = infer_entity_type_from_content(&chunk.content);

    let entity = ExtractedEntity {
        name: name.clone(),
        entity_type: entity_type.to_string(),
        aliases: Vec::new(),
        description: Some(format!("{name} is a type defined in {file_name}")),
    };

    let def_claim = ExtractedClaim {
        statement: format!("{name} is a type defined in {file_name}"),
        claim_type: "definition".to_string(),
        confidence: 0.99,
        entities: vec![name.clone(), file_name.clone()],
        source_quote: Some(chunk.content.lines().next().unwrap_or("").to_string()),
        extraction_tier: ExtractionTier::Structural,
    };

    let file_entity = ExtractedEntity {
        name: file_name.clone(),
        entity_type: "file".to_string(),
        aliases: Vec::new(),
        description: Some(format!("Source file {file_name}")),
    };

    let file_contains = ExtractedRelation {
        from_entity: file_name.clone(),
        to_entity: name.clone(),
        relation_type: "contains".to_string(),
        description: Some(format!("{file_name} contains type {name}")),
        confidence: 0.99,
    };

    let mut result = ExtractionResult {
        claims: vec![def_claim],
        entities: vec![entity, file_entity],
        relations: vec![file_contains],
    };

    // If this is `impl Trait for Type`, emit an `implements` relation.
    if let Some(trait_name) = &chunk.metadata.trait_name {
        if !trait_name.is_empty() {
            let trait_entity = ExtractedEntity {
                name: trait_name.clone(),
                entity_type: "concept".to_string(),
                aliases: Vec::new(),
                description: Some(format!("Trait implemented by {name}")),
            };
            let implements_rel = ExtractedRelation {
                from_entity: name.clone(),
                to_entity: trait_name.clone(),
                relation_type: "implements".to_string(),
                description: Some(format!("{name} implements {trait_name}")),
                confidence: 0.99,
            };
            result.entities.push(trait_entity);
            result.relations.push(implements_rel);

            // Also emit a claim for the implementation.
            let impl_claim = ExtractedClaim {
                statement: format!("{name} implements the {trait_name} trait"),
                claim_type: "definition".to_string(),
                confidence: 0.99,
                entities: vec![name.clone(), trait_name.clone()],
                source_quote: Some(chunk.content.lines().next().unwrap_or("").to_string()),
                extraction_tier: ExtractionTier::Structural,
            };
            result.claims.push(impl_claim);
        }
    }

    // For each field type, emit a `depends_on` relation.
    for field_type in &chunk.metadata.field_types {
        let field_entity = ExtractedEntity {
            name: field_type.clone(),
            entity_type: "concept".to_string(),
            aliases: Vec::new(),
            description: None,
        };
        let depends_rel = ExtractedRelation {
            from_entity: name.clone(),
            to_entity: field_type.clone(),
            relation_type: "depends_on".to_string(),
            description: Some(format!("{name} has a field of type {field_type}")),
            confidence: 0.99,
        };
        result.entities.push(field_entity);
        result.relations.push(depends_rel);
    }

    result
}
```

- [ ] **Step 7: Run tests to verify they pass**

```bash
cargo test -p thinkingroot-extract --no-default-features 2>&1 | grep -E "^test result|FAILED"
cargo test -p thinkingroot-parse --no-default-features 2>&1 | grep -E "^test result|FAILED"
cargo test -p thinkingroot-core --no-default-features 2>&1 | grep -E "^test result|FAILED"
```
Expected: all `test result: ok`

- [ ] **Step 8: Commit**

```bash
git add crates/thinkingroot-core/src/ir.rs \
        crates/thinkingroot-parse/src/code.rs \
        crates/thinkingroot-extract/src/structural.rs
git commit -m "feat(extract): richer structural extraction — impl→implements, struct fields→depends_on, zero LLM"
```

---

## Task 5: KNOWN_RELATIONS Injection into LLM Prompts

**Problem:** The LLM is shown which entities exist in the graph (`<KNOWN_ENTITIES>`) but not which relations already exist. It re-extracts the same relations on every compile and has no guidance on the target relation set for known entity pairs.

**Fix:** Add `get_known_relations()` to `GraphStore`, extend `GraphPrimedContext` with a `KnownRelation` type, and inject a `<KNOWN_RELATIONS>` block into the extraction prompt.

**Files:**
- Modify: `crates/thinkingroot-graph/src/graph.rs`
- Modify: `crates/thinkingroot-extract/src/graph_context.rs`
- Modify: `crates/thinkingroot-serve/src/pipeline.rs`

- [ ] **Step 1: Write failing tests**

Add to `crates/thinkingroot-extract/src/graph_context.rs` in `#[cfg(test)]` mod:

```rust
#[test]
fn known_relations_appear_in_prompt_section() {
    let ctx = GraphPrimedContext {
        entities: vec![
            KnownEntity { name: "AuthService".to_string(), entity_type: "service".to_string() },
        ],
        relations: vec![
            KnownRelation {
                from: "AuthService".to_string(),
                to: "JWT".to_string(),
                relation_type: "uses".to_string(),
            },
        ],
    };
    let section = ctx.prompt_section();
    assert!(section.contains("KNOWN_RELATIONS"), "section must include KNOWN_RELATIONS block");
    assert!(section.contains("AuthService"), "section must include from entity");
    assert!(section.contains("JWT"), "section must include to entity");
    assert!(section.contains("uses"), "section must include relation type");
}

#[test]
fn empty_relations_still_produces_entities_section() {
    let ctx = GraphPrimedContext {
        entities: vec![
            KnownEntity { name: "MyService".to_string(), entity_type: "service".to_string() },
        ],
        relations: vec![],
    };
    let section = ctx.prompt_section();
    assert!(section.contains("KNOWN_ENTITIES"));
    assert!(!section.contains("KNOWN_RELATIONS"), "no relations block when empty");
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p thinkingroot-extract known_relations_appear_in_prompt --no-default-features 2>&1 | tail -5
```
Expected: compilation error — `KnownRelation` doesn't exist yet.

- [ ] **Step 3: Add `KnownRelation` and extend `GraphPrimedContext` in graph_context.rs**

Replace the entire `crates/thinkingroot-extract/src/graph_context.rs`:

```rust
/// Maximum number of known entities injected into a single LLM prompt.
pub const MAX_KNOWN_ENTITIES: usize = 200;
/// Maximum number of known relations injected into a single LLM prompt.
pub const MAX_KNOWN_RELATIONS: usize = 100;

/// A single entity known to the knowledge graph.
pub struct KnownEntity {
    pub name: String,
    pub entity_type: String,
}

/// A single relation already in the knowledge graph.
pub struct KnownRelation {
    pub from: String,
    pub to: String,
    pub relation_type: String,
}

/// A snapshot of existing graph state, formatted for injection into LLM
/// extraction prompts. Tells the LLM which entities and relations already
/// exist so it uses canonical names and avoids re-extracting known edges.
pub struct GraphPrimedContext {
    pub entities: Vec<KnownEntity>,
    pub relations: Vec<KnownRelation>,
}

impl GraphPrimedContext {
    pub fn new(entities: Vec<KnownEntity>) -> Self {
        Self { entities, relations: Vec::new() }
    }

    pub fn from_tuples(tuples: Vec<(String, String)>) -> Self {
        let entities = tuples
            .into_iter()
            .map(|(name, entity_type)| KnownEntity { name, entity_type })
            .collect();
        Self { entities, relations: Vec::new() }
    }

    pub fn with_relations(mut self, relations: Vec<KnownRelation>) -> Self {
        self.relations = relations;
        self
    }

    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    /// Build the combined `<KNOWN_ENTITIES>` + `<KNOWN_RELATIONS>` XML section
    /// to embed in an LLM extraction prompt.
    pub fn prompt_section(&self) -> String {
        if self.entities.is_empty() {
            return String::new();
        }

        let mut lines = Vec::new();

        // ── KNOWN_ENTITIES block ──────────────────────────────────────────────
        lines.push("<KNOWN_ENTITIES>".to_string());
        lines.push(
            "The following entities already exist in the knowledge graph. \
Use the EXACT names below when referencing these entities. \
Only create new entities for concepts not already represented."
                .to_string(),
        );
        lines.push(String::new());
        for entity in self.entities.iter().take(MAX_KNOWN_ENTITIES) {
            lines.push(format!("- {} ({})", entity.name, entity.entity_type));
        }
        lines.push("</KNOWN_ENTITIES>".to_string());

        // ── KNOWN_RELATIONS block (only when non-empty) ───────────────────────
        if !self.relations.is_empty() {
            lines.push(String::new());
            lines.push("<KNOWN_RELATIONS>".to_string());
            lines.push(
                "The following relations already exist in the knowledge graph. \
Do NOT re-extract these exact pairs — only extract NEW relations not listed here."
                    .to_string(),
            );
            lines.push(String::new());
            for rel in self.relations.iter().take(MAX_KNOWN_RELATIONS) {
                lines.push(format!("- {} --[{}]--> {}", rel.from, rel.relation_type, rel.to));
            }
            lines.push("</KNOWN_RELATIONS>".to_string());
        }

        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_context_produces_empty_string() {
        let ctx = GraphPrimedContext::new(vec![]);
        assert!(ctx.prompt_section().is_empty());
    }

    #[test]
    fn known_entities_produce_prompt_section() {
        let ctx = GraphPrimedContext::new(vec![
            KnownEntity { name: "GraphStore".to_string(), entity_type: "system".to_string() },
            KnownEntity { name: "Claim".to_string(), entity_type: "concept".to_string() },
        ]);
        let section = ctx.prompt_section();
        assert!(section.contains("KNOWN_ENTITIES"));
        assert!(section.contains("GraphStore"));
        assert!(section.contains("Claim"));
    }

    #[test]
    fn from_tuples_converts_correctly() {
        let tuples = vec![
            ("GraphStore".to_string(), "system".to_string()),
            ("Claim".to_string(), "concept".to_string()),
        ];
        let ctx = GraphPrimedContext::from_tuples(tuples);
        assert_eq!(ctx.entities.len(), 2);
    }

    #[test]
    fn limits_to_max_entities() {
        let tuples: Vec<(String, String)> = (0..500)
            .map(|i| (format!("Entity{i}"), "concept".to_string()))
            .collect();
        let ctx = GraphPrimedContext::from_tuples(tuples);
        let section = ctx.prompt_section();
        let entry_count = section.lines().filter(|l| l.starts_with("- Entity")).count();
        assert_eq!(entry_count, MAX_KNOWN_ENTITIES);
    }

    #[test]
    fn known_relations_appear_in_prompt_section() {
        let ctx = GraphPrimedContext {
            entities: vec![
                KnownEntity { name: "AuthService".to_string(), entity_type: "service".to_string() },
            ],
            relations: vec![
                KnownRelation {
                    from: "AuthService".to_string(),
                    to: "JWT".to_string(),
                    relation_type: "uses".to_string(),
                },
            ],
        };
        let section = ctx.prompt_section();
        assert!(section.contains("KNOWN_RELATIONS"));
        assert!(section.contains("AuthService"));
        assert!(section.contains("JWT"));
        assert!(section.contains("uses"));
    }

    #[test]
    fn empty_relations_still_produces_entities_section() {
        let ctx = GraphPrimedContext {
            entities: vec![
                KnownEntity { name: "MyService".to_string(), entity_type: "service".to_string() },
            ],
            relations: vec![],
        };
        let section = ctx.prompt_section();
        assert!(section.contains("KNOWN_ENTITIES"));
        assert!(!section.contains("KNOWN_RELATIONS"));
    }
}
```

- [ ] **Step 4: Add `get_known_relations` to `GraphStore`**

Add after `get_known_entities` (around line 637) in `crates/thinkingroot-graph/src/graph.rs`:

```rust
/// Returns `(from_name, to_name, relation_type)` triples for all relations in the graph.
/// Used by graph-primed extraction to inject KNOWN_RELATIONS into LLM prompts.
/// Capped implicitly by MAX_KNOWN_RELATIONS in GraphPrimedContext.
pub fn get_known_relations(&self) -> Result<Vec<(String, String, String)>> {
    let result = self.query_read(
        r#"?[from_name, to_name, rel_type] :=
            *entity_relations{from_id, to_id, relation_type: rel_type},
            *entities{id: from_id, canonical_name: from_name},
            *entities{id: to_id, canonical_name: to_name}"#,
    )?;
    Ok(result
        .rows
        .iter()
        .map(|row| (
            dv_to_string(&row[0]),
            dv_to_string(&row[1]),
            dv_to_string(&row[2]),
        ))
        .collect())
}
```

- [ ] **Step 5: Load known_relations in the pipeline and pass to extractor**

In `crates/thinkingroot-serve/src/pipeline.rs`, find the graph-primed context block (the one that calls `get_known_entities`, around line 122). Replace it:

```rust
// ── Graph-Primed Context: inject known entities + relations into extraction ──
let known_entities_ctx = match storage.graph.get_known_entities() {
    Ok(entities) if !entities.is_empty() => {
        tracing::info!("graph-primed context: {} known entities loaded", entities.len());
        thinkingroot_extract::GraphPrimedContext::from_tuples(entities)
    }
    Ok(_) => thinkingroot_extract::GraphPrimedContext::new(Vec::new()),
    Err(e) => {
        tracing::warn!("failed to load known entities for graph-priming: {e}");
        thinkingroot_extract::GraphPrimedContext::new(Vec::new())
    }
};

let known_entities = match storage.graph.get_known_relations() {
    Ok(relations) if !relations.is_empty() => {
        tracing::info!("graph-primed context: {} known relations loaded", relations.len());
        let known_rels: Vec<thinkingroot_extract::graph_context::KnownRelation> = relations
            .into_iter()
            .map(|(from, to, rel_type)| thinkingroot_extract::graph_context::KnownRelation {
                from,
                to,
                relation_type: rel_type,
            })
            .collect();
        known_entities_ctx.with_relations(known_rels)
    }
    Ok(_) => known_entities_ctx,
    Err(e) => {
        tracing::warn!("failed to load known relations for graph-priming: {e}");
        known_entities_ctx
    }
};
```

Note: the variable `known_entities` (used by the rest of the pipeline to call `.with_known_entities(known_entities)`) is preserved with the same name — the pipeline code below this block is unchanged.

- [ ] **Step 6: Export `KnownRelation` from the extract crate**

In `crates/thinkingroot-extract/src/lib.rs`, add to the pub use section:

```rust
pub use graph_context::{GraphPrimedContext, KnownRelation};
```

Find the existing `pub use graph_context::GraphPrimedContext;` line and replace it.

- [ ] **Step 7: Run all tests**

```bash
cargo test --no-default-features 2>&1 | grep -E "^test result|FAILED"
```
Expected: all `test result: ok`

- [ ] **Step 8: Commit**

```bash
git add crates/thinkingroot-graph/src/graph.rs \
        crates/thinkingroot-extract/src/graph_context.rs \
        crates/thinkingroot-extract/src/lib.rs \
        crates/thinkingroot-serve/src/pipeline.rs
git commit -m "feat(extract): inject KNOWN_RELATIONS into LLM prompts — prevents re-extraction of existing graph edges"
```

---

## Task 6: Cross-File Incremental Staleness Fix

**Problem:** When file A changes and its entities are updated/removed, relations between those entities and entities from unchanged files (B, C, ...) are never re-evaluated. The affected_triples computation in Phase 4 only collects triples contributed by the changed source, not triples involving that source's entities that were contributed by OTHER sources.

**Fix:** After collecting a source's affected_triples, also collect all entity_relations triples that involve any entity whose definition came from that source. These cross-file triples are added to `affected_triples` and re-evaluated in Phase 5.

**Files:**
- Modify: `crates/thinkingroot-graph/src/graph.rs`
- Modify: `crates/thinkingroot-serve/src/pipeline.rs`

- [ ] **Step 1: Write the failing test**

Add to `crates/thinkingroot-graph/src/graph.rs` in `#[cfg(test)]` mod:

```rust
#[test]
fn get_all_triples_involving_entities_returns_cross_file_edges() {
    let store = mem_store();

    let e1 = thinkingroot_core::Entity::new("Alpha", thinkingroot_core::types::EntityType::Service);
    let e2 = thinkingroot_core::Entity::new("Beta", thinkingroot_core::types::EntityType::Service);
    let e3 = thinkingroot_core::Entity::new("Gamma", thinkingroot_core::types::EntityType::Database);
    store.insert_entity(&e1).unwrap();
    store.insert_entity(&e2).unwrap();
    store.insert_entity(&e3).unwrap();

    let eid1 = e1.id.to_string();
    let eid2 = e2.id.to_string();
    let eid3 = e3.id.to_string();

    // e1→uses→e2 contributed by src_a, e2→depends_on→e3 contributed by src_b.
    let src_a = thinkingroot_core::Source::new("a.rs".into(), thinkingroot_core::types::SourceType::File);
    let src_b = thinkingroot_core::Source::new("b.rs".into(), thinkingroot_core::types::SourceType::File);
    store.insert_source(&src_a).unwrap();
    store.insert_source(&src_b).unwrap();

    store.link_entities_for_source(&src_a.id.to_string(), &eid1, &eid2, "Uses", 0.9).unwrap();
    store.link_entities_for_source(&src_b.id.to_string(), &eid2, &eid3, "DependsOn", 0.8).unwrap();
    store.rebuild_entity_relations().unwrap();

    // Query triples involving e1 (which is from src_a).
    let triples = store.get_all_triples_involving_entities(&[eid1.clone()]).unwrap();
    assert_eq!(triples.len(), 1);
    assert!(triples.iter().any(|(f, t, _)| f == &eid1 && t == &eid2));

    // Query triples involving e2 (appears in BOTH triples).
    let triples2 = store.get_all_triples_involving_entities(&[eid2.clone()]).unwrap();
    assert_eq!(triples2.len(), 2, "e2 is in both triples (as target of first, source of second)");

    // Empty input returns empty.
    let empty = store.get_all_triples_involving_entities(&[]).unwrap();
    assert!(empty.is_empty());
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test -p thinkingroot-graph get_all_triples_involving_entities --no-default-features 2>&1 | tail -5
```
Expected: FAIL — method doesn't exist yet.

- [ ] **Step 3: Add `get_all_triples_involving_entities` to `GraphStore`**

Add after `get_source_relation_triples` in `crates/thinkingroot-graph/src/graph.rs`:

```rust
/// Get all `(from_id, to_id, relation_type)` triples in `entity_relations`
/// where at least one endpoint is in `entity_ids`.
///
/// Used by the incremental pipeline to collect cross-file triples that need
/// re-evaluation when a source's entities are removed or changed.
/// Returns deduplicated triples.
pub fn get_all_triples_involving_entities(
    &self,
    entity_ids: &[String],
) -> Result<Vec<(String, String, String)>> {
    if entity_ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut seen = std::collections::HashSet::new();

    for eid in entity_ids {
        let mut params = BTreeMap::new();
        params.insert("eid".into(), DataValue::Str(eid.clone().into()));

        // Triples where this entity is the source (from_id == eid).
        let from_result = self
            .db
            .run_script(
                "?[from_id, to_id, rel_type] := \
                 *entity_relations{from_id: $eid, to_id, relation_type: rel_type}",
                params.clone(),
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))?;

        // Triples where this entity is the target (to_id == eid).
        let to_result = self
            .db
            .run_script(
                "?[from_id, to_id, rel_type] := \
                 *entity_relations{from_id, to_id: $eid, relation_type: rel_type}",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))?;

        for row in from_result.rows.iter().chain(to_result.rows.iter()) {
            seen.insert((
                dv_to_string(&row[0]),
                dv_to_string(&row[1]),
                dv_to_string(&row[2]),
            ));
        }
    }

    Ok(seen.into_iter().collect())
}
```

- [ ] **Step 4: Run test to verify it passes**

```bash
cargo test -p thinkingroot-graph get_all_triples_involving_entities --no-default-features 2>&1 | tail -5
```
Expected: PASS

- [ ] **Step 5: Extend Phase 4 in the pipeline to include cross-file triples**

In `crates/thinkingroot-serve/src/pipeline.rs`, inside the Phase 4 block (the `for doc in &truly_changed` loop, around line 305), after the existing `affected_triples.extend(storage.graph.get_source_relation_triples(source_id)?)` call, add:

```rust
for (source_id, _, _) in &existing_sources {
    // Existing: triples this source directly contributed.
    affected_triples
        .extend(storage.graph.get_source_relation_triples(source_id)?);

    // NEW: cross-file triples — all entity_relations triples where an
    // endpoint entity was contributed by this source. This catches
    // relations like "E_B (unchanged file) → uses → E_A (changed file)"
    // that are contributed by file B's source but reference file A's entity.
    let entity_ids_from_source = storage.graph.get_entity_ids_for_source(source_id)?;
    if !entity_ids_from_source.is_empty() {
        let cross_file_triples = storage
            .graph
            .get_all_triples_involving_entities(&entity_ids_from_source)?;
        affected_triples.extend(cross_file_triples);
        tracing::debug!(
            "cross-file staleness: {} cross-file triples added for source {}",
            entity_ids_from_source.len(),
            source_id
        );
    }

    // ... existing stale vector capture code below (unchanged)
```

Do the same inside the `for (source_id, uri) in &deleted_sources` loop — same pattern, same addition.

- [ ] **Step 6: Run all graph and pipeline tests**

```bash
cargo test -p thinkingroot-graph --no-default-features 2>&1 | grep -E "^test result|FAILED"
cargo test -p thinkingroot-serve --no-default-features 2>&1 | grep -E "^test result|FAILED"
```
Expected: all `test result: ok`

- [ ] **Step 7: Run full workspace test suite**

```bash
cargo test --no-default-features 2>&1 | grep -E "^test result|FAILED|error\["
```
Expected: all `test result: ok`, 0 errors.

- [ ] **Step 8: Commit**

```bash
git add crates/thinkingroot-graph/src/graph.rs \
        crates/thinkingroot-serve/src/pipeline.rs
git commit -m "fix(pipeline): cross-file incremental staleness — affected_triples now includes all entity_relations touching changed source's entities"
```

---

## Self-Review

**1. Spec coverage:**
- Issue 1 (RelatedTo overuse) → Task 1 ✅
- Issue 2 (Relation deduplication) → Task 3 ✅
- Issue 3 (Structural extractor limited) → Task 4 ✅
- Issue 4 (All strengths 1.0) → Task 1 (confidence field) + Task 2 (noisy-OR) ✅
- Issue 5 (LLM no known relations) → Task 5 ✅
- Issue 6 (Cross-file staleness) → Task 6 ✅

**2. Placeholder scan:** None found. Every step contains exact Rust code.

**3. Type consistency:**
- `ExtractedRelation.confidence: f64` — added in Task 1 Step 3, used in Task 1 Step 6 and Task 4 Step 6 ✅
- `ChunkMetadata.trait_name: Option<String>` — added in Task 4 Step 3, populated in Step 4, used in Step 6 ✅
- `ChunkMetadata.field_types: Vec<String>` — added in Task 4 Step 3, populated in Step 5, used in Step 6 ✅
- `KnownRelation` — defined in Task 5 Step 3, exported in Step 6, used in pipeline Step 5 ✅
- `get_all_triples_involving_entities(&[String])` — defined in Task 6 Step 3, called in Step 5 ✅
