import { useCallback, useState } from "react";
import {
  Copy,
  ExternalLink,
  FileDown,
  GitBranch,
  Lightbulb,
  Package,
  Search,
  StickyNote,
  X,
} from "lucide-react";

import {
  playgroundBranchConversation,
  playgroundExportTr,
  playgroundGaps,
  playgroundHandoffUrl,
  playgroundOpenProposal,
  playgroundQuiz,
  playgroundSaveNote,
  type GapRow,
  type HandoffUrl,
  type QuizItem,
} from "@/lib/tauri";
import { cn } from "@/lib/utils";

/**
 * Playground actions toolbar.
 *
 * Surfaces the 7 researcher-facing verbs that aren't already wired
 * elsewhere in the Playground surface:
 *  - Save note            (workspace/notes/<slug>.md)
 *  - Open proposal        (POST /branches/playground/proposals)
 *  - Branch conversation  (POST /branches)
 *  - Quiz                 (brain.investigate, quiz prompt)
 *  - Export `.tr`         (root pack, default ~/Downloads/<ws>.tr)
 *  - Hand-off URL         (tr+mcp:// deep-link + mcp.json snippet)
 *  - Find gaps            (GET /gaps)
 *
 * Each button opens a focused inline drawer rather than a modal —
 * keeps the Playground feel "no contexts to navigate, everything is
 * one click away".
 */
export function PlaygroundActions({
  workspace,
}: {
  workspace: string | null;
}) {
  const [open, setOpen] = useState<ActionId | null>(null);
  const disabled = !workspace;

  return (
    <div className="flex shrink-0 flex-col border-b border-border bg-surface/20">
      <div className="flex shrink-0 items-center gap-1 px-3 py-1.5 text-xs">
        <ActionButton
          id="save-note"
          label="Save note"
          icon={<StickyNote className="size-3.5" />}
          active={open === "save-note"}
          disabled={disabled}
          onClick={() => setOpen(open === "save-note" ? null : "save-note")}
        />
        <ActionButton
          id="branch"
          label="Branch"
          icon={<GitBranch className="size-3.5" />}
          active={open === "branch"}
          disabled={disabled}
          onClick={() => setOpen(open === "branch" ? null : "branch")}
        />
        <ActionButton
          id="proposal"
          label="Open proposal"
          icon={<FileDown className="size-3.5" />}
          active={open === "proposal"}
          disabled={disabled}
          onClick={() => setOpen(open === "proposal" ? null : "proposal")}
        />
        <ActionButton
          id="quiz"
          label="Quiz"
          icon={<Lightbulb className="size-3.5" />}
          active={open === "quiz"}
          disabled={disabled}
          onClick={() => setOpen(open === "quiz" ? null : "quiz")}
        />
        <ActionButton
          id="gaps"
          label="Gaps"
          icon={<Search className="size-3.5" />}
          active={open === "gaps"}
          disabled={disabled}
          onClick={() => setOpen(open === "gaps" ? null : "gaps")}
        />
        <ActionButton
          id="export"
          label="Export .tr"
          icon={<Package className="size-3.5" />}
          active={open === "export"}
          disabled={disabled}
          onClick={() => setOpen(open === "export" ? null : "export")}
        />
        <ActionButton
          id="handoff"
          label="Hand-off"
          icon={<ExternalLink className="size-3.5" />}
          active={open === "handoff"}
          disabled={disabled}
          onClick={() => setOpen(open === "handoff" ? null : "handoff")}
        />
      </div>
      {open && workspace && (
        <div className="border-t border-border bg-background/60 px-3 py-3">
          <div className="flex items-start justify-between gap-2 pb-2">
            <h4 className="text-xs font-semibold uppercase tracking-wide text-muted-foreground">
              {LABEL[open]}
            </h4>
            <button
              type="button"
              aria-label="Close panel"
              onClick={() => setOpen(null)}
              className="rounded-md p-0.5 text-muted-foreground hover:bg-muted/60 hover:text-foreground"
            >
              <X className="size-3.5" />
            </button>
          </div>
          {open === "save-note" && <SaveNotePanel workspace={workspace} />}
          {open === "branch" && <BranchPanel workspace={workspace} />}
          {open === "proposal" && <ProposalPanel workspace={workspace} />}
          {open === "quiz" && <QuizPanel workspace={workspace} />}
          {open === "gaps" && <GapsPanel workspace={workspace} />}
          {open === "export" && <ExportPanel workspace={workspace} />}
          {open === "handoff" && <HandoffPanel workspace={workspace} />}
        </div>
      )}
    </div>
  );
}

type ActionId =
  | "save-note"
  | "branch"
  | "proposal"
  | "quiz"
  | "gaps"
  | "export"
  | "handoff";

const LABEL: Record<ActionId, string> = {
  "save-note": "Save note",
  branch: "Branch conversation",
  proposal: "Open knowledge proposal",
  quiz: "Quiz from corpus",
  gaps: "Knowledge gaps",
  export: "Export .tr pack",
  handoff: "Hand-off to external agent",
};

function ActionButton({
  id,
  label,
  icon,
  active,
  disabled,
  onClick,
}: {
  id: string;
  label: string;
  icon: React.ReactNode;
  active: boolean;
  disabled: boolean;
  onClick: () => void;
}) {
  return (
    <button
      key={id}
      type="button"
      onClick={onClick}
      disabled={disabled}
      aria-pressed={active}
      className={cn(
        "flex items-center gap-1.5 rounded-md px-2 py-1 transition-colors",
        active
          ? "bg-accent/15 text-accent"
          : disabled
            ? "text-muted-foreground/40"
            : "text-muted-foreground hover:bg-muted/40 hover:text-foreground",
      )}
    >
      {icon}
      {label}
    </button>
  );
}

// ─── Save note ────────────────────────────────────────────────

function SaveNotePanel({ workspace }: { workspace: string }) {
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const [saving, setSaving] = useState(false);
  const [result, setResult] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const submit = useCallback(async () => {
    if (!title.trim() || !body.trim() || saving) return;
    setSaving(true);
    setError(null);
    try {
      const out = await playgroundSaveNote(workspace, title.trim(), body);
      setResult(out.relative_path);
      setTitle("");
      setBody("");
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  }, [workspace, title, body, saving]);

  return (
    <form
      className="flex flex-col gap-2 text-xs"
      onSubmit={(e) => {
        e.preventDefault();
        void submit();
      }}
    >
      <input
        type="text"
        value={title}
        onChange={(e) => setTitle(e.target.value)}
        placeholder="Note title"
        className="rounded-md border border-border bg-background px-2 py-1.5 text-xs focus:border-accent focus:outline-none"
        autoFocus
      />
      <textarea
        value={body}
        onChange={(e) => setBody(e.target.value)}
        placeholder="Markdown body — typically the AI reply to save…"
        rows={6}
        className="rounded-md border border-border bg-background px-2 py-1.5 font-mono text-xs focus:border-accent focus:outline-none"
      />
      <div className="flex items-center justify-between gap-2">
        <button
          type="submit"
          disabled={saving || !title.trim() || !body.trim()}
          className={cn(
            "rounded-md px-3 py-1 text-xs font-medium transition-colors",
            saving || !title.trim() || !body.trim()
              ? "bg-muted/30 text-muted-foreground/60"
              : "bg-accent text-accent-foreground hover:bg-accent/90",
          )}
        >
          {saving ? "Saving…" : "Save note"}
        </button>
        {result && (
          <span className="truncate text-muted-foreground">
            Wrote {result}
          </span>
        )}
        {error && <span className="truncate text-destructive">{error}</span>}
      </div>
    </form>
  );
}

// ─── Branch ───────────────────────────────────────────────────

function BranchPanel({ workspace }: { workspace: string }) {
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [creating, setCreating] = useState(false);
  const [created, setCreated] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const submit = useCallback(async () => {
    if (!name.trim() || creating) return;
    setCreating(true);
    setError(null);
    try {
      const out = await playgroundBranchConversation(
        workspace,
        name.trim(),
        undefined,
        description.trim() || undefined,
      );
      setCreated(out.branch);
      setName("");
      setDescription("");
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setCreating(false);
    }
  }, [workspace, name, description, creating]);

  return (
    <form
      className="flex flex-col gap-2 text-xs"
      onSubmit={(e) => {
        e.preventDefault();
        void submit();
      }}
    >
      <input
        type="text"
        value={name}
        onChange={(e) => setName(e.target.value)}
        placeholder="Branch name (e.g. exploration/auth)"
        className="rounded-md border border-border bg-background px-2 py-1.5 focus:border-accent focus:outline-none"
        autoFocus
      />
      <input
        type="text"
        value={description}
        onChange={(e) => setDescription(e.target.value)}
        placeholder="Optional description"
        className="rounded-md border border-border bg-background px-2 py-1.5 focus:border-accent focus:outline-none"
      />
      <div className="flex items-center justify-between gap-2">
        <button
          type="submit"
          disabled={creating || !name.trim()}
          className={cn(
            "rounded-md px-3 py-1 text-xs font-medium transition-colors",
            creating || !name.trim()
              ? "bg-muted/30 text-muted-foreground/60"
              : "bg-accent text-accent-foreground hover:bg-accent/90",
          )}
        >
          {creating ? "Creating…" : "Create branch"}
        </button>
        {created && (
          <span className="truncate text-muted-foreground">
            Created <code>{created}</code>
          </span>
        )}
        {error && <span className="truncate text-destructive">{error}</span>}
      </div>
    </form>
  );
}

// ─── Proposal ─────────────────────────────────────────────────

function ProposalPanel({ workspace }: { workspace: string }) {
  const [branch, setBranch] = useState("playground");
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [proposalId, setProposalId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const submit = useCallback(async () => {
    if (!title.trim() || !body.trim() || submitting) return;
    setSubmitting(true);
    setError(null);
    try {
      const out = await playgroundOpenProposal(
        workspace,
        branch.trim() || "playground",
        title.trim(),
        body,
      );
      setProposalId(out.proposal_id);
      setTitle("");
      setBody("");
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSubmitting(false);
    }
  }, [workspace, branch, title, body, submitting]);

  return (
    <form
      className="flex flex-col gap-2 text-xs"
      onSubmit={(e) => {
        e.preventDefault();
        void submit();
      }}
    >
      <div className="flex items-center gap-2">
        <label className="shrink-0 text-muted-foreground">Branch:</label>
        <input
          type="text"
          value={branch}
          onChange={(e) => setBranch(e.target.value)}
          className="flex-1 rounded-md border border-border bg-background px-2 py-1.5 focus:border-accent focus:outline-none"
        />
      </div>
      <input
        type="text"
        value={title}
        onChange={(e) => setTitle(e.target.value)}
        placeholder="Proposal title"
        className="rounded-md border border-border bg-background px-2 py-1.5 focus:border-accent focus:outline-none"
      />
      <textarea
        value={body}
        onChange={(e) => setBody(e.target.value)}
        placeholder="Proposal body — describe what the agent should consider adding…"
        rows={5}
        className="rounded-md border border-border bg-background px-2 py-1.5 font-mono focus:border-accent focus:outline-none"
      />
      <div className="flex items-center justify-between gap-2">
        <button
          type="submit"
          disabled={submitting || !title.trim() || !body.trim()}
          className={cn(
            "rounded-md px-3 py-1 text-xs font-medium transition-colors",
            submitting || !title.trim() || !body.trim()
              ? "bg-muted/30 text-muted-foreground/60"
              : "bg-accent text-accent-foreground hover:bg-accent/90",
          )}
        >
          {submitting ? "Opening…" : "Open proposal"}
        </button>
        {proposalId && (
          <span className="truncate text-muted-foreground">
            Opened <code>{proposalId}</code>
          </span>
        )}
        {error && <span className="truncate text-destructive">{error}</span>}
      </div>
    </form>
  );
}

// ─── Quiz ─────────────────────────────────────────────────────

function QuizPanel({ workspace }: { workspace: string }) {
  const [topic, setTopic] = useState("");
  const [count, setCount] = useState(5);
  const [loading, setLoading] = useState(false);
  const [items, setItems] = useState<QuizItem[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  const submit = useCallback(async () => {
    if (!topic.trim() || loading) return;
    setLoading(true);
    setError(null);
    setItems(null);
    try {
      const out = await playgroundQuiz(workspace, topic.trim(), count);
      setItems(out);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [workspace, topic, count, loading]);

  return (
    <div className="flex flex-col gap-2 text-xs">
      <div className="flex items-center gap-2">
        <input
          type="text"
          value={topic}
          onChange={(e) => setTopic(e.target.value)}
          placeholder="Topic (e.g. 'how the witness mesh dedups')"
          className="flex-1 rounded-md border border-border bg-background px-2 py-1.5 focus:border-accent focus:outline-none"
        />
        <input
          type="number"
          min={1}
          max={20}
          value={count}
          onChange={(e) => setCount(parseInt(e.target.value || "5", 10))}
          className="w-14 rounded-md border border-border bg-background px-2 py-1.5 focus:border-accent focus:outline-none"
        />
        <button
          type="button"
          onClick={submit}
          disabled={loading || !topic.trim()}
          className={cn(
            "shrink-0 rounded-md px-3 py-1 text-xs font-medium transition-colors",
            loading || !topic.trim()
              ? "bg-muted/30 text-muted-foreground/60"
              : "bg-accent text-accent-foreground hover:bg-accent/90",
          )}
        >
          {loading ? "Thinking…" : "Generate"}
        </button>
      </div>
      {error && <p className="text-destructive">{error}</p>}
      {items && items.length === 0 && (
        <p className="text-muted-foreground">
          Corpus doesn't cover this topic — try compiling more sources or
          narrowing the question.
        </p>
      )}
      {items && items.length > 0 && (
        <ul className="flex max-h-72 flex-col gap-2 overflow-auto">
          {items.map((it, i) => (
            <li
              key={i}
              className="rounded-md border border-border bg-background/40 px-2 py-1.5"
            >
              <p className="font-semibold">{it.question}</p>
              <p className="mt-1 text-muted-foreground">{it.answer}</p>
              {it.citations.length > 0 && (
                <p className="mt-1 truncate font-mono text-[10px] text-muted-foreground/70">
                  cites: {it.citations.slice(0, 4).join(", ")}
                  {it.citations.length > 4 && "…"}
                </p>
              )}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

// ─── Gaps ─────────────────────────────────────────────────────

function GapsPanel({ workspace }: { workspace: string }) {
  const [loading, setLoading] = useState(false);
  const [rows, setRows] = useState<GapRow[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [entity, setEntity] = useState("");

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const out = await playgroundGaps(workspace, {
        entity: entity.trim() || undefined,
        minConfidence: 0.5,
      });
      setRows(out);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setRows(null);
    } finally {
      setLoading(false);
    }
  }, [workspace, entity]);

  return (
    <div className="flex flex-col gap-2 text-xs">
      <div className="flex items-center gap-2">
        <input
          type="text"
          value={entity}
          onChange={(e) => setEntity(e.target.value)}
          placeholder="Filter by entity (optional)"
          className="flex-1 rounded-md border border-border bg-background px-2 py-1.5 focus:border-accent focus:outline-none"
        />
        <button
          type="button"
          onClick={load}
          disabled={loading}
          className={cn(
            "shrink-0 rounded-md px-3 py-1 text-xs font-medium transition-colors",
            loading
              ? "bg-muted/30 text-muted-foreground/60"
              : "bg-accent text-accent-foreground hover:bg-accent/90",
          )}
        >
          {loading ? "Loading…" : "Find gaps"}
        </button>
      </div>
      {error && <p className="text-destructive">{error}</p>}
      {rows && rows.length === 0 && (
        <p className="text-muted-foreground">
          No gaps right now. Run <code>reflect</code> on this workspace to
          regenerate the pattern catalog if you expect some.
        </p>
      )}
      {rows && rows.length > 0 && (
        <ul className="flex max-h-72 flex-col gap-1 overflow-auto">
          {rows.map((g) => (
            <li
              key={g.gap_id}
              className="flex items-start justify-between gap-2 rounded-md border border-border bg-background/40 px-2 py-1.5"
            >
              <div className="min-w-0">
                <p className="truncate font-semibold">
                  {g.entity || "(unnamed)"}{" "}
                  <span className="text-muted-foreground">
                    [{g.entity_type}]
                  </span>
                </p>
                <p className="truncate text-muted-foreground">
                  expected <code>{g.missing_claim_type}</code> · pattern{" "}
                  {(g.pattern_confidence * 100).toFixed(0)}%
                </p>
              </div>
              <span className="shrink-0 rounded bg-muted/50 px-1.5 py-px font-mono text-[10px]">
                {g.status}
              </span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

// ─── Export ───────────────────────────────────────────────────

function ExportPanel({ workspace }: { workspace: string }) {
  const [running, setRunning] = useState(false);
  const [result, setResult] = useState<{ path: string; bytes: number } | null>(
    null,
  );
  const [error, setError] = useState<string | null>(null);

  const submit = useCallback(async () => {
    if (running) return;
    setRunning(true);
    setError(null);
    try {
      const out = await playgroundExportTr(workspace);
      setResult({ path: out.path, bytes: out.bytes });
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setRunning(false);
    }
  }, [workspace, running]);

  return (
    <div className="flex flex-col gap-2 text-xs">
      <p className="text-muted-foreground">
        Bundles every source + the Witness Mesh + the Living Paper into a
        signed-by-default <code>.tr</code> pack at{" "}
        <code>~/Downloads/{workspace}.tr</code>.
      </p>
      <div className="flex items-center justify-between gap-2">
        <button
          type="button"
          onClick={submit}
          disabled={running}
          className={cn(
            "rounded-md px-3 py-1 text-xs font-medium transition-colors",
            running
              ? "bg-muted/30 text-muted-foreground/60"
              : "bg-accent text-accent-foreground hover:bg-accent/90",
          )}
        >
          {running ? "Packing…" : "Export .tr"}
        </button>
        {result && (
          <span className="truncate text-muted-foreground" title={result.path}>
            {formatBytes(result.bytes)} → {basename(result.path)}
          </span>
        )}
        {error && <span className="truncate text-destructive">{error}</span>}
      </div>
    </div>
  );
}

// ─── Hand-off ─────────────────────────────────────────────────

function HandoffPanel({ workspace }: { workspace: string }) {
  const [loading, setLoading] = useState(false);
  const [data, setData] = useState<HandoffUrl | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState<"url" | "snippet" | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const out = await playgroundHandoffUrl(workspace);
      setData(out);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [workspace]);

  const copy = useCallback(async (text: string, which: "url" | "snippet") => {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(which);
      setTimeout(() => setCopied(null), 1200);
    } catch {
      // No-op: triple-click + Cmd-C still works.
    }
  }, []);

  return (
    <div className="flex flex-col gap-2 text-xs">
      {!data && (
        <button
          type="button"
          onClick={load}
          disabled={loading}
          className={cn(
            "self-start rounded-md px-3 py-1 text-xs font-medium transition-colors",
            loading
              ? "bg-muted/30 text-muted-foreground/60"
              : "bg-accent text-accent-foreground hover:bg-accent/90",
          )}
        >
          {loading ? "Resolving…" : "Generate hand-off URL"}
        </button>
      )}
      {error && <p className="text-destructive">{error}</p>}
      {data && (
        <>
          <div className="flex items-center gap-2">
            <code className="min-w-0 flex-1 truncate rounded-md border border-border bg-background px-2 py-1.5 font-mono">
              {data.url}
            </code>
            <button
              type="button"
              onClick={() => copy(data.url, "url")}
              className="shrink-0 rounded-md p-1 text-muted-foreground hover:bg-muted/60 hover:text-foreground"
              aria-label="Copy URL"
            >
              <Copy className="size-3" />
            </button>
            {copied === "url" && (
              <span className="shrink-0 text-muted-foreground">Copied</span>
            )}
          </div>
          <details className="rounded-md border border-border bg-background/40 px-2 py-1.5">
            <summary className="cursor-pointer text-muted-foreground">
              mcp.json snippet (paste into Claude Code / Cursor / Codex)
            </summary>
            <div className="mt-2 flex items-start gap-2">
              <pre className="min-w-0 flex-1 overflow-auto rounded bg-background p-2 font-mono text-[10px] leading-tight">
                {data.mcp_config_snippet}
              </pre>
              <button
                type="button"
                onClick={() => copy(data.mcp_config_snippet, "snippet")}
                className="shrink-0 rounded-md p-1 text-muted-foreground hover:bg-muted/60 hover:text-foreground"
                aria-label="Copy snippet"
              >
                <Copy className="size-3" />
              </button>
            </div>
            {copied === "snippet" && (
              <p className="mt-1 text-muted-foreground">Copied</p>
            )}
          </details>
        </>
      )}
    </div>
  );
}

function basename(p: string): string {
  return p.split(/[\/\\]/).pop() || p;
}

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KiB`;
  if (n < 1024 * 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)} MiB`;
  return `${(n / (1024 * 1024 * 1024)).toFixed(2)} GiB`;
}
