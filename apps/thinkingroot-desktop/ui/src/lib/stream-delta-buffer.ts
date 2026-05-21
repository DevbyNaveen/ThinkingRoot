/**
 * Coalesce high-frequency token deltas to one store write per animation
 * frame — cuts React/markdown churn during assistant streaming.
 */

let pending = "";
let rafId: number | null = null;
let activeFlush: ((batch: string) => void) | null = null;

export function resetStreamDeltaBuffer(): void {
  pending = "";
  activeFlush = null;
  if (rafId != null) {
    cancelAnimationFrame(rafId);
    rafId = null;
  }
}

export function pushStreamDelta(delta: string, flush: (batch: string) => void): void {
  if (!delta) return;
  pending += delta;
  activeFlush = flush;
  if (rafId != null) return;
  rafId = requestAnimationFrame(() => {
    rafId = null;
    const batch = pending;
    pending = "";
    if (batch && activeFlush) {
      activeFlush(batch);
    }
  });
}

/** Drain any buffered text before finalizing or cancelling a turn. */
export function flushStreamDeltaBuffer(flush: (batch: string) => void): void {
  if (rafId != null) {
    cancelAnimationFrame(rafId);
    rafId = null;
  }
  if (pending) {
    const batch = pending;
    pending = "";
    flush(batch);
  }
  activeFlush = null;
}
