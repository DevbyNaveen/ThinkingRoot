import {
  brainBrief,
  brainLoad,
  type BrainSnapshot,
  type WorkspaceBrief,
} from "@/lib/tauri";

export interface BrainSnapshotCacheEntry {
  snap: BrainSnapshot;
  brief: WorkspaceBrief | null;
  loadedAt: number;
}

type Listener = (workspace: string, entry: BrainSnapshotCacheEntry) => void;

const cache = new Map<string, BrainSnapshotCacheEntry>();
const inflight = new Map<string, Promise<BrainSnapshotCacheEntry>>();
const listeners = new Set<Listener>();

export function getCachedBrainSnapshot(
  workspace: string,
): BrainSnapshotCacheEntry | null {
  return cache.get(workspace) ?? null;
}

export function subscribeBrainSnapshotCache(listener: Listener): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export async function refreshBrainSnapshotCache(
  workspace: string,
): Promise<BrainSnapshotCacheEntry> {
  const existing = inflight.get(workspace);
  if (existing) return existing;

  const promise = Promise.all([
    brainLoad(),
    // Briefs enrich the header but should never block graph readiness.
    brainBrief().catch(() => null),
  ])
    .then(([snap, brief]) => {
      const entry = { snap, brief, loadedAt: Date.now() };
      cache.set(workspace, entry);
      for (const listener of listeners) listener(workspace, entry);
      return entry;
    })
    .finally(() => {
      inflight.delete(workspace);
    });

  inflight.set(workspace, promise);
  return promise;
}
