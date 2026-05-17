import { useCallback, useEffect, useMemo, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeSanitize from "rehype-sanitize";
import { FileText, RefreshCw, RotateCcw } from "lucide-react";

import {
  paperGet,
  paperRegenerate,
  type PaperPayload,
} from "@/lib/tauri";
import { transformCitations } from "@/components/playground/CitationChip";
import { cn } from "@/lib/utils";

/**
 * Living Paper viewer.
 *
 * Renders the per-compile `paper.md` artefact written by the engine's
 * Phase 10b synthesiser. The YAML frontmatter is the
 * machine-readable spine; the markdown body is what the human reads.
 * This panel strips the frontmatter for display (the section index
 * + workspace metadata are surfaced separately in the header
 * strip) and renders the body via ReactMarkdown.
 *
 * Mermaid code-blocks render as preformatted text in v1 — wiring
 * the mermaid npm package in is part of the next ship. The
 * structurally-valid Mermaid source is still byte-perfect in the
 * markdown, so a future viewer (or the public hub) renders it
 * without changing the `paper.md` byte-stream.
 */
export function PaperPanel({
  workspace,
  refreshNonce,
}: {
  workspace: string | null;
  refreshNonce?: number;
}) {
  const [payload, setPayload] = useState<PaperPayload | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [regenerating, setRegenerating] = useState(false);

  const load = useCallback(async () => {
    if (!workspace) {
      setPayload(null);
      setError(null);
      return;
    }
    setLoading(true);
    setError(null);
    try {
      const fresh = await paperGet(workspace);
      setPayload(fresh);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setPayload(null);
    } finally {
      setLoading(false);
    }
  }, [workspace]);

  useEffect(() => {
    void load();
  }, [load, refreshNonce]);

  // Trigger a server-side resynthesis. The sidecar writes the new
  // bytes atomically before returning, so we hydrate state from the
  // command's response directly — no follow-up `paperGet` round-trip.
  const regenerate = useCallback(async () => {
    if (!workspace || regenerating) return;
    setRegenerating(true);
    setError(null);
    try {
      const out = await paperRegenerate(workspace);
      setPayload((prev) => ({
        path: prev?.path ?? "",
        exists: true,
        markdown: out.markdown,
      }));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setRegenerating(false);
    }
  }, [workspace, regenerating]);

  const { frontmatter, body } = useMemo(() => splitFrontmatter(payload?.markdown), [
    payload?.markdown,
  ]);

  if (!workspace) {
    return (
      <EmptyState
        title="No workspace selected"
        hint="Pick a workspace from the sidebar to see its Living Paper."
      />
    );
  }

  if (loading && !payload) {
    return <EmptyState title="Loading paper…" hint="" />;
  }

  if (error) {
    return (
      <EmptyState
        title="Couldn't load paper.md"
        hint={error}
        action={{ label: "Retry", onClick: load }}
      />
    );
  }

  if (!payload?.exists) {
    return (
      <EmptyState
        title="No paper yet"
        hint={
          "Compile this workspace to generate a Living Paper — the engine writes " +
          ".thinkingroot/paper.md after every successful compile."
        }
        action={{ label: "Refresh", onClick: load }}
      />
    );
  }

  return (
    <div className="flex h-full flex-col">
      <header className="flex shrink-0 items-center justify-between gap-2 border-b border-border bg-surface/60 px-4 py-2">
        <div className="flex min-w-0 items-center gap-2">
          <FileText className="size-4 text-muted-foreground" />
          <h3 className="truncate text-sm font-medium">Living Paper</h3>
          {frontmatter && (
            <span className="ml-2 truncate text-xs text-muted-foreground">
              {frontmatter.witness_count ?? "?"} witnesses ·{" "}
              {frontmatter.source_count ?? "?"} sources
            </span>
          )}
        </div>
        <div className="flex shrink-0 items-center gap-1">
          <button
            type="button"
            onClick={regenerate}
            disabled={regenerating}
            aria-label="Regenerate paper"
            title="Regenerate from the current Witness Mesh state"
            className={cn(
              "flex items-center gap-1 rounded-md px-2 py-1 text-xs font-medium transition-colors",
              regenerating
                ? "text-muted-foreground/60"
                : "text-muted-foreground hover:bg-muted/60 hover:text-foreground",
            )}
          >
            <RefreshCw
              className={cn("size-3.5", regenerating && "animate-spin")}
            />
            {regenerating ? "Synthesising…" : "Regenerate"}
          </button>
          <button
            type="button"
            onClick={load}
            aria-label="Reload paper"
            className="rounded-md p-1 text-muted-foreground transition-colors hover:bg-muted/60 hover:text-foreground"
          >
            <RotateCcw className="size-3.5" />
          </button>
        </div>
      </header>
      <div className="prose prose-sm dark:prose-invert max-w-none flex-1 overflow-auto px-6 py-4">
        <ReactMarkdown
          remarkPlugins={[remarkGfm]}
          rehypePlugins={[rehypeSanitize]}
          components={{
            p: ({ children }) => <p>{transformCitations(children)}</p>,
            li: ({ children }) => <li>{transformCitations(children)}</li>,
            td: ({ children }) => <td>{transformCitations(children)}</td>,
          }}
        >
          {body}
        </ReactMarkdown>
      </div>
    </div>
  );
}

interface FrontmatterPreview {
  workspace?: string;
  witness_count?: number;
  source_count?: number;
}

/** Split a `paper.md` body at the YAML frontmatter fence. Returns
 * `{ frontmatter, body }` where `frontmatter` is parsed best-effort
 * for the header strip and `body` is the renderable markdown. */
function splitFrontmatter(
  markdown: string | undefined,
): { frontmatter: FrontmatterPreview | null; body: string } {
  if (!markdown) return { frontmatter: null, body: "" };
  if (!markdown.startsWith("---\n")) {
    return { frontmatter: null, body: markdown };
  }
  const rest = markdown.slice(4);
  const endIdx = rest.indexOf("\n---");
  if (endIdx < 0) return { frontmatter: null, body: markdown };
  const fmYaml = rest.slice(0, endIdx);
  // Crude key extraction — we don't pull in a YAML parser just to
  // surface two integers in the header strip. The full machine-
  // readable spine is available to AI agents that parse the file
  // directly.
  const fm: FrontmatterPreview = {};
  for (const line of fmYaml.split("\n")) {
    const m = line.match(/^([a-z_]+):\s*(.*)$/);
    if (!m) continue;
    const key = m[1] ?? "";
    const value = (m[2] ?? "").trim();
    if (key === "workspace") fm.workspace = value;
    else if (key === "witness_count") fm.witness_count = parseInt(value, 10);
    else if (key === "source_count") fm.source_count = parseInt(value, 10);
  }
  const bodyStart = endIdx + 4; // skip "\n---"
  const newline = rest.indexOf("\n", bodyStart);
  const body = newline >= 0 ? rest.slice(newline + 1).trimStart() : "";
  return { frontmatter: fm, body };
}

function EmptyState({
  title,
  hint,
  action,
}: {
  title: string;
  hint: string;
  action?: { label: string; onClick: () => void };
}) {
  return (
    <div className="flex h-full flex-col items-center justify-center gap-2 px-8 text-center">
      <FileText className="size-8 text-muted-foreground/60" />
      <h3 className="text-sm font-medium">{title}</h3>
      {hint && (
        <p className={cn("max-w-md text-xs text-muted-foreground")}>{hint}</p>
      )}
      {action && (
        <button
          type="button"
          onClick={action.onClick}
          className="mt-2 rounded-md border border-border bg-surface px-3 py-1 text-xs font-medium text-foreground transition-colors hover:bg-muted/60"
        >
          {action.label}
        </button>
      )}
    </div>
  );
}
