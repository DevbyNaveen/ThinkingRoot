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
 * `{bestTypeMap, claimToEntities}` — the two derived structures the
 * canvas needs to colour nodes by semantic type and to map citation
 * activations onto entity nodes.  Sending arrays as plain objects
 * is fine; the payload is ~1–2 MB at the upper bound and only flows
 * once per snapshot refresh.
 *
 * Determinism: same input → same output, byte-for-byte.  Sort the
 * entity names longest-first so multi-word names win over their
 * prefixes (e.g. "User Account" wins over "User").
 */

interface EntityIn {
  name: string;
  entity_type: string;
  claim_count: number;
}

interface ClaimIn {
  id: string;
  statement: string;
  /** Optional in the wire shape — engine omits for some structural rows. */
  claim_type?: string;
  /** "rooted" | "attested" | "unknown" */
  tier?: string;
}

export interface EntityResolverRequest {
  reqId: number;
  entities: EntityIn[];
  claims: ClaimIn[];
}

export interface EntityResolverResponse {
  reqId: number;
  bestType: Array<[string, string]>;
  claimToEntities: Array<[string, string[]]>;
  /** Wall-time the worker spent on this request, for telemetry. */
  elapsedMs: number;
}

// Wire spelling matches `ClaimType` serde snake_case from
// `crates/thinkingroot-core/src/types/claim.rs` (verified against
// engine.rs:5031 and extractor.rs:636). The pre-fix `"apisignature"`
// (no underscore) never matched the backend's `"api_signature"` —
// every ApiSignature claim silently fell to MAX_SAFE_INTEGER rank
// and lost every tie-break. Ranking order = bias for which claim
// type wins when an entity is mentioned by multiple claims.
const TYPE_PRIORITY: ReadonlyArray<string> = [
  "definition",
  "api_signature",
  "architecture",
  "requirement",
  "fact",
];
const TYPE_RANK = new Map<string, number>(
  TYPE_PRIORITY.map((t, i) => [t, i] as const),
);

function escapeRegex(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function resolve(req: EntityResolverRequest): EntityResolverResponse {
  const start = performance.now();
  const bestType = new Map<string, string>();
  const bestRank = new Map<string, number>();
  const claimToEntities = new Map<string, string[]>();

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
      const incoming = claim.claim_type;
      const incomingRank =
        incoming === undefined
          ? Number.MAX_SAFE_INTEGER
          : (TYPE_RANK.get(incoming.toLowerCase()) ?? Number.MAX_SAFE_INTEGER);
      const isRooted = claim.tier === "rooted";

      // matchAll over the claim text — single DFA traversal, O(N).
      const matches = claim.statement.matchAll(matcher);
      const seen = new Set<string>();
      for (const m of matches) {
        const name = m[0];
        const currentRank = bestRank.get(name);
        if (
          incoming !== undefined &&
          (currentRank === undefined || incomingRank < currentRank)
        ) {
          bestType.set(name, incoming);
          bestRank.set(name, incomingRank);
        }
        if (isRooted) {
          const cur = bestType.get(name);
          if (!cur || cur.toLowerCase() !== "definition") {
            bestType.set(name, "rooted");
          }
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
    bestType: Array.from(bestType.entries()),
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
