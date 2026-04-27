/**
 * Primary sidebar — single combined nav.
 *
 * Layout (top → bottom):
 *   1. + New Conversation  → creates a fresh conversation in the
 *      currently-active workspace and switches to chats surface.
 *   2. WORKSPACES + ⟳ + 📂  → tree of workspaces auto-discovered from
 *      disk via `workspace_scan`. Each row collapses to a list of
 *      that workspace's conversations.
 *   4. MCP TOOLS → live list pulled from the sidecar's well-known
 *      manifest. A row's badge reflects the actual sidecar state
 *      (running / down) — never a fabricated value.
 *   5. Footer — Settings, auth state, app version.
 *
 * The whole sidebar refreshes when the `workspaces-changed` Tauri
 * event fires (today emitted by `workspace_set_active`; the auto-scan
 * triggers a manual refresh too).
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import * as ScrollArea from "@radix-ui/react-scroll-area";
import {
  ChevronDown,
  ChevronRight,
  Folder,
  FolderPlus,
  MessageSquarePlus,
  RefreshCw,
  ShieldCheck,
  SlidersHorizontal,
  Cpu,
  Plug,
} from "lucide-react";
import type { LucideIcon } from "lucide-react";
import { open as openDialog } from "@tauri-apps/plugin-dialog";

import { cn } from "@/lib/utils";
import { useApp } from "@/store/app";
import { Button } from "@/components/ui/button";
import { toast } from "@/store/toast";
import {
  authState,
  conversationsCreate,
  conversationsList,
  mcpListConnected,
  workspaceAdd,
  workspaceList,
  workspaceScan,
  workspaceSetActive,
  type AuthState,
  type ConversationSummary,
  type McpServerRow,
  type WorkspaceView,
} from "@/lib/tauri";
import type { Surface } from "@/types";

type WorkspaceWithConvs = WorkspaceView & {
  conversations: ConversationSummary[];
  expanded: boolean;
};

const MAX_PINNED_CONVS = 6;

export function Sidebar() {
  const open = useApp((s) => s.sidebarOpen);
  const surface = useApp((s) => s.surface);
  const setSurface = useApp((s) => s.setSurface);
  const activeWorkspace = useApp((s) => s.activeWorkspace);
  const setActiveWorkspace = useApp((s) => s.setActiveWorkspace);
  const activeConv = useApp((s) => s.activeConversationId);
  const setActiveConv = useApp((s) => s.setActiveConversationId);

  const [workspaces, setWorkspaces] = useState<WorkspaceWithConvs[]>([]);
  const [mcp, setMcp] = useState<McpServerRow[]>([]);
  const [auth, setAuth] = useState<AuthState | null>(null);
  const [scanning, setScanning] = useState(false);

  const refresh = useCallback(async () => {
    try {
      const [list, allConvs] = await Promise.all([
        workspaceList(),
        conversationsList(),
      ]);
      const grouped: WorkspaceWithConvs[] = list.map((w) => ({
        ...w,
        conversations: allConvs.filter((c) => c.workspace === w.name),
        expanded: w.name === activeWorkspace,
      }));
      setWorkspaces(grouped);
    } catch (e) {
      toast("Sidebar reload failed", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
    }
  }, [activeWorkspace]);

  // First load: auto-scan disk + populate. Auto-scan is one-shot per
  // app launch; user can re-trigger via the ⟳ button.
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        await workspaceScan();
      } catch (e) {
        // Scan failures are non-fatal — show a toast and proceed
        // with whatever the registry already has.
        toast("Auto-scan skipped", {
          kind: "warn",
          body: e instanceof Error ? e.message : String(e),
        });
      }
      if (cancelled) return;
      await refresh();
      try {
        const [m, a] = await Promise.all([mcpListConnected(), authState()]);
        if (cancelled) return;
        setMcp(m);
        setAuth(a);
      } catch {
        /* honest empty if sidecar / auth unreachable */
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [refresh]);

  if (!open) return null;

  return (
    <aside
      className="flex h-full w-[260px] shrink-0 flex-col border-r border-border bg-surface"
      aria-label="Primary navigation"
    >
      <Header />

      <ScrollArea.Root className="flex-1 overflow-hidden">
        <ScrollArea.Viewport className="h-full w-full">
          <div className="flex flex-col px-2 pb-3 pt-2">
            <PrimaryActions
              surface={surface}
              setSurface={setSurface}
              activeWorkspace={activeWorkspace}
              hasWorkspaces={workspaces.length > 0}
              onNewConversation={async () => {
                // If nothing's active yet, auto-pick the first
                // workspace the user has — opening the chat surface
                // empty-handed feels broken.
                let target = activeWorkspace;
                if (!target) {
                  if (workspaces.length === 0) {
                    toast("No workspace yet", {
                      kind: "warn",
                      body: "Click the folder icon next to Workspaces to add one, or run `root compile <path>` in your terminal.",
                    });
                    return;
                  }
                  const first = workspaces[0];
                  if (!first) return;
                  target = first.name;
                  try {
                    await workspaceSetActive(target);
                    setActiveWorkspace(target);
                  } catch {
                    /* fall through — user can still chat */
                  }
                }
                try {
                  const c = await conversationsCreate(target);
                  setActiveConv(c.id);
                  setSurface("chats");
                  await refresh();
                } catch (e) {
                  toast("Create conversation failed", {
                    kind: "error",
                    body: e instanceof Error ? e.message : String(e),
                  });
                }
              }}
            />

            <SectionHeader
              label="Workspaces"
              right={
                <div className="flex items-center gap-1">
                  <IconBtn
                    title="Re-scan disk"
                    aria-label="Re-scan"
                    busy={scanning}
                    onClick={async () => {
                      setScanning(true);
                      try {
                        const r = await workspaceScan();
                        if (r.registered.length > 0) {
                          toast(
                            `Found ${r.registered.length} new workspace${r.registered.length === 1 ? "" : "s"}`,
                            { kind: "success" },
                          );
                        }
                        await refresh();
                      } catch (e) {
                        toast("Scan failed", {
                          kind: "error",
                          body: e instanceof Error ? e.message : String(e),
                        });
                      } finally {
                        setScanning(false);
                      }
                    }}
                  >
                    <RefreshCw
                      className={cn("size-3.5", scanning && "animate-spin")}
                    />
                  </IconBtn>
                  <IconBtn
                    title="Add workspace folder"
                    aria-label="Add workspace"
                    onClick={async () => {
                      try {
                        const picked = await openDialog({
                          directory: true,
                          multiple: false,
                        });
                        if (typeof picked !== "string") return;
                        await workspaceAdd({ path: picked });
                        await refresh();
                      } catch (e) {
                        toast("Add failed", {
                          kind: "error",
                          body: e instanceof Error ? e.message : String(e),
                        });
                      }
                    }}
                  >
                    <FolderPlus className="size-3.5" />
                  </IconBtn>
                </div>
              }
            />

            {workspaces.length === 0 ? (
              <p className="px-2 py-3 text-[11px] text-muted-foreground">
                No workspaces yet. Click the folder icon above to add one,
                or run <code className="font-mono">root compile &lt;path&gt;</code>{" "}
                in the terminal.
              </p>
            ) : (
              <ul className="flex flex-col">
                {workspaces.map((w) => (
                  <WorkspaceRow
                    key={w.name}
                    workspace={w}
                    activeWorkspace={activeWorkspace}
                    activeConv={activeConv}
                    onSelectWorkspace={async () => {
                      // Switching workspaces is a *context* change.
                      // Whatever surface the user is on (Brain,
                      // Privacy, Settings, chats), keep them there —
                      // each surface re-loads its own data when the
                      // active workspace changes.
                      try {
                        await workspaceSetActive(w.name);
                        setActiveWorkspace(w.name);
                      } catch (e) {
                        toast("Set active failed", {
                          kind: "error",
                          body: e instanceof Error ? e.message : String(e),
                        });
                      }
                    }}
                    onSelectConv={(id) => {
                      setActiveWorkspace(w.name);
                      setActiveConv(id);
                      setSurface("chats");
                    }}
                    onToggle={() =>
                      setWorkspaces((prev) =>
                        prev.map((row) =>
                          row.name === w.name
                            ? { ...row, expanded: !row.expanded }
                            : row,
                        ),
                      )
                    }
                  />
                ))}
              </ul>
            )}

            <SectionHeader label="MCP Tools" />
            {mcp.length === 0 ? (
              <p className="px-2 py-2 text-[11px] text-muted-foreground">
                Sidecar starting…
              </p>
            ) : (
              <ul className="flex flex-col">
                {mcp.map((row, i) => (
                  <li
                    key={`${row.name}-${i}`}
                    className="flex items-center gap-2 rounded-md px-2 py-1 text-xs"
                    title={row.description ?? row.name}
                  >
                    <span
                      className={cn(
                        "size-1.5 shrink-0 rounded-full",
                        row.status === "running"
                          ? "bg-emerald-500"
                          : "bg-zinc-500",
                      )}
                    />
                    <Plug className="size-3 shrink-0 text-muted-foreground" />
                    <span className="truncate text-foreground/80">{row.name}</span>
                    <span className="ml-auto font-mono text-[9px] uppercase text-muted-foreground">
                      {row.transport}
                    </span>
                  </li>
                ))}
              </ul>
            )}
          </div>
        </ScrollArea.Viewport>
        <ScrollArea.Scrollbar orientation="vertical" className="w-2 p-0.5">
          <ScrollArea.Thumb className="rounded-full bg-muted-foreground/30" />
        </ScrollArea.Scrollbar>
      </ScrollArea.Root>

      <Footer auth={auth} surface={surface} setSurface={setSurface} />
    </aside>
  );
}

function Header() {
  return (
    <header className="window-drag flex h-11 shrink-0 items-center gap-2 border-b border-border px-3">
      <img
        src="/logo.png"
        alt="ThinkingRoot logo"
        draggable={false}
        className="window-no-drag h-5 w-5 object-contain opacity-80"
      />
      <h1 className="window-no-drag text-sm font-medium tracking-tight">
        ThinkingRoot
      </h1>
    </header>
  );
}

function PrimaryActions({
  surface,
  setSurface,
  activeWorkspace,
  hasWorkspaces,
  onNewConversation,
}: {
  surface: Surface;
  setSurface: (s: Surface) => void;
  activeWorkspace: string | null;
  hasWorkspaces: boolean;
  onNewConversation: () => void;
}) {
  return (
    <div className="mb-2 flex flex-col gap-0.5">
      <Button
        variant="outline"
        size="sm"
        onClick={onNewConversation}
        className="mb-1 h-9 justify-start gap-2 text-xs"
        disabled={!hasWorkspaces}
        title={
          activeWorkspace
            ? `New chat in ${activeWorkspace}`
            : hasWorkspaces
              ? "Picks the first workspace"
              : "Add a workspace first"
        }
      >
        <MessageSquarePlus className="size-4" />
        <span>New conversation</span>
      </Button>

      <NavRow
        Icon={Cpu}
        label="Brain"
        active={surface === "brain"}
        onClick={() => setSurface("brain")}
      />
      <NavRow
        Icon={ShieldCheck}
        label="Privacy"
        active={surface === "privacy"}
        onClick={() => setSurface("privacy")}
      />
    </div>
  );
}

function WorkspaceRow({
  workspace,
  activeWorkspace,
  activeConv,
  onSelectWorkspace,
  onSelectConv,
  onToggle,
}: {
  workspace: WorkspaceWithConvs;
  activeWorkspace: string | null;
  activeConv: string | null;
  onSelectWorkspace: () => void;
  onSelectConv: (id: string) => void;
  onToggle: () => void;
}) {
  const isActive = workspace.name === activeWorkspace;
  const visibleConvs = useMemo(
    () => workspace.conversations.slice(0, MAX_PINNED_CONVS),
    [workspace.conversations],
  );
  const overflow = workspace.conversations.length - visibleConvs.length;

  return (
    <li>
      <div
        className={cn(
          "group flex w-full items-center gap-1 rounded-md px-1.5 py-1 text-xs",
          isActive ? "bg-accent/10 text-accent" : "text-foreground hover:bg-muted/60",
        )}
      >
        <button
          type="button"
          onClick={onToggle}
          className="flex size-4 shrink-0 items-center justify-center rounded hover:bg-muted/50"
          aria-label={workspace.expanded ? "Collapse" : "Expand"}
        >
          {workspace.expanded ? (
            <ChevronDown className="size-3" />
          ) : (
            <ChevronRight className="size-3" />
          )}
        </button>
        <button
          type="button"
          onClick={onSelectWorkspace}
          className="flex flex-1 items-center gap-1.5 truncate text-left font-medium"
          title={workspace.path}
        >
          <Folder className="size-3.5 shrink-0 text-muted-foreground" />
          <span className="truncate">{workspace.name}</span>
          {!workspace.compiled && (
            <span className="ml-auto rounded bg-zinc-500/20 px-1 py-0.5 font-mono text-[8px] uppercase tracking-wider text-zinc-400">
              new
            </span>
          )}
        </button>
      </div>

      {workspace.expanded && (
        <ul className="ml-4 flex flex-col border-l border-border/60 pl-1">
          {workspace.conversations.length === 0 && (
            <li className="px-2 py-1.5 text-[10px] italic text-muted-foreground">
              No conversations yet.
            </li>
          )}
          {visibleConvs.map((c) => {
            const selected = activeConv === c.id;
            return (
              <li key={c.id}>
                <button
                  type="button"
                  onClick={() => onSelectConv(c.id)}
                  className={cn(
                    "group flex w-full items-center justify-between gap-1 rounded-md px-2 py-1 text-left text-[11px] transition-colors",
                    selected
                      ? "bg-muted text-foreground"
                      : "text-muted-foreground hover:bg-muted/50 hover:text-foreground",
                  )}
                  title={c.title}
                >
                  <span className="line-clamp-1 flex-1">{c.title}</span>
                  <span className="shrink-0 font-mono text-[9px] text-muted-foreground/80">
                    {timeago(c.updated_at)}
                  </span>
                </button>
              </li>
            );
          })}
          {overflow > 0 && (
            <li className="px-2 py-1 text-[10px] text-muted-foreground/80">
              See all ({overflow + visibleConvs.length})
            </li>
          )}
        </ul>
      )}
    </li>
  );
}

function SectionHeader({
  label,
  right,
}: {
  label: string;
  right?: React.ReactNode;
}) {
  return (
    <div className="mt-3 flex h-7 items-center justify-between px-2">
      <span className="text-[10px] font-semibold uppercase tracking-widest text-muted-foreground">
        {label}
      </span>
      {right}
    </div>
  );
}

function IconBtn({
  children,
  title,
  busy,
  onClick,
  ...rest
}: React.PropsWithChildren<{
  title: string;
  busy?: boolean;
  onClick: () => void;
  "aria-label"?: string;
}>) {
  return (
    <button
      type="button"
      onClick={onClick}
      title={title}
      disabled={busy}
      className="flex h-6 w-6 items-center justify-center rounded text-muted-foreground transition-colors hover:bg-muted/60 hover:text-foreground disabled:opacity-50"
      {...rest}
    >
      {children}
    </button>
  );
}

function NavRow({
  Icon,
  label,
  active,
  onClick,
}: {
  Icon: LucideIcon;
  label: string;
  active: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={cn(
        "flex items-center gap-2 rounded-md px-2 py-1.5 text-left text-xs transition-colors",
        active
          ? "bg-muted text-foreground"
          : "text-muted-foreground hover:bg-muted/50 hover:text-foreground",
      )}
    >
      <Icon className="size-3.5" />
      <span>{label}</span>
    </button>
  );
}

function Footer({
  auth,
  surface,
  setSurface,
}: {
  auth: AuthState | null;
  surface: Surface;
  setSurface: (s: Surface) => void;
}) {
  const storage = auth?.signed_in
    ? "Signed in · cloud sync"
    : "Local only";
  return (
    <footer className="flex shrink-0 flex-col gap-0.5 border-t border-border px-2 py-2">
      <NavRow
        Icon={SlidersHorizontal}
        label="Settings"
        active={surface === "settings"}
        onClick={() => setSurface("settings")}
      />
      <div
        className="flex items-center gap-1.5 px-2 py-1 text-[9px] text-muted-foreground"
        title={
          auth
            ? `Storage: local ${auth.storage.local ? "✓" : "✗"} · cloud ${auth.storage.cloud ? "✓" : "✗"}`
            : "Storage state unknown"
        }
      >
        <span
          className={cn(
            "size-1.5 rounded-full",
            auth?.signed_in ? "bg-sky-500" : "bg-zinc-500",
          )}
        />
        <span className="uppercase tracking-wider">{storage}</span>
      </div>
    </footer>
  );
}

function timeago(iso: string): string {
  try {
    const t = new Date(iso).getTime();
    const dt = Math.max(0, Date.now() - t);
    if (dt < 60_000) return "now";
    if (dt < 3_600_000) return `${Math.floor(dt / 60_000)}m`;
    if (dt < 86_400_000) return `${Math.floor(dt / 3_600_000)}h`;
    return `${Math.floor(dt / 86_400_000)}d`;
  } catch {
    return "";
  }
}
