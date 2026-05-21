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
import {
  forwardRef,
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type PointerEvent as ReactPointerEvent,
} from "react";
import { AlertTriangle, Loader2 } from "lucide-react";

import { Button } from "@/components/ui/button";
import { RefreshIcon } from "@/components/ui/refresh-icon";
import { useApp } from "@/store/app";
import { onWorkspacesChanged, type BrainSnapshot } from "@/lib/tauri";
import {
  getCachedBrainSnapshot,
  refreshBrainSnapshotCache,
  subscribeBrainSnapshotCache,
} from "@/store/brain-cache";
import { cn } from "@/lib/utils";
import { BrainGraph } from "./BrainGraph";
import { BrainTable } from "./BrainTable";

const SPLIT_STORAGE_KEY = "brain_split_v1";
/** Visible seam height (px) — also excluded from drag math. */
const SPLITTER_H = 8;
const MIN_GRAPH_PX = 80;
const MIN_TABLE_PX = 140;
const DEFAULT_SPLIT_RATIO = 0.55;

function readPersistedSplitRatio(): number {
  if (typeof window === "undefined") return DEFAULT_SPLIT_RATIO;
  try {
    const raw = window.localStorage.getItem(SPLIT_STORAGE_KEY);
    if (!raw) return DEFAULT_SPLIT_RATIO;
    const n = Number.parseFloat(raw);
    if (!Number.isFinite(n)) return DEFAULT_SPLIT_RATIO;
    // Legacy v1 stored 10–90 percent; new writes store 0–1 ratio.
    const ratio = n > 1 ? n / 100 : n;
    return Math.max(0.1, Math.min(0.9, ratio));
  } catch {
    return DEFAULT_SPLIT_RATIO;
  }
}

function clampSplitRatio(ratio: number, trackPx: number): number {
  if (trackPx <= MIN_GRAPH_PX + MIN_TABLE_PX) return DEFAULT_SPLIT_RATIO;
  const min = MIN_GRAPH_PX / trackPx;
  const max = 1 - MIN_TABLE_PX / trackPx;
  return Math.max(min, Math.min(max, ratio));
}

function ratioFromPointer(clientY: number, rect: DOMRect): number {
  const track = rect.height - SPLITTER_H;
  if (track <= 0) return DEFAULT_SPLIT_RATIO;
  const graphPx = Math.max(
    MIN_GRAPH_PX,
    Math.min(track - MIN_TABLE_PX, clientY - rect.top),
  );
  return graphPx / track;
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

  const [splitRatio, setSplitRatio] = useState(() => readPersistedSplitRatio());
  const [isDragging, setIsDragging] = useState(false);
  const [containerHeight, setContainerHeight] = useState(0);
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
      window.localStorage.setItem(SPLIT_STORAGE_KEY, splitRatio.toFixed(4));
    } catch {
      /* localStorage may be disabled in private mode — non-fatal */
    }
  }, [splitRatio]);

  useLayoutEffect(() => {
    const el = containerRef.current;
    if (!el) return;
    const measure = () => {
      const h = el.getBoundingClientRect().height;
      setContainerHeight(h);
      setSplitRatio((prev) => clampSplitRatio(prev, Math.max(0, h - SPLITTER_H)));
    };
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  useEffect(() => {
    if (!isDragging) return;
    const prevCursor = document.body.style.cursor;
    const prevSelect = document.body.style.userSelect;
    document.body.style.cursor = "row-resize";
    document.body.style.userSelect = "none";
    return () => {
      document.body.style.cursor = prevCursor;
      document.body.style.userSelect = prevSelect;
    };
  }, [isDragging]);

  const applySplitFromPointer = useCallback((clientY: number) => {
    const el = containerRef.current;
    if (!el) return;
    const rect = el.getBoundingClientRect();
    setSplitRatio(clampSplitRatio(ratioFromPointer(clientY, rect), rect.height - SPLITTER_H));
  }, []);

  const endSplitDrag = useCallback((e: ReactPointerEvent) => {
    if (e.currentTarget.hasPointerCapture(e.pointerId)) {
      e.currentTarget.releasePointerCapture(e.pointerId);
    }
    setIsDragging(false);
  }, []);

  const trackPx = Math.max(0, containerHeight - SPLITTER_H);
  const graphPanePx =
    trackPx > 0
      ? Math.round(clampSplitRatio(splitRatio, trackPx) * trackPx)
      : null;

  if (!activeWorkspace) {
    return (
      <div className="flex h-full flex-col items-center justify-center gap-2 px-4 text-center">
        <p className="text-xs text-muted-foreground">No workspace selected.</p>
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col">
      {!panelMode && (
        <>
          <header className="flex h-11 shrink-0 items-center gap-2 border-b border-border px-4">
            <span className="text-sm font-medium">{activeWorkspace}</span>
            <span className="text-muted-foreground">·</span>
            <span className="text-xs text-muted-foreground">Brain</span>
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
        className={`relative flex flex-1 flex-col overflow-hidden bg-background ${isDragging ? "select-none" : ""}`}
      >
        <div
          className="relative shrink-0 overflow-hidden bg-background"
          style={
            graphPanePx !== null
              ? { height: graphPanePx, minHeight: MIN_GRAPH_PX }
              : { height: `${splitRatio * 100}%`, minHeight: MIN_GRAPH_PX }
          }
        >
          {snap ? (
            <BrainGraphHud
              snap={snap}
              loading={loading}
              panelMode={panelMode}
              lastLoadedAt={lastLoadedAt}
              contradictionCount={brief?.contradiction_count ?? 0}
              onReload={() => void load()}
            />
          ) : null}
          <div className={isDragging ? "pointer-events-none h-full" : "h-full"}>
            {snap ? (
              <BrainGraph
                key={activeWorkspace}
                cacheKey={activeWorkspace}
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
        </div>

        <BrainSplitHandle
          isDragging={isDragging}
          splitPercent={Math.round(splitRatio * 100)}
          onNudge={(delta) => {
            setSplitRatio((prev) =>
              clampSplitRatio(prev + delta, trackPx > 0 ? trackPx : 1),
            );
          }}
          onPointerDown={(e) => {
            if (e.button !== 0) return;
            e.preventDefault();
            e.stopPropagation();
            e.currentTarget.setPointerCapture(e.pointerId);
            setIsDragging(true);
            applySplitFromPointer(e.clientY);
          }}
          onPointerMove={(e) => {
            if (!e.currentTarget.hasPointerCapture(e.pointerId)) return;
            applySplitFromPointer(e.clientY);
          }}
          onPointerUp={endSplitDrag}
          onPointerCancel={endSplitDrag}
        />

        <div
          className="flex-1 overflow-hidden bg-background"
          style={{ minHeight: MIN_TABLE_PX }}
        >
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

/** Draggable seam between graph and claims table. */
const BrainSplitHandle = forwardRef<
  HTMLDivElement,
  {
    isDragging: boolean;
    splitPercent: number;
    onNudge: (delta: number) => void;
    onPointerDown: (e: ReactPointerEvent<HTMLDivElement>) => void;
    onPointerMove: (e: ReactPointerEvent<HTMLDivElement>) => void;
    onPointerUp: (e: ReactPointerEvent<HTMLDivElement>) => void;
    onPointerCancel: (e: ReactPointerEvent<HTMLDivElement>) => void;
  }
>(function BrainSplitHandle(
  {
    isDragging,
    splitPercent,
    onNudge,
    onPointerDown,
    onPointerMove,
    onPointerUp,
    onPointerCancel,
  },
  ref,
) {
  return (
    <div
      ref={ref}
      role="separator"
      aria-orientation="horizontal"
      aria-label="Resize graph and claims table"
      aria-valuemin={10}
      aria-valuemax={90}
      aria-valuenow={splitPercent}
      tabIndex={0}
      onKeyDown={(e) => {
        if (e.key !== "ArrowUp" && e.key !== "ArrowDown") return;
        e.preventDefault();
        const step = e.shiftKey ? 0.06 : 0.025;
        onNudge(e.key === "ArrowDown" ? step : -step);
      }}
      className="relative z-30 shrink-0 touch-none select-none"
      style={{ height: SPLITTER_H }}
    >
      {/* Wide hit target — layout stays SPLITTER_H; drag stays smooth off-seam */}
      <div
        className="absolute inset-x-0 -top-3 -bottom-3 cursor-row-resize"
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={onPointerUp}
        onPointerCancel={onPointerCancel}
      />
      <div
        className={cn(
          "pointer-events-none flex h-full w-full items-center justify-center transition-[background-color,border-color,box-shadow]",
          isDragging
            ? "border-y-2 border-accent bg-accent/20 shadow-[inset_0_0_0_1px_hsl(var(--accent)/0.45)]"
            : "border-y border-border bg-muted/25",
        )}
      >
        <div
          className={cn(
            "rounded-full transition-all",
            isDragging
              ? "h-1 w-16 bg-accent/80"
              : "h-1 w-10 bg-muted-foreground/40",
          )}
          aria-hidden
        />
      </div>
    </div>
  );
});

function BrainGraphHud({
  snap,
  loading,
  panelMode,
  lastLoadedAt,
  contradictionCount,
  onReload,
}: {
  snap: BrainSnapshot;
  loading: boolean;
  panelMode: boolean;
  lastLoadedAt: number;
  contradictionCount: number;
  onReload: () => void;
}) {
  return (
    <div className="pointer-events-none absolute right-2 top-2 z-20 flex items-center gap-1.5">
      <span
        className={cn(
          "rounded-lg border border-border/50 bg-background/95 px-2.5 py-1",
          "text-[10px] tabular-nums text-muted-foreground",
        )}
      >
        {snap.claims.length} claims · {snap.entities.length} entities ·{" "}
        {snap.relations.length} relations
        {loading ? (
          <span className="ml-1.5 text-accent">updating</span>
        ) : !panelMode && lastLoadedAt > 0 ? (
          <span className="ml-1.5 opacity-70">· {formatCacheAge(lastLoadedAt)}</span>
        ) : null}
        {contradictionCount > 0 ? (
          <span className="ml-1.5 text-amber-600 dark:text-amber-400">
            · {contradictionCount} contradiction
            {contradictionCount === 1 ? "" : "s"}
          </span>
        ) : null}
      </span>
      <Button
        type="button"
        variant="ghost"
        size="icon"
        className={cn(
          "pointer-events-auto h-7 w-7 rounded-lg border border-border/50",
          "bg-background/95 hover:bg-muted/60",
        )}
        onClick={onReload}
        disabled={loading}
        aria-label={loading ? "Refreshing graph" : "Refresh graph"}
        title={loading ? "Refreshing…" : "Refresh graph"}
      >
        {loading ? (
          <Loader2 className="size-3.5 animate-spin text-accent" aria-hidden />
        ) : (
          <RefreshIcon className="size-3.5 text-muted-foreground" aria-hidden />
        )}
      </Button>
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
