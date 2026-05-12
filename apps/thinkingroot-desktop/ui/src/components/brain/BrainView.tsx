/**
 * Brain workbench. Two-pane: graph on top, virtualised claim table
 * below. Loads from `brain_load`, which fans out to claims +
 * entities + relations + rooted ids in one Tauri trip. Header pulls
 * `brainBrief` separately so the count line is the daemon's
 * authoritative summary (with top entities + recent decisions),
 * not the front-end's count of whatever happens to be loaded.
 *
 * Split position is persisted to localStorage so the graph/table
 * ratio survives a reload.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { Folder, RefreshCw, AlertTriangle } from "lucide-react";

import { Button } from "@/components/ui/button";
import { useApp } from "@/store/app";
import {
  onWorkspacesChanged,
} from "@/lib/tauri";
import {
  getCachedBrainSnapshot,
  refreshBrainSnapshotCache,
  subscribeBrainSnapshotCache,
} from "@/store/brain-cache";
import { BrainGraph } from "./BrainGraph";
import { BrainTable } from "./BrainTable";

const SPLIT_STORAGE_KEY = "brain_split_v1";

function readPersistedSplit(): number {
  if (typeof window === "undefined") return 55;
  try {
    const raw = window.localStorage.getItem(SPLIT_STORAGE_KEY);
    if (!raw) return 55;
    const n = Number.parseFloat(raw);
    if (!Number.isFinite(n)) return 55;
    return Math.max(10, Math.min(90, n));
  } catch {
    return 55;
  }
}

export function BrainView({
  panelMode = false,
  isVisible = true,
}: {
  panelMode?: boolean;
  /** When false (e.g. another rail tab is active) BrainView is still
   *  mounted but its force-layout simulation pauses to avoid burning
   *  CPU on a canvas the user can't see.  Defaults to true for the
   *  non-panel mount sites that don't toggle visibility. */
  isVisible?: boolean;
}) {
  const activeWorkspace = useApp((s) => s.activeWorkspace);
  const compileProgress = useApp((s) => s.compileProgress);
  const initialCache = activeWorkspace ? getCachedBrainSnapshot(activeWorkspace) : null;
  const [snap, setSnap] = useState(() => initialCache?.snap ?? null);
  const [brief, setBrief] = useState(() => initialCache?.brief ?? null);
  const [lastLoadedAt, setLastLoadedAt] = useState(() => initialCache?.loadedAt ?? 0);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  const [topHeight, setTopHeight] = useState<number>(() => readPersistedSplit());
  const [isDragging, setIsDragging] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);

  const [searchQuery, setSearchQuery] = useState("");
  const [tierFilter, setTierFilter] = useState<"all" | "rooted" | "attested" | "unknown">("all");

  const load = useCallback(async (opts: { background?: boolean } = {}) => {
    if (!activeWorkspace) return;
    const workspace = activeWorkspace;
    const background = opts.background ?? false;
    if (!background) setLoading(true);
    else if (!getCachedBrainSnapshot(workspace)) setLoading(true);
    setError(null);
    try {
      const entry = await refreshBrainSnapshotCache(workspace);
      if (useApp.getState().activeWorkspace !== workspace) return;
      setSnap(entry.snap);
      setBrief(entry.brief);
      setLastLoadedAt(entry.loadedAt);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      if (!getCachedBrainSnapshot(workspace)) {
        setSnap(null);
        setBrief(null);
        setLastLoadedAt(0);
      }
    } finally {
      setLoading(false);
    }
  }, [activeWorkspace]);

  useEffect(() => {
    if (!activeWorkspace) return;
    const cached = getCachedBrainSnapshot(activeWorkspace);
    if (cached) {
      setSnap(cached.snap);
      setBrief(cached.brief);
      setLastLoadedAt(cached.loadedAt);
      setError(null);
      void load({ background: true });
    } else {
      setSnap(null);
      setBrief(null);
      setLastLoadedAt(0);
      void load();
    }
  }, [activeWorkspace, load]);

  useEffect(() => {
    if (!activeWorkspace) return;
    return subscribeBrainSnapshotCache((workspace, entry) => {
      if (workspace !== activeWorkspace) return;
      setSnap(entry.snap);
      setBrief(entry.brief);
      setLastLoadedAt(entry.loadedAt);
      setError(null);
      setLoading(false);
    });
  }, [activeWorkspace]);

  useEffect(() => {
    if (!activeWorkspace || compileProgress?.phase !== "done") return;
    void load({ background: true });
  }, [activeWorkspace, compileProgress, load]);

  useEffect(() => {
    if (!activeWorkspace) return;
    let unlisten: (() => void) | undefined;
    onWorkspacesChanged(() => {
      void load({ background: true });
    }).then((un) => {
      unlisten = un;
    });
    return () => {
      unlisten?.();
    };
  }, [activeWorkspace, load]);

  useEffect(() => {
    if (typeof window === "undefined") return;
    try {
      window.localStorage.setItem(SPLIT_STORAGE_KEY, topHeight.toFixed(2));
    } catch {
      /* localStorage may be disabled in private mode — non-fatal */
    }
  }, [topHeight]);

  useEffect(() => {
    if (!isDragging) return;

    const handleMouseMove = (e: MouseEvent) => {
      if (!containerRef.current) return;
      const rect = containerRef.current.getBoundingClientRect();
      const offset = e.clientY - rect.top;
      const percent = (offset / rect.height) * 100;
      // Clamp between 10% and 90%
      setTopHeight(Math.max(10, Math.min(90, percent)));
    };

    const handleMouseUp = () => setIsDragging(false);

    window.addEventListener("mousemove", handleMouseMove);
    window.addEventListener("mouseup", handleMouseUp);
    return () => {
      window.removeEventListener("mousemove", handleMouseMove);
      window.removeEventListener("mouseup", handleMouseUp);
    };
  }, [isDragging]);

  if (!activeWorkspace) {
    return (
      <div className="flex h-full flex-col items-center justify-center gap-2 px-4 text-center">
        <p className="text-xs text-muted-foreground">No workspace selected.</p>
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col">
      {panelMode ? (
        <div className="flex shrink-0 items-center justify-between border-b border-border/50 px-3 py-1.5">
          {snap && (
            <span className="text-[10px] text-muted-foreground">
              {snap.claims.length} claims · {snap.entities.length} entities
              {loading && " · updating"}
            </span>
          )}
          <Button
            variant="ghost"
            size="icon"
            className="ml-auto h-5 w-5"
            onClick={() => void load()}
            disabled={loading}
            aria-label="Reload"
          >
            <RefreshCw className={loading ? "size-3 animate-spin" : "size-3"} />
          </Button>
        </div>
      ) : (
        <>
          <header className="flex h-11 shrink-0 items-center gap-2 border-b border-border px-4">
            <Folder className="size-4 text-muted-foreground" />
            <span className="text-sm font-medium">{activeWorkspace}</span>
            <span className="text-muted-foreground">·</span>
            <span className="text-xs text-muted-foreground">Brain</span>
            {snap && (
              <span className="ml-2 text-[10px] text-muted-foreground">
                {snap.claims.length} claims · {snap.entities.length} entities ·{" "}
                {snap.relations.length} relations
                {loading && <> · updating</>}
                {!loading && lastLoadedAt > 0 && (
                  <> · cached {formatCacheAge(lastLoadedAt)}</>
                )}
                {brief && brief.contradiction_count > 0 && (
                  <>
                    {" · "}
                    <span className="text-amber-600 dark:text-amber-400">
                      {brief.contradiction_count} contradiction
                      {brief.contradiction_count === 1 ? "" : "s"}
                    </span>
                  </>
                )}
              </span>
            )}
            <Button
              variant="ghost"
              size="icon"
              className="ml-auto h-7 w-7"
              onClick={() => void load()}
              disabled={loading}
              aria-label="Reload"
            >
              <RefreshCw className={loading ? "size-3.5 animate-spin" : "size-3.5"} />
            </Button>
          </header>
          {brief && brief.top_entities.length > 0 && (
            <div className="flex shrink-0 items-center gap-2 overflow-x-auto border-b border-border bg-muted/10 px-4 py-1.5">
              <span className="text-[10px] font-semibold uppercase tracking-widest text-muted-foreground">
                Top
              </span>
              {brief.top_entities.slice(0, 8).map((e) => (
                <button
                  key={e.name}
                  type="button"
                  onClick={() => setSearchQuery(e.name)}
                  className="flex shrink-0 items-center gap-1 rounded-full bg-muted/40 px-2 py-0.5 text-[10px] text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
                  title={`Filter table for ${e.name} (${e.entity_type})`}
                >
                  <span className="font-medium">{e.name}</span>
                  <span>·</span>
                  <span>{e.claim_count}</span>
                </button>
              ))}
            </div>
          )}
        </>
      )}

      {error && (
        <div className={panelMode 
          ? "px-3 py-2 text-[11px] text-destructive"
          : "flex items-start gap-2 border-b border-destructive/20 bg-destructive/10 px-4 py-2 text-xs text-destructive"}>
          {!panelMode && <AlertTriangle className="mt-0.5 size-3.5 shrink-0" />}
          <span>{error}</span>
        </div>
      )}

      <div
        ref={containerRef}
        className={`relative flex flex-1 flex-col overflow-hidden ${isDragging ? "select-none" : ""}`}
      >
        <div
          className="relative min-h-[100px] border-b border-border"
          style={{ height: `${topHeight}%` }}
        >
          <div className={isDragging ? "pointer-events-none h-full" : "h-full"}>
            {snap ? (
              <BrainGraph
                key={activeWorkspace}
                cacheKey={activeWorkspace}
                isRefreshing={loading}
                isVisible={isVisible}
                entities={snap.entities}
                relations={snap.relations}
                claims={snap.claims}
                searchQuery={searchQuery}
              />
            ) : (
              <Skeleton text="Loading graph…" />
            )}
          </div>

          {/* Invisible resize handle */}
          <div
            className="absolute bottom-[-4px] left-0 right-0 z-50 h-2 cursor-row-resize bg-transparent"
            onMouseDown={(e) => {
              e.preventDefault();
              setIsDragging(true);
            }}
          />
        </div>
        <div className="flex-1 overflow-hidden">
          <div className={isDragging ? "pointer-events-none h-full" : "h-full"}>
            {snap ? (
              <BrainTable 
                claims={snap.claims}
                query={searchQuery}
                setQuery={setSearchQuery}
                tierFilter={tierFilter}
                setTierFilter={setTierFilter}
              />
            ) : (
              <Skeleton text="Loading claims…" />
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

function Skeleton({ text }: { text: string }) {
  return (
    <div className="flex h-full items-center justify-center text-xs text-muted-foreground">
      {text}
    </div>
  );
}

function formatCacheAge(loadedAt: number): string {
  const ageMs = Math.max(0, Date.now() - loadedAt);
  if (ageMs < 5_000) return "now";
  if (ageMs < 60_000) return `${Math.round(ageMs / 1_000)}s ago`;
  return `${Math.round(ageMs / 60_000)}m ago`;
}
