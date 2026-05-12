/**
 * Persist + restore d3-force layouts for the Brain graph.
 *
 * Without this, every page reload re-runs the simulation from
 * `alpha=1` on the main thread (or worker), which on a 10k-node
 * workspace is a 5–15s freeze before the canvas reads as stable.
 * With it, the worker initialises with cached positions, alpha
 * starts near zero, and the first paint is already converged.
 *
 * Storage format (versioned for forward-compat):
 *
 *   ```json
 *   {
 *     "v": 1,
 *     "fingerprint": "<cyrb53 of sorted entity names>",
 *     "nodeCount": 9545,
 *     "positions": [["entity-name", x, y], ...],
 *     "transform": { "x": 100, "y": 200, "k": 0.8 },
 *     "savedAt": 1762800000000
 *   }
 *   ```
 *
 * The fingerprint is the discriminator: if the entity set changes
 * (compile added/removed entities) the cached positions are still
 * useful for the unchanged nodes, so `loadGraphLayout` returns the
 * full Map and the caller filters down to the current nodes.  When
 * the fingerprint matches exactly the caller can also skip the
 * sim warm-up entirely.
 *
 * localStorage is fine up to ~5 MB; at 9.5k nodes the payload is
 * ~250 KB with JSON overhead.  Above ~50k nodes we'd want IndexedDB,
 * not today's problem — `loadGraphLayout` gracefully returns null on
 * QuotaExceeded so a too-big workspace just won't persist.
 */

import { cyrb53 } from "@/lib/cyrb53";

const STORAGE_VERSION = 1;
const STORAGE_PREFIX = "tr.brain.layout.v1:";

export interface PersistedLayout {
  fingerprint: string;
  fingerprintMatches: boolean;
  positions: Map<string, { x: number; y: number }>;
  transform?: { x: number; y: number; k: number };
  savedAt: number;
}

interface SerializedLayout {
  v: number;
  fingerprint: string;
  nodeCount: number;
  positions: Array<[string, number, number]>;
  transform?: { x: number; y: number; k: number };
  savedAt: number;
}

/** Fingerprint a list of entity names — order-insensitive. */
export function fingerprintEntities(entityNames: readonly string[]): string {
  // Sorting is what makes the fingerprint order-insensitive; the
  // join separator is a control char so a name containing ":" can't
  // forge a different list with the same hash.
  const sorted = [...entityNames].sort();
  return cyrb53(sorted.join(""));
}

function storageKey(workspace: string): string {
  return `${STORAGE_PREFIX}${workspace}`;
}

export function loadGraphLayout(
  workspace: string,
  currentFingerprint: string,
): PersistedLayout | null {
  if (typeof window === "undefined") return null;
  let raw: string | null;
  try {
    raw = window.localStorage.getItem(storageKey(workspace));
  } catch {
    return null;
  }
  if (!raw) return null;

  let parsed: SerializedLayout;
  try {
    parsed = JSON.parse(raw) as SerializedLayout;
  } catch {
    return null;
  }
  if (parsed.v !== STORAGE_VERSION || !Array.isArray(parsed.positions)) {
    return null;
  }

  const positions = new Map<string, { x: number; y: number }>();
  for (const entry of parsed.positions) {
    if (!Array.isArray(entry) || entry.length !== 3) continue;
    const [name, x, y] = entry;
    if (typeof name !== "string") continue;
    if (typeof x !== "number" || typeof y !== "number") continue;
    if (!Number.isFinite(x) || !Number.isFinite(y)) continue;
    positions.set(name, { x, y });
  }
  if (positions.size === 0) return null;

  return {
    fingerprint: parsed.fingerprint,
    fingerprintMatches: parsed.fingerprint === currentFingerprint,
    positions,
    transform: parsed.transform,
    savedAt: parsed.savedAt,
  };
}

export function saveGraphLayout(
  workspace: string,
  fingerprint: string,
  positions: Map<string, { x: number; y: number }>,
  transform: { x: number; y: number; k: number } | undefined,
): void {
  if (typeof window === "undefined") return;
  const flat: Array<[string, number, number]> = [];
  for (const [name, pos] of positions) {
    if (!Number.isFinite(pos.x) || !Number.isFinite(pos.y)) continue;
    // Round to 1 decimal — sub-pixel precision is invisible at any
    // realistic zoom and trims the payload by ~30 %.
    flat.push([name, Math.round(pos.x * 10) / 10, Math.round(pos.y * 10) / 10]);
  }
  const payload: SerializedLayout = {
    v: STORAGE_VERSION,
    fingerprint,
    nodeCount: flat.length,
    positions: flat,
    transform,
    savedAt: Date.now(),
  };
  try {
    window.localStorage.setItem(storageKey(workspace), JSON.stringify(payload));
  } catch {
    // QuotaExceeded or storage disabled — fail open.  The graph
    // still works, the user just doesn't get the warm-start speedup
    // on the next session.
  }
}

export function clearGraphLayout(workspace: string): void {
  if (typeof window === "undefined") return;
  try {
    window.localStorage.removeItem(storageKey(workspace));
  } catch {
    /* see above */
  }
}
