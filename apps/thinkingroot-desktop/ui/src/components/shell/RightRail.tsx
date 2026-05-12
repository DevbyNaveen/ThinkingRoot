/**
 * Right panel — Cursor-style tabbed inspector with drag-to-resize.
 *
 * Tab bar (top, icon-only):
 *   Hammer  → Compile   (workspace card + live compile progress)
 *   FolderTree → Files (project + .thinkingroot tree, preview, pack export)
 *   Cpu     → Brain     (BrainView in panel mode)
 *   GitBranch → Branches (BranchesView in panel mode)
 *   Code2   → Builders  (workspace backend connect surface)
 *   Globe2  → Browser   (manual web browser)
 *   ShieldCheck → Privacy (PrivacyDashboard in panel mode)
 *
 * The left edge has an invisible drag handle that lets the user
 * resize the panel (min 220px, max 600px). Width is persisted in
 * the app store so it survives reloads.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import {
  PanelRight,
  GitBranch,
  RefreshCw,
  Folder,
  Hammer,
  GitMerge,
  ShieldCheck,
  CheckCircle2,
  AlertCircle,
  Loader2,
  Square,
  Cpu,
  Code2,
  BookOpen,
  FolderTree,
  Package,
  Globe2,
  Terminal as TerminalIcon,
} from "lucide-react";

import { cn } from "@/lib/utils";
import { useApp } from "@/store/app";
import { Button } from "@/components/ui/button";
import { toast } from "@/store/toast";
import { BrainView } from "@/components/brain/BrainView";
import { BranchesView } from "@/components/branches/BranchesView";
import { PrivacyDashboard } from "@/components/privacy/PrivacyDashboard";
import { ReadmeView } from "@/components/readme/ReadmeView";
import { WorkspaceFilesPanel } from "@/components/shell/WorkspaceFilesPanel";
import { BrowserPanel } from "@/components/browser/BrowserPanel";
import { TerminalPanel } from "@/components/terminal/TerminalPanel";
import { BuildersPanel } from "@/components/builders/BuildersPanel";
import {
  branchCheckout,
  branchList,
  workspaceCompile,
  workspaceCompileStop,
  workspaceList,
  type BranchView,
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
  { id: "compile",  Icon: Hammer,       label: "Compile"  },
  { id: "files",    Icon: FolderTree,   label: "Files"    },
  { id: "brain",    Icon: Cpu,          label: "Brain"    },
  { id: "readme",   Icon: BookOpen,     label: "Readme"   },
  { id: "branches", Icon: GitBranch,    label: "Branches" },
  { id: "builders", Icon: Code2,        label: "Builders" },
  { id: "browser",  Icon: Globe2,       label: "Browser"  },
  { id: "terminal", Icon: TerminalIcon, label: "Terminal" },
  { id: "privacy",  Icon: ShieldCheck,  label: "Privacy"  },
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
              <Icon className="size-3.5" />
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
        {activeTab === "readme" && (
          <div className="flex-1 overflow-hidden">
            <ReadmeView panelMode />
          </div>
        )}
        {activeTab === "branches" && (
          <div className="flex-1 overflow-hidden">
            <BranchesView panelMode />
          </div>
        )}
        {activeTab === "builders" && (
          <BuildersPanel activeWorkspace={activeWorkspace} />
        )}
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
      {activeWorkspace && <BranchPanel workspace={activeWorkspace} />}
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

function BranchPanel({ workspace }: { workspace: string }) {
  const [branches, setBranches] = useState<BranchView[]>([]);
  const [loading, setLoading]   = useState(false);
  const [error, setError]       = useState<string | null>(null);

  async function load() {
    setLoading(true);
    setError(null);
    try {
      setBranches(await branchList(workspace));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => { void load(); }, [workspace]);

  return (
    <section className="flex flex-col gap-2.5">
      <header className="flex items-center gap-1.5 text-xs">
        <GitBranch className="size-3.5 text-muted-foreground" />
        <h3 className="font-medium">Branches</h3>
        <Button
          variant="ghost"
          size="icon"
          className="ml-auto h-5 w-5"
          onClick={load}
          aria-label="Reload"
        >
          <RefreshCw className={loading ? "size-3 animate-spin" : "size-3"} />
        </Button>
      </header>
      {error && <p className="text-[11px] text-destructive">{error}</p>}
      {!error && branches.length === 0 && !loading && (
        <p className="text-[10px] text-muted-foreground">
          No branches yet. Use{" "}
          <code className="font-mono">/branch &lt;name&gt;</code> in chat to fork.
        </p>
      )}
      <ul className="flex flex-col gap-0.5">
        {branches.map((b) => (
          <li key={b.name}>
            <button
              type="button"
              onClick={async () => {
                try {
                  await branchCheckout(workspace, b.name);
                  toast(`HEAD → ${b.name}`, { kind: "success" });
                  await load();
                } catch (e) {
                  toast("Checkout failed", {
                    kind: "error",
                    body: e instanceof Error ? e.message : String(e),
                  });
                }
              }}
              className={cn(
                "flex w-full items-center gap-1.5 rounded-xl px-2.5 py-2 text-left text-[11px] transition-colors",
                b.current
                  ? "bg-accent/12 text-accent"
                  : "text-foreground hover:bg-muted/35",
              )}
              title={b.description ?? b.name}
            >
              {b.current ? (
                <GitMerge className="size-3 shrink-0 text-accent" />
              ) : (
                <GitBranch className="size-3 shrink-0 text-muted-foreground" />
              )}
              <span className="truncate font-mono">{b.name}</span>
              <span className="ml-auto font-mono text-[9px] uppercase text-muted-foreground/70">
                {b.status}
              </span>
            </button>
          </li>
        ))}
      </ul>
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

  return (
    <section className="relative flex flex-col gap-2.5 overflow-hidden rounded-xl bg-muted/15 p-3">
      <div className="pointer-events-none absolute inset-0 bg-gradient-to-br from-accent/5 to-transparent" />
      <header className="relative z-10 flex items-center gap-2 text-xs">
        {isDone ? (
          <CheckCircle2 className="size-4 text-emerald-500" />
        ) : isError ? (
          <AlertCircle className="size-4 text-destructive" />
        ) : (
          <Loader2 className="size-4 animate-spin text-accent" />
        )}
        <h3 className="font-medium tracking-tight text-foreground">{title}</h3>
        {!isDone && !isError && !isCancelled && (
          <>
            <span className="ml-auto font-mono text-[9px] font-medium text-accent">
              {eta ? `${percent}% · ETA ${eta}` : `${percent}%`}
            </span>
            <Button
              variant="ghost"
              size="icon"
              onClick={handleStop}
              disabled={stopping}
              aria-label="Stop compile"
              title="Stop compile"
              className="h-5 w-5 text-muted-foreground hover:text-destructive"
            >
              {stopping ? <Loader2 className="size-3 animate-spin" /> : <Square className="size-3" />}
            </Button>
          </>
        )}
      </header>
      <div className="relative z-10 mt-1 flex flex-col gap-1.5">
        <div className="h-1.5 w-full overflow-hidden rounded-full bg-muted/50">
          <div
            className={cn(
              "h-full transition-all duration-300 ease-out",
                isDone
                  ? "bg-emerald-500"
                  : isError
                    ? "bg-destructive"
                    : isCancelled
                      ? "bg-muted-foreground"
                      : "bg-accent",
            )}
            style={{ width: `${percent}%` }}
          />
        </div>
        <p className="mt-0.5 text-[10px] leading-snug text-muted-foreground" title={details}>
          {details}
        </p>
        {progress.phase === "extraction_progress" && extractionStalled && !isDone && !isError && !isCancelled && (
          <p className="text-[10px] leading-snug text-amber-200/90">
            Over 75s on this count — likely a slow or stuck LLM call. Stop (■) aborts the run; check Settings → Credentials and provider
            limits.
          </p>
        )}
        {progress.phase === "done" && progress.failed_batches !== undefined && progress.failed_batches > 0 && (
          <p className="text-[10px] leading-snug text-amber-200/90">
            ⚠ {progress.failed_batches} LLM batches failed permanently — knowledge graph is partial.
          </p>
        )}
        {progress.phase === "done" && progress.incremental_summary && (
          <CompileSummaryPanel summary={progress.incremental_summary} />
        )}
      </div>
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
    <div className="mt-1.5 flex flex-col gap-1 rounded-md border border-border/40 bg-muted/20 p-2">
      <div className="flex flex-wrap items-baseline gap-x-2 gap-y-0.5 text-[10px] text-muted-foreground">
        <span className="font-mono text-foreground">{totalSec}s</span>
        <span>· {summary.sources_truly_changed}/{summary.sources_total} sources</span>
        <span>· +{summary.claims_added} −{summary.claims_deleted} claims</span>
        {summary.llm_calls > 0 && <span>· {summary.llm_calls} LLM calls</span>}
        {summary.cache_hits > 0 && <span>· {summary.cache_hits} cache hits</span>}
        {summary.bytes_re_extracted > 0 && (
          <span>· {formatBytes(summary.bytes_re_extracted)} re-extracted</span>
        )}
      </div>
      {orderedPhases.length > 0 && (
        <div className="flex flex-wrap gap-x-2 gap-y-0.5 font-mono text-[9px] text-muted-foreground/80">
          {orderedPhases.map((name) => (
            <span key={name}>
              {name} <span className="text-foreground">{(summary.phase_timings[name] ?? 0)}ms</span>
            </span>
          ))}
        </div>
      )}
    </div>
  );
}
