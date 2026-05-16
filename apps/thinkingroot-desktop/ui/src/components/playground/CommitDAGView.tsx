import { useCallback, useEffect, useMemo, useState } from "react";
import { GitCommit, RefreshCw, User2, Bot, ChevronRight, ChevronDown } from "lucide-react";

import { cn } from "@/lib/utils";
import {
  commitList,
  type CognitionCommit,
  type CommitAuthor,
} from "@/lib/tauri";

interface Props {
  /** Workspace name; `null` means no workspace mounted yet (the panel
   *  renders an honest empty state in that case rather than calling
   *  the sidecar with a missing context). */
  workspace: string | null;
  /** Bumped by PlaygroundView whenever a compile finishes so we
   *  re-fetch the DAG against the latest substrate state. */
  refreshNonce: number;
}

/**
 * Chat-as-commit-DAG view — Phase β.3 of the Cognition Commits
 * design (`docs/2026-05-15-cognition-commits-design.md`).
 *
 * Renders the workspace's `main` branch cognition commits newest
 * first. Each commit is a card showing:
 *
 *   - short id (first 8 hex chars) + author + relative time
 *   - parent short id with a chevron showing the DAG relation
 *   - user prompt (collapsed by default)
 *   - assistant reasoning (collapsed by default)
 *   - citation pills (clickable — future enhancement walks to the
 *     witness detail panel; β.3 just shows them)
 *   - gap pills + witnesses_added pills when present
 *
 * Honest empty state: "no commits yet — start a chat to see this DAG
 * grow." The auto-commit hook in `agent_streaming.rs` populates the
 * DAG as the user converses, so the empty state is short-lived in
 * practice.
 */
export function CommitDAGView({ workspace, refreshNonce }: Props) {
  const [commits, setCommits] = useState<CognitionCommit[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());

  const load = useCallback(async () => {
    if (!workspace) {
      setCommits([]);
      setError(null);
      return;
    }
    setLoading(true);
    setError(null);
    try {
      const rows = await commitList({ branch: "main", limit: 200 });
      setCommits(rows);
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      setError(msg);
    } finally {
      setLoading(false);
    }
  }, [workspace]);

  useEffect(() => {
    void load();
  }, [workspace, refreshNonce, load]);

  const toggleExpand = useCallback((id: string) => {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(id)) {
        next.delete(id);
      } else {
        next.add(id);
      }
      return next;
    });
  }, []);

  return (
    <div className="flex h-full flex-col">
      <header className="flex shrink-0 items-center justify-between gap-2 border-b border-border bg-surface/60 px-4 py-2">
        <div className="flex min-w-0 items-center gap-2">
          <GitCommit className="size-4 text-muted-foreground" />
          <h3 className="truncate text-sm font-semibold">Commits</h3>
          <span className="text-xs text-muted-foreground">
            {commits.length === 0
              ? "chat history as a content-addressed DAG"
              : `${commits.length} commit${commits.length === 1 ? "" : "s"} on main`}
          </span>
        </div>
        <button
          type="button"
          onClick={() => void load()}
          disabled={loading}
          aria-label="Reload commits"
          className={cn(
            "rounded-md p-1 text-muted-foreground transition-colors",
            loading
              ? "cursor-not-allowed opacity-50"
              : "hover:bg-muted/40 hover:text-foreground",
          )}
        >
          <RefreshCw className={cn("size-3.5", loading && "animate-spin")} />
        </button>
      </header>

      <div className="flex-1 overflow-y-auto">
        {error ? (
          <ErrorState message={error} onRetry={() => void load()} />
        ) : loading && commits.length === 0 ? (
          <LoadingState />
        ) : commits.length === 0 ? (
          <EmptyState />
        ) : (
          <ol className="flex flex-col gap-1 px-3 py-3">
            {commits.map((commit) => (
              <CommitCard
                key={commit.id}
                commit={commit}
                expanded={expanded.has(commit.id)}
                onToggle={() => toggleExpand(commit.id)}
              />
            ))}
          </ol>
        )}
      </div>
    </div>
  );
}

interface CardProps {
  commit: CognitionCommit;
  expanded: boolean;
  onToggle: () => void;
}

function CommitCard({ commit, expanded, onToggle }: CardProps) {
  const author = parseAuthor(commit.author);
  const shortId = commit.id.slice(0, 8);
  const parentShort = commit.parent ? commit.parent.slice(0, 8) : null;
  const created = useMemo(() => formatRelative(commit.created_at), [
    commit.created_at,
  ]);

  return (
    <li className="rounded-md border border-border bg-background">
      <button
        type="button"
        onClick={onToggle}
        aria-expanded={expanded}
        className="flex w-full items-center gap-2 px-3 py-2 text-left transition-colors hover:bg-muted/30"
      >
        {expanded ? (
          <ChevronDown className="size-3.5 text-muted-foreground" />
        ) : (
          <ChevronRight className="size-3.5 text-muted-foreground" />
        )}
        <code className="rounded bg-muted/40 px-1.5 py-0.5 font-mono text-xs">
          {shortId}
        </code>
        {parentShort ? (
          <span className="flex items-center gap-1 text-xs text-muted-foreground">
            <span>←</span>
            <code className="rounded bg-muted/30 px-1 py-0.5 font-mono">
              {parentShort}
            </code>
          </span>
        ) : (
          <span className="text-xs text-muted-foreground italic">genesis</span>
        )}
        <span className="ml-2 flex items-center gap-1 text-xs text-muted-foreground">
          {author.kind === "agent" ? (
            <Bot className="size-3" />
          ) : (
            <User2 className="size-3" />
          )}
          <span>{author.label}</span>
        </span>
        <span className="ml-auto text-xs text-muted-foreground">{created}</span>
      </button>

      {expanded && (
        <div className="space-y-3 border-t border-border px-3 py-2 text-sm">
          {commit.prompt && (
            <Section label="Prompt">
              <p className="whitespace-pre-wrap text-foreground/90">
                {commit.prompt}
              </p>
            </Section>
          )}
          {commit.reasoning && (
            <Section label="Reasoning">
              <p className="whitespace-pre-wrap text-foreground/90">
                {commit.reasoning}
              </p>
            </Section>
          )}
          {commit.citations.length > 0 && (
            <Section label={`Citations (${commit.citations.length})`}>
              <div className="flex flex-wrap gap-1">
                {commit.citations.map((id) => (
                  <CitationPill key={id} witnessId={id} />
                ))}
              </div>
            </Section>
          )}
          {commit.witnesses_added.length > 0 && (
            <Section label={`Witnesses added (${commit.witnesses_added.length})`}>
              <div className="flex flex-wrap gap-1">
                {commit.witnesses_added.map((id) => (
                  <CitationPill key={id} witnessId={id} variant="added" />
                ))}
              </div>
            </Section>
          )}
          {commit.gaps_surfaced.length > 0 && (
            <Section label={`Gaps surfaced (${commit.gaps_surfaced.length})`}>
              <div className="flex flex-wrap gap-1">
                {commit.gaps_surfaced.map((gap) => (
                  <span
                    key={gap}
                    className="rounded-full bg-warning/15 px-2 py-0.5 text-xs text-warning"
                  >
                    {gap}
                  </span>
                ))}
              </div>
            </Section>
          )}
        </div>
      )}
    </li>
  );
}

function Section({
  label,
  children,
}: {
  label: string;
  children: React.ReactNode;
}) {
  return (
    <div>
      <div className="mb-1 text-xs font-semibold uppercase tracking-wide text-muted-foreground">
        {label}
      </div>
      {children}
    </div>
  );
}

function CitationPill({
  witnessId,
  variant = "cited",
}: {
  witnessId: string;
  variant?: "cited" | "added";
}) {
  const short = witnessId.slice(0, 8);
  return (
    <code
      className={cn(
        "rounded-full px-2 py-0.5 font-mono text-xs",
        variant === "added"
          ? "bg-accent/15 text-accent"
          : "bg-muted/40 text-foreground/80",
      )}
      title={witnessId}
    >
      {short}
    </code>
  );
}

function EmptyState() {
  return (
    <div className="flex h-full flex-col items-center justify-center gap-2 px-6 text-center text-sm text-muted-foreground">
      <GitCommit className="size-6 opacity-40" />
      <p>No commits yet.</p>
      <p className="text-xs">
        Start a chat — every successful turn lands as a content-addressed
        commit on this branch.
      </p>
    </div>
  );
}

function LoadingState() {
  return (
    <div className="flex h-full items-center justify-center text-sm text-muted-foreground">
      Loading commits…
    </div>
  );
}

function ErrorState({
  message,
  onRetry,
}: {
  message: string;
  onRetry: () => void;
}) {
  return (
    <div className="flex h-full flex-col items-center justify-center gap-3 px-6 text-center text-sm">
      <p className="text-destructive">Failed to load commits</p>
      <code className="max-w-md whitespace-pre-wrap rounded bg-muted/40 px-2 py-1 text-xs">
        {message}
      </code>
      <button
        type="button"
        onClick={onRetry}
        className="rounded-md border border-border bg-background px-3 py-1 text-xs font-medium hover:bg-muted/40"
      >
        Retry
      </button>
    </div>
  );
}

interface ParsedAuthor {
  kind: "user" | "agent";
  label: string;
}

/** Tolerant projection of the wire `CommitAuthor` shape into a
 *  display-ready `{ kind, label }`. Falls back to `agent / unknown`
 *  when the wire emits an unrecognised shape so the UI never crashes
 *  on future field additions. */
function parseAuthor(raw: CommitAuthor | Record<string, unknown>): ParsedAuthor {
  if (typeof raw !== "object" || raw === null) {
    return { kind: "agent", label: "unknown" };
  }
  const kind = (raw as { kind?: unknown }).kind;
  if (kind === "user") {
    const id =
      typeof (raw as { id?: unknown }).id === "string"
        ? ((raw as { id: string }).id as string)
        : "anonymous";
    return { kind: "user", label: id };
  }
  if (kind === "agent") {
    const model =
      typeof (raw as { model?: unknown }).model === "string"
        ? (raw as { model: string }).model
        : "";
    const principal =
      typeof (raw as { principal?: unknown }).principal === "string"
        ? (raw as { principal: string }).principal
        : "";
    const label = model ? `${principal || "agent"} · ${model}` : principal || "agent";
    return { kind: "agent", label };
  }
  return { kind: "agent", label: "unknown" };
}

/** Format an ISO-8601 timestamp as a relative-time string. Returns
 *  `"just now" | "5m ago" | "3h ago" | "yesterday" | ISO date`. Pure;
 *  no Intl dependency to keep the bundle small. */
function formatRelative(iso: string): string {
  const then = Date.parse(iso);
  if (Number.isNaN(then)) {
    return iso;
  }
  const now = Date.now();
  const diffSec = Math.max(0, Math.round((now - then) / 1000));
  if (diffSec < 60) return "just now";
  const diffMin = Math.round(diffSec / 60);
  if (diffMin < 60) return `${diffMin}m ago`;
  const diffHr = Math.round(diffMin / 60);
  if (diffHr < 24) return `${diffHr}h ago`;
  const diffDay = Math.round(diffHr / 24);
  if (diffDay === 1) return "yesterday";
  if (diffDay < 7) return `${diffDay}d ago`;
  return new Date(then).toISOString().slice(0, 10);
}
