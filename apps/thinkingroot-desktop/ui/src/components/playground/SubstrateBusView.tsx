import { useCallback, useEffect, useState } from "react";
import {
  Activity,
  RefreshCw,
  Play,
  Power,
  PowerOff,
  AlertTriangle,
} from "lucide-react";

import {
  substrateBusReports,
  substrateBusStart,
  substrateBusStop,
  substrateBusRunNow,
  type SubAgentReport,
} from "@/lib/tauri";
import { cn } from "@/lib/utils";

interface Props {
  workspace: string | null;
}

/**
 * Substrate Bus feed — Phase δ.4 of the Cognition Commits design.
 *
 * Renders the background sub-agents' recent observations as a feed.
 * Per-workspace; idempotent start/stop. Each agent gets a chip with
 * a "Run now" button so the user can force a fresh tick without
 * waiting on the agent's schedule.
 *
 * Honest empty state when the bus isn't running: explicit "Start
 * substrate bus" button rather than pretending observations exist.
 */
export function SubstrateBusView({ workspace }: Props) {
  const [reports, setReports] = useState<SubAgentReport[]>([]);
  const [agents, setAgents] = useState<string[]>([]);
  const [running, setRunning] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    if (!workspace) {
      setReports([]);
      setRunning(false);
      return;
    }
    try {
      const rs = await substrateBusReports();
      setReports(rs);
      if (rs.length > 0) {
        const uniq = Array.from(new Set(rs.map((r) => r.agent)));
        setAgents(uniq);
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, [workspace]);

  useEffect(() => {
    void load();
    // Poll every 30s when the panel is open; cheap, observations
    // arrive on minute timescales.
    const id = setInterval(() => void load(), 30_000);
    return () => clearInterval(id);
  }, [workspace, load]);

  const start = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      const resp = await substrateBusStart();
      setRunning(resp.running);
      setAgents(resp.agents);
      await load();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }, [load]);

  const stop = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      const resp = await substrateBusStop();
      setRunning(resp.running);
      setReports([]);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }, []);

  const runAgent = useCallback(
    async (agent: string) => {
      setBusy(true);
      setError(null);
      try {
        await substrateBusRunNow(agent);
        await load();
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
      } finally {
        setBusy(false);
      }
    },
    [load],
  );

  if (!workspace) {
    return (
      <div className="flex h-full items-center justify-center text-sm text-muted-foreground">
        Mount a workspace to enable the substrate bus.
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col">
      <header className="flex shrink-0 items-center gap-2 border-b border-border bg-surface/60 px-4 py-2">
        <Activity className="size-4 text-muted-foreground" />
        <h3 className="truncate text-sm font-semibold">Substrate Bus</h3>
        <span className="text-xs text-muted-foreground">
          background sub-agents · reconciler / gap-hunter / curator / watcher
        </span>
        <div className="ml-auto flex items-center gap-1">
          <button
            type="button"
            onClick={() => void load()}
            disabled={busy}
            aria-label="Reload reports"
            className={cn(
              "rounded-md p-1 text-muted-foreground transition-colors",
              busy
                ? "cursor-not-allowed opacity-50"
                : "hover:bg-muted/40 hover:text-foreground",
            )}
          >
            <RefreshCw className="size-3.5" />
          </button>
          {running ? (
            <button
              type="button"
              onClick={() => void stop()}
              disabled={busy}
              className="flex items-center gap-1 rounded-md border border-border bg-background px-2 py-0.5 text-xs hover:bg-muted/40"
            >
              <PowerOff className="size-3" />
              <span>Stop</span>
            </button>
          ) : (
            <button
              type="button"
              onClick={() => void start()}
              disabled={busy}
              className="flex items-center gap-1 rounded-md border border-accent bg-accent/10 px-2 py-0.5 text-xs text-accent hover:bg-accent/20"
            >
              <Power className="size-3" />
              <span>Start</span>
            </button>
          )}
        </div>
      </header>

      <div className="flex-1 overflow-y-auto px-4 py-3">
        {error && (
          <div className="mb-3 flex items-center gap-2 rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive">
            <AlertTriangle className="size-4" />
            <span>{error}</span>
          </div>
        )}

        {agents.length > 0 && (
          <div className="mb-3 flex flex-wrap gap-2">
            {agents.map((a) => (
              <button
                key={a}
                type="button"
                onClick={() => void runAgent(a)}
                disabled={busy}
                className={cn(
                  "flex items-center gap-1 rounded-full border border-border bg-surface/60 px-2 py-0.5 text-xs",
                  busy
                    ? "cursor-not-allowed opacity-60"
                    : "hover:bg-muted/40",
                )}
              >
                <Play className="size-3" />
                <span className="font-mono">{a}</span>
              </button>
            ))}
          </div>
        )}

        {reports.length === 0 ? (
          <EmptyState running={running} />
        ) : (
          <ol className="flex flex-col gap-2">
            {reports.map((r, i) => (
              <ReportCard key={`${r.agent}-${r.started_at}-${i}`} r={r} />
            ))}
          </ol>
        )}
      </div>
    </div>
  );
}

function EmptyState({ running }: { running: boolean }) {
  return (
    <div className="flex h-full flex-col items-center justify-center gap-2 px-6 text-center text-sm text-muted-foreground">
      <Activity className="size-6 opacity-40" />
      <p>
        {running
          ? "Bus is running — waiting for the first agent tick."
          : "Substrate bus not started for this workspace."}
      </p>
      <p className="text-xs">
        {running
          ? "Hit a 'Run now' chip above to force a fresh observation."
          : "Hit 'Start' to spin up the reconciler, gap-hunter, curator, and watcher sub-agents."}
      </p>
    </div>
  );
}

function ReportCard({ r }: { r: SubAgentReport }) {
  const failed = r.error !== null;
  return (
    <li
      className={cn(
        "rounded-md border bg-background p-2",
        failed ? "border-destructive/40 bg-destructive/5" : "border-border",
      )}
    >
      <div className="flex items-center gap-2">
        <code className="rounded bg-muted/40 px-1.5 py-0.5 font-mono text-xs">
          {r.agent}
        </code>
        <span className="text-xs text-muted-foreground">
          {new Date(r.finished_at).toLocaleString()}
        </span>
        {failed && (
          <span className="ml-auto flex items-center gap-1 text-xs text-destructive">
            <AlertTriangle className="size-3" /> failed
          </span>
        )}
      </div>
      {r.summary && (
        <p className="mt-1 text-sm text-foreground/90">{r.summary}</p>
      )}
      {r.error && (
        <code className="mt-1 block whitespace-pre-wrap rounded bg-destructive/10 px-2 py-1 text-xs text-destructive">
          {r.error}
        </code>
      )}
      {r.observations.length > 0 && (
        <ul className="mt-2 space-y-0.5 text-xs text-muted-foreground">
          {r.observations.map((o, i) => (
            <li key={i}>• {o}</li>
          ))}
        </ul>
      )}
    </li>
  );
}
