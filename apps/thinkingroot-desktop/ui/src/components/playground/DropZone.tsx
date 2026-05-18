import { useEffect } from "react";
import { CheckCircle2, FileUp, Loader2, XCircle } from "lucide-react";

import { cn } from "@/lib/utils";
import { useFileDropStore, type FileDropZoneState } from "@/store/file-drop";

/**
 * Playground status strip for inbox ingest + compile after a window drop.
 * Drag UI and routing live in {@link WindowDragDropOverlay}.
 */
export function DropZone({
  workspace,
  visible,
}: {
  workspace: string | null;
  visible: boolean;
}) {
  const state = useFileDropStore((s) => s.zoneState);
  const setZoneState = useFileDropStore((s) => s.setZoneState);

  useEffect(() => {
    if (state.kind === "done" || state.kind === "error") {
      const t = setTimeout(() => setZoneState({ kind: "idle" }), 6000);
      return () => clearTimeout(t);
    }
    return;
  }, [state.kind, setZoneState]);

  if (!visible) return null;

  return (
    <div
      className={cn(
        "flex items-center gap-3 rounded-md border border-dashed px-4 py-2 text-sm transition-colors",
        state.kind === "idle"
          ? "border-border bg-surface/40 text-muted-foreground"
          : state.kind === "error"
            ? "border-destructive/50 bg-destructive/5 text-destructive"
            : state.kind === "done"
              ? "border-emerald-500/40 bg-emerald-500/5 text-emerald-700 dark:text-emerald-300"
              : "border-accent/40 bg-accent/5 text-accent",
      )}
    >
      <ZoneIcon state={state} />
      <ZoneLabel state={state} workspace={workspace} />
    </div>
  );
}

function ZoneIcon({ state }: { state: FileDropZoneState }) {
  if (state.kind === "ingesting" || state.kind === "compiling") {
    return <Loader2 className="size-4 shrink-0 animate-spin" />;
  }
  if (state.kind === "done") {
    return state.compiledOk ? (
      <CheckCircle2 className="size-4 shrink-0" />
    ) : (
      <XCircle className="size-4 shrink-0" />
    );
  }
  if (state.kind === "error") {
    return <XCircle className="size-4 shrink-0" />;
  }
  return <FileUp className="size-4 shrink-0" />;
}

function ZoneLabel({
  state,
  workspace,
}: {
  state: FileDropZoneState;
  workspace: string | null;
}) {
  switch (state.kind) {
    case "idle":
      return (
        <span>
          {workspace
            ? `Drop files anywhere — they land in ${workspace}/inbox and compile.`
            : "Drop files anywhere — pick a workspace first."}
        </span>
      );
    case "ingesting":
      return <span>Copying {state.count} file{state.count === 1 ? "" : "s"} into inbox…</span>;
    case "compiling":
      return (
        <span>
          {state.outcome.copied} added — compiling…
          {state.outcome.skipped_duplicate > 0 &&
            ` (${state.outcome.skipped_duplicate} duplicate)`}
        </span>
      );
    case "done":
      return (
        <span>
          {state.outcome.copied > 0 &&
            (state.compiledOk ? "Compile finished" : "Compile failed (run manually)")}
        </span>
      );
    case "error":
      return <span>Drop failed: {state.message}</span>;
  }
}
