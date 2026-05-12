import { useEffect, useRef, useState, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { AlertTriangle, CheckCircle2, Loader2, XCircle } from "lucide-react";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

/**
 * Engine connection state for the desktop UI.
 *
 * - `booting` — initial state on app launch; sidecar is being probed.
 * - `healthy` — daemon is reachable; main UI renders.
 * - `repair_needed` — install or runtime problem; render blocking panel.
 * - `crashed` — daemon was running but exited; render blocking panel.
 *
 * Spec: `docs/superpowers/specs/2026-05-11-install-runtime-smoothness-design.md` §4.
 * Source events: `engine_status_changed` emitted by
 * `apps/thinkingroot-desktop/src-tauri/src/agent_runtime_subprocess.rs`
 * (Slice C T7 for the `repair_needed` path, Slice D T2 for the
 * watchdog `stopped`/`crashed` path).
 */
export type EngineStatus = "booting" | "healthy" | "repair_needed" | "crashed";

// Mirrors `TypedFixAction` in
// `apps/thinkingroot-desktop/src-tauri/src/commands/doctor.rs`.
// `#[serde(tag = "kind", rename_all = "kebab-case")]` on the Rust side
// renames variants to `shell-hint` / `run-command` / `fill-in`.
type FixAction =
  | { kind: "shell-hint"; command: string }
  | { kind: "run-command"; command: string }
  | { kind: "fill-in"; prompt: string; credential_key: string };

// Mirrors `TypedCheckResult`.
interface CheckResult {
  id: string;
  label: string;
  status: "ok" | "warn" | "fail" | "skipped";
  detail: string;
  fix?: FixAction | null;
}

// Mirrors `TypedDoctorReport`.
interface DoctorReport {
  schema_version: number;
  checks: CheckResult[];
  summary: { ok: number; warn: number; fail: number; skipped: number };
}

// Payload shapes for `engine_status_changed`. The watchdog emits
// `{ status: "stopped" | "crashed", exit_code }`; the install-manifest
// branch emits `{ status: "repair_needed", failing_check_ids }`.
interface EngineStatusEventPayload {
  status: "repair_needed" | "stopped" | "crashed";
  failing_check_ids?: string[];
  exit_code?: number | null;
}

interface EngineGateProps {
  children: ReactNode;
}

/**
 * Top-level state machine wrapping the main UI.
 *
 * Subscribes to the `engine_status_changed` Tauri event and probes
 * `doctor_check` on mount. On any non-healthy transition the
 * full-screen blocking panel renders the freshest doctor report —
 * the main app tree is unmounted until the engine is healthy again.
 *
 * "stopped" events (clean SIGTERM during app shutdown) intentionally
 * do NOT flip the panel: the app is going away anyway, and flashing
 * a panel during teardown is worse than silence.
 */
export function EngineGate({ children }: EngineGateProps) {
  const [engineStatus, setEngineStatus] = useState<EngineStatus>("booting");
  const [doctorReport, setDoctorReport] = useState<DoctorReport | null>(null);
  const [reportLoading, setReportLoading] = useState(false);
  const [fixInFlight, setFixInFlight] = useState(false);
  const [bootProbeDone, setBootProbeDone] = useState(false);

  // Hold the latest cancellation flag in a ref so the event listener
  // (which captures it at mount time) can read the live value.
  const cancelledRef = useRef(false);

  // Pull-style refresh used by both the initial probe and event-driven
  // re-checks. Always re-reads from the CLI so the panel never shows
  // stale rows.
  const refreshReport = async (opts?: {
    forceStatusIfFailures?: EngineStatus;
  }): Promise<DoctorReport | null> => {
    setReportLoading(true);
    try {
      const report = await invoke<DoctorReport>("doctor_check");
      if (cancelledRef.current) return null;
      setDoctorReport(report);
      if (report.summary.fail === 0) {
        setEngineStatus("healthy");
      } else if (opts?.forceStatusIfFailures) {
        setEngineStatus(opts.forceStatusIfFailures);
      } else {
        // Got failing rows without an explicit event-driven status —
        // treat as `repair_needed`.
        setEngineStatus((prev) =>
          prev === "crashed" ? "crashed" : "repair_needed",
        );
      }
      return report;
    } catch (err) {
      // Couldn't even reach the CLI. Surface a synthetic report so
      // the panel has something to show — silent failures here would
      // violate Honesty Rule #1.
      console.error("doctor_check failed:", err);
      if (cancelledRef.current) return null;
      const message = err instanceof Error ? err.message : String(err);
      setDoctorReport({
        schema_version: 1,
        checks: [
          {
            id: "doctor.unreachable",
            label: "Could not run `root doctor`",
            status: "fail",
            detail: message,
            fix: null,
          },
        ],
        summary: { ok: 0, warn: 0, fail: 1, skipped: 0 },
      });
      setEngineStatus((prev) => (prev === "crashed" ? "crashed" : "repair_needed"));
      return null;
    } finally {
      if (!cancelledRef.current) setReportLoading(false);
    }
  };

  // Initial doctor probe + event subscription.
  useEffect(() => {
    cancelledRef.current = false;
    let unlistenFn: UnlistenFn | null = null;

    // Subscribe BEFORE the initial probe so we don't miss a crash
    // that happens during boot. `listen()` returns a promise; if we
    // get cancelled mid-await, drop the handle immediately.
    listen<EngineStatusEventPayload>("engine_status_changed", (event) => {
      if (cancelledRef.current) return;
      const payload = event.payload;
      if (payload.status === "stopped") {
        // Clean SIGTERM — usually app shutdown. Leave the UI alone.
        return;
      }
      const forced: EngineStatus =
        payload.status === "crashed" ? "crashed" : "repair_needed";
      // Optimistically flip status so the panel appears even before
      // the report fetch resolves.
      setEngineStatus(forced);
      void refreshReport({ forceStatusIfFailures: forced });
    })
      .then((fn) => {
        if (cancelledRef.current) {
          fn();
        } else {
          unlistenFn = fn;
        }
      })
      .catch((err) => {
        console.error("listen(engine_status_changed) failed:", err);
      });

    // Initial probe.
    void refreshReport().finally(() => {
      if (!cancelledRef.current) setBootProbeDone(true);
    });

    return () => {
      cancelledRef.current = true;
      if (unlistenFn) unlistenFn();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const handleRecheck = async () => {
    await refreshReport();
  };

  const handleFixAll = async () => {
    setFixInFlight(true);
    try {
      const report = await invoke<DoctorReport>("doctor_apply_fix", {
        checkId: null,
      });
      if (cancelledRef.current) return;
      setDoctorReport(report);
      if (report.summary.fail === 0) {
        setEngineStatus("healthy");
      }
    } catch (err) {
      console.error("doctor_apply_fix failed:", err);
    } finally {
      if (!cancelledRef.current) setFixInFlight(false);
    }
  };

  // Healthy → render children unmodified.
  if (engineStatus === "healthy") {
    return <>{children}</>;
  }

  // Booting → render a minimal splash until the first probe lands.
  // Once the probe completes we either go `healthy` (above) or flip
  // to `repair_needed` (below).
  if (engineStatus === "booting" && !bootProbeDone) {
    return (
      <div className="flex h-full w-full items-center justify-center bg-background text-muted-foreground">
        <div className="flex items-center gap-2 text-sm">
          <Loader2 className="size-4 animate-spin" />
          <span>Starting ThinkingRoot…</span>
        </div>
      </div>
    );
  }

  // Non-healthy → full-screen blocking panel.
  return (
    <BlockingPanel
      status={engineStatus}
      report={doctorReport}
      reportLoading={reportLoading}
      fixInFlight={fixInFlight}
      onRecheck={handleRecheck}
      onFixAll={handleFixAll}
    />
  );
}

interface BlockingPanelProps {
  status: EngineStatus;
  report: DoctorReport | null;
  reportLoading: boolean;
  fixInFlight: boolean;
  onRecheck: () => void;
  onFixAll: () => void;
}

function BlockingPanel({
  status,
  report,
  reportLoading,
  fixInFlight,
  onRecheck,
  onFixAll,
}: BlockingPanelProps) {
  const title =
    status === "crashed"
      ? "ThinkingRoot engine crashed"
      : "ThinkingRoot engine is unavailable";

  const subtitle =
    status === "crashed"
      ? "The local engine exited unexpectedly. Run the diagnostics below to recover."
      : "One or more environment checks failed. Resolve them to continue.";

  // Anything that isn't `ok` is worth surfacing in the blocking
  // panel — `warn` rows give context, `fail` rows block, `skipped`
  // rows are honest absences (per witness-mesh `lsp::skipped@v1`
  // convention).
  const surfacedChecks =
    report?.checks.filter((c) => c.status !== "ok") ?? [];
  const okCount = report?.summary.ok ?? 0;

  // Whether any failing row actually has a fix the engine can apply.
  const hasApplicableFix =
    report?.checks.some(
      (c) => c.status === "fail" && c.fix && c.fix.kind === "run-command",
    ) ?? false;

  return (
    <div
      role="alertdialog"
      aria-modal="true"
      aria-label={title}
      className="fixed inset-0 z-[60] flex items-center justify-center bg-background/95 backdrop-blur-sm"
    >
      <div className="flex max-h-[90vh] w-full max-w-2xl flex-col overflow-hidden rounded-xl border border-border bg-surface-elevated shadow-elevated">
        <header className="flex items-start gap-3 border-b border-border px-5 py-4">
          <AlertTriangle
            className={cn(
              "mt-0.5 size-5 shrink-0",
              status === "crashed" ? "text-destructive" : "text-warn",
            )}
            aria-hidden
          />
          <div className="min-w-0 flex-1">
            <h2 className="text-sm font-medium tracking-tight text-foreground">
              {title}
            </h2>
            <p className="mt-1 text-[11px] leading-relaxed text-muted-foreground">
              {subtitle}
            </p>
          </div>
        </header>

        <div className="flex-1 overflow-y-auto px-5 py-4">
          {reportLoading && !report && (
            <div className="flex h-32 items-center justify-center gap-2 text-sm text-muted-foreground">
              <Loader2 className="size-4 animate-spin" />
              <span>Running diagnostics…</span>
            </div>
          )}

          {report && surfacedChecks.length === 0 && reportLoading && (
            <div className="flex h-32 items-center justify-center gap-2 text-sm text-muted-foreground">
              <Loader2 className="size-4 animate-spin" />
              <span>Re-checking…</span>
            </div>
          )}

          {report && surfacedChecks.length === 0 && !reportLoading && (
            <div className="flex items-start gap-2 rounded-md border border-border bg-background p-3 text-xs text-muted-foreground">
              <CheckCircle2 className="mt-0.5 size-4 shrink-0 text-success" />
              <p>
                Diagnostics returned no failing checks. Press "Re-check" to
                re-probe the engine, or wait for it to recover automatically.
              </p>
            </div>
          )}

          {report && surfacedChecks.length > 0 && (
            <ul className="flex flex-col gap-2">
              {surfacedChecks.map((c) => (
                <CheckRow key={c.id} check={c} />
              ))}
              {okCount > 0 && (
                <li className="px-1 pt-1 text-[11px] text-muted-foreground">
                  {okCount} other check{okCount === 1 ? "" : "s"} passed.
                </li>
              )}
            </ul>
          )}
        </div>

        <footer className="flex items-center justify-between gap-2 border-t border-border px-5 py-3">
          <p className="text-[11px] text-muted-foreground">
            ThinkingRoot won't compile or chat until the engine is healthy.
          </p>
          <div className="flex items-center gap-2">
            <Button
              variant="outline"
              size="sm"
              onClick={onRecheck}
              disabled={reportLoading || fixInFlight}
              className="h-8 text-xs"
            >
              {reportLoading ? (
                <>
                  <Loader2 className="size-3.5 animate-spin" />
                  Checking…
                </>
              ) : (
                "Re-check"
              )}
            </Button>
            <Button
              size="sm"
              onClick={onFixAll}
              disabled={fixInFlight || reportLoading || !hasApplicableFix}
              className="h-8 text-xs"
              title={
                hasApplicableFix
                  ? undefined
                  : "No automatic fixes available — follow the shell hints above."
              }
            >
              {fixInFlight ? (
                <>
                  <Loader2 className="size-3.5 animate-spin" />
                  Applying fixes…
                </>
              ) : (
                "Fix automatically"
              )}
            </Button>
          </div>
        </footer>
      </div>
    </div>
  );
}

function CheckRow({ check }: { check: CheckResult }) {
  const tone =
    check.status === "fail"
      ? {
          Icon: XCircle,
          iconClass: "text-destructive",
          borderClass: "border-destructive/40",
        }
      : check.status === "warn"
        ? {
            Icon: AlertTriangle,
            iconClass: "text-warn",
            borderClass: "border-warn/40",
          }
        : {
            // skipped — honest absence, not a failure.
            Icon: AlertTriangle,
            iconClass: "text-muted-foreground",
            borderClass: "border-border",
          };
  const Icon = tone.Icon;

  return (
    <li
      className={cn(
        "flex items-start gap-2 rounded-md border bg-background p-3",
        tone.borderClass,
      )}
    >
      <Icon className={cn("mt-0.5 size-4 shrink-0", tone.iconClass)} aria-hidden />
      <div className="min-w-0 flex-1">
        <div className="text-xs font-medium text-foreground">{check.label}</div>
        <div className="mt-0.5 break-words text-[11px] leading-relaxed text-muted-foreground">
          {check.detail}
        </div>
        {check.fix && <FixHint fix={check.fix} />}
      </div>
    </li>
  );
}

function FixHint({ fix }: { fix: FixAction }) {
  if (fix.kind === "shell-hint") {
    return (
      <pre className="mt-2 overflow-x-auto rounded border border-border bg-surface px-2 py-1 font-mono text-[11px] leading-relaxed text-foreground">
        <span className="text-muted-foreground">$ </span>
        {fix.command}
      </pre>
    );
  }
  if (fix.kind === "run-command") {
    return (
      <p className="mt-2 text-[11px] text-muted-foreground">
        Automatic fix available — click "Fix automatically" below.
      </p>
    );
  }
  // fill-in — credential the user must paste in.
  return (
    <p className="mt-2 text-[11px] text-muted-foreground">
      {fix.prompt}{" "}
      <code className="font-mono text-[11px] text-foreground">
        ({fix.credential_key})
      </code>
    </p>
  );
}
