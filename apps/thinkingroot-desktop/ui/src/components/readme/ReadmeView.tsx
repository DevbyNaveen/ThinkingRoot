/**
 * Workspace Readme tab — renders the auto-synthesised README markdown
 * pulled from the daemon (`GET /api/v1/ws/{ws}/readme`, served from
 * `<workspace>/.thinkingroot/README.md`). Pipeline Phase 10 maintains
 * that file on every dirty compile.
 *
 * Markdown rendering is sanitised via rehype-sanitize because workspace
 * content can carry foreign markdown from connectors (GitHub PR
 * descriptions, Slack messages, etc.). Defence-in-depth even for
 * trusted local content.
 */
import { useCallback, useEffect, useState } from "react";
import { AlertTriangle, BookOpen } from "lucide-react";
import ReactMarkdown from "react-markdown";
import rehypeSanitize from "rehype-sanitize";
import remarkGfm from "remark-gfm";

import { Button } from "@/components/ui/button";
import { RefreshIcon } from "@/components/ui/refresh-icon";
import { useApp } from "@/store/app";
import { workspaceReadme } from "@/lib/tauri";

export function ReadmeView({
  panelMode = false,
  omitOuterToolbar = false,
}: {
  panelMode?: boolean;
  /** When embedded in Files panel, hide the slim meta row (parent supplies chrome). */
  omitOuterToolbar?: boolean;
}) {
  const activeWorkspace = useApp((s) => s.activeWorkspace);
  const [content, setContent] = useState<string>("");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const md = await workspaceReadme();
      setContent(md);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setContent("");
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    if (activeWorkspace) void load();
    else setContent("");
  }, [activeWorkspace, load]);

  if (!activeWorkspace) {
    return (
      <div className="flex h-full flex-col items-center justify-center gap-2 px-4 text-center">
        <p className="text-xs text-muted-foreground">No workspace selected.</p>
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col">
      {panelMode && !omitOuterToolbar ? (
        <div className="flex shrink-0 items-center justify-between border-b border-border/50 px-3 py-1.5">
          <span className="text-[10px] text-muted-foreground">
            {content
              ? `${countLines(content)} lines · auto-generated`
              : "no README yet"}
          </span>
          <Button
            variant="ghost"
            size="icon"
            className="ml-auto h-5 w-5"
            onClick={() => void load()}
            disabled={loading}
            aria-label="Reload"
          >
            <RefreshIcon className={loading ? "size-3 animate-spin" : "size-3"} />
          </Button>
        </div>
      ) : !panelMode ? (
        <header className="flex h-11 shrink-0 items-center gap-2 border-b border-border px-4">
          <BookOpen className="size-4 text-muted-foreground" />
          <span className="text-sm font-medium">{activeWorkspace}</span>
          <span className="text-muted-foreground">·</span>
          <span className="text-xs text-muted-foreground">Readme</span>
          <Button
            variant="ghost"
            size="icon"
            className="ml-auto h-7 w-7"
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
        {content ? (
          <article className="prose prose-sm prose-invert max-w-none">
            <ReactMarkdown
              remarkPlugins={[remarkGfm]}
              rehypePlugins={[rehypeSanitize]}
            >
              {content}
            </ReactMarkdown>
          </article>
        ) : !loading ? (
          <EmptyState />
        ) : null}
      </div>
    </div>
  );
}

function EmptyState() {
  return (
    <div className="flex h-full flex-col items-center justify-center gap-2 text-center">
      <BookOpen className="size-6 text-muted-foreground/40" />
      <p className="text-xs text-muted-foreground">No README yet.</p>
      <p className="text-[10px] text-muted-foreground/70">
        Run <code className="font-mono">root compile</code> to generate one.
      </p>
    </div>
  );
}

function countLines(s: string): number {
  if (!s) return 0;
  const trailing = s.endsWith("\n") ? 0 : 1;
  return s.split("\n").length - 1 + trailing;
}
