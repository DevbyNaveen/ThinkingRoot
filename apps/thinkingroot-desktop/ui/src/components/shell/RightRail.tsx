/**
 * Right panel — Cursor-style tabbed inspector with drag-to-resize.
 *
 * Tab bar (top, icon-only):
 *   Hammer  → Compile   (workspace card + live compile progress + branch graph)
 *   FolderTree → Workspace (readme + folder tree in one panel; readme default)
 *   KnowledgeMark → Knowledge (custom substrate glyph, not Lucide stock)
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
import { KnowledgeMark } from "@/components/shell/KnowledgeMark";
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
  workspaceCompileStatus,
  workspaceCompileStop,
  workspaceList,
  type CompileProgress,
  type IncrementalSummary,
  type WorkspaceView,
} from "@/lib/tauri";
import {
  pickPrimaryDiagnostic,
  substrateBadge,
  SUBSTRATE_BADGE_SURFACE_CLASS,
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
  { id: "brain", Icon: KnowledgeMark, label: "Knowledge" },
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
        <header className="flex h-9 w-full items-center justify-center">
          <Button
            variant="ghost"
            size="icon"
            onClick={toggle}
            aria-label="Open panel"
            className="h-6 w-6"
          >
            <PanelRight className="size-3" />
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
      <header className="flex h-9 shrink-0 items-center border-b border-border py-0 pl-1.5 pr-0.5">
        <nav className="flex flex-1 items-center gap-px" aria-label="Panel tabs">
          {TABS.map(({ id, Icon, label }) => (
            <button
              key={id}
              type="button"
              onClick={() => setTab(id)}
              title={label}
              aria-label={label}
              aria-pressed={activeTab === id}
              className={cn(
                "flex h-6 w-6 items-center justify-center rounded-md transition-colors",
                activeTab === id
                  ? "bg-muted text-foreground"
                  : "text-muted-foreground/60 hover:bg-muted/50 hover:text-foreground",
              )}
            >
              <Icon
                className="size-3"
                strokeWidth={activeTab === id ? 2 : 1.5}
              />
            </button>
          ))}
        </nav>

        <Button
          variant="ghost"
          size="icon"
          onClick={toggle}
          aria-label="Close panel"
          className="h-6 w-6 shrink-0 text-muted-foreground/60 hover:text-foreground"
        >
          <PanelRight className="size-3" />
        </Button>
      </header>

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
    <div className="flex flex-col gap-4 overflow-y-auto px-3 py-4">
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
  const compileButtonShort = isPopulated ? "Recompile" : "Compile";
  const badgeTitle = (() => {
    switch (status?.substrate.kind) {
      case "absent":
        return "Behind — no substrate yet. Run Compile to initialise.";
      case "empty":
        return "Behind — substrate exists but has no claims yet. Add sources and recompile.";
      case "populated":
        return `Up to date — ${status.substrate.claim_count} claim(s), ${status.substrate.entity_count} entity(s).`;
      case "orphaned":
        return "Behind — substrate was removed on disk while the daemon still had it open.";
      case "corrupt":
        return `Substrate refused to open: ${status.substrate.reason}`;
      default:
        return "Loading substrate state…";
    }
  })();
  const shortPath = w?.path.replace(/^\/Users\/[^/]+|^\/home\/[^/]+/, "~");

  return (
    <section className="flex flex-col gap-2 border-b border-border/30 pb-3">
      <div className="min-w-0 space-y-0.5">
        <div className="flex items-center gap-2">
          <h3
            className="min-w-0 flex-1 truncate text-[13px] font-semibold leading-tight tracking-tight text-foreground"
            title={activeWorkspace}
          >
            {activeWorkspace}
          </h3>
          <span
            className={cn(
              "shrink-0 px-1.5 py-px font-mono text-[9px] tracking-wide normal-case",
              SUBSTRATE_BADGE_SURFACE_CLASS,
            )}
            title={badgeTitle}
          >
            {badge.label}
          </span>
        </div>
        {shortPath ? (
          <p
            className="truncate font-mono text-[10px] leading-tight text-muted-foreground/75"
            title={w?.path}
          >
            {shortPath}
          </p>
        ) : null}
      </div>
      {(queryDiag ?? compileDiag) ? (
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
      ) : null}
      <div className="flex flex-wrap items-center gap-1 pt-0.5">
        <Button
          variant="default"
          size="sm"
          className="h-7 shrink-0 gap-1 rounded-md border-0 bg-white px-2.5 text-[11px] font-medium text-neutral-950 shadow-none hover:bg-white/90"
          disabled={busy}
          title={compileButtonLabel}
          onClick={async () => {
            setBusy(true);
            try {
              // Pre-flight: ask the Tauri side whether a compile is
              // already running. Pre-fix this was skipped, so a
              // stale slot (event drop, prior crash) surfaced as a
              // hard-error toast on click. Now we surface a soft
              // warning the user can act on.
              const status = await workspaceCompileStatus();
              if (status.running && status.workspace !== activeWorkspace) {
                toast("Compile already running", {
                  kind: "warn",
                  body: `Another workspace (${
                    status.workspace ?? "unknown"
                  }) is being compiled. Stop it first or wait for it to finish.`,
                });
                return;
              }
              await workspaceCompile({ target: activeWorkspace });
              // Pre-fix: "Compile queued" — there is no queue; the
              // single slot either accepts the click or rejects it.
              // The honest message reflects what actually happened.
              toast("Compile started", {
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
          {busy ? (
            <Loader2 className="size-3 animate-spin" aria-hidden />
          ) : (
            <Hammer className="size-3 opacity-90" aria-hidden />
          )}
          {compileButtonShort}
        </Button>
        <Button
          variant="ghost"
          size="sm"
          className="h-7 shrink-0 gap-1 rounded-md px-2 text-[11px] font-medium text-muted-foreground hover:bg-muted/50 hover:text-foreground"
          type="button"
          title="Export workspace as .tr pack"
          onClick={() => setPackExportTarget({ workspace: activeWorkspace })}
        >
          <Package className="size-3 opacity-80" aria-hidden />
          Export .tr
        </Button>
        <Button
          variant="ghost"
          size="sm"
          className="h-7 shrink-0 gap-1 rounded-md px-2 text-[11px] font-medium text-muted-foreground hover:bg-muted/50 hover:text-foreground"
          type="button"
          title="Open readme and folder inspector"
          onClick={() => setRightRailTab("files")}
        >
          <FolderTree className="size-3 opacity-80" aria-hidden />
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

/**
 * Render the daemon's `CompileTick.step_elapsed_ms` (a u64 ms value
 * that resets at every step boundary) as a compact running counter
 * for the indeterminate-sub-phase fallback in the ETA slot. Differs
 * from {@link formatEta}: this is "time spent in this step so far",
 * not "estimated time remaining". Format mirrors `formatEta` so the
 * two read consistently when they alternate on the same compile
 * (1–59s ⇒ `Ns`, 60s+ ⇒ `MmSSs`).
 */
function formatStepElapsed(ms: number): string {
  if (!Number.isFinite(ms) || ms <= 0) return "";
  const seconds = Math.floor(ms / 1000);
  if (seconds < 60) return `${seconds}s`;
  const m = Math.floor(seconds / 60);
  const s = seconds % 60;
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
      // `bg-accent` (not `bg-primary`). The Tailwind config
      // (`apps/thinkingroot-desktop/ui/tailwind.config.ts`) defines
      // only `accent` / `destructive` / `success` / `warn` / `info`
      // and the muted/foreground neutrals — there is no `primary`
      // colour in the palette. Pre-fix `bg-primary` resolved to no
      // CSS at all, so the "lit" segments rendered as transparent
      // boxes over the bar track and the bar looked empty even at
      // 99% filled (visible in the 01:28 screenshot from 2026-05-18).
      !isDone && !isError && !isCancelled && "bg-accent",
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
  /**
   * Monotonic-max percent guard. The compile pipeline's user-visible
   * step labels (`Reading`/`Extracting`/`Linking`/`Persisting`) don't
   * map monotonically onto pipeline-internal phase order — Phase 4
   * source-removal lives under `Persisting` (82-95% band) but happens
   * BEFORE Phase 5 entity-relations (under `Linking`, 62-82% band).
   * Without clamping, the bar would swing backward 33% twice per
   * incremental compile. The substep caption (`progress.detail`)
   * gives the user accurate "what's happening now" feedback; the bar
   * just shows monotonic "fraction of work done."
   *
   * Reset on a new compile (`started`) or on an auto-retry boundary
   * (`retrying`) — the retry's fresh Reading-5% tick is a genuine
   * progress reset, not a backward jump.
   */
  const maxPercentRef = useRef<number>(0);

  /**
   * Monotonic-max step-band guard. The daemon's `set_step` calls
   * aren't monotonic (Phase 5 = Linking, Phase 6 = Persisting,
   * Phase 7 = Linking again) — see pipeline.rs:1060/1151/1406. Without
   * clamping, the displayed label flips `Linking → Persisting →
   * Linking` even though the percent bar moves forward monotonically.
   * The user complaint pre-2026-05-18 was "label flickers between
   * Linking and Persisting"; this ref pins the label to its
   * high-water mark so the visible state machine reads
   * `Reading → Extracting → Linking → Persisting → Packing` linearly.
   *
   * The index value is into `STEP_ORDER`; 0 = reading, 4 = packing.
   * Reset on `started` / `retrying`, same triggers as `maxPercentRef`.
   */
  const maxStepBandRef = useRef<number>(0);

  // Reset the monotonic-max trackers on compile start + retry boundary.
  // Done/Failed/Cancelled don't reset — the terminal-state percent
  // (100%) is always >= max so clamping is a no-op.
  useEffect(() => {
    if (!progress) {
      maxPercentRef.current = 0;
      maxStepBandRef.current = 0;
      return;
    }
    if (progress.phase === "started" || progress.phase === "retrying") {
      maxPercentRef.current = 0;
      maxStepBandRef.current = 0;
    }
  }, [progress?.phase]);

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

  // Tick step → progress-bar range. The step taxonomy is linear; the
  // band ordering is the source of truth for the step-label monotonic
  // clamp below.
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

  /** Ordering of step bands. Index N+1 is "further along" than N. */
  const STEP_ORDER = [
    "reading",
    "extracting",
    "linking",
    "persisting",
    "packing",
  ] as const;
  type StepName = (typeof STEP_ORDER)[number];

  switch (progress.phase) {
    case "tick": {
      // Unified 250ms-cadence event. This is the **single source of
      // truth** the daemon prefers — every other variant in the union
      // is legacy back-compat. See the type doc in `lib/tauri.ts`.
      //
      // Step-label monotonic clamp: the daemon's `set_step` calls
      // aren't monotonic (Phase 5 = Linking, Phase 6 = Persisting,
      // Phase 7 = Linking again). If we displayed the raw step the
      // label would flip `Linking → Persisting → Linking` even
      // though the percent bar moves forward monotonically. Clamp
      // the displayed step to its high-water mark so the label
      // sequence is always `Reading → Extracting → Linking →
      // Persisting → Packing` linearly. The percent bar already has
      // its own `maxPercentRef` clamp below.
      const stepIdx = STEP_ORDER.indexOf(progress.step);
      if (stepIdx > maxStepBandRef.current) {
        maxStepBandRef.current = stepIdx;
      }
      // Bound the displayed index into the valid range [0, 4].
      const clampedIdx = Math.max(
        0,
        Math.min(STEP_ORDER.length - 1, maxStepBandRef.current),
      );
      const displayStep: StepName = STEP_ORDER[clampedIdx] as StepName;
      const [base, top] = TICK_RANGE[displayStep];
      const range = top - base;
      const stepPct =
        progress.total > 0 && progress.step === displayStep
          ? Math.floor((progress.done / progress.total) * range)
          : 0;
      percent = base + stepPct;
      // Use the displayed (clamped) step name. `step_label` from the
      // daemon corresponds to the RAW step — substitute it only when
      // raw == clamped so we never show "Linking" while clamped to
      // Persisting.
      title =
        progress.step === displayStep
          ? progress.step_label || displayStep
          : displayStep;
      if (progress.total > 0 && progress.step === displayStep) {
        details = `${progress.done} / ${progress.total}`;
      } else {
        const sec = (progress.step_elapsed_ms / 1000).toFixed(1);
        const sub = progress.detail || displayStep;
        details = `${sec}s elapsed · ${sub}`;
      }
      break;
    }
    case "booting":
      title = "Waiting for engine"; details = `Workspace: ${progress.workspace}`; percent = 2; break;
    case "connecting":
      // Bridges the Started→first-tick window so the bar shows
      // movement during the HTTP POST + server-side pipeline setup.
      // Monotonic-max keeps this from reading as a backward jump
      // when Started already painted 5%.
      title = "Connecting to engine";
      details = "Waiting for first progress event…";
      percent = 4;
      break;
    case "retrying":
      // Auto-retry boundary — the monotonic-max effect above has
      // already reset the tracker on this phase. The bar resets to 3%
      // and climbs again as the retry's pipeline emits tick events.
      title = `Retrying compile (attempt ${progress.attempt + 1}/2)`;
      details = `After ${(progress.after_ms / 1000).toFixed(1)}s — ${progress.first_error}`;
      percent = 3;
      break;
    case "started":
      title = "Starting compilation"; details = `Workspace: ${progress.workspace}`; percent = 5; break;
    // ── Legacy per-phase events (2026-05-18 cleanup) ─────────────────
    // The 20 legacy ProgressEvent variants below — diff_*, parse_complete,
    // extraction_*, grounding_*, fingerprint_done, rooting_*, linking_*,
    // vector_*, compilation_*, verification_done — each set their own
    // title / details / percent and competed with the canonical `tick`
    // event for the bar label. The daemon still emits them for backward
    // compatibility with editor MCP consumers; the desktop UI now treats
    // them as no-ops and waits for the next CompileTick to paint
    // authoritative state on its own 250 ms cadence.
    case "diff_start":
    case "diff_complete":
    case "parse_complete":
    case "extraction_start":
    case "extraction_progress":
    case "extraction_complete":
    case "extraction_partial":
    case "grounding_start":
    case "grounding_progress":
    case "grounding_done":
    case "fingerprint_done":
    case "rooting_start":
    case "rooting_progress":
    case "rooting_done":
    case "linking_start":
    case "linking_progress":
    case "vector_progress":
    case "vector_update_done":
    case "compilation_progress":
    case "compilation_done":
    case "verification_done":
      title = "Compiling…";
      details = "";
      percent = maxPercentRef.current;
      break;
    case "phase_done":
      // `phase_done` fires after **every** internal pipeline phase
      // (parse → diff → extract → link → persist → audit → …). It is
      // an informational event, NOT a positional one. Pre-fix this
      // set `percent = 99`, which combined with the monotonic-max
      // clamp below pegged the bar to 99% from the moment the FIRST
      // phase (usually `parse` at real 5% progress) completed —
      // making the bar useless for the remaining ~95% of the run.
      // Leave the bar position alone; the next `tick` event will
      // re-derive percent from the current step's band.
      title = "Phase complete";
      details = `${progress.name} in ${progress.elapsed_ms}ms`;
      percent = maxPercentRef.current;
      break;
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

  // Monotonic-max clamp: during in-flight phases, the bar never goes
  // below its high-water mark. Terminal states (done/failed/cancelled)
  // use 100% directly. The displayed percent drives both `filledCount`
  // and the ARIA `aria-valuenow` so screen readers and sighted users
  // see the same number. Mutating the ref in render is safe here
  // because we read it BEFORE the write and the displayed value is a
  // pure function of (ref-before, current-percent).
  const inFlight = !isDone && !isError && !isCancelled;
  const displayPercent = inFlight
    ? Math.max(maxPercentRef.current, percent)
    : percent;
  if (inFlight && displayPercent > maxPercentRef.current) {
    maxPercentRef.current = displayPercent;
  }

  const filledCount = substrateFilledSegments(displayPercent);
  const activeTickBand = substrateActiveTickBand(progress);
  const indeterminateTick =
    progress.phase === "tick" &&
    progress.total === 0 &&
    !isDone &&
    !isError &&
    !isCancelled;
  const meterAriaText = `${title}. ${details || `${Math.round(displayPercent)}%`}`;

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
                  <span className="text-foreground">{Math.round(displayPercent)}%</span>
                  {eta ? (
                    // Daemon supplied a known ETA — sub-phase has
                    // total > 0 and at least one tick of progress to
                    // extrapolate from.
                    <>
                      <span> · </span>
                      <span className="text-accent">ETA {eta}</span>
                    </>
                  ) : progress.phase === "tick" && progress.step_elapsed_ms > 0 ? (
                    // Indeterminate sub-phase (`updating relations`,
                    // `synthesizing paper`, etc. — daemon's total is
                    // 0 so it can't divide). Show a live counter of
                    // step-local elapsed seconds so the user sees the
                    // clock tick rather than a frozen percentage with
                    // no time signal. Matches the daemon's
                    // `step_elapsed_ms` semantics (resets at each
                    // step boundary) so the counter restarts when
                    // the bar enters a new step band.
                    <>
                      <span> · </span>
                      <span className="text-muted-foreground">
                        {formatStepElapsed(progress.step_elapsed_ms)}
                      </span>
                    </>
                  ) : null}
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
        progressPercent={displayPercent}
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
      {progress.phase === "retrying" && (
        <p className="text-[10px] leading-snug text-amber-700 dark:text-amber-300/95">
          First attempt failed — retrying once before giving up. Click Stop
          (■) to bail out now, or wait for the retry to complete.
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
