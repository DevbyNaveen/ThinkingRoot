/**
 * Brain workbench. Two-pane: graph on top, virtualised claim table
 * below. Loads from `brain_load`, which fans out to claims +
 * entities + relations + rooted ids in one Tauri trip.
 */
import { useEffect, useState, useRef } from "react";
import { Folder, RefreshCw, AlertTriangle } from "lucide-react";

import { Button } from "@/components/ui/button";
import { useApp } from "@/store/app";
import { brainLoad, type BrainSnapshot } from "@/lib/tauri";
import { BrainGraph } from "./BrainGraph";
import { BrainTable } from "./BrainTable";

export function BrainView() {
  const activeWorkspace = useApp((s) => s.activeWorkspace);
  const [snap, setSnap] = useState<BrainSnapshot | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  const [topHeight, setTopHeight] = useState(55);
  const [isDragging, setIsDragging] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);

  const [searchQuery, setSearchQuery] = useState("");
  const [tierFilter, setTierFilter] = useState<"all" | "rooted" | "attested" | "unknown">("all");

  async function load() {
    setLoading(true);
    setError(null);
    try {
      const s = await brainLoad();
      setSnap(s);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setSnap(null);
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    if (activeWorkspace) void load();
  }, [activeWorkspace]);

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
      <div className="flex h-full flex-col items-center justify-center gap-2 px-8 text-center">
        <h2 className="text-base font-medium">No workspace selected</h2>
        <p className="max-w-sm text-sm text-muted-foreground">
          Pick a workspace from the sidebar to load its knowledge graph.
        </p>
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col">
      <header className="flex h-11 shrink-0 items-center gap-2 border-b border-border px-4">
        <Folder className="size-4 text-muted-foreground" />
        <span className="text-sm font-medium">{activeWorkspace}</span>
        <span className="text-muted-foreground">·</span>
        <span className="text-xs text-muted-foreground">Brain</span>
        {snap && (
          <span className="ml-2 text-[10px] text-muted-foreground">
            {snap.claims.length} claims · {snap.entities.length} entities ·{" "}
            {snap.relations.length} relations
          </span>
        )}
        <Button
          variant="ghost"
          size="icon"
          className="ml-auto h-7 w-7"
          onClick={load}
          disabled={loading}
          aria-label="Reload"
        >
          <RefreshCw className={loading ? "size-3.5 animate-spin" : "size-3.5"} />
        </Button>
      </header>

      {error && (
        <div className="flex items-start gap-2 border-b border-destructive/20 bg-destructive/10 px-4 py-2 text-xs text-destructive">
          <AlertTriangle className="mt-0.5 size-3.5 shrink-0" />
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
