import { useEffect, useRef } from "react";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { FileUp } from "lucide-react";

import { routeDesktopFileDrop } from "@/lib/route-file-drop";
import { cn } from "@/lib/utils";
import { useFileDropStore } from "@/store/file-drop";

/**
 * Full-window drag target — blur + dashed frame while files hover,
 * routes drops to chat composer or workspace inbox by surface.
 */
export function WindowDragDropOverlay() {
  const dragOverlay = useFileDropStore((s) => s.dragOverlay);
  const setDragOverlay = useFileDropStore((s) => s.setDragOverlay);
  const depthRef = useRef(0);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;

    void getCurrentWebview()
      .onDragDropEvent((event) => {
        const payload = event.payload;
        if (payload.type === "enter") {
          depthRef.current += 1;
          setDragOverlay(true);
          return;
        }
        if (payload.type === "over") {
          setDragOverlay(true);
          return;
        }
        if (payload.type === "leave") {
          depthRef.current = Math.max(0, depthRef.current - 1);
          if (depthRef.current === 0) setDragOverlay(false);
          return;
        }
        if (payload.type === "drop") {
          depthRef.current = 0;
          setDragOverlay(false);
          const paths = payload.paths.filter(
            (p) => !p.toLowerCase().endsWith(".tr"),
          );
          if (paths.length > 0) void routeDesktopFileDrop(paths);
        }
      })
      .then((fn) => {
        if (cancelled) {
          fn();
          return;
        }
        unlisten = fn;
      });

    return () => {
      cancelled = true;
      unlisten?.();
      depthRef.current = 0;
      setDragOverlay(false);
    };
  }, [setDragOverlay]);

  if (!dragOverlay) return null;

  return (
    <div
      className="pointer-events-none fixed inset-0 z-[65] flex items-center justify-center"
      role="status"
      aria-live="polite"
      aria-label="Drop files anywhere"
    >
      <div className="absolute inset-0 bg-background/50 backdrop-blur-[3px]" />
      <div
        className={cn(
          "absolute inset-3 rounded-2xl border-2 border-dashed sm:inset-5",
          "border-muted-foreground/30 bg-muted/[0.06]",
        )}
        aria-hidden
      />
      <div className="relative flex flex-col items-center gap-2 px-6 text-center">
        <span className="flex size-10 items-center justify-center rounded-full border border-border/50 bg-muted/40 text-muted-foreground">
          <FileUp className="size-5" strokeWidth={1.75} aria-hidden />
        </span>
        <p className="text-sm font-medium text-foreground/90">Drop anywhere</p>
        <p className="max-w-xs text-[12px] leading-snug text-muted-foreground">
          PDFs, images, code, and other files
        </p>
      </div>
    </div>
  );
}
