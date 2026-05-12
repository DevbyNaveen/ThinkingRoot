import { useCallback, useEffect, useMemo, useState } from "react";
import { AlertTriangle, Loader2, ShieldCheck, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { RefreshIcon } from "@/components/ui/refresh-icon";
import { cn } from "@/lib/utils";
import {
  privacyForget,
  privacySummary,
  type PrivacySource,
  type PrivacySummary,
} from "@/lib/tauri";
import { toast } from "@/store/toast";
import { useApp } from "@/store/app";
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
  const activeWorkspace = useApp((s) => s.activeWorkspace);
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
  }, [refresh, activeWorkspace]);

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
      <div className="min-h-0 flex-1 overflow-y-auto">
        <div className="flex w-full flex-col gap-5 px-4 py-5">
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
    <div className="flex shrink-0 items-center justify-between gap-3 border-b border-border/60 bg-surface/90 px-3 py-2">
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-1.5">
          <ShieldCheck className="size-3.5 shrink-0 text-accent/90" />
          <h2 className="text-xs font-semibold tracking-tight">Privacy</h2>
        </div>
        <p className="mt-0.5 text-[10px] leading-snug text-muted-foreground">
          Local substrate on this machine only.
        </p>
      </div>
      <Button
        variant="ghost"
        size="icon"
        onClick={onRefresh}
        disabled={loading}
        className="h-8 w-8 shrink-0 text-muted-foreground hover:text-foreground"
        aria-label="Refresh"
      >
        <RefreshIcon className={loading ? "size-3.5 animate-spin" : "size-3.5"} />
      </Button>
    </div>
  );
}

function LoadingCard() {
  return (
    <div className="flex items-center justify-center gap-2 py-12 text-xs text-muted-foreground">
      <Loader2 className="size-3.5 animate-spin" />
      <span>Reading workspace…</span>
    </div>
  );
}

function ErrorCard({ message, onRetry }: { message: string; onRetry: () => void }) {
  const isWorkspaceMissing = message.includes("WORKSPACE");
  return (
    <div className="rounded-lg border border-warn/35 bg-warn/8 px-3 py-3 text-xs text-warn">
      <div className="flex items-start gap-2">
        <AlertTriangle className="mt-0.5 size-3.5 shrink-0" />
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
      <p className="text-[11px] leading-relaxed text-muted-foreground">
        <span className="font-medium tabular-nums text-foreground/90">
          {summary.source_count.toLocaleString()}
        </span>{" "}
        sources
        <span className="mx-2 text-border/70">·</span>
        <span className="font-medium tabular-nums text-foreground/90">
          {summary.claim_count.toLocaleString()}
        </span>{" "}
        claims
        <span className="mx-2 text-border/70">·</span>
        <span className="font-medium tabular-nums text-foreground/90">
          {summary.entity_count.toLocaleString()}
        </span>{" "}
        entities
      </p>

      <section>
        <div className="flex flex-wrap items-baseline justify-between gap-x-2 gap-y-1">
          <h3 className="text-xs font-medium tracking-tight text-foreground">
            Indexed sources
          </h3>
          <code className="max-w-full truncate rounded bg-muted/50 px-1.5 py-0.5 font-mono text-[10px] text-muted-foreground">
            {summary.workspace}
          </code>
        </div>

        {sources.length === 0 ? (
          <p className="mt-3 text-[11px] text-muted-foreground">
            No sources yet. Compile a workspace to populate the graph.
          </p>
        ) : (
          <ul className="mt-3 divide-y divide-border/50 border-t border-border/50">
            {sources.map((source) => (
              <li
                key={source.id}
                className="flex items-start gap-2 py-2.5 first:pt-3"
              >
                <div className="min-w-0 flex-1">
                  <p
                    className="break-all font-mono text-[11px] leading-snug text-foreground/95"
                    title={source.uri}
                  >
                    {source.uri}
                  </p>
                  <p className="mt-0.5 text-[10px] text-muted-foreground">
                    {source.source_type} · {source.id}
                  </p>
                </div>
                <Button
                  variant="ghost"
                  size="sm"
                  onClick={() => onForget(source)}
                  className={cn(
                    "h-7 shrink-0 gap-1 px-2 text-[10px]",
                    "text-muted-foreground hover:bg-destructive/10 hover:text-destructive",
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
