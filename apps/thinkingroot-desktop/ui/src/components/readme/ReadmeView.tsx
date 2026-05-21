/**
 * Right-rail Living Paper — renders `.thinkingroot/paper.md` with
 * Mermaid diagrams via {@link ChatMarkdown}. Legacy `.thinkingroot/README.md`
 * is no longer shown in the desktop shell.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { AlertTriangle, FileText, RefreshCw } from "lucide-react";

import { ChatMarkdown } from "@/components/chat/ChatMarkdown";
import { Button } from "@/components/ui/button";
import { RefreshIcon } from "@/components/ui/refresh-icon";
import { splitPaperFrontmatter } from "@/lib/paper-frontmatter";
import { cn } from "@/lib/utils";
import { paperGet, paperRegenerate } from "@/lib/tauri";
import { useApp } from "@/store/app";

export function ReadmeView({
  panelMode = false,
  omitOuterToolbar = false,
  refreshNonce = 0,
}: {
  panelMode?: boolean;
  /** When embedded in Files panel, hide the slim meta row (parent supplies chrome). */
  omitOuterToolbar?: boolean;
  /** Bumped when a compile finishes so paper.md reloads. */
  refreshNonce?: number;
}) {
  const activeWorkspace = useApp((s) => s.activeWorkspace);
  const compileProgress = useApp((s) => s.compileProgress);
  const [loading, setLoading] = useState(false);
  const [regenerating, setRegenerating] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [markdown, setMarkdown] = useState("");
  const [exists, setExists] = useState(false);

  const load = useCallback(async () => {
    if (!activeWorkspace) {
      setMarkdown("");
      setExists(false);
      setError(null);
      return;
    }
    setLoading(true);
    setError(null);
    try {
      const payload = await paperGet(activeWorkspace);
      setExists(payload.exists);
      setMarkdown(payload.markdown);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setMarkdown("");
      setExists(false);
    } finally {
      setLoading(false);
    }
  }, [activeWorkspace]);

  useEffect(() => {
    void load();
  }, [load, refreshNonce]);

  useEffect(() => {
    if (!activeWorkspace || compileProgress?.phase !== "done") return;
    void load();
  }, [activeWorkspace, compileProgress?.phase, load]);

  const regenerate = useCallback(async () => {
    if (!activeWorkspace || regenerating) return;
    setRegenerating(true);
    setError(null);
    try {
      const out = await paperRegenerate(activeWorkspace);
      setExists(true);
      setMarkdown(out.markdown);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setRegenerating(false);
    }
  }, [activeWorkspace, regenerating]);

  const { frontmatter, body } = useMemo(
    () => splitPaperFrontmatter(markdown),
    [markdown],
  );

  if (!activeWorkspace) {
    return (
      <div className="flex h-full flex-col items-center justify-center gap-2 px-4 text-center">
        <p className="text-xs text-muted-foreground">No workspace selected.</p>
      </div>
    );
  }

  const metaLine = frontmatter
    ? `${frontmatter.witness_count ?? "?"} witnesses · ${frontmatter.source_count ?? "?"} sources`
    : exists
      ? `${countLines(body)} lines`
      : "no Living Paper yet";

  return (
    <div className="flex h-full flex-col">
      {panelMode && !omitOuterToolbar ? (
        <div className="flex shrink-0 items-center justify-between gap-2 border-b border-border/50 px-3 py-1.5">
          <span className="truncate text-[10px] text-muted-foreground">{metaLine}</span>
          <div className="flex shrink-0 items-center gap-0.5">
            <Button
              variant="ghost"
              size="sm"
              className="h-5 gap-1 px-1.5 text-[10px] text-muted-foreground"
              onClick={() => void regenerate()}
              disabled={regenerating || loading}
              title="Regenerate from current Witness Mesh"
            >
              <RefreshCw className={cn("size-3", regenerating && "animate-spin")} />
              {regenerating ? "…" : "Regen"}
            </Button>
            <Button
              variant="ghost"
              size="icon"
              className="h-5 w-5"
              onClick={() => void load()}
              disabled={loading}
              aria-label="Reload"
            >
              <RefreshIcon className={loading ? "size-3 animate-spin" : "size-3"} />
            </Button>
          </div>
        </div>
      ) : !panelMode ? (
        <header className="flex h-11 shrink-0 items-center gap-2 border-b border-border px-4">
          <FileText className="size-4 text-muted-foreground" />
          <span className="text-sm font-medium">{activeWorkspace}</span>
          <span className="text-muted-foreground">·</span>
          <span className="text-xs text-muted-foreground">Living Paper</span>
          <Button
            variant="ghost"
            size="sm"
            className="ml-auto h-7 gap-1 px-2 text-xs"
            onClick={() => void regenerate()}
            disabled={regenerating || loading}
          >
            <RefreshCw className={cn("size-3.5", regenerating && "animate-spin")} />
            Regenerate
          </Button>
          <Button
            variant="ghost"
            size="icon"
            className="h-7 w-7"
            onClick={() => void load()}
            disabled={loading}
            aria-label="Reload"
          >
            <RefreshIcon
              className={loading ? "size-3.5 animate-spin" : "size-3.5"}
            />
          </Button>
        </header>
      ) : null}

      {error && (
        <div
          className={
            panelMode
              ? "px-3 py-2 text-[11px] text-destructive"
              : "flex items-start gap-2 border-b border-destructive/20 bg-destructive/10 px-4 py-2 text-xs text-destructive"
          }
        >
          {!panelMode && <AlertTriangle className="mt-0.5 size-3.5 shrink-0" />}
          <span>{error}</span>
        </div>
      )}

      <div className="flex-1 overflow-y-auto px-4 py-4">
        {exists && body ? (
          <article className="prose prose-sm prose-invert max-w-none">
            <ChatMarkdown citations>{body}</ChatMarkdown>
          </article>
        ) : !loading ? (
          <EmptyState onRegenerate={() => void regenerate()} regenerating={regenerating} />
        ) : null}
      </div>
    </div>
  );
}

function EmptyState({
  onRegenerate,
  regenerating,
}: {
  onRegenerate: () => void;
  regenerating: boolean;
}) {
  return (
    <div className="flex h-full flex-col items-center justify-center gap-2 text-center">
      <FileText className="size-6 text-muted-foreground/40" />
      <p className="text-xs text-muted-foreground">No Living Paper yet.</p>
      <p className="max-w-xs text-[10px] text-muted-foreground/70">
        Run <code className="font-mono">Compile</code> to write{" "}
        <code className="font-mono">.thinkingroot/paper.md</code>, or regenerate from
        the current graph.
      </p>
      <Button
        variant="outline"
        size="sm"
        className="mt-1 h-7 text-xs"
        onClick={onRegenerate}
        disabled={regenerating}
      >
        {regenerating ? "Synthesising…" : "Regenerate paper"}
      </Button>
    </div>
  );
}

function countLines(s: string): number {
  if (!s) return 0;
  const trailing = s.endsWith("\n") ? 0 : 1;
  return s.split("\n").length - 1 + trailing;
}
