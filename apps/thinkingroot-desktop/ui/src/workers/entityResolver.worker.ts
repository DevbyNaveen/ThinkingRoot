/// <reference lib="webworker" />
/**
 * Brain-graph entity-resolver worker.
 *
 * Why this is a worker: on a 10k-claim workspace the main-thread
 * version compiled a ~120 KB alternation regex (one alternative per
 * entity name) and ran `matchAll` over every claim's statement.
 * That's ~10k passes of a DFA of width O(entity-name-count) — about
 * 200–600 ms of pure CPU that blocked the React commit phase and
 * the canvas first paint.
 *
 * The worker receives `{entities, claims}` and returns
 * `{bestFamily, bestConfidence, claimToEntities}` — three derived
 * structures the canvas needs to:
 *   - colour nodes by witness super-family (hue),
 *   - modulate alpha by the entity's strongest claim confidence,
 *   - map citation activations onto entity nodes.
 *
 * Determinism: same input → same output, byte-for-byte. Sort the
 * entity names longest-first so multi-word names win over their
 * prefixes (e.g. "User Account" wins over "User").
 */

import {
  claimTypeToSuperFamily,
  seedFamilyFromEntityType,
  superFamilyRank,
  type SuperFamily,
} from "../lib/witnessFamily";

interface EntityIn {
  name: string;
  /**
   * Engine-provided structural type ("inferred" when the entity only
   * appears in a relation). The worker reads this so an entity with
   * no resolved claims still receives a deterministic super-family.
   */
  entity_type: string;
  claim_count: number;
}

interface ClaimIn {
  id: string;
  statement: string;
  /** Optional in the wire shape — engine omits for some structural rows. */
  claim_type?: string;
  /** 0–1, from the structural extractor / future rule catalog. */
  confidence?: number;
  /** "rooted" | "attested" | "unknown" — legacy admission tier. */
  tier?: string;
}

export interface EntityResolverRequest {
  reqId: number;
  entities: EntityIn[];
  claims: ClaimIn[];
}

export interface EntityResolverResponse {
  reqId: number;
  /** Per entity → winning super-family (string for cheap structural-clone). */
  bestFamily: Array<[string, SuperFamily]>;
  /** Per entity → max confidence (0–1) seen across its backing claims. */
  bestConfidence: Array<[string, number]>;
  /** claim_id → entity names mentioned in its statement. */
  claimToEntities: Array<[string, string[]]>;
  /** Wall-time the worker spent on this request, for telemetry. */
  elapsedMs: number;
}

function escapeRegex(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function resolve(req: EntityResolverRequest): EntityResolverResponse {
  const start = performance.now();
  const bestFamily = new Map<string, SuperFamily>();
  const bestRank = new Map<string, number>();
  const bestConf = new Map<string, number>();
  const claimToEntities = new Map<string, string[]>();

  // Seed every entity from its engine-provided `entity_type` so the
  // first paint shows real colors. `entity_type` and `claim_type` are
  // distinct vocabularies — `entity_type` describes what kind of
  // thing the entity IS (function/file/library/person/…), while
  // `claim_type` describes statements about it. The resolver upgrades
  // the seed once claims are matched in the loop below.
  for (const e of req.entities) {
    const seeded = seedFamilyFromEntityType(e.entity_type);
    bestFamily.set(e.name, seeded);
    bestRank.set(e.name, superFamilyRank(seeded));
  }

  const sortedNames = req.entities
    .map((e) => e.name)
    .filter((n) => n.length > 0)
    .sort((a, b) => b.length - a.length);

  const matcher =
    sortedNames.length > 0
      ? new RegExp(sortedNames.map(escapeRegex).join("|"), "g")
      : null;

  if (matcher) {
    for (const claim of req.claims) {
      const family =
        claimTypeToSuperFamily(claim.claim_type) ??
        // tier == "rooted" → tests-equivalent grounding tier in the
        // legacy substrate. Keep promoting those entities into "tests"
        // so existing rooted-grounded knowledge keeps its strong hue.
        (claim.tier === "rooted" ? "tests" : null);
      const familyRank =
        family === null ? Number.MAX_SAFE_INTEGER : superFamilyRank(family);
      const conf =
        typeof claim.confidence === "number" && Number.isFinite(claim.confidence)
          ? Math.max(0, Math.min(1, claim.confidence))
          : 0;

      const matches = claim.statement.matchAll(matcher);
      const seen = new Set<string>();
      for (const m of matches) {
        const name = m[0];
        if (family !== null) {
          const currentRank = bestRank.get(name);
          if (currentRank === undefined || familyRank < currentRank) {
            bestFamily.set(name, family);
            bestRank.set(name, familyRank);
          }
        }
        const prevConf = bestConf.get(name);
        if (prevConf === undefined || conf > prevConf) {
          bestConf.set(name, conf);
        }
        seen.add(name);
      }
      if (seen.size > 0) {
        claimToEntities.set(claim.id, Array.from(seen));
      }
    }
  }

  return {
    reqId: req.reqId,
    bestFamily: Array.from(bestFamily.entries()),
    bestConfidence: Array.from(bestConf.entries()),
    claimToEntities: Array.from(claimToEntities.entries()),
    elapsedMs: Math.round(performance.now() - start),
  };
}

self.onmessage = (e: MessageEvent<EntityResolverRequest>) => {
  // Defensive: discard malformed input rather than crash the worker,
  // since the worker's lifetime spans many requests.
  const req = e.data;
  if (
    !req ||
    typeof req.reqId !== "number" ||
    !Array.isArray(req.entities) ||
    !Array.isArray(req.claims)
  ) {
    return;
  }
  const resp = resolve(req);
  (self as DedicatedWorkerGlobalScope).postMessage(resp);
};
