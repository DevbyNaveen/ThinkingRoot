/**
 * Right-rail inspector. Body switches by surface:
 *
 *   chats    → active workspace card + quick ops (compile, branch,
 *              merge), recent provenance pills.
 *   brain    → workspace stats + branch list with checkout.
 *   privacy  → source detail card for the selected source.
 *   settings → small "what writes where" cheatsheet.
 *
 * Header always shows the current context title (workspace · entity)
 * so the user can pick up where they left off.
 */
import { useEffect, useState } from "react";
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
} from "lucide-react";

import { cn } from "@/lib/utils";
import { useApp } from "@/store/app";
import { Button } from "@/components/ui/button";
import { toast } from "@/store/toast";
import {
  branchCheckout,
  branchList,
  workspaceCompile,
  workspaceList,
  type BranchView,
  type WorkspaceView,
} from "@/lib/tauri";

export function RightRail() {
  const open = useApp((s) => s.rightRailOpen);
  const toggle = useApp((s) => s.toggleRightRail);
  const surface = useApp((s) => s.surface);
  const activeWorkspace = useApp((s) => s.activeWorkspace);

  if (!open) {
    return (
      <div className="flex h-full w-10 shrink-0 flex-col items-center border-l border-border bg-surface">
        <header className="flex h-11 w-full items-center justify-center border-b border-border">
          <Button
            variant="ghost"
            size="icon"
            onClick={toggle}
            aria-label="Show inspector"
            className="h-7 w-7"
          >
            <PanelRight className="size-3.5" />
          </Button>
        </header>
      </div>
    );
  }

  return (
    <aside
      className="flex h-full w-72 shrink-0 flex-col border-l border-border bg-surface"
      aria-label="Inspector"
    >
      <header className="flex h-11 items-center justify-between gap-2 border-b border-border px-3">
        <h2 className="truncate text-sm font-medium tracking-tight">
          {headerTitle(surface, activeWorkspace)}
        </h2>
        <Button
          variant="ghost"
          size="icon"
          onClick={toggle}
          aria-label="Hide inspector"
          className="h-7 w-7"
        >
          <PanelRight className="size-3.5" />
        </Button>
      </header>

      <div className="flex flex-1 flex-col gap-3 overflow-y-auto px-3 py-3">
        {(surface === "chats" || surface === "brain") && (
          <WorkspaceCard activeWorkspace={activeWorkspace} />
        )}
        <CompilationProgressIndicator />
        {(surface === "chats" || surface === "brain") && activeWorkspace && (
          <BranchPanel workspace={activeWorkspace} />
        )}
        {surface === "privacy" && <PrivacyHelp />}
        {surface === "settings" && <SettingsHelp />}
      </div>
    </aside>
  );
}

function headerTitle(surface: string, workspace: string | null): string {
  if (surface === "chats") return workspace ? `${workspace} · context` : "Pick a workspace";
  if (surface === "brain") return workspace ? `${workspace} · ops` : "No workspace";
  if (surface === "privacy") return "Privacy detail";
  if (surface === "settings") return "Settings reference";
  return "Inspector";
}

function WorkspaceCard({ activeWorkspace }: { activeWorkspace: string | null }) {
  const [w, setW] = useState<WorkspaceView | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let cancelled = false;
    if (!activeWorkspace) {
      setW(null);
      return;
    }
    workspaceList()
      .then((list) => {
        if (cancelled) return;
        setW(list.find((x) => x.name === activeWorkspace) ?? null);
      })
      .catch(() => setW(null));
    return () => {
      cancelled = true;
    };
  }, [activeWorkspace]);

  if (!activeWorkspace) {
    return (
      <p className="rounded-md border border-dashed border-border p-3 text-[11px] text-muted-foreground">
        No workspace selected. Pick one from the sidebar to see ops here.
      </p>
    );
  }

  return (
    <section className="flex flex-col gap-2 rounded-lg border border-border/60 bg-background/40 p-3">
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
        <p className="font-mono text-[10px] text-muted-foreground" title={w.path}>
          {w.path.replace(/^\/Users\/[^/]+|^\/home\/[^/]+/, "~")}
        </p>
      )}
      <div className="flex items-center gap-1.5 pt-1">
        <Button
          variant="outline"
          size="sm"
          className="flex-1 gap-1.5 text-xs"
          disabled={busy}
          onClick={async () => {
            setBusy(true);
            try {
              await workspaceCompile({ target: activeWorkspace });
              toast("Compile queued", {
                kind: "info",
                body: "Watch progress on the Brain tab.",
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
          {w?.compiled ? "Recompile" : "Compile"}
        </Button>
      </div>
    </section>
  );
}

function BranchPanel({ workspace }: { workspace: string }) {
  const [branches, setBranches] = useState<BranchView[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function load() {
    setLoading(true);
    setError(null);
    try {
      const list = await branchList(workspace);
      setBranches(list);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    void load();
  }, [workspace]);

  return (
    <section className="flex flex-col gap-2 rounded-lg border border-border/60 bg-background/40 p-3">
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
      {error && (
        <p className="text-[11px] text-destructive">{error}</p>
      )}
      {!error && branches.length === 0 && !loading && (
        <p className="text-[10px] text-muted-foreground">
          No branches yet. Use{" "}
          <code className="font-mono">/branch &lt;name&gt;</code> in chat to fork.
        </p>
      )}
      <ul className="flex flex-col">
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
                "flex w-full items-center gap-1.5 rounded-md px-2 py-1 text-left text-[11px]",
                b.current
                  ? "bg-accent/10 text-accent"
                  : "text-foreground hover:bg-muted/60",
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

function PrivacyHelp() {
  return (
    <section className="flex flex-col gap-2 rounded-lg border border-border/60 bg-background/40 p-3 text-[11px] text-muted-foreground">
      <ShieldCheck className="size-4 text-accent" />
      <p className="text-foreground">Forget is irreversible.</p>
      <p>
        Selecting "Forget" removes the source row plus every claim,
        relation, and rooted-pin that referenced it. The desktop
        rebuilds the in-memory cache atomically — `root query` reflects
        the redaction immediately.
      </p>
    </section>
  );
}

function SettingsHelp() {
  return (
    <section className="flex flex-col gap-2 rounded-lg border border-border/60 bg-background/40 p-3 text-[11px] text-muted-foreground">
      <p className="text-foreground">Where things live</p>
      <ul className="list-disc pl-3">
        <li>Provider keys → ~/.config/thinkingroot/desktop.toml</li>
        <li>Conversations → &lt;workspace&gt;/.thinkingroot/conversations/</li>
        <li>Workspace registry → ~/.config/thinkingroot/workspaces.toml</li>
        <li>Sidecar logs → tracing → stderr (run with RUST_LOG=debug)</li>
      </ul>
    </section>
  );
}

function CompilationProgressIndicator() {
  const progress = useApp((s) => s.compileProgress);
  if (!progress) return null;

  let title = "Compiling...";
  let details = "";
  let percent = 0;
  let isDone = false;
  let isError = false;

  switch (progress.phase) {
    case "started":
      title = "Starting compilation";
      details = `Workspace: ${progress.workspace}`;
      percent = 5;
      break;
    case "parse_complete":
      title = "Parsing source files";
      details = `Parsed ${progress.files} files`;
      percent = 15;
      break;
    case "extraction_start":
      title = "Extracting claims";
      details = `Starting ${progress.total_batches} batches`;
      percent = 20;
      break;
    case "extraction_progress":
      title = "Extracting claims";
      details = `${progress.done} / ${progress.total} chunks`;
      percent = 20 + Math.floor((progress.done / Math.max(1, progress.total)) * 30);
      break;
    case "extraction_complete":
      title = "Extraction complete";
      details = `${progress.claims} claims, ${progress.entities} entities`;
      percent = 50;
      break;
    case "grounding_progress":
      title = "Grounding entities";
      details = `${progress.done} / ${progress.total}`;
      percent = 50 + Math.floor((progress.done / Math.max(1, progress.total)) * 15);
      break;
    case "linking_start":
      title = "Linking knowledge graph";
      details = `${progress.total_entities} entities to link`;
      percent = 65;
      break;
    case "linking_progress":
      title = "Linking knowledge graph";
      details = `${progress.done} / ${progress.total}`;
      percent = 65 + Math.floor((progress.done / Math.max(1, progress.total)) * 15);
      break;
    case "vector_progress":
      title = "Building vector index";
      details = `${progress.done} / ${progress.total}`;
      percent = 80 + Math.floor((progress.done / Math.max(1, progress.total)) * 19);
      break;
    case "done":
      title = "Compilation complete";
      details = `${progress.claims} claims, ${progress.entities} entities`;
      percent = 100;
      isDone = true;
      break;
    case "failed":
      title = "Compilation failed";
      details = progress.error;
      percent = 100;
      isError = true;
      break;
  }

  return (
    <section className="flex flex-col gap-2 rounded-lg border border-border/60 bg-background/40 p-3 shadow-sm relative overflow-hidden">
      <div className="absolute inset-0 bg-gradient-to-br from-accent/5 to-transparent pointer-events-none" />
      <header className="flex items-center gap-2 text-xs relative z-10">
        {isDone ? (
          <CheckCircle2 className="size-4 text-emerald-500 drop-shadow-sm" />
        ) : isError ? (
          <AlertCircle className="size-4 text-destructive drop-shadow-sm" />
        ) : (
          <Loader2 className="size-4 animate-spin text-accent drop-shadow-sm" />
        )}
        <h3 className="font-medium tracking-tight text-foreground">{title}</h3>
        {!isDone && !isError && (
          <span className="ml-auto font-mono text-[9px] text-accent font-medium">
            {percent}%
          </span>
        )}
      </header>
      <div className="flex flex-col gap-1.5 relative z-10 mt-1">
        <div className="h-1.5 w-full overflow-hidden rounded-full bg-muted/50 border border-border/50">
          <div
            className={cn(
              "h-full transition-all duration-300 ease-out",
              isDone ? "bg-emerald-500" : isError ? "bg-destructive" : "bg-accent bg-stripe-gradient"
            )}
            style={{ width: `${percent}%` }}
          />
        </div>
        <p className="text-[10px] text-muted-foreground truncate mt-0.5" title={details}>
          {details}
        </p>
      </div>
    </section>
  );
}
