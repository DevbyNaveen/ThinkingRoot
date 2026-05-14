import { FlaskConical } from "lucide-react";

import { useApp } from "@/store/app";
import { PaperPanel } from "@/components/playground/PaperPanel";

/**
 * Playground surface — the researcher / student-facing workspace
 * view. v1 ships the Living Paper viewer as the primary panel;
 * subsequent commits add the drop-zone, citation chips, the
 * three-pane layout, and the cross-workspace chat.
 *
 * The naming convention: "Playground" is the surface the user
 * selects from the icon rail; underneath it composes the same
 * substrate primitives (workspaces, witnesses, branches, paper)
 * that the developer-facing surfaces also use.
 */
export function PlaygroundView() {
  const workspace = useApp((s) => s.activeWorkspace);

  return (
    <div className="flex h-full flex-col">
      <header className="flex shrink-0 items-center gap-2 border-b border-border bg-surface/40 px-4 py-2">
        <FlaskConical className="size-4 text-accent" />
        <h2 className="text-sm font-semibold">Playground</h2>
        <span className="text-xs text-muted-foreground">
          Drop sources, ask questions, watch the Living Paper grow.
        </span>
      </header>
      <div className="flex flex-1 overflow-hidden">
        <PaperPanel workspace={workspace} />
      </div>
    </div>
  );
}
