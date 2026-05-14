import { useEffect, useState } from "react";
import { FileText, FlaskConical, MessageSquareText } from "lucide-react";

import { useApp } from "@/store/app";
import { ChatView } from "@/components/chat/ChatView";
import { DropZone } from "@/components/playground/DropZone";
import { PaperPanel } from "@/components/playground/PaperPanel";
import { SourceDetailPanel } from "@/components/playground/SourceDetailPanel";
import { SourceLibrary } from "@/components/playground/SourceLibrary";
import { cn } from "@/lib/utils";
import type { PlaygroundSource } from "@/lib/tauri";

type PlaygroundTab = "paper" | "chat";

/**
 * Playground surface — the researcher / student-facing workspace
 * view. v1 ships:
 *   - DropZone (drag-drop file ingest + auto-compile)
 *   - SourceLibrary (left-rail file list grouped by kind)
 *   - Paper / Chat tab switcher in the center pane:
 *       · Paper tab → Living Paper viewer
 *       · Chat tab  → ChatView (the same workspace-scoped chat the
 *         Conversations surface uses; the differentiator is the
 *         surrounding Playground context, not a separate chat
 *         engine)
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
  const [tab, setTab] = useState<PlaygroundTab>("paper");
  const [selectedSource, setSelectedSource] = useState<PlaygroundSource | null>(
    null,
  );
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
  // Clear selection on workspace switch — selected source IDs are
  // workspace-scoped (ULIDs), keeping a stale one open would show
  // "no witnesses" because the new workspace doesn't carry the id.
  useEffect(() => {
    setSelectedSource(null);
  }, [workspace]);

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
        <SourceLibrary
          workspace={workspace}
          refreshNonce={refreshNonce}
          selectedSourceId={selectedSource?.id ?? null}
          onSelect={setSelectedSource}
        />
        <section className="flex min-w-0 flex-1 flex-col">
          <div className="flex shrink-0 items-center gap-1 border-b border-border bg-surface/20 px-2 py-1">
            <TabButton
              active={tab === "paper"}
              onClick={() => setTab("paper")}
              icon={<FileText className="size-3.5" />}
              label="Paper"
            />
            <TabButton
              active={tab === "chat"}
              onClick={() => setTab("chat")}
              icon={<MessageSquareText className="size-3.5" />}
              label="Chat"
            />
          </div>
          <div className="flex flex-1 overflow-hidden">
            {tab === "paper" ? (
              <PaperPanel workspace={workspace} refreshNonce={refreshNonce} />
            ) : (
              <ChatView />
            )}
          </div>
        </section>
        {selectedSource && (
          <SourceDetailPanel
            sourceId={selectedSource.id}
            sourceUri={selectedSource.uri}
            onClose={() => setSelectedSource(null)}
          />
        )}
      </div>
    </div>
  );
}

function TabButton({
  active,
  onClick,
  icon,
  label,
}: {
  active: boolean;
  onClick: () => void;
  icon: React.ReactNode;
  label: string;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-pressed={active}
      className={cn(
        "flex items-center gap-1.5 rounded-md px-2.5 py-1 text-xs font-medium transition-colors",
        active
          ? "bg-accent/15 text-accent"
          : "text-muted-foreground hover:bg-muted/40 hover:text-foreground",
      )}
    >
      {icon}
      {label}
    </button>
  );
}
