/**
 * Right panel — Cursor-style tabbed inspector with drag-to-resize.
 *
 * Tab bar (top, icon-only):
 *   Hammer  → Compile   (workspace card + live compile progress)
 *   FolderTree → Files (project + .thinkingroot tree, preview, pack export)
 *   Cpu     → Brain     (BrainView in panel mode)
 *   GitBranch → Branches (BranchesView in panel mode)
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
  BookOpen,
  FolderTree,
  Package,
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
import {
  branchCheckout,
  branchList,
  workspaceCompile,
  workspaceCompileStop,
  workspaceList,
  type BranchView,
  type WorkspaceView,
} from "@/lib/tauri";
import type { RightRailTab } from "@/types";

const MIN_WIDTH = 250;
const MAX_WIDTH = 800;
const DEFAULT_WIDTH = 450;

const TABS: { id: RightRailTab; Icon: React.ElementType; label: string }[] = [
  { id: "compile", Icon: Hammer,       label: "Compile"  },
  { id: "files",   Icon: FolderTree,   label: "Files"    },
  { id: "brain",   Icon: Cpu,          label: "Brain"    },
  { id: "readme",  Icon: BookOpen,     label: "Readme"   },
  { id: "branches",Icon: GitBranch,    label: "Branches" },
  { id: "privacy", Icon: ShieldCheck,  label: "Privacy"  },
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
        {activeTab === "brain" && (
          <div className="flex-1 overflow-hidden">
            <BrainView panelMode />
          </div>
        )}
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
        {activeTab === "privacy" && (
          <div className="flex-1 overflow-hidden">
            <PrivacyDashboard panelMode />
          </div>
        )}
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

  return (
    <section className="flex flex-col gap-3.5">
      <div className="text-[10px] font-semibold uppercase tracking-widest text-muted-foreground/70">
        Workspace
      </div>
      <div className="flex items-center gap-1.5 text-xs">
        <Folder className="size-3.5 text-muted-foreground" />
        <span className="truncate font-medium">{activeWorkspace}</span>
        {w?.compiled ? (
          <span className="ml-auto rounded-full bg-emerald-500/15 px-1.5 py-0.5 font-mono text-[9px] uppercase tracking-wider text-emerald-400">
            compiled
          </span>
        ) : (
          <span className="ml-auto rounded-full bg-amber-500/15 px-1.5 py-0.5 font-mono text-[9px] uppercase tracking-wider text-amber-400">
            pending
          </span>
        )}
      </div>
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
          {w?.compiled ? "Recompile Workspace" : "Compile Workspace"}
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

function CompilationProgressIndicator() {
  const progress = useApp((s) => s.compileProgress);
  const [stopping, setStopping] = useState(false);
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

  switch (progress.phase) {
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
      title = "Extracting claims"; details = `Starting ${progress.total_batches} batches`; percent = 20; break;
    case "extraction_progress":
      title = "Extracting claims"; details = `${progress.done} / ${progress.total} chunks`;
      percent = 20 + Math.floor((progress.done / Math.max(1, progress.total)) * 30); break;
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
            <span className="ml-auto font-mono text-[9px] font-medium text-accent">{percent}%</span>
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
        <p className="mt-0.5 truncate text-[10px] text-muted-foreground" title={details}>
          {details}
        </p>
      </div>
    </section>
  );
}
