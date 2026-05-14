import { useEffect, useState } from "react";
import { FlaskConical } from "lucide-react";

import { useApp } from "@/store/app";
import { DropZone } from "@/components/playground/DropZone";
import { PaperPanel } from "@/components/playground/PaperPanel";
import { SourceLibrary } from "@/components/playground/SourceLibrary";

/**
 * Playground surface — the researcher / student-facing workspace
 * view. v1 ships the DropZone (drag-drop file ingest + auto-compile)
 * + SourceLibrary (left-rail file list) + Living Paper viewer.
 * Subsequent commits add citation chips, the cross-workspace chat,
 * and the provenance-trace right panel.
 *
 * The naming convention: "Playground" is the surface the user
 * selects from the icon rail; underneath it composes the same
 * substrate primitives (workspaces, witnesses, branches, paper)
 * that the developer-facing surfaces also use.
 */
export function PlaygroundView() {
  const workspace = useApp((s) => s.activeWorkspace);
  const surface = useApp((s) => s.surface);
  const compileProgress = useApp((s) => s.compileProgress);
  // Refresh SourceLibrary + PaperPanel whenever a compile finishes
  // by bumping a nonce. Both children depend on `refreshNonce` /
  // `workspace`; we don't reload mid-compile (the substrate is in
  // flux) — only on the "done" / "failed" / "cancelled" terminal.
  const [refreshNonce, setRefreshNonce] = useState(0);
  useEffect(() => {
    if (
      compileProgress?.phase === "done" ||
      compileProgress?.phase === "failed" ||
      compileProgress?.phase === "cancelled"
    ) {
      setRefreshNonce((n) => n + 1);
    }
  }, [compileProgress?.phase]);

  return (
    <div className="flex h-full flex-col">
      <header className="flex shrink-0 items-center gap-2 border-b border-border bg-surface/40 px-4 py-2">
        <FlaskConical className="size-4 text-accent" />
        <h2 className="text-sm font-semibold">Playground</h2>
        <span className="text-xs text-muted-foreground">
          Drop sources, ask questions, watch the Living Paper grow.
        </span>
      </header>
      <div className="shrink-0 border-b border-border bg-background px-4 py-2">
        <DropZone workspace={workspace} visible={surface === "playground"} />
      </div>
      <div className="flex flex-1 overflow-hidden">
        <SourceLibrary workspace={workspace} refreshNonce={refreshNonce} />
        <PaperPanel workspace={workspace} refreshNonce={refreshNonce} />
      </div>
    </div>
  );
}
