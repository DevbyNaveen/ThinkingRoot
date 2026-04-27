import { useEffect, useMemo, useState } from "react";
import {
  Search,
  Plus,
  Folder,
  Activity as ActivityIcon,
  AlertTriangle,
  GitBranch,
  GitMerge,
} from "lucide-react";
import * as ScrollArea from "@radix-ui/react-scroll-area";
import { cn } from "@/lib/utils";
import { useApp } from "@/store/app";
import type { ConversationSummary, Surface } from "@/types";
import { Button } from "@/components/ui/button";
import {
  configRead,
  workspaceList,
  workspaceSetActive,
  traceListSessions,
  traceOpen,
  gitBranches,
  type WorkspaceView,
  type TraceFileInfo,
  type BranchInfo,
} from "@/lib/tauri";
import { toast } from "@/store/toast";

const SURFACE_TITLES: Record<Surface, string> = {
  chats: "Conversations",
  brain: "Workspace",
  satellites: "Satellites",
  trace: "Sessions",
  privacy: "Privacy",
  settings: "Settings",
};

/** Secondary sidebar — content depends on the active surface. */
export function Sidebar() {
  const surface = useApp((s) => s.surface);
  const sidebarOpen = useApp((s) => s.sidebarOpen);
  if (!sidebarOpen) return null;

  return (
    <aside
      className="flex h-full w-60 shrink-0 flex-col border-r border-border bg-surface"
      aria-label={SURFACE_TITLES[surface]}
    >
      <SidebarHeader surface={surface} />
      <ScrollArea.Root className="flex-1 overflow-hidden">
        <ScrollArea.Viewport className="h-full w-full">
          <SidebarBody surface={surface} />
        </ScrollArea.Viewport>
        <ScrollArea.Scrollbar orientation="vertical" className="w-2 p-0.5">
          <ScrollArea.Thumb className="rounded-full bg-muted-foreground/30" />
        </ScrollArea.Scrollbar>
      </ScrollArea.Root>
    </aside>
  );
}

function SidebarHeader({ surface }: { surface: Surface }) {
  return (
    <header className="flex h-11 items-center justify-between gap-2 border-b border-border px-3">
      <div className="flex items-center gap-2">
        <h2 className="text-sm font-medium tracking-tight">
          {SURFACE_TITLES[surface]}
        </h2>
      </div>
      <div className="flex items-center gap-1">
        <Button variant="ghost" size="icon" aria-label="Search" className="h-7 w-7">
          <Search className="size-3.5" />
        </Button>
        <Button variant="ghost" size="icon" aria-label="New" className="h-7 w-7">
          <Plus className="size-3.5" />
        </Button>
      </div>
    </header>
  );
}

function SidebarBody({ surface }: { surface: Surface }) {
  if (surface === "chats") return <ConversationsList />;
  if (surface === "brain") return <BranchesList />;
  if (surface === "satellites") return <SatellitesJumpList />;
  if (surface === "trace") return <TraceSessionsList />;
  // Settings has no sidebar body — its main pane is sectioned itself.
  return (
    <div className="px-3 py-6 text-xs text-muted-foreground">
      <p>Settings sections live in the main pane.</p>
    </div>
  );
}

// ──────────────────────────────────────────────────────────────────
// Brain — git branches of the active workspace (GitHub-style list)
// ──────────────────────────────────────────────────────────────────

function BranchesList() {
  const [rootPath, setRootPath] = useState<string | null>(null);
  const [branches, setBranches] = useState<BranchInfo[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const cfg = await configRead();
        const path = cfg.entries["THINKINGROOT_WORKSPACE"] ?? null;
        if (cancelled) return;
        setRootPath(path);
        if (!path) {
          setBranches([]);
          return;
        }
        const list = await gitBranches(path);
        if (!cancelled) setBranches(list);
      } catch (e) {
        if (!cancelled)
          setError(e instanceof Error ? e.message : String(e));
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  if (error) {
    return (
      <div className="flex items-start gap-2 px-3 py-3 text-xs text-destructive">
        <AlertTriangle className="mt-0.5 size-3.5 shrink-0" />
        <span>{error}</span>
      </div>
    );
  }
  if (!rootPath) {
    return (
      <div className="px-3 py-6 text-xs text-muted-foreground">
        <p className="mb-2">No active workspace.</p>
        <p>
          Pick one in <span className="font-medium">Satellites</span> or run
          <span className="ml-1 font-mono">workspace-active</span> from ⌘K.
        </p>
      </div>
    );
  }
  if (branches === null) {
    return (
      <div className="px-3 py-3 text-xs text-muted-foreground">Loading…</div>
    );
  }
  if (branches.length === 0) {
    return (
      <div className="px-3 py-6 text-xs text-muted-foreground">
        No branches found. The active workspace might not be a git repo.
      </div>
    );
  }

  const local = branches.filter((b) => b.kind === "local");

  return (
    <div className="flex flex-col gap-3 px-2 py-2">
      {local.length > 0 && (
        <Section heading="Local" Icon={GitBranch} count={local.length}>
          <ul className="flex flex-col">
            {local.map((b) => (
              <li key={`local-${b.name}`}>
                <BranchRow branch={b} />
              </li>
            ))}
          </ul>
        </Section>
      )}
    </div>
  );
}

function Section({
  heading,
  Icon,
  count,
  children,
}: {
  heading: string;
  Icon: typeof GitBranch;
  count: number;
  children: React.ReactNode;
}) {
  return (
    <section>
      <h3 className="flex items-center gap-1.5 px-2 pb-1 text-[10px] font-semibold uppercase tracking-widest text-muted-foreground">
        <Icon className="size-3" />
        {heading}
        <span className="ml-auto font-mono normal-case tracking-normal text-muted-foreground/70">
          {count}
        </span>
      </h3>
      {children}
    </section>
  );
}

function BranchRow({ branch }: { branch: BranchInfo }) {
  const isCurrent = branch.current;
  const fullName = branch.remote ? `${branch.remote}/${branch.name}` : branch.name;
  return (
    <div
      className={cn(
        "group flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-xs",
        isCurrent && "bg-accent/10 text-accent",
        !isCurrent && "text-foreground hover:bg-muted/60",
      )}
      title={fullName}
    >
      {isCurrent ? (
        <GitMerge className="size-3.5 shrink-0 text-accent" />
      ) : (
        <GitBranch className="size-3.5 shrink-0 text-muted-foreground" />
      )}
      <span className="flex-1 truncate font-mono text-[11px]">{fullName}</span>
      {isCurrent && (
        <span className="rounded-full bg-accent/15 px-1.5 py-0.5 text-[9px] font-medium uppercase tracking-wider text-accent">
          on
        </span>
      )}
    </div>
  );
}

// ──────────────────────────────────────────────────────────────────
// Satellites — quick-switch list of workspaces
// ──────────────────────────────────────────────────────────────────

function SatellitesJumpList() {
  const [workspaces, setWorkspaces] = useState<WorkspaceView[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  async function refresh() {
    try {
      const ws = await workspaceList();
      setWorkspaces(ws);
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  useEffect(() => {
    void refresh();
  }, []);

  if (error) {
    return (
      <div className="flex items-start gap-2 px-3 py-3 text-xs text-destructive">
        <AlertTriangle className="mt-0.5 size-3.5 shrink-0" />
        <span>{error}</span>
      </div>
    );
  }
  if (workspaces === null) {
    return (
      <div className="px-3 py-3 text-xs text-muted-foreground">Loading…</div>
    );
  }
  if (workspaces.length === 0) {
    return (
      <div className="px-3 py-6 text-xs text-muted-foreground">
        No workspaces. Use <span className="font-medium">Compile new</span> in
        the main pane to add one.
      </div>
    );
  }

  return (
    <ul className="flex flex-col py-1">
      {workspaces.map((w) => (
        <li key={w.name}>
          <button
            type="button"
            onClick={async () => {
              try {
                await workspaceSetActive(w.name);
                toast(`Active: ${w.name}`, { kind: "success", durationMs: 1800 });
                void refresh();
              } catch (e) {
                toast("Set active failed", {
                  kind: "error",
                  body: e instanceof Error ? e.message : String(e),
                });
              }
            }}
            className={cn(
              "flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-xs transition-colors",
              "hover:bg-muted/60",
              w.active && "bg-accent/10 text-accent",
            )}
          >
            <Folder className="size-3.5 shrink-0 text-muted-foreground" />
            <div className="grid min-w-0 flex-1 grid-cols-1 gap-0.5">
              <span className="truncate font-medium">{w.name}</span>
              <span className="truncate font-mono text-[10px] text-muted-foreground" title={w.path}>
                {w.path.replace(/^\/Users\/[^/]+|^\/home\/[^/]+/, '~')}
              </span>
            </div>
            {w.active && (
              <span className="shrink-0 rounded-full bg-accent/15 px-1.5 py-0.5 text-[9px] font-medium text-accent">
                active
              </span>
            )}
          </button>
        </li>
      ))}
    </ul>
  );
}

// ──────────────────────────────────────────────────────────────────
// Trace — list of session files
// ──────────────────────────────────────────────────────────────────

function TraceSessionsList() {
  const [sessions, setSessions] = useState<TraceFileInfo[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [openingPath, setOpeningPath] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    traceListSessions()
      .then((s) => {
        if (!cancelled) setSessions(s);
      })
      .catch((e) => {
        if (!cancelled)
          setError(e instanceof Error ? e.message : String(e));
      });
    return () => {
      cancelled = true;
    };
  }, []);

  if (error) {
    return (
      <div className="flex items-start gap-2 px-3 py-3 text-xs text-destructive">
        <AlertTriangle className="mt-0.5 size-3.5 shrink-0" />
        <span>{error}</span>
      </div>
    );
  }
  if (sessions === null) {
    return (
      <div className="px-3 py-3 text-xs text-muted-foreground">Loading…</div>
    );
  }
  if (sessions.length === 0) {
    return (
      <div className="px-3 py-6 text-xs text-muted-foreground">
        No trace sessions yet. Each chat turn writes a hash-chained trace
        the next time you exit the app.
      </div>
    );
  }

  return (
    <ul className="flex flex-col py-1">
      {sessions.map((s) => (
        <li key={s.path}>
          <button
            type="button"
            onClick={async () => {
              setOpeningPath(s.path);
              try {
                await traceOpen(s.path);
              } catch (e) {
                toast("Open failed", {
                  kind: "error",
                  body: e instanceof Error ? e.message : String(e),
                });
              } finally {
                setOpeningPath(null);
              }
            }}
            disabled={openingPath === s.path}
            className={cn(
              "flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-xs transition-colors",
              "hover:bg-muted/60 disabled:opacity-50",
            )}
          >
            <ActivityIcon className="size-3.5 shrink-0 text-muted-foreground" />
            <div className="grid min-w-0 flex-1 grid-cols-1 gap-0.5">
              <span className="truncate font-medium">{s.name}</span>
              <span className="truncate font-mono text-[10px] text-muted-foreground">
                {(s.bytes / 1024).toFixed(1)} KB ·{" "}
                {s.modified_rfc3339?.slice(0, 19) ?? "no mtime"}
              </span>
            </div>
          </button>
        </li>
      ))}
    </ul>
  );
}

// ──────────────────────────────────────────────────────────────────
// Chats — conversations list (was the only working surface pre-D-15)
// ──────────────────────────────────────────────────────────────────

function ConversationsList() {
  const conversations = useApp((s) => s.conversations);
  const active = useApp((s) => s.activeConversationId);
  const setActive = useApp((s) => s.setActiveConversationId);

  const groups = useMemo(() => groupByDate(conversations), [conversations]);

  if (conversations.length === 0) {
    return (
      <div className="px-3 py-10 text-center">
        <p className="text-xs text-muted-foreground">
          No conversations yet. Press{" "}
          <kbd className="rounded bg-muted px-1.5 py-0.5 font-mono text-[10px]">
            ⌘N
          </kbd>{" "}
          to start one.
        </p>
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-3 px-2 py-2">
      {groups.map(([label, items]) => (
        <section key={label}>
          <h3 className="px-2 pb-1 text-[10px] font-semibold uppercase tracking-widest text-muted-foreground">
            {label}
          </h3>
          <ul className="flex flex-col">
            {items.map((c) => (
              <li key={c.id}>
                <button
                  type="button"
                  onClick={() => setActive(c.id)}
                  className={cn(
                    "group flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-sm transition-colors",
                    "hover:bg-muted/60",
                    active === c.id && "bg-muted",
                  )}
                >
                  <span
                    className={cn(
                      "size-1.5 shrink-0 rounded-full mt-1.5",
                      active === c.id ? "bg-accent" : "bg-transparent",
                    )}
                  />
                  <span className="flex-1 line-clamp-2 min-w-0 text-left text-xs leading-relaxed">{c.title}</span>
                </button>
              </li>
            ))}
          </ul>
        </section>
      ))}
    </div>
  );
}

function groupByDate(items: ConversationSummary[]): [string, ConversationSummary[]][] {
  const today: ConversationSummary[] = [];
  const yesterday: ConversationSummary[] = [];
  const older: ConversationSummary[] = [];
  const now = new Date();
  const startOfToday = new Date(now.getFullYear(), now.getMonth(), now.getDate());
  const startOfYesterday = new Date(startOfToday);
  startOfYesterday.setDate(startOfYesterday.getDate() - 1);

  for (const item of items) {
    if (item.lastMessageAt >= startOfToday) today.push(item);
    else if (item.lastMessageAt >= startOfYesterday) yesterday.push(item);
    else older.push(item);
  }

  const groups: [string, ConversationSummary[]][] = [];
  if (today.length) groups.push(["Today", today]);
  if (yesterday.length) groups.push(["Yesterday", yesterday]);
  if (older.length) groups.push(["Earlier", older]);
  return groups;
}
