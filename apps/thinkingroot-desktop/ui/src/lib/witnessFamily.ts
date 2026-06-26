/**
 * Single source of truth for entity coloring in the brain graph.
 *
 * The witness-mesh substrate (`crates/thinkingroot-extract/src/rule_catalog.rs`)
 * defines 24 rule families. For graph rendering we collapse them into
 * six semantic super-families plus an honest "unattested" bucket. Two
 * orthogonal signals drive the visual:
 *
 *   - hue → super-family (what *kind* of evidence backs this entity)
 *   - alpha → max claim confidence (how strong that evidence is)
 *
 * Tiebreak when an entity has witnesses from multiple super-families:
 *   tests > code > docs > config > human > media
 *
 * This module is intentionally DOM-free and React-free so the
 * entity-resolver worker can import it directly.
 */

// ── Super-family vocabulary ──────────────────────────────────────────

export type SuperFamily =
  | "tests"
  | "code"
  | "docs"
  | "config"
  | "human"
  | "media"
  | "unattested";

/** Priority order — lower index wins when an entity has mixed witnesses. */
export const SUPER_FAMILY_PRIORITY: ReadonlyArray<SuperFamily> = [
  "tests",
  "code",
  "docs",
  "config",
  "human",
  "media",
  "unattested",
];

const SUPER_FAMILY_RANK: ReadonlyMap<SuperFamily, number> = new Map(
  SUPER_FAMILY_PRIORITY.map((f, i) => [f, i] as const),
);

export function superFamilyRank(family: SuperFamily): number {
  return SUPER_FAMILY_RANK.get(family) ?? Number.MAX_SAFE_INTEGER;
}

// ── Rule-family → super-family (witness-mesh path) ───────────────────
//
// Mirrors `family` strings emitted by `rule_catalog.rs`. Keep this
// table aligned with the catalog: when a new family lands there, add
// it here in the same commit.

const RULE_FAMILY_TO_SUPER: ReadonlyMap<string, SuperFamily> = new Map([
  // tests
  ["cargo-test", "tests"],
  ["pytest", "tests"],
  ["jest", "tests"],
  ["junit", "tests"],
  // code
  ["tree-sitter", "code"],
  ["lsp", "code"],
  ["code", "code"],
  // docs
  ["rustdoc", "docs"],
  ["jsdoc", "docs"],
  ["javadoc", "docs"],
  ["markdown", "docs"],
  // config
  ["toml", "config"],
  ["json", "config"],
  ["yaml", "config"],
  ["csv", "config"],
  ["manifest", "config"],
  // human
  ["comment", "human"],
  ["git", "human"],
  ["conversation", "human"],
  ["legacy", "human"],
  // media
  ["audio", "media"],
  ["image", "media"],
  ["video", "media"],
  // edge: graph-only signal, no anchored byte evidence
  ["edge", "unattested"],
]);

/**
 * Map a witness `rule` string (e.g. `tree-sitter::function-decl@v1`)
 * to its super-family. Returns `null` when the rule prefix is unknown
 * so callers can decide how to render an unrecognised family rather
 * than silently bucketing it.
 */
export function ruleToSuperFamily(rule: string): SuperFamily | null {
  const sep = rule.indexOf("::");
  const family = sep === -1 ? rule : rule.slice(0, sep);
  return RULE_FAMILY_TO_SUPER.get(family) ?? null;
}

// ── claim_type → super-family (current substrate path) ───────────────
//
// The pre-witness-mesh claim_type strings emitted by
// `crates/thinkingroot-extract/src/structural.rs`. These describe the
// *kind of statement* a claim makes about an entity — they upgrade
// the entity's super-family once the resolver worker finds claims
// that mention the entity.

const CLAIM_TYPE_TO_SUPER: ReadonlyMap<string, SuperFamily> = new Map([
  ["definition", "code"],
  ["api_signature", "code"],
  ["apisignature", "code"],
  ["dependency", "config"],
  ["fact", "human"],
  ["architecture", "code"],
  ["requirement", "docs"],
  // Folded in from the legacy per-type SEMANTIC_PALETTE so every
  // claim_type the engine emits maps to a super-family (no silent
  // fall-through to the seed). These describe human/authored or
  // declarative statements about an entity.
  ["decision", "human"],
  ["opinion", "human"],
  ["preference", "human"],
  ["plan", "docs"],
  ["metric", "config"],
]);

export function claimTypeToSuperFamily(
  claimType: string | undefined,
): SuperFamily | null {
  if (!claimType) return null;
  return CLAIM_TYPE_TO_SUPER.get(claimType.toLowerCase()) ?? null;
}

// ── entity_type → super-family (initial-paint path) ──────────────────
//
// `entity_type` is a different vocabulary from `claim_type` — it
// describes the *kind of thing* the entity is, not the kind of
// statement about it. Real values emitted by
// `crates/thinkingroot-extract/src/structural.rs`:
//   system, library, function, file, concept, module, person,
//   data_row, api, config_section, config_key, service, inferred.
//
// This map runs before the resolver worker reply lands, so every
// entity gets a meaningful color from the first paint — never the
// hollow-ring fallback unless the entity genuinely has no evidence.

const ENTITY_TYPE_TO_SUPER: ReadonlyMap<string, SuperFamily> = new Map([
  // code — AST/LSP-shaped entities (covers structural + LLM vocab)
  ["function", "code"],
  ["file", "code"],
  ["module", "code"],
  ["system", "code"],
  ["api", "code"],
  ["database", "code"],
  // config — declared dependencies, config keys, external services
  ["library", "config"],
  ["config_key", "config"],
  ["config_section", "config"],
  ["config", "config"],
  ["data_row", "config"],
  ["service", "config"],
  // docs — markdown headings + abstract concepts come through here
  ["concept", "docs"],
  // human — git authors, teams, orgs, free-text "User"
  ["person", "human"],
  ["team", "human"],
  ["organization", "human"],
  // unattested — relation-only nodes from partial compiles
  ["inferred", "unattested"],
]);

export function entityTypeToSuperFamily(
  entityType: string | undefined,
): SuperFamily | null {
  if (!entityType) return null;
  return ENTITY_TYPE_TO_SUPER.get(entityType.toLowerCase()) ?? null;
}

/**
 * Resolve an entity's super-family from its engine-provided
 * `entity_type` with a sensible default. Unknown entity_type strings
 * (future extractors, LLM-derived types) fall through to `code` —
 * the dominant bucket in a typical code workspace — rather than
 * `unattested`, so they remain visible while the worker reply is in
 * flight. Genuine "no evidence" entities are flagged separately via
 * `entity_type === "inferred"`.
 */
export function seedFamilyFromEntityType(entityType: string | undefined): SuperFamily {
  return entityTypeToSuperFamily(entityType) ?? "code";
}

// ── Color palette ────────────────────────────────────────────────────
//
// Hue/sat/light tuned for the existing dark canvas at
// `BrainGraph.tsx`. Three anchors reuse hues already in the legacy
// `getSemanticColor` (purple/green/amber) for muscle-memory carryover.

interface FamilyPaint {
  /** Hex/HSL ignored — we paint with rgba so confidence can modulate alpha. */
  r: number;
  g: number;
  b: number;
}

const FAMILY_PAINT: Record<SuperFamily, FamilyPaint> = {
  // green — already used for `rooted` in the legacy palette
  tests: { r: 110, g: 230, b: 150 },
  // blue
  code: { r: 120, g: 190, b: 255 },
  // purple — already used for `definition` in the legacy palette
  docs: { r: 210, g: 150, b: 255 },
  // amber — already used for `architecture` in the legacy palette
  config: { r: 255, g: 190, b: 100 },
  // pink — already used for `requirement` in the legacy palette
  human: { r: 255, g: 150, b: 200 },
  // teal
  media: { r: 120, g: 220, b: 230 },
  // neutral — hollow ring uses this as stroke, not fill
  unattested: { r: 180, g: 180, b: 180 },
};

/**
 * Returns the rgba fill for a node, painted at full strength.
 *
 * Two states, both honest to the substrate:
 *
 *   - evidence-backed entity (claim_count > 0)  → solid family color
 *   - relation-only entity   (claim_count == 0) → hollow ring
 *
 * Per-claim confidence (0.50 / 0.95 / 0.99) is *not* aggregated into
 * a per-entity alpha. That collapse hid weak signal and washed out
 * the graph on workspaces where most entity names never literally
 * appear in any claim's `statement`. The graph's job is the
 * categorical question — "what kind of evidence?" — full strength.
 * Confidence belongs on the per-claim row in BrainTable, not on the
 * canvas node.
 */
export function familyFill(family: SuperFamily): {
  fillStyle: string;
  hollow: boolean;
} {
  const paint = FAMILY_PAINT[family];
  const hollow = family === "unattested";
  const a = hollow ? 0.65 : 0.95;
  return {
    fillStyle: `rgba(${paint.r}, ${paint.g}, ${paint.b}, ${a})`,
    hollow,
  };
}

/** RGB triple, for callers that need to derive a gradient (e.g. halos). */
export function familyRgb(family: SuperFamily): {
  r: number;
  g: number;
  b: number;
} {
  return FAMILY_PAINT[family];
}
