/**
 * Right panel — Cursor-style tabbed inspector with drag-to-resize.
 *
 * Tab bar (top, icon-only):
 *   Hammer  → Compile   (workspace card + live compile progress + branch graph)
 *   FolderTree → Workspace (readme + folder tree in one panel; readme default)
 *   Glyph   → Knowledge (BrainView in panel mode)
 *   Globe2  → Browser   (manual web browser)
 *   Terminal → Terminal (PTY shells)
 *   ShieldCheck → Privacy (PrivacyDashboard in panel mode)
 *
 * The left edge has an invisible drag handle that lets the user
 * resize the panel (min 220px, max 600px). Width is persisted in
 * the app store so it survives reloads.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import {
  PanelRight,
  Folder,
  Hammer,
  ShieldCheck,
  CheckCircle2,
  AlertCircle,
  Loader2,
  Square,
  FolderTree,
  Package,
  Globe2,
  Terminal as TerminalIcon,
} from "lucide-react";

import { cn } from "@/lib/utils";
import { ThinkingRootGlyph } from "@/components/shell/ThinkingRootGlyph";
import { useApp } from "@/store/app";
import { Button } from "@/components/ui/button";
import { toast } from "@/store/toast";
import { BrainView } from "@/components/brain/BrainView";
import { PrivacyDashboard } from "@/components/privacy/PrivacyDashboard";
import { WorkspaceFilesPanel } from "@/components/shell/WorkspaceFilesPanel";
import { BranchResolutionRiver } from "@/components/shell/CompileBranchPipeline";
import { BrowserPanel } from "@/components/browser/BrowserPanel";
import { TerminalPanel } from "@/components/terminal/TerminalPanel";
import {
  workspaceCompile,
  workspaceCompileStop,
  workspaceList,
  type CompileProgress,
  type IncrementalSummary,
  type WorkspaceView,
} from "@/lib/tauri";
import {
  pickPrimaryDiagnostic,
  substrateBadge,
  useWorkspaceConnection,
  useWorkspaceStatus,
  useWorkspaceStatusSubscription,
} from "@/store/workspace-status";
import type { RightRailTab } from "@/types";

const MIN_WIDTH = 250;
const MAX_WIDTH = 800;
const DEFAULT_WIDTH = 450;

const TABS: { id: RightRailTab; Icon: React.ElementType; label: string }[] = [
  { id: "compile", Icon: Hammer, label: "Compile" },
  { id: "files", Icon: FolderTree, label: "Workspace" },
  { id: "brain", Icon: ThinkingRootGlyph, label: "Knowledge" },
  { id: "browser", Icon: Globe2, label: "Browser" },
  { id: "terminal", Icon: TerminalIcon, label: "Terminal" },
  { id: "privacy", Icon: ShieldCheck, label: "Privacy" },
];

export function RightRail() {
  const open          = useApp((s) => s.rightRailOpen);
  const toggle        = useApp((s) => s.toggleRightRail);
  const activeTab     = useApp((s) => s.rightRailTab);
  const setTab        = useApp((s) => s.setRightRailTab);
  const storedWidth   = useApp((s) => s.rightRailWidth);
  const setStoreWidth = useApp((s) => s.setRightRailWidth);
  const activeWorkspace = useApp((s) => s.activeWorkspace);

  // Local width during drag; synced to store on mouse-up.
  const [width, setWidth]     = useState(storedWidth ?? DEFAULT_WIDTH);
  const dragging              = useRef(false);
  const startX                = useRef(0);
  const startWidth            = useRef(width);
  const railRef               = useRef<HTMLElement>(null);

  // Keep local width in sync when store changes externally.
  useEffect(() => {
    setWidth(storedWidth ?? DEFAULT_WIDTH);
  }, [storedWidth]);

  const onMouseDown = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    dragging.current  = true;
    startX.current    = e.clientX;
    startWidth.current = width;

    const onMove = (ev: MouseEvent) => {
      if (!dragging.current) return;
      // Dragging leftward increases the panel width.
      const delta = startX.current - ev.clientX;
      const next  = Math.min(MAX_WIDTH, Math.max(MIN_WIDTH, startWidth.current + delta));
      setWidth(next);
    };

    const onUp = (ev: MouseEvent) => {
      dragging.current = false;
      const delta = startX.current - ev.clientX;
      const next  = Math.min(MAX_WIDTH, Math.max(MIN_WIDTH, startWidth.current + delta));
      setStoreWidth(next);
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup",   onUp);
    };

    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup",   onUp);
  }, [width, setStoreWidth]);

  // ── Collapsed state ─────────────────────────────────────────────
  if (!open) {
    return (
      <div className="flex h-full w-10 shrink-0 flex-col items-center bg-surface">
        <header className="flex h-11 w-full items-center justify-center">
          <Button
            variant="ghost"
            size="icon"
            onClick={toggle}
            aria-label="Open panel"
            className="h-7 w-7"
          >
            <PanelRight className="size-3.5" />
          </Button>
        </header>
      </div>
    );
  }

  // ── Open state ──────────────────────────────────────────────────
  return (
    <aside
      ref={railRef}
      className="relative flex h-full shrink-0 flex-col border-l border-border bg-surface"
      style={{ width }}
      aria-label="Inspector panel"
    >
      {/* ── Drag handle (left edge) ────────────────────────────── */}
      <div
        className="absolute left-0 top-0 z-10 h-full w-1 cursor-col-resize select-none opacity-0 transition-opacity hover:opacity-100 active:opacity-100"
        style={{ background: "hsl(var(--accent) / 0.4)" }}
        onMouseDown={onMouseDown}
        aria-label="Resize panel"
      />

      {/* ── Tab bar ────────────────────────────────────────────── */}
      <header className="flex h-11 shrink-0 items-center border-b border-border pl-2 pr-1">
        {/* Tab icons */}
        <nav className="flex flex-1 items-center gap-0.5" aria-label="Panel tabs">
          {TABS.map(({ id, Icon, label }) => (
            <button
              key={id}
              type="button"
              onClick={() => setTab(id)}
              title={label}
              aria-label={label}
              aria-pressed={activeTab === id}
              className={cn(
                "flex h-7 w-7 items-center justify-center rounded-md transition-colors",
                activeTab === id
                  ? "bg-muted text-foreground"
                  : "text-muted-foreground/60 hover:bg-muted/50 hover:text-foreground",
              )}
            >
              <Icon
                className={id === "brain" ? "size-3.5 opacity-90" : "size-3.5"}
              />
            </button>
          ))}
        </nav>

        {/* Collapse button */}
        <Button
          variant="ghost"
          size="icon"
          onClick={toggle}
          aria-label="Close panel"
          className="h-7 w-7 shrink-0 text-muted-foreground/60 hover:text-foreground"
        >
          <PanelRight className="size-3.5" />
        </Button>
      </header>

      {/* ── Panel label ────────────────────────────────────────── */}
      <div className="flex h-7 shrink-0 items-center border-b border-border/50 px-3">
        <span className="text-[10px] font-semibold uppercase tracking-widest text-muted-foreground/50">
          {TABS.find((t) => t.id === activeTab)?.label}
          {activeWorkspace ? ` · ${activeWorkspace}` : ""}
        </span>
      </div>

      {/* ── Panel content ──────────────────────────────────────── */}
      <div className="flex min-h-0 flex-1 flex-col overflow-hidden">
        {activeTab === "compile" && (
          <CompilePanel activeWorkspace={activeWorkspace} />
        )}
        {activeTab === "files" && (
          <WorkspaceFilesPanel activeWorkspace={activeWorkspace} />
        )}
        {/* Brain stays permanently mounted while the rail is open so
            the force-layout worker keeps its simulation state and the
            d3 canvas keeps its GPU upload across tab switches.  The
            BrainView pauses its simulation internally when
            `isVisible` is false, so an unfocused Brain tab costs
            zero CPU. */}
        <div
          className={cn(
            "min-h-0 flex-1 overflow-hidden",
            activeTab === "brain" ? "flex flex-col" : "hidden",
          )}
        >
          <BrainView panelMode isVisible={activeTab === "brain"} />
        </div>
        {activeTab === "privacy" && (
          <div className="flex-1 overflow-hidden">
            <PrivacyDashboard />
          </div>
        )}
        {/* Browser is also permanently mounted while the rail is open
            so native child WebViews can be hidden/shown cleanly and do
            not get recreated on every tab switch. */}
        <div
          className={cn(
            "min-h-0 flex-1 overflow-hidden",
            activeTab === "browser" ? "flex flex-col" : "hidden",
          )}
        >
          <BrowserPanel isActive={activeTab === "browser"} />
        </div>
        {/* Terminal stays mounted across rail-tab switches so xterm
            scrollback and the live shell process survive when the
            user pops over to Compile / Brain. Only its visibility
            toggles. */}
        <div
          className={cn(
            "min-h-0 flex-1 overflow-hidden",
            activeTab === "terminal" ? "flex flex-col" : "hidden",
          )}
        >
          <TerminalPanel isActive={activeTab === "terminal"} />
        </div>
      </div>
    </aside>
  );
}

// ── Compile panel ──────────────────────────────────────────────────

function CompilePanel({ activeWorkspace }: { activeWorkspace: string | null }) {
  return (
    <div className="flex flex-col gap-6 overflow-y-auto px-4 py-5">
      <WorkspaceCard activeWorkspace={activeWorkspace} />
      <CompilationProgressIndicator />
      {activeWorkspace && <BranchResolutionRiver workspace={activeWorkspace} />}
    </div>
  );
}

function WorkspaceCard({ activeWorkspace }: { activeWorkspace: string | null }) {
  const setPackExportTarget = useApp((s) => s.setPackExportTarget);
  const setRightRailTab = useApp((s) => s.setRightRailTab);
  const [w, setW] = useState<WorkspaceView | null>(null);
  const [busy, setBusy] = useState(false);

  // Slice 0 — subscribe to the daemon's unified status stream for the
  // active workspace. The previous code read `w.compiled` (a single
  // boolean from `workspaceList()`) which collapsed five distinct
  // substrate states to "compiled / uncompiled" and disagreed with
  // the chat banner, the export dialog, and the MCP TOOLS panel.
  // The unified status replaces that with a five-tone badge driven
  // by `substrate.kind`.
  useWorkspaceStatusSubscription(activeWorkspace);
  const status = useWorkspaceStatus(activeWorkspace);
  const conn = useWorkspaceConnection(activeWorkspace);

  useEffect(() => {
    let cancelled = false;
    if (!activeWorkspace) { setW(null); return; }
    workspaceList()
      .then((list) => {
        if (cancelled) return;
        setW(list.find((x) => x.name === activeWorkspace) ?? null);
      })
      .catch(() => setW(null));
    return () => { cancelled = true; };
  }, [activeWorkspace]);

  if (!activeWorkspace) {
    return (
      <p className="text-[11px] text-muted-foreground">
        No workspace selected. Pick one from the sidebar.
      </p>
    );
  }

  const badge = substrateBadge(status);
  const compileDiag = pickPrimaryDiagnostic(status, "for_compile");
  const queryDiag = pickPrimaryDiagnostic(status, "for_query");
  const isPopulated = status?.substrate.kind === "populated";
  const compileButtonLabel = isPopulated ? "Recompile Workspace" : "Compile Workspace";
  const badgeTitle = (() => {
    switch (status?.substrate.kind) {
      case "absent":
        return "No substrate yet — run Compile to initialise.";
      case "empty":
        return "Substrate exists but has no claims yet — add sources and recompile.";
      case "populated":
        return `Substrate ready — ${status.substrate.claim_count} claim(s), ${status.substrate.entity_count} entity(s).`;
      case "orphaned":
        return "Substrate was deleted from disk while the daemon held it open.";
      case "corrupt":
        return `Substrate refused to open: ${status.substrate.reason}`;
      default:
        return "Loading substrate state…";
    }
  })();
  const toneClass = (() => {
    switch (badge.tone) {
      case "ok":
        return "bg-emerald-500/15 text-emerald-400";
      case "warn":
        return "bg-amber-500/15 text-amber-400";
      case "error":
        return "bg-rose-500/15 text-rose-400";
      case "info":
        return "bg-sky-500/15 text-sky-400";
      case "muted":
      default:
        return "bg-muted/40 text-muted-foreground";
    }
  })();

  return (
    <section className="flex flex-col gap-3.5">
      <div className="text-[10px] font-semibold uppercase tracking-widest text-muted-foreground/70">
        Workspace
      </div>
      <div className="flex items-center gap-1.5 text-xs">
        <Folder className="size-3.5 text-muted-foreground" />
        <span className="truncate font-medium">{activeWorkspace}</span>
        <span
          className={cn(
            "ml-auto rounded-full px-1.5 py-0.5 font-mono text-[9px] uppercase tracking-wider",
            toneClass,
          )}
          title={badgeTitle}
        >
          {badge.label}
        </span>
      </div>
      {!conn.connected && conn.lastSeenMs && (
        <p className="text-[10px] text-muted-foreground/80">
          Status disconnected — last seen{" "}
          {Math.round((Date.now() - conn.lastSeenMs) / 1000)}s ago
        </p>
      )}
      {(queryDiag ?? compileDiag) && (
        <p
          className={cn(
            "text-[11px] leading-snug",
            (queryDiag ?? compileDiag)?.severity === "error"
              ? "text-rose-400"
              : (queryDiag ?? compileDiag)?.severity === "warn"
                ? "text-amber-400"
                : "text-muted-foreground",
          )}
          title={(queryDiag ?? compileDiag)?.code}
        >
          {(queryDiag ?? compileDiag)?.message}
        </p>
      )}
      {w && (
        <p className="font-mono text-[10px] text-muted-foreground/80" title={w.path}>
          {w.path.replace(/^\/Users\/[^/]+|^\/home\/[^/]+/, "~")}
        </p>
      )}
      <div className="flex flex-wrap items-center gap-1.5 pt-1">
        <Button
          variant="outline"
          size="sm"
          className="h-8 min-w-[160px] gap-1.5 rounded-xl border-border/70 bg-background/40 px-3 text-xs hover:bg-muted/40"
          disabled={busy}
          onClick={async () => {
            setBusy(true);
            try {
              await workspaceCompile({ target: activeWorkspace });
              toast("Compile queued", {
                kind: "info",
                body: "Progress shown below.",
              });
            } catch (e) {
              toast("Compile failed", {
                kind: "error",
                body: e instanceof Error ? e.message : String(e),
              });
            } finally {
              setBusy(false);
            }
          }}
        >
          <Hammer className="size-3" />
          {compileButtonLabel}
        </Button>
        <Button
          variant="outline"
          size="sm"
          className="h-8 gap-1.5 rounded-xl border-border/70 bg-background/40 px-3 text-xs hover:bg-muted/40"
          type="button"
          onClick={() => setPackExportTarget({ workspace: activeWorkspace })}
        >
          <Package className="size-3" />
          Export .tr
        </Button>
        <Button
          variant="ghost"
          size="sm"
          className="h-8 gap-1 rounded-xl px-2.5 text-[11px] text-muted-foreground hover:text-foreground"
          type="button"
          onClick={() => setRightRailTab("files")}
        >
          <FolderTree className="size-3.5" />
          Files
        </Button>
      </div>
    </section>
  );
}

/**
 * Group an individual `phase` discriminator into the umbrella phase
 * that owns it. Used for ETA bookkeeping: every event in a phase
 * shares one start clock so the ETA reflects elapsed work time, not
 * the gap between two events.
 */
function compilePhaseGroup(phase: string): string {
  if (phase.startsWith("extraction") || phase === "parse_complete") return "extract";
  if (phase.startsWith("grounding")) return "ground";
  if (phase.startsWith("rooting")) return "root";
  if (phase.startsWith("linking")) return "link";
  if (phase.startsWith("vector")) return "index";
  if (phase.startsWith("compilation")) return "compile";
  if (phase.startsWith("diff")) return "diff";
  return phase;
}

function formatEta(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds <= 0) return "";
  if (seconds < 60) return `${Math.round(seconds)}s`;
  const m = Math.floor(seconds / 60);
  const s = Math.round(seconds % 60);
  return `${m}m${s.toString().padStart(2, "0")}s`;
}

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const k = 1024;
  if (n < k * k) return `${(n / k).toFixed(2)} KiB`;
  if (n < k * k * k) return `${(n / (k * k)).toFixed(2)} MiB`;
  return `${(n / (k * k * k)).toFixed(2)} GiB`;
}

/** Discrete segments for the compile “substrate meter” (full-width pixel bar). */
const SUBSTRATE_SEGMENTS = 40;

const TICK_STEPS = [
  "reading",
  "extracting",
  "linking",
  "persisting",
  "packing",
] as const;

function substrateFilledSegments(percent: number): number {
  const p = Number.isFinite(percent) ? Math.min(100, Math.max(0, percent)) : 0;
  return Math.min(
    SUBSTRATE_SEGMENTS,
    Math.max(0, Math.round((p / 100) * SUBSTRATE_SEGMENTS)),
  );
}

function substrateActiveTickBand(progress: CompileProgress): number {
  if (progress.phase !== "tick") return -1;
  const idx = TICK_STEPS.indexOf(progress.step);
  return idx >= 0 ? idx : 0;
}

function substrateSegmentClass(
  lit: boolean,
  isDone: boolean,
  isError: boolean,
  isCancelled: boolean,
  isBand: boolean,
  indeterminateTick: boolean,
  segIndex: number,
  filledCount: number,
): string {
  const tailPulse =
    indeterminateTick &&
    lit &&
    filledCount > 0 &&
    segIndex >= filledCount - 2;
  if (lit) {
    return cn(
      "h-[12px] min-h-[12px] min-w-0 flex-1 rounded-[1px] transition-[background-color,opacity] duration-200 ease-out motion-reduce:transition-none",
      tailPulse && "motion-safe:animate-pulse",
      isDone && "bg-emerald-600 dark:bg-emerald-400",
      isError && "bg-destructive",
      isCancelled && "bg-muted-foreground/75",
      !isDone && !isError && !isCancelled && "bg-primary",
    );
  }
  return cn(
    "h-[12px] min-h-[12px] min-w-0 flex-1 rounded-[1px] transition-colors duration-200 ease-out motion-reduce:transition-none",
    isBand
      ? "bg-muted-foreground/14 dark:bg-muted-foreground/22"
      : "bg-muted/50 dark:bg-muted/40",
  );
}

function SubstrateCompileMeter({
  filledCount,
  progressPercent,
  isDone,
  isError,
  isCancelled,
  activeTickBand,
  indeterminateTick,
  ariaValueText,
}: {
  filledCount: number;
  /** Authoritative 0–100 from the same mapping as the legacy bar (ARIA). */
  progressPercent: number;
  isDone: boolean;
  isError: boolean;
  isCancelled: boolean;
  activeTickBand: number;
  indeterminateTick: boolean;
  ariaValueText: string;
}) {
  const busy = !isDone && !isError && !isCancelled;
  const pct = Math.round(Math.min(100, Math.max(0, progressPercent)));

  return (
    <div
      role="progressbar"
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={pct}
      aria-valuetext={ariaValueText}
      aria-busy={busy}
      className={cn(
        "flex w-full gap-px rounded-[1px] p-px",
        "ring-1 ring-border/60 bg-muted/25 dark:bg-muted/15",
        busy && "motion-safe:shadow-[inset_0_1px_0_rgba(0,0,0,0.06)] dark:motion-safe:shadow-[inset_0_1px_0_rgba(255,255,255,0.04)]",
      )}
    >
      {Array.from({ length: SUBSTRATE_SEGMENTS }, (_, i) => {
        const col = Math.floor(i / (SUBSTRATE_SEGMENTS / TICK_STEPS.length));
        const isBand = activeTickBand >= 0 && col === activeTickBand;
        const lit = i < filledCount;
        return (
          <div
            key={`substrate-seg-${i}`}
            className={substrateSegmentClass(
              lit,
              isDone,
              isError,
              isCancelled,
              isBand,
              indeterminateTick,
              i,
              filledCount,
            )}
          />
        );
      })}
    </div>
  );
}

function CompilationProgressIndicator() {
  const progress = useApp((s) => s.compileProgress);
  const [stopping, setStopping] = useState(false);
  /** Same done/total for a long time while extracting — usually one slow LLM batch, not a frozen UI. */
  const [extractionStalled, setExtractionStalled] = useState(false);
  const extractionSigRef = useRef<string>("");
  const extractionChangeAtRef = useRef<number>(0);
  /**
   * Per-group phase start clock for ETA computation. Reset whenever
   * `compilePhaseGroup(progress.phase)` changes — every new umbrella
   * phase gets a fresh wall clock so the rate calculation reflects
   * the work of that phase only.
   */
  const phaseGroupRef = useRef<string>("");
  const phaseStartedAtRef = useRef<number>(0);
  /** Kept-alive ETA string for the currently-running phase. */
  const [eta, setEta] = useState<string>("");

  useEffect(() => {
    if (!progress || progress.phase !== "extraction_progress") {
      setExtractionStalled(false);
      return;
    }
    const sig = `${progress.done}/${progress.total}`;
    const now = Date.now();
    if (sig !== extractionSigRef.current) {
      extractionSigRef.current = sig;
      extractionChangeAtRef.current = now;
      setExtractionStalled(false);
    }
    const tick = window.setInterval(() => {
      if (Date.now() - extractionChangeAtRef.current > 75_000) {
        setExtractionStalled(true);
      }
    }, 4000);
    return () => window.clearInterval(tick);
  }, [progress]);

  // Track per-phase start time; recompute ETA whenever a counted-progress
  // event arrives. Done/failed/cancelled clear the ETA so a stale value
  // doesn't bleed into the terminal UI.
  useEffect(() => {
    if (!progress) {
      setEta("");
      phaseGroupRef.current = "";
      return;
    }
    if (progress.phase === "done" || progress.phase === "failed" || progress.phase === "cancelled") {
      setEta("");
      return;
    }
    // The unified `tick` event carries an authoritative `eta_ms` from
    // the daemon — trust it and skip our client-side rate guess. The
    // daemon has the full picture (queue depth, batch size, retry
    // backoff) while the client only sees the events.
    if (progress.phase === "tick") {
      if (progress.eta_ms !== null && progress.eta_ms > 0) {
        setEta(formatEta(progress.eta_ms / 1000));
      } else {
        setEta("");
      }
      return;
    }
    const group = compilePhaseGroup(progress.phase);
    if (group !== phaseGroupRef.current) {
      phaseGroupRef.current = group;
      phaseStartedAtRef.current = Date.now();
      setEta("");
      return;
    }
    // Compute ETA only for events that carry done/total — every other
    // phase already telegraphs progress via its own message line.
    if ("done" in progress && "total" in progress && typeof progress.done === "number" && typeof progress.total === "number" && progress.total > 0 && progress.done > 0) {
      const elapsedSec = (Date.now() - phaseStartedAtRef.current) / 1000;
      if (elapsedSec >= 1) {
        const rate = progress.done / elapsedSec;
        const remaining = (progress.total - progress.done) / rate;
        setEta(formatEta(remaining));
      }
    }
  }, [progress]);

  if (!progress) return null;

  async function handleStop() {
    setStopping(true);
    try {
      const ran = await workspaceCompileStop();
      toast(ran ? "Stopping compile…" : "No compile in flight", { kind: "info" });
    } catch (e) {
      toast("Stop failed", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
    } finally {
      setStopping(false);
    }
  }

  let title   = "Compiling…";
  let details = "";
  let percent = 0;
  let isDone  = false;
  let isError = false;
  let isCancelled = false;

  // Tick step → progress-bar range. Matches the legacy per-phase
  // percent stops below so a daemon that emits BOTH event styles
  // doesn't make the bar jump back and forth as events alternate.
  const TICK_RANGE: Record<
    "reading" | "extracting" | "linking" | "persisting" | "packing",
    [number, number]
  > = {
    reading: [5, 18],
    extracting: [20, 60],
    linking: [62, 82],
    persisting: [82, 95],
    packing: [95, 99],
  };

  switch (progress.phase) {
    case "tick": {
      // Unified 250ms-cadence event. This is the **single source of
      // truth** the daemon prefers — every other variant in the union
      // is legacy back-compat. See the type doc in `lib/tauri.ts`.
      const [base, top] = TICK_RANGE[progress.step];
      const range = top - base;
      const stepPct =
        progress.total > 0
          ? Math.floor((progress.done / progress.total) * range)
          : 0;
      percent = base + stepPct;
      title = progress.step_label || progress.step;
      if (progress.total > 0) {
        // Compact `47 / 523` counter — the daemon's own ETA goes in
        // the header next to the percent.
        details = `${progress.done} / ${progress.total}`;
      } else {
        // total === 0 ⇒ indeterminate (e.g. early in extract before
        // the walker has finished enumerating sources). Render
        // elapsed-time so the user sees the engine is alive.
        const sec = (progress.step_elapsed_ms / 1000).toFixed(1);
        details = `${sec}s elapsed · counting sources…`;
      }
      break;
    }
    case "booting":
      title = "Waiting for engine"; details = `Workspace: ${progress.workspace}`; percent = 2; break;
    case "diff_start":
      title = "Diffing workspace state"; details = "Comparing changed and unchanged sources"; percent = 8; break;
    case "diff_complete":
      title = "Diff complete"; details = `${progress.changed} changed · ${progress.unchanged} unchanged · ${progress.deleted} deleted`; percent = 12; break;
    case "started":
      title = "Starting compilation"; details = `Workspace: ${progress.workspace}`; percent = 5; break;
    case "parse_complete":
      title = "Parsing source files"; details = `Parsed ${progress.files} files`; percent = 15; break;
    case "extraction_start":
      title = "Extracting claims";
      details = `Starting ${progress.total_batches} batches · ${progress.total_chunks} chunks total (count jumps when each batch finishes)`;
      percent = 20;
      break;
    case "extraction_progress":
      title = "Extracting claims";
      details = `${progress.done} / ${progress.total} chunks · can sit on one number while a batch runs`;
      percent = 20 + Math.floor((progress.done / Math.max(1, progress.total)) * 30);
      break;
    case "extraction_complete":
      title = "Extraction complete"; details = `${progress.claims} claims, ${progress.entities} entities`; percent = 50; break;
    case "extraction_partial":
      title = "Extraction partially failed"; details = `${progress.failed_batches} failed batches`; percent = 50; break;
    case "grounding_start":
      title = "Grounding claims"; details = `${progress.llm_claims} LLM + ${progress.structural_claims} structural`; percent = 52; break;
    case "grounding_progress":
      title = "Grounding entities"; details = `${progress.done} / ${progress.total}`;
      percent = 50 + Math.floor((progress.done / Math.max(1, progress.total)) * 15); break;
    case "grounding_done":
      title = "Grounding complete"; details = `${progress.accepted} accepted · ${progress.rejected} rejected`; percent = 66; break;
    case "fingerprint_done":
      title = "Fingerprint complete"; details = `${progress.truly_changed} changed · ${progress.cutoffs} cutoffs`; percent = 68; break;
    case "rooting_start":
      title = "Rooting claims"; details = `${progress.candidates} candidates`; percent = 70; break;
    case "rooting_progress":
      title = "Rooting claims"; details = `${progress.done} / ${progress.total}`;
      percent = 70 + Math.floor((progress.done / Math.max(1, progress.total)) * 8); break;
    case "rooting_done":
      title = "Rooting complete"; details = `${progress.rooted} rooted · ${progress.attested} attested`; percent = 78; break;
    case "linking_start":
      title = "Linking knowledge graph"; details = `${progress.total_entities} entities to link`; percent = 65; break;
    case "linking_progress":
      title = "Linking knowledge graph"; details = `${progress.done} / ${progress.total}`;
      percent = 65 + Math.floor((progress.done / Math.max(1, progress.total)) * 15); break;
    case "vector_progress":
      title = "Building vector index"; details = `${progress.done} / ${progress.total}`;
      percent = 80 + Math.floor((progress.done / Math.max(1, progress.total)) * 19); break;
    case "vector_update_done":
      title = "Vector index updated"; details = `${progress.entities_indexed} entities · ${progress.claims_indexed} claims`; percent = 95; break;
    case "compilation_progress":
      title = "Compiling artifacts"; details = `${progress.done} / ${progress.total}`;
      percent = 90 + Math.floor((progress.done / Math.max(1, progress.total)) * 7); break;
    case "compilation_done":
      title = "Artifacts complete"; details = `${progress.artifacts} artifacts`; percent = 98; break;
    case "verification_done":
      title = "Verification complete"; details = `Health ${progress.health}`; percent = 99; break;
    case "phase_done":
      title = "Phase complete"; details = `${progress.name} in ${progress.elapsed_ms}ms`; percent = 99; break;
    case "cancelled":
      title = "Compilation stopped"; details = "Stopped by user";
      percent = 100; isCancelled = true; break;
    case "done":
      title = "Compilation complete"; details = `${progress.claims} claims, ${progress.entities} entities`;
      percent = 100; isDone = true; break;
    case "failed":
      title = "Compilation failed"; details = progress.error;
      percent = 100; isError = true; break;
  }

  const filledCount = substrateFilledSegments(percent);
  const activeTickBand = substrateActiveTickBand(progress);
  const indeterminateTick =
    progress.phase === "tick" &&
    progress.total === 0 &&
    !isDone &&
    !isError &&
    !isCancelled;
  const meterAriaText = `${title}. ${details || `${Math.round(percent)}%`}`;

  return (
    <section
      role="status"
      aria-live="polite"
      aria-label="Compile progress"
      className="-mx-4 flex flex-col gap-2 border-y border-border/60 bg-muted/[0.06] px-4 py-3 dark:bg-muted/10"
    >
      <header className="flex min-w-0 items-start gap-2">
        <div className="mt-0.5 shrink-0">
          {isDone ? (
            <CheckCircle2
              className="size-4 text-emerald-600 dark:text-emerald-400"
              aria-hidden
            />
          ) : isError ? (
            <AlertCircle className="size-4 text-destructive" aria-hidden />
          ) : (
            <Loader2
              className="size-4 animate-spin text-primary motion-reduce:animate-none"
              aria-hidden
            />
          )}
        </div>
        <div className="min-w-0 flex-1">
          <div className="flex min-w-0 items-baseline justify-between gap-2">
            <h3 className="truncate font-mono text-[11px] font-medium leading-tight text-foreground">
              {title}
            </h3>
            {!isDone && !isError && !isCancelled && (
              <div className="flex shrink-0 items-center gap-1">
                <span className="whitespace-nowrap font-mono text-[10px] tabular-nums text-muted-foreground">
                  {eta ? (
                    <>
                      <span className="text-foreground">{Math.round(percent)}%</span>
                      <span> · </span>
                      <span className="text-primary">ETA {eta}</span>
                    </>
                  ) : (
                    <span className="text-foreground">{Math.round(percent)}%</span>
                  )}
                </span>
                <Button
                  variant="ghost"
                  size="icon"
                  onClick={handleStop}
                  disabled={stopping}
                  aria-label="Stop compile"
                  title="Stop compile"
                  className="h-6 w-6 shrink-0 text-muted-foreground hover:text-destructive"
                >
                  {stopping ? (
                    <Loader2 className="size-3.5 animate-spin" aria-hidden />
                  ) : (
                    <Square className="size-3.5 fill-current" aria-hidden />
                  )}
                </Button>
              </div>
            )}
          </div>
        </div>
      </header>

      <SubstrateCompileMeter
        filledCount={filledCount}
        progressPercent={percent}
        isDone={isDone}
        isError={isError}
        isCancelled={isCancelled}
        activeTickBand={activeTickBand}
        indeterminateTick={indeterminateTick}
        ariaValueText={meterAriaText}
      />

      {progress.phase === "tick" && (
        <div className="flex justify-between gap-0.5 font-mono text-[8px] uppercase tracking-wide text-muted-foreground/80">
          {TICK_STEPS.map((step, i) => (
            <span
              key={step}
              className={cn(
                "min-w-0 flex-1 truncate text-center leading-none",
                i === activeTickBand && "font-semibold text-foreground",
              )}
              title={step}
            >
              {step === "reading"
                ? "read"
                : step === "extracting"
                  ? "extr"
                  : step === "linking"
                    ? "link"
                    : step === "persisting"
                      ? "save"
                      : "pkg"}
            </span>
          ))}
        </div>
      )}

      <p
        className="font-mono text-[10px] leading-relaxed text-muted-foreground"
        title={details}
      >
        {details}
      </p>
      {progress.phase === "extraction_progress" &&
        extractionStalled &&
        !isDone &&
        !isError &&
        !isCancelled && (
          <p className="text-[10px] leading-snug text-amber-700 dark:text-amber-300/95">
            Over 75s on this count — likely a slow or stuck LLM call. Stop
            (■) aborts the run; check Settings → Credentials and provider
            limits.
          </p>
        )}
      {progress.phase === "done" &&
        progress.failed_batches !== undefined &&
        progress.failed_batches > 0 && (
          <p className="text-[10px] leading-snug text-amber-700 dark:text-amber-300/95">
            {progress.failed_batches} LLM batches failed permanently — knowledge
            graph is partial.
          </p>
        )}
      {progress.phase === "done" && progress.incremental_summary && (
        <CompileSummaryPanel summary={progress.incremental_summary} />
      )}
    </section>
  );
}

/**
 * Per-phase timing breakdown rendered after a successful compile.
 *
 * Mirrors the CLI `summary_printer` output (canonical PHASE_NAMES order)
 * so the user sees the same data on both surfaces. Only phases with
 * non-zero elapsed are shown — a fingerprint-cutoff steady-state run
 * legitimately has only `diff` and `audit` populated.
 */
const PHASE_DISPLAY_ORDER = [
  "diff",
  "extract",
  "ground",
  "fingerprint",
  "remove_sources",
  "entity_relations",
  "link",
  "structural_persist",
  "audit",
  "other",
];

function CompileSummaryPanel({ summary }: { summary: IncrementalSummary }) {
  const orderedPhases = PHASE_DISPLAY_ORDER.filter(
    (name) => (summary.phase_timings[name] ?? 0) > 0,
  );
  const totalSec = (summary.total_elapsed_ms / 1000).toFixed(1);
  return (
    <div className="mt-2 border-t border-border/55 pt-2 font-mono text-[10px] leading-relaxed text-muted-foreground">
      <p className="text-foreground/90">
        <span className="tabular-nums">{totalSec}s</span>
        <span className="text-muted-foreground"> total · </span>
        <span>
          {summary.sources_truly_changed}/{summary.sources_total} sources
        </span>
        <span className="text-muted-foreground"> · </span>
        <span>
          +{summary.claims_added} −{summary.claims_deleted} claims
        </span>
        {summary.llm_calls > 0 && (
          <>
            <span className="text-muted-foreground"> · </span>
            <span>{summary.llm_calls} LLM calls</span>
          </>
        )}
        {summary.cache_hits > 0 && (
          <>
            <span className="text-muted-foreground"> · </span>
            <span>{summary.cache_hits} cache hits</span>
          </>
        )}
        {summary.bytes_re_extracted > 0 && (
          <>
            <span className="text-muted-foreground"> · </span>
            <span>{formatBytes(summary.bytes_re_extracted)} re-extracted</span>
          </>
        )}
      </p>
      {orderedPhases.length > 0 && (
        <p className="mt-1.5 break-words text-[9px] leading-snug text-muted-foreground/85">
          {orderedPhases.map((name) => (
            <span key={name} className="mr-2 inline-block">
              {name}
              <span className="text-foreground/90">
                {" "}
                {(summary.phase_timings[name] ?? 0)}ms
              </span>
            </span>
          ))}
        </p>
      )}
    </div>
  );
}
