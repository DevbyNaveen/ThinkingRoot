/**
 * Slice 10 — brain graph live-activity store + streaming `[claim:<id>]`
 * citation parser.
 *
 * The chat token-handler feeds every assistant token through
 * {@link BrainCitationParser.feed}; emitted citations are forwarded to
 * {@link useBrainActivation.touch} which records an exponential-decay
 * activation per claim id. The d3-force render loop in `BrainGraph.tsx`
 * subscribes and re-renders the cited node with a pulse class.
 *
 * Honesty contract:
 * - Citations are emitted only when the literal `[claim:<id>]` marker
 *   appears in the stream. No fabrication from retrieval metadata.
 * - The store decays activations on a `requestAnimationFrame` loop so a
 *   highlight visibly fades out within ~2s of the citation, matching
 *   the user's mental model of "the model is *currently* thinking
 *   about claim X."
 */

import { create } from "zustand";

const CITATION_PREFIX = "[claim:";
const MAX_PENDING = 80;
const MAX_ID_LEN = 64;

/** One emitted citation. */
export interface BrainCitation {
  claimId: string;
}

/** Mirrors the engine-side `thinkingroot_extract::citation::CitationParser`. */
export class BrainCitationParser {
  private buf = "";
  private seen = new Set<string>();

  feed(chunk: string): BrainCitation[] {
    if (!chunk && !this.buf) return [];
    this.buf += chunk;
    const out: BrainCitation[] = [];

    // Bounded loop — at most one emission per pass through the buffer.
    // eslint-disable-next-line no-constant-condition
    while (true) {
      const prefixAt = this.buf.indexOf(CITATION_PREFIX);
      if (prefixAt === -1) {
        // Drop everything except a tail that could still become a
        // marker in the next chunk.
        if (this.buf.length > MAX_PENDING) {
          this.buf = this.buf.slice(this.buf.length - MAX_PENDING);
        }
        break;
      }
      if (prefixAt > 0) this.buf = this.buf.slice(prefixAt);

      const after = CITATION_PREFIX.length;
      const closeAt = this.buf.indexOf("]", after);
      if (closeAt === -1) {
        if (this.buf.length > MAX_PENDING) {
          // Skip past this aborted prefix so the next legitimate
          // marker can start matching.
          this.buf = this.buf.slice(1);
          continue;
        }
        break;
      }

      const id = this.buf.slice(after, closeAt);
      this.buf = this.buf.slice(closeAt + 1);
      if (this.isValid(id)) {
        if (!this.seen.has(id)) {
          this.seen.add(id);
          out.push({ claimId: id });
        }
      }
    }
    return out;
  }

  reset() {
    this.buf = "";
    this.seen.clear();
  }

  private isValid(id: string) {
    if (id.length === 0 || id.length > MAX_ID_LEN) return false;
    return /^[A-Za-z0-9_-]+$/.test(id);
  }
}

export type ActivationKind = "cited" | "retrieved" | "cascade";

export interface Activation {
  intensity: number;
  /** Performance.now() of the last touch — drives the decay function. */
  lastTouchedMs: number;
  kind: ActivationKind;
}

interface BrainActivationStore {
  /** Map keyed by entity id (NOT claim id — citation handler resolves
   *  the claim → entity edge before calling touch). */
  activations: Record<string, Activation>;
  touch: (entityId: string, kind: ActivationKind, intensity: number) => void;
  /** Record N entities at once (used by spreading-activation cascades). */
  touchMany: (
    entries: Array<{
      entityId: string;
      kind: ActivationKind;
      intensity: number;
    }>,
  ) => void;
  /** Apply exponential decay; called from a requestAnimationFrame loop. */
  decay: (nowMs: number) => void;
  clear: () => void;
}

const TAU_MS = 2000; // half-life ~1.4s

export const useBrainActivation = create<BrainActivationStore>((set) => ({
  activations: {},
  touch: (entityId, kind, intensity) =>
    set((s) => ({
      activations: {
        ...s.activations,
        [entityId]: {
          intensity: Math.min(1.5, Math.max(intensity, s.activations[entityId]?.intensity ?? 0)),
          lastTouchedMs: performance.now(),
          kind,
        },
      },
    })),
  touchMany: (entries) =>
    set((s) => {
      const now = performance.now();
      const next = { ...s.activations };
      for (const e of entries) {
        const prev = next[e.entityId]?.intensity ?? 0;
        next[e.entityId] = {
          intensity: Math.min(1.5, Math.max(e.intensity, prev)),
          lastTouchedMs: now,
          kind: e.kind,
        };
      }
      return { activations: next };
    }),
  decay: (nowMs) =>
    set((s) => {
      const next: Record<string, Activation> = {};
      for (const [id, a] of Object.entries(s.activations)) {
        const dt = nowMs - a.lastTouchedMs;
        const intensity = a.intensity * Math.exp(-dt / TAU_MS);
        if (intensity > 0.05) {
          next[id] = { ...a, intensity };
        }
      }
      return { activations: next };
    }),
  clear: () => set({ activations: {} }),
}));
