import { useCallback, useEffect, useRef, useState } from "react";
import { CheckCircle2, FileUp, Loader2, XCircle } from "lucide-react";

import {
  onPlaygroundFilesDropped,
  playgroundDrop,
  workspaceCompile,
  type DropOutcome,
} from "@/lib/tauri";
import { cn } from "@/lib/utils";

type ZoneState =
  | { kind: "idle" }
  | { kind: "ingesting"; count: number }
  | { kind: "compiling"; outcome: DropOutcome }
  | { kind: "done"; outcome: DropOutcome; compiledOk: boolean }
  | { kind: "error"; message: string };

/**
 * DropZone — accepts files dragged onto the desktop window and
 * routes them into the active workspace's `inbox/` directory, then
 * auto-triggers a compile.
 *
 * Listens for `playground-files-dropped` events emitted by the Tauri
 * lib.rs window-event handler. The handler already strips `.tr`
 * paths off to the install sheet, so anything reaching this listener
 * is genuine source material.
 *
 * Honest UX:
 * - Visual feedback is staged (ingesting → compiling → done) so a
 *   user can see what's happening.
 * - The `DropOutcome` summary surfaces verbatim — we never silently
 *   overwrite a same-name source; the user sees `skipped_duplicate`
 *   and can rename + retry.
 * - Compile failure doesn't roll back the file copy; the toast
 *   honestly reports "files added, compile failed — run compile
 *   manually".
 */
export function DropZone({
  workspace,
  visible,
}: {
  workspace: string | null;
  visible: boolean;
}) {
  const [state, setState] = useState<ZoneState>({ kind: "idle" });
  // Hold the latest workspace in a ref so the listener callback (set
  // up once on mount) can read the current value without re-binding.
  const workspaceRef = useRef(workspace);
  workspaceRef.current = workspace;

  const handleDrop = useCallback(async (paths: string[]) => {
    const ws = workspaceRef.current;
    if (!ws) {
      setState({
        kind: "error",
        message: "No active workspace — pick one before dropping files.",
      });
      return;
    }
    setState({ kind: "ingesting", count: paths.length });
    try {
      const outcome = await playgroundDrop(ws, paths);
      if (outcome.copied === 0) {
        setState({ kind: "done", outcome, compiledOk: true });
        return;
      }
      setState({ kind: "compiling", outcome });
      let compiledOk = true;
      try {
        await workspaceCompile({ target: ws });
      } catch {
        compiledOk = false;
      }
      setState({ kind: "done", outcome, compiledOk });
    } catch (e) {
      setState({
        kind: "error",
        message: e instanceof Error ? e.message : String(e),
      });
    }
  }, []);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    onPlaygroundFilesDropped(handleDrop).then((un) => {
      unlisten = un;
    });
    return () => {
      unlisten?.();
    };
  }, [handleDrop]);

  // Auto-clear the "done" / "error" badge so the strip returns to
  // idle and the user can drop again without manual dismissal.
  useEffect(() => {
    if (state.kind === "done" || state.kind === "error") {
      const t = setTimeout(() => setState({ kind: "idle" }), 6000);
      return () => clearTimeout(t);
    }
    return;
  }, [state.kind]);

  if (!visible) {
    // Still listen (the effect above), but render nothing — the
    // user is on a different surface.
    return null;
  }

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

function ZoneIcon({ state }: { state: ZoneState }) {
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
  state: ZoneState;
  workspace: string | null;
}) {
  switch (state.kind) {
    case "idle":
      return (
        <span>
          {workspace
            ? `Drop files onto this window — they'll land in ${workspace}/inbox and compile.`
            : "Drop files — no workspace is active yet."}
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
          {summariseOutcome(state.outcome)}
          {state.outcome.copied > 0 &&
            (state.compiledOk
              ? " · compile finished"
              : " · compile failed (run manually)")}
        </span>
      );
    case "error":
      return <span>Drop failed: {state.message}</span>;
  }
}

function summariseOutcome(o: DropOutcome): string {
  const parts: string[] = [];
  if (o.copied > 0) parts.push(`${o.copied} added`);
  if (o.skipped_duplicate > 0)
    parts.push(`${o.skipped_duplicate} duplicate skipped`);
  if (o.skipped_unreadable > 0)
    parts.push(`${o.skipped_unreadable} unreadable skipped`);
  if (parts.length === 0) return "Nothing to add";
  return parts.join(", ");
}
