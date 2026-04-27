import { useCallback, useEffect, useMemo, useState } from "react";
import {
  AlertTriangle,
  Database,
  FileText,
  Loader2,
  ShieldCheck,
  Trash2,
  Users,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import {
  privacyForget,
  privacySummary,
  type PrivacySource,
  type PrivacySummary,
} from "@/lib/tauri";
import { toast } from "@/store/toast";
import { ForgetDialog } from "./ForgetDialog";

/**
 * Privacy dashboard. Lists every locally-stored source the engine
 * knows about plus the aggregate counts of claims and entities. The
 * "Forget" action delegates to the Rust side, which removes the
 * source row and every descendant claim/edge/vector/contradiction in
 * one transaction, then rebuilds the in-memory cache so subsequent
 * Brain queries reflect the redaction immediately.
 *
 * No fake data — when no workspace is mounted the dashboard renders
 * an honest empty state and prompts the user to set
 * `THINKINGROOT_WORKSPACE` in Settings.
 */
export function PrivacyDashboard() {
  const [state, setState] = useState<
    | { kind: "loading" }
    | { kind: "ready"; summary: PrivacySummary }
    | { kind: "error"; message: string }
  >({ kind: "loading" });
  const [pending, setPending] = useState<PrivacySource | null>(null);
  const [forgetting, setForgetting] = useState(false);

  const refresh = useCallback(async () => {
    setState({ kind: "loading" });
    try {
      const summary = await privacySummary();
      setState({ kind: "ready", summary });
    } catch (err) {
      setState({
        kind: "error",
        message: err instanceof Error ? err.message : String(err),
      });
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  async function confirmForget() {
    if (!pending) return;
    setForgetting(true);
    try {
      const removed = await privacyForget(pending.uri);
      toast(
        removed > 0 ? "Source forgotten" : "Source not found",
        {
          kind: removed > 0 ? "success" : "info",
          body:
            removed > 0
              ? `Removed ${pending.uri} and all descendant claims.`
              : "The graph no longer references this URI.",
          durationMs: 4000,
        },
      );
      setPending(null);
      await refresh();
    } catch (err) {
      toast("Forget failed", {
        kind: "error",
        body: err instanceof Error ? err.message : String(err),
      });
    } finally {
      setForgetting(false);
    }
  }

  return (
    <div className="flex h-full flex-col">
      <Header onRefresh={refresh} loading={state.kind === "loading"} />
      <div className="flex-1 overflow-y-auto">
        <div className="mx-auto flex w-full max-w-4xl flex-col gap-6 px-6 py-8">
          {state.kind === "loading" && <LoadingCard />}
          {state.kind === "error" && (
            <ErrorCard message={state.message} onRetry={refresh} />
          )}
          {state.kind === "ready" && (
            <ReadyView
              summary={state.summary}
              onForget={(s) => setPending(s)}
            />
          )}
        </div>
      </div>
      <ForgetDialog
        source={pending}
        forgetting={forgetting}
        onConfirm={confirmForget}
        onCancel={() => setPending(null)}
      />
    </div>
  );
}

function Header({ onRefresh, loading }: { onRefresh: () => void; loading: boolean }) {
  return (
    <div className="flex shrink-0 items-center justify-between gap-3 border-b border-border bg-surface px-3 py-2">
      <div className="flex items-center gap-2">
        <ShieldCheck className="size-4 text-accent" />
        <h2 className="text-sm font-medium tracking-tight">Privacy</h2>
        <span className="text-[10px] text-muted-foreground">
          Local data lives only on this machine.
        </span>
      </div>
      <Button
        size="sm"
        variant="outline"
        onClick={onRefresh}
        disabled={loading}
        className="h-7 text-xs"
      >
        {loading ? (
          <Loader2 className="size-3 animate-spin" />
        ) : (
          <span>Refresh</span>
        )}
      </Button>
    </div>
  );
}

function LoadingCard() {
  return (
    <div className="flex h-40 items-center justify-center gap-2 text-sm text-muted-foreground">
      <Loader2 className="size-4 animate-spin" />
      <span>Reading workspace…</span>
    </div>
  );
}

function ErrorCard({ message, onRetry }: { message: string; onRetry: () => void }) {
  // The most common failure mode here is "THINKINGROOT_WORKSPACE not
  // set" — surface that as a setup hint instead of a generic error.
  const isWorkspaceMissing = message.includes("WORKSPACE");
  return (
    <div className="rounded-md border border-warn/40 bg-warn/10 p-4 text-xs text-warn">
      <div className="flex items-start gap-2">
        <AlertTriangle className="mt-0.5 size-4 shrink-0" />
        <div className="min-w-0">
          <p className="font-medium">
            {isWorkspaceMissing ? "No workspace mounted" : "Could not load summary"}
          </p>
          <p className="mt-1 text-[11px] leading-relaxed text-muted-foreground">
            {isWorkspaceMissing
              ? "Set a workspace path in Settings → Workspace before the privacy dashboard can list local data."
              : message}
          </p>
        </div>
      </div>
      <Button variant="outline" size="sm" onClick={onRetry} className="mt-3 h-7 text-xs">
        Retry
      </Button>
    </div>
  );
}

function ReadyView({
  summary,
  onForget,
}: {
  summary: PrivacySummary;
  onForget: (source: PrivacySource) => void;
}) {
  const sources = useMemo(
    () => [...summary.sources].sort((a, b) => a.uri.localeCompare(b.uri)),
    [summary.sources],
  );

  return (
    <>
      <div className="grid grid-cols-1 gap-3 md:grid-cols-3">
        <Counter Icon={FileText} label="Sources" value={summary.source_count} />
        <Counter Icon={Database} label="Claims" value={summary.claim_count} />
        <Counter Icon={Users} label="Entities" value={summary.entity_count} />
      </div>

      <section className="rounded-xl border border-border bg-surface p-5">
        <header className="flex items-center gap-2">
          <h3 className="text-sm font-medium tracking-tight">Sources</h3>
          <span className="text-[10px] text-muted-foreground">
            workspace <code className="rounded bg-muted px-1 font-mono text-[10px]">{summary.workspace}</code>
          </span>
        </header>
        {sources.length === 0 ? (
          <p className="mt-4 text-xs text-muted-foreground">
            No sources in this workspace yet. Compile a folder to populate the
            graph.
          </p>
        ) : (
          <ul className="mt-3 flex flex-col divide-y divide-border">
            {sources.map((source) => (
              <li key={source.id} className="flex items-center gap-3 py-2">
                <div className="min-w-0 flex-1">
                  <p
                    className="truncate font-mono text-[12px] text-foreground"
                    title={source.uri}
                  >
                    {source.uri}
                  </p>
                  <p className="text-[10px] text-muted-foreground">
                    {source.source_type} · {source.id}
                  </p>
                </div>
                <Button
                  variant="ghost"
                  size="sm"
                  onClick={() => onForget(source)}
                  className={cn(
                    "h-7 gap-1 text-xs",
                    "text-destructive hover:bg-destructive/10",
                  )}
                >
                  <Trash2 className="size-3" />
                  Forget
                </Button>
              </li>
            ))}
          </ul>
        )}
      </section>
    </>
  );
}

function Counter({
  Icon,
  label,
  value,
}: {
  Icon: typeof FileText;
  label: string;
  value: number;
}) {
  return (
    <div className="flex items-center gap-3 rounded-lg border border-border bg-surface p-4">
      <div className="flex size-9 shrink-0 items-center justify-center rounded-md bg-accent/10 text-accent">
        <Icon className="size-4" />
      </div>
      <div>
        <p className="text-[10px] font-semibold uppercase tracking-widest text-muted-foreground">
          {label}
        </p>
        <p className="text-lg font-medium tabular-nums tracking-tight">
          {value.toLocaleString()}
        </p>
      </div>
    </div>
  );
}
