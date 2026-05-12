import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import {
  AlertTriangle,
  CheckCircle2,
  Circle,
  Loader2,
  Sparkles,
  XCircle,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  getSetupCompleteAt,
  markSetupComplete,
  resetCircuitBreaker,
} from "@/lib/tauri";
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
// `{ status: "stopped" | "crashed", exit_code }` on daemon exit, or
// `{ status: "restarting", attempt, backoff_ms }` between exponential
// backoff attempts (Slice F T2). When the breaker trips after the
// final attempt it emits `{ status: "repair_needed",
// failing_check_ids: ["daemon.restart.exhausted"],
// circuit_breaker_until }`. The install-manifest branch also emits
// `{ status: "repair_needed", failing_check_ids }`.
interface EngineStatusEventPayload {
  status: "repair_needed" | "stopped" | "crashed" | "restarting";
  failing_check_ids?: string[];
  exit_code?: number | null;
  // Slice F T2 — restart attempt counter (1..=4) + the wall-clock the
  // watchdog is sleeping before the next spawn.
  attempt?: number;
  backoff_ms?: number;
  // Slice F T3 — RFC3339 timestamp set when the breaker tripped.
  circuit_breaker_until?: string | null;
}

// Transient side-state for the in-progress restart banner. Lives
// alongside `engineStatus` (not part of the state machine) so the main
// UI stays mounted underneath the banner during the brief backoff
// window — flipping the gate to `crashed` for every restart attempt
// would tear down children every 1-2s.
interface RestartInfo {
  attempt: number;
  backoff_ms: number;
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
  // `null` = first-run setup not yet complete (wizard eligible).
  // `string` = ISO-8601 timestamp when the user finished setup; any
  // future `repair_needed` falls back to the standard panel.
  const [setupCompleteAt, setSetupCompleteAt] = useState<string | null>(null);
  // Non-null only between `restarting` events from the watchdog
  // (Slice F T2). Renders a corner banner over the main UI without
  // unmounting children.
  const [restartInfo, setRestartInfo] = useState<RestartInfo | null>(null);

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
        // Daemon is back — drop any in-progress restart banner.
        setRestartInfo(null);
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

    // Load the install-manifest setup-complete timestamp.  We treat a
    // read failure as "setup not complete" — the wizard surfacing on
    // a recoverable error is friendlier than the standard panel.
    getSetupCompleteAt()
      .then((ts) => {
        if (!cancelledRef.current) setSetupCompleteAt(ts);
      })
      .catch((err) => {
        console.error("get_setup_complete_at failed:", err);
        if (!cancelledRef.current) setSetupCompleteAt(null);
      });

    // Subscribe BEFORE the initial probe so we don't miss a crash
    // that happens during boot. `listen()` returns a promise; if we
    // get cancelled mid-await, drop the handle immediately.
    listen<EngineStatusEventPayload>("engine_status_changed", (event) => {
      if (cancelledRef.current) return;
      const payload = event.payload;
      if (payload.status === "restarting") {
        // Watchdog is between exponential-backoff attempts. Show a
        // non-blocking banner; do NOT flip `engineStatus` so the main
        // UI stays rendered underneath during the brief backoff
        // window (typically <2s — flicker-grade if we unmounted).
        setRestartInfo({
          attempt: payload.attempt ?? 1,
          backoff_ms: payload.backoff_ms ?? 0,
        });
        return;
      }
      if (payload.status === "stopped") {
        // Clean SIGTERM — usually app shutdown. Leave the UI alone.
        // Also clear any lingering banner; we won't be restarting.
        setRestartInfo(null);
        return;
      }
      // crashed / repair_needed → terminal states; banner should
      // disappear because either the daemon is back (next event will
      // be a healthy doctor row from refreshReport) or the breaker
      // has tripped (BlockingPanel takes over).
      setRestartInfo(null);
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

  // First-launch detection. The wizard variant surfaces only when:
  //   1. The user has never completed setup (`setup_complete_at` is null).
  //   2. We're in the install-manifest-failure branch (`repair_needed`,
  //      not a daemon crash — `crashed` always uses the standard panel).
  //   3. Every failing check is setup-relevant — i.e. `credentials.*`,
  //      `workspace.*`, or `models.*`. System-level failures
  //      (`binary.*`, `config.*`, `daemon.*`, `install.*`) escape to
  //      the standard panel even on first launch because they aren't
  //      something a friendly wizard can talk the user through.
  const isWizardVariant = useMemo(() => {
    if (setupCompleteAt !== null) return false;
    if (engineStatus !== "repair_needed") return false;
    if (!doctorReport) return false;
    const failing = doctorReport.checks.filter((c) => c.status === "fail");
    if (failing.length === 0) return false;
    return failing.every(
      (c) =>
        c.id.startsWith("credentials.") ||
        c.id.startsWith("workspace.") ||
        c.id.startsWith("models."),
    );
  }, [setupCompleteAt, engineStatus, doctorReport]);

  // Once setup-relevant checks resolve and we transition to healthy
  // for the first time, stamp the install manifest so the wizard
  // never surfaces again. Future failures fall through to the
  // standard panel.
  useEffect(() => {
    if (engineStatus === "healthy" && setupCompleteAt === null) {
      const now = new Date().toISOString();
      markSetupComplete()
        .then(() => {
          if (!cancelledRef.current) setSetupCompleteAt(now);
        })
        .catch((err) => {
          console.error("mark_setup_complete failed:", err);
        });
    }
  }, [engineStatus, setupCompleteAt]);

  // Wizard escape hatch — "Skip for now" still stamps the manifest so
  // the standard panel surfaces on subsequent launches if anything
  // remains broken.
  const handleSkipWizard = async () => {
    try {
      await markSetupComplete();
    } catch (err) {
      console.error("mark_setup_complete (skip) failed:", err);
    }
    if (cancelledRef.current) return;
    setSetupCompleteAt(new Date().toISOString());
    setEngineStatus("healthy");
  };

  // Healthy → render children unmodified. The restart banner overlays
  // (corner, non-blocking) when the watchdog is between attempts but
  // the daemon hasn't yet been declared crashed.
  if (engineStatus === "healthy") {
    return (
      <>
        {children}
        {restartInfo && (
          <RestartBanner
            attempt={restartInfo.attempt}
            backoffMs={restartInfo.backoff_ms}
          />
        )}
      </>
    );
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

  // First-launch wizard variant — friendlier copy, numbered steps,
  // contextual action buttons. Same data, same handlers; just a
  // different presentation layer for first-run users.
  if (isWizardVariant) {
    return (
      <WizardPanel
        report={doctorReport}
        reportLoading={reportLoading}
        fixInFlight={fixInFlight}
        onRecheck={handleRecheck}
        onFixAll={handleFixAll}
        onSkip={handleSkipWizard}
      />
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

          {report?.checks.some(
            (c) =>
              c.id === "daemon.restart.exhausted" && c.status === "fail",
          ) && (
            <CircuitBreakerSection
              onReset={async () => {
                try {
                  await resetCircuitBreaker();
                  await onRecheck();
                } catch (err) {
                  // Non-fatal — the row stays surfaced and the user
                  // can retry. We never silently swallow on the
                  // happy path; the catch here is logged for
                  // diagnostics only.
                  console.error("reset_circuit_breaker failed:", err);
                }
              }}
            />
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

// ────────────────────────────────────────────────────────────────────
// Restart banner + circuit-breaker section (Slice F T7)
// ────────────────────────────────────────────────────────────────────

/** Non-blocking corner overlay that surfaces while the watchdog is
 *  between exponential-backoff restart attempts. The main UI stays
 *  fully usable underneath — this is a transient status hint, not a
 *  gate. Wired in the `healthy` branch of `EngineGate` so it only
 *  shows when the engine was healthy before the restart started. */
function RestartBanner({
  attempt,
  backoffMs,
}: {
  attempt: number;
  backoffMs: number;
}) {
  return (
    <div
      role="status"
      aria-live="polite"
      className="fixed bottom-4 right-4 z-50 flex items-center gap-3 rounded-lg border border-warn/30 bg-warn/5 px-4 py-2 text-xs text-warn shadow-elevated"
    >
      <Loader2 className="size-4 animate-spin" aria-hidden />
      <span>
        Engine restarting (attempt {attempt}
        {backoffMs > 0 && (
          <> — waiting {(backoffMs / 1000).toFixed(1)}s</>
        )}
        )
      </span>
    </div>
  );
}

/** Surfaces inside the BlockingPanel when the watchdog has tripped its
 *  circuit breaker (`daemon.restart.exhausted` fail row). Clicking
 *  "Reset and try again" calls `reset_circuit_breaker` then a fresh
 *  `doctor_check` so the row clears and auto-restart resumes. Failure
 *  is honest: the loading state ends and the row stays visible. */
function CircuitBreakerSection({
  onReset,
}: {
  onReset: () => Promise<void>;
}) {
  const [resetting, setResetting] = useState(false);
  return (
    <div className="mt-4 rounded-lg border border-destructive/30 bg-destructive/5 p-4">
      <h3 className="text-sm font-medium text-destructive">
        Auto-restart suspended
      </h3>
      <p className="mt-1 text-xs text-muted-foreground">
        The engine has crashed too many times in a row. Click below to
        clear the safety lock and try again. Auto-restart will resume.
      </p>
      <Button
        variant="outline"
        size="sm"
        className="mt-3 h-8 text-xs"
        disabled={resetting}
        onClick={async () => {
          setResetting(true);
          try {
            await onReset();
          } finally {
            setResetting(false);
          }
        }}
      >
        {resetting ? (
          <>
            <Loader2 className="size-3.5 animate-spin" />
            Resetting…
          </>
        ) : (
          "Reset and try again"
        )}
      </Button>
    </div>
  );
}

// ────────────────────────────────────────────────────────────────────
// Wizard variant — first-launch onboarding panel
// ────────────────────────────────────────────────────────────────────

interface WizardPanelProps {
  report: DoctorReport | null;
  reportLoading: boolean;
  fixInFlight: boolean;
  onRecheck: () => void;
  onFixAll: () => void;
  onSkip: () => void;
}

/** Friendlier first-launch presentation of the same doctor report.
 *  Surfaces only when `setup_complete_at` is null AND every failing
 *  check is setup-relevant (credentials / workspace / models). On
 *  completion the parent stamps the install manifest so subsequent
 *  failures use the standard panel. */
function WizardPanel({
  report,
  reportLoading,
  fixInFlight,
  onRecheck,
  onFixAll,
  onSkip,
}: WizardPanelProps) {
  // The "steps" of the wizard ARE the setup-relevant rows — failing
  // ones are active steps, ok rows are checked-off steps, warn /
  // skipped rows stay informational but visible.  We sort by a stable
  // family ordering so credentials come before workspaces come before
  // models — the natural setup arc.
  const setupChecks = useMemo(() => {
    if (!report) return [] as CheckResult[];
    const order = (id: string): number => {
      if (id.startsWith("credentials.")) return 0;
      if (id.startsWith("workspace.")) return 1;
      if (id.startsWith("models.")) return 2;
      return 3;
    };
    return [...report.checks]
      .filter(
        (c) =>
          c.id.startsWith("credentials.") ||
          c.id.startsWith("workspace.") ||
          c.id.startsWith("models."),
      )
      .sort((a, b) => order(a.id) - order(b.id));
  }, [report]);

  const failCount = report?.summary.fail ?? 0;
  const canContinue = failCount === 0;

  // Same fix-availability rule as the standard panel.
  const hasApplicableFix =
    report?.checks.some(
      (c) => c.status === "fail" && c.fix && c.fix.kind === "run-command",
    ) ?? false;

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label="Welcome to ThinkingRoot"
      className="fixed inset-0 z-[60] flex items-center justify-center bg-background/95 backdrop-blur-sm"
    >
      <div className="flex max-h-[90vh] w-full max-w-2xl flex-col overflow-hidden rounded-xl border border-border bg-surface-elevated shadow-elevated">
        <header className="flex items-start gap-3 border-b border-border px-6 py-5">
          <div className="mt-0.5 flex size-8 shrink-0 items-center justify-center rounded-full bg-accent/10">
            <Sparkles className="size-4 text-accent" aria-hidden />
          </div>
          <div className="min-w-0 flex-1">
            <h2 className="text-base font-medium tracking-tight text-foreground">
              Welcome to ThinkingRoot
            </h2>
            <p className="mt-1 text-xs leading-relaxed text-muted-foreground">
              Just a couple of quick steps to finish setting up.
            </p>
          </div>
        </header>

        <div className="flex-1 overflow-y-auto px-6 py-5">
          {reportLoading && !report && (
            <div className="flex h-32 items-center justify-center gap-2 text-sm text-muted-foreground">
              <Loader2 className="size-4 animate-spin" />
              <span>Getting things ready…</span>
            </div>
          )}

          {report && setupChecks.length === 0 && !reportLoading && (
            <div className="flex items-start gap-2 rounded-md bg-background p-3 text-xs text-muted-foreground">
              <CheckCircle2 className="mt-0.5 size-4 shrink-0 text-success" />
              <p>You're all set. Press "Continue" to start using ThinkingRoot.</p>
            </div>
          )}

          {report && setupChecks.length > 0 && (
            <ol className="flex flex-col gap-3">
              {setupChecks.map((c, idx) => (
                <WizardStepRow key={c.id} step={idx + 1} check={c} />
              ))}
            </ol>
          )}
        </div>

        <footer className="flex items-center justify-between gap-2 border-t border-border px-6 py-4">
          <Button
            variant="ghost"
            size="sm"
            onClick={onSkip}
            disabled={fixInFlight}
            className="h-8 text-xs text-muted-foreground hover:text-foreground"
            title="Skip setup — you can finish later from Settings."
          >
            Skip for now
          </Button>
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
            {hasApplicableFix && !canContinue && (
              <Button
                variant="outline"
                size="sm"
                onClick={onFixAll}
                disabled={fixInFlight || reportLoading}
                className="h-8 text-xs"
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
            )}
            <Button
              size="sm"
              onClick={onSkip}
              disabled={!canContinue || fixInFlight}
              className="h-8 text-xs"
              title={
                canContinue
                  ? "All setup steps complete — start using ThinkingRoot."
                  : "Resolve the remaining steps to continue."
              }
            >
              Continue
            </Button>
          </div>
        </footer>
      </div>
    </div>
  );
}

interface WizardStepRowProps {
  step: number;
  check: CheckResult;
}

/** A single numbered wizard step. Renders one of three states:
 *  - ok      → checkmark + muted label, "step complete"
 *  - fail    → numbered circle + action button (label-based — "Add a
 *              provider key", "Choose a workspace", etc.)
 *  - warn /
 *    skipped → numbered circle, no action, informational only. */
function WizardStepRow({ step, check }: WizardStepRowProps) {
  const isDone = check.status === "ok";
  const isFail = check.status === "fail";

  return (
    <li className="flex items-start gap-3">
      <div className="mt-0.5 flex size-6 shrink-0 items-center justify-center">
        {isDone ? (
          <CheckCircle2 className="size-5 text-success" aria-hidden />
        ) : isFail ? (
          <div className="flex size-5 items-center justify-center rounded-full bg-accent/10 text-[11px] font-medium text-accent">
            {step}
          </div>
        ) : (
          <Circle className="size-5 text-muted-foreground/50" aria-hidden />
        )}
      </div>
      <div className="min-w-0 flex-1 pb-1">
        <div
          className={cn(
            "text-xs font-medium",
            isDone ? "text-muted-foreground line-through" : "text-foreground",
          )}
        >
          {check.label}
        </div>
        {!isDone && (
          <div className="mt-0.5 break-words text-[11px] leading-relaxed text-muted-foreground">
            {check.detail}
          </div>
        )}
        {isFail && check.fix && <WizardFixHint check={check} />}
      </div>
    </li>
  );
}

/** Label-based action prompt for a wizard step. Translates the check
 *  id family into a user-facing action label without inventing
 *  capabilities the engine doesn't have — actual remediation still
 *  goes through the existing fix kinds (shell-hint / run-command /
 *  fill-in). */
function WizardFixHint({ check }: { check: CheckResult }) {
  const fix = check.fix!;
  const actionLabel = wizardActionLabel(check.id);

  if (fix.kind === "shell-hint") {
    return (
      <div className="mt-2 flex flex-col gap-1.5">
        <div className="text-[11px] font-medium text-foreground">
          {actionLabel}
        </div>
        <pre className="overflow-x-auto rounded border border-border bg-surface px-2 py-1 font-mono text-[11px] leading-relaxed text-foreground">
          <span className="text-muted-foreground">$ </span>
          {fix.command}
        </pre>
      </div>
    );
  }
  if (fix.kind === "run-command") {
    return (
      <p className="mt-2 text-[11px] text-muted-foreground">
        Click "Fix automatically" below to {actionLabel.toLowerCase()}.
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

/** Map a check id to a friendly action verb. Pure string mapping —
 *  no engine round-trips, no fabricated state. */
function wizardActionLabel(id: string): string {
  if (id.startsWith("credentials.")) return "Add a provider key";
  if (id === "workspace.active.exists" || id.startsWith("workspace.active."))
    return "Choose a workspace";
  if (id.startsWith("workspace.")) return "Set up a workspace";
  if (id.startsWith("models.")) return "Pick a model";
  return "Resolve this step";
}
