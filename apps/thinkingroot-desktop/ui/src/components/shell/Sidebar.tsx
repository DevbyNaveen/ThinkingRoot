/**
 * Primary sidebar — single combined nav.
 *
 * Layout (top → bottom):
 *   1. + New Conversation  → creates a fresh conversation in the
 *      currently-active workspace and switches to chats surface.
 *   2. WORKSPACES + ⟳ + add → flat sections: workspace title, then
 *      conversation titles (minimal list; no file-tree chrome).
 *      When the main pane is Settings, this column switches to a
 *      category list (Provider, Workspace, …) plus Back to chats.
 * Settings and Docs are also reachable from the account menu at the
 * bottom of the rail (opens above the control; no bottom blur strip).
 *
 * The workspace list reloads when the `workspaces-changed` Tauri event
 * fires (after `workspace_set_active`, `workspace_remove`, `workspace_scan`
 * when new folders are registered, and after a compile finishes so the
 * compiled / new badges stay honest).
 */
import { useCallback, useEffect, useRef, useState } from "react";
import * as ScrollArea from "@radix-ui/react-scroll-area";
import {
  ArrowLeft,
  BookOpen,
  Braces,
  FileCode,
  FolderPlus,
  Package,
  Sparkles,
  Terminal,
  Plus,
  RefreshCw,
  SlidersHorizontal,
  SquarePen,
  Plug,
  ChevronDown,
  KeyRound,
  LogIn,
  FolderOpen,
  Paintbrush,
  Bell,
  Cloud,
} from "lucide-react";
import { open as openDialog } from "@tauri-apps/plugin-dialog";

import { cn } from "@/lib/utils";
import {
  SIDEBAR_DEFAULT_WIDTH,
  SIDEBAR_MAX_WIDTH,
  SIDEBAR_MIN_WIDTH,
} from "@/lib/sidebar-layout";
import { useApp } from "@/store/app";
import { Button } from "@/components/ui/button";
import { toast } from "@/store/toast";
import {
  authState,
  cloudLoginStart,
  conversationsCreate,
  conversationsList,
  workspaceAdd,
  workspaceList,
  workspaceScan,
  workspaceSetActive,
  onConversationsChanged,
  onWorkspacesChanged,
  type AuthState,
  type ConversationSummary,
  type WorkspaceView,
} from "@/lib/tauri";
import type { DocSectionId, Surface, SettingsSectionId } from "@/types";

const MAX_PINNED_CONVS = 6;

const SETTINGS_NAV: Array<{
  id: SettingsSectionId;
  label: string;
  icon: typeof KeyRound;
}> = [
  { id: "provider", label: "Provider", icon: KeyRound },
  { id: "workspace", label: "Workspace", icon: FolderOpen },
  { id: "appearance", label: "Appearance", icon: Paintbrush },
  { id: "mcp", label: "MCP", icon: Plug },
  { id: "channels", label: "Channels", icon: Bell },
  { id: "cloud", label: "Cloud", icon: Cloud },
];

const DOCS_NAV: Array<{
  id: DocSectionId;
  label: string;
  icon: typeof BookOpen;
}> = [
  { id: "overview", label: "Overview", icon: BookOpen },
  { id: "cursor", label: "Cursor / MCP", icon: Plug },
  { id: "node", label: "Node", icon: Braces },
  { id: "python", label: "Python", icon: FileCode },
  { id: "curl", label: "curl", icon: Terminal },
  { id: "lovable", label: "Lovable", icon: Sparkles },
  { id: "export", label: ".tr Export", icon: Package },
];

type WorkspaceWithConvs = WorkspaceView & {
  conversations: ConversationSummary[];
};

export function Sidebar() {
  const open = useApp((s) => s.sidebarOpen);
  const surface = useApp((s) => s.surface);
  const setSurface = useApp((s) => s.setSurface);
  const activeWorkspace = useApp((s) => s.activeWorkspace);
  const setActiveWorkspace = useApp((s) => s.setActiveWorkspace);
  const activeConv = useApp((s) => s.activeConversationId);
  const setSettingsSection = useApp((s) => s.setSettingsSection);
  const settingsSection = useApp((s) => s.settingsSection);
  const docsSection = useApp((s) => s.docsSection);
  const setDocsSection = useApp((s) => s.setDocsSection);
  const setActiveConv = useApp((s) => s.setActiveConversationId);

  const [workspaces, setWorkspaces] = useState<WorkspaceWithConvs[]>([]);
  const [scanning, setScanning] = useState(false);

  const storedWidth = useApp((s) => s.sidebarWidth);
  const setStoreWidth = useApp((s) => s.setSidebarWidth);

  const [width, setWidth] = useState(storedWidth ?? SIDEBAR_DEFAULT_WIDTH);
  const dragging = useRef(false);
  const startX = useRef(0);
  const startWidth = useRef(width);
  const railRef = useRef<HTMLElement>(null);

  useEffect(() => {
    const raw = storedWidth ?? SIDEBAR_DEFAULT_WIDTH;
    const clamped = Math.min(
      SIDEBAR_MAX_WIDTH,
      Math.max(SIDEBAR_MIN_WIDTH, raw),
    );
    if (clamped !== raw) setStoreWidth(clamped);
    setWidth(clamped);
  }, [storedWidth, setStoreWidth]);

  const onMouseDown = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    dragging.current = true;
    startX.current = e.clientX;
    startWidth.current = width;

    const onMove = (ev: MouseEvent) => {
      if (!dragging.current) return;
      const delta = ev.clientX - startX.current;
      const next = Math.min(
        SIDEBAR_MAX_WIDTH,
        Math.max(SIDEBAR_MIN_WIDTH, startWidth.current + delta),
      );
      setWidth(next);
    };

    const onUp = (ev: MouseEvent) => {
      dragging.current = false;
      const delta = ev.clientX - startX.current;
      const next = Math.min(
        SIDEBAR_MAX_WIDTH,
        Math.max(SIDEBAR_MIN_WIDTH, startWidth.current + delta),
      );
      setStoreWidth(next);
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
    };

    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
  }, [width, setStoreWidth]);

  const refresh = useCallback(async () => {
    try {
      const [list, allConvs] = await Promise.all([
        workspaceList(),
        conversationsList(),
      ]);
      // Playground is the auto-mounted scratchpad workspace; we pin
      // it to the top so it's always one click away no matter how
      // many real workspaces the user has added. The rest keep
      // registry order.
      const sorted = [...list].sort((a, b) => {
        if (a.name === "playground" && b.name !== "playground") return -1;
        if (b.name === "playground" && a.name !== "playground") return 1;
        return 0;
      });
      const grouped: WorkspaceWithConvs[] = sorted.map((w) => ({
        ...w,
        conversations: allConvs.filter((c) => c.workspace === w.name),
      }));
      setWorkspaces(grouped);
    } catch (e) {
      toast("Sidebar reload failed", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
    }
  }, []);

  useEffect(() => {
    let cancelled = false;
    let unlistenWs: (() => void) | undefined;
    let unlistenConv: (() => void) | undefined;
    const bump = () => {
      if (!cancelled) void refresh();
    };
    onWorkspacesChanged(bump).then((fn) => {
      if (!cancelled) unlistenWs = fn;
    });
    onConversationsChanged(bump).then((fn) => {
      if (!cancelled) unlistenConv = fn;
    });
    return () => {
      cancelled = true;
      unlistenWs?.();
      unlistenConv?.();
    };
  }, [refresh]);

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
    })();
    return () => {
      cancelled = true;
    };
  }, [refresh]);

  const createConversationIn = useCallback(
    async (target: string) => {
      try {
        await workspaceSetActive(target);
        setActiveWorkspace(target);
      } catch (e) {
        toast("Set active failed", {
          kind: "error",
          body: e instanceof Error ? e.message : String(e),
        });
        return;
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
    },
    [refresh, setActiveConv, setActiveWorkspace, setSurface],
  );

  if (!open) return null;

  return (
    <aside
      ref={railRef}
      className="relative flex h-full shrink-0 flex-col border-r border-border bg-surface"
      style={{ width, minWidth: SIDEBAR_MIN_WIDTH, maxWidth: SIDEBAR_MAX_WIDTH }}
      aria-label="Primary navigation"
    >
      {/* ── Drag handle (right edge) ────────────────────────────── */}
      <div
        className="absolute right-0 top-0 z-10 h-full w-1 cursor-col-resize select-none opacity-0 transition-opacity hover:opacity-100 active:opacity-100"
        style={{ background: "hsl(var(--accent) / 0.4)" }}
        onMouseDown={onMouseDown}
        aria-label="Resize panel"
      />
      <Header />

      <div className="relative flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden">
        {surface === "settings" ? (
          <ScrollArea.Root className="h-full min-h-0 min-w-0">
            <ScrollArea.Viewport className="h-full min-h-0 w-full max-h-full">
              <div className="flex min-w-0 flex-col px-2 pb-4 pt-2">
                <SettingsSidebarNav
                  active={settingsSection}
                  onPick={setSettingsSection}
                  onBackToChats={() => setSurface("chats")}
                />
              </div>
            </ScrollArea.Viewport>
            <ScrollArea.Scrollbar orientation="vertical" className="w-1.5 touch-none select-none p-0">
              <ScrollArea.Thumb className="rounded-sm bg-muted-foreground/18" />
            </ScrollArea.Scrollbar>
          </ScrollArea.Root>
        ) : surface === "docs" ? (
          <ScrollArea.Root className="h-full min-h-0 min-w-0">
            <ScrollArea.Viewport className="h-full min-h-0 w-full max-h-full">
              <div className="flex min-w-0 flex-col px-2 pb-4 pt-2">
                <DocsSidebarNav
                  active={docsSection}
                  onPick={setDocsSection}
                  onBackToChats={() => setSurface("chats")}
                />
              </div>
            </ScrollArea.Viewport>
            <ScrollArea.Scrollbar orientation="vertical" className="w-1.5 touch-none select-none p-0">
              <ScrollArea.Thumb className="rounded-sm bg-muted-foreground/18" />
            </ScrollArea.Scrollbar>
          </ScrollArea.Root>
        ) : (
          <>
            <div className="window-no-drag shrink-0 bg-surface px-2 pb-2.5 pt-2">
              <PrimaryActions
                surface={surface}
                setSurface={setSurface}
                activeWorkspace={activeWorkspace}
                hasWorkspaces={workspaces.length > 0}
                onNewConversation={async () => {
                  let target = activeWorkspace;
                  if (!target) {
                    if (workspaces.length === 0) {
                      toast("No workspace yet", {
                        kind: "warn",
                        body: "Use Add workspace next to Workspaces, or run `root compile <path>` in your terminal.",
                      });
                      return;
                    }
                    const first = workspaces[0];
                    if (!first) return;
                    target = first.name;
                  }
                  await createConversationIn(target);
                }}
              />

              <SectionHeader
                label="Workspaces"
                right={
                  <div className="flex items-center gap-1">
                    <IconBtn
                      title="Refresh workspace list"
                      aria-label="Refresh workspaces"
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
                          } else {
                            toast("Workspace list refreshed", {
                              kind: "success",
                              body: "Scanned your workspace roots for folders containing `.thinkingroot`.",
                            });
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
                          const previousActive = useApp.getState().activeWorkspace;
                          const w = await workspaceAdd({ path: picked });
                          useApp.getState().setActiveWorkspace(w.name);
                          try {
                            await workspaceSetActive(w.name);
                          } catch (e) {
                            useApp.getState().setActiveWorkspace(previousActive);
                            toast("Set active failed", {
                              kind: "error",
                              body: e instanceof Error ? e.message : String(e),
                            });
                            await refresh();
                          }
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
            </div>

            <ScrollArea.Root className="min-h-0 flex-1 min-w-0 overflow-hidden">
              <ScrollArea.Viewport className="sidebar-scroll-viewport h-full min-h-0 w-full max-h-full overflow-x-hidden">
                <div className="box-border flex min-w-0 max-w-full flex-col px-2 pb-4 pt-1 pr-3">
                  {workspaces.length === 0 ? (
                    <p className="px-2 py-3 text-[11px] text-muted-foreground">
                      No workspaces yet. Use refresh to scan for folders that already
                      contain <code className="font-mono">.thinkingroot</code>, or add
                      one with the button beside Workspaces.
                    </p>
                  ) : (
                    <ul className="flex min-w-0 max-w-full flex-col">
                      {workspaces.map((w) => (
                        <WorkspaceRow
                          key={w.name}
                          workspace={w}
                          surface={surface}
                          activeWorkspace={activeWorkspace}
                          activeConv={activeConv}
                          onNewChat={() => void createConversationIn(w.name)}
                          onSelectWorkspace={async () => {
                            // Workspace click should take the user back to chats:
                            // one clear selection state in the sidebar.
                            try {
                              await workspaceSetActive(w.name);
                              setActiveWorkspace(w.name);
                              setSurface("chats");
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
                        />
                      ))}
                    </ul>
                  )}
                </div>
              </ScrollArea.Viewport>
              <ScrollArea.Scrollbar
                orientation="vertical"
                className="right-1 w-1.5 touch-none select-none p-0"
              >
                <ScrollArea.Thumb className="rounded-sm bg-muted-foreground/18" />
              </ScrollArea.Scrollbar>
            </ScrollArea.Root>
          </>
        )}
      </div>

      <SidebarAuthStrip />
    </aside>
  );
}

function authLabel(auth: AuthState | null): string {
  if (auth == null) return "…";
  if (!auth.signed_in) return "Login";
  if (auth.handle) return `@${auth.handle}`;
  if (auth.server) {
    try {
      return new URL(auth.server).host;
    } catch {
      return auth.server;
    }
  }
  return "Signed in";
}

/** Account / Login — opens upward; Docs + Settings + sign-in notes. */
function SidebarAuthStrip() {
  const setSurface = useApp((s) => s.setSurface);
  const [auth, setAuth] = useState<AuthState | null>(null);
  const [menuOpen, setMenuOpen] = useState(false);
  const wrapRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let alive = true;
    const refresh = () => {
      authState()
        .then((a) => {
          if (alive) setAuth(a);
        })
        .catch(() => {
          if (alive) setAuth(null);
        });
    };
    refresh();
    window.addEventListener("focus", refresh);
    return () => {
      alive = false;
      window.removeEventListener("focus", refresh);
    };
  }, []);

  useEffect(() => {
    if (!menuOpen) return;
    const onDoc = (e: MouseEvent) => {
      const el = wrapRef.current;
      if (el && !el.contains(e.target as Node)) setMenuOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setMenuOpen(false);
    };
    document.addEventListener("mousedown", onDoc, true);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDoc, true);
      document.removeEventListener("keydown", onKey);
    };
  }, [menuOpen]);

  const label = authLabel(auth);

  const goDocs = () => {
    setSurface("docs");
    setMenuOpen(false);
  };
  const goSettings = () => {
    useApp.getState().setSettingsSection("provider");
    setSurface("settings");
    setMenuOpen(false);
  };
  const signIn = () => {
    void cloudLoginStart();
    setMenuOpen(false);
  };

  return (
    <div
      ref={wrapRef}
      className="window-no-drag relative shrink-0 border-t border-border/40 px-2 pb-2 pt-1"
    >
      <button
        type="button"
        onClick={() => setMenuOpen((o) => !o)}
        aria-expanded={menuOpen}
        aria-haspopup="menu"
        className="flex w-full min-w-0 items-center justify-between gap-2 rounded-md px-2 py-1.5 text-left text-[11px] font-medium text-muted-foreground transition-colors hover:bg-muted/50 hover:text-foreground"
      >
        <span className="truncate">{label}</span>
        <ChevronDown
          className={cn(
            "size-3.5 shrink-0 opacity-60 transition-transform duration-200",
            menuOpen && "rotate-180",
          )}
          aria-hidden
        />
      </button>

      {menuOpen ? (
        <div
          role="menu"
          aria-label="Account menu"
          className="absolute bottom-full left-2 right-2 z-[100] mb-1 overflow-hidden rounded-lg border border-border/80 bg-surface py-1 shadow-lg"
        >
          <button
            type="button"
            role="menuitem"
            onClick={goDocs}
            className="flex w-full items-center gap-2 px-3 py-2 text-left text-xs text-foreground hover:bg-muted/60"
          >
            <BookOpen className="size-3.5 shrink-0 text-muted-foreground" />
            Docs
          </button>
          <button
            type="button"
            role="menuitem"
            onClick={goSettings}
            className="flex w-full items-center gap-2 px-3 py-2 text-left text-xs text-foreground hover:bg-muted/60"
          >
            <SlidersHorizontal className="size-3.5 shrink-0 text-muted-foreground" />
            Settings
          </button>
          <div className="my-1 h-px bg-border/60" role="separator" />
          {auth?.signed_in ? (
            <div className="space-y-1 px-3 py-2 text-[10px] leading-snug text-muted-foreground">
              <p className="font-medium text-foreground/90">Cloud session</p>
              {auth.handle ? (
                <p className="truncate">@{auth.handle}</p>
              ) : auth.server ? (
                <p className="truncate">{auth.server}</p>
              ) : (
                <p>Signed in</p>
              )}
              {auth.tier && (
                <p className="text-muted-foreground/80">
                  {auth.tier} tier
                  {auth.credits_remaining != null && auth.credits_total != null
                    ? ` · ${auth.credits_remaining}/${auth.credits_total} credits`
                    : ""}
                </p>
              )}
            </div>
          ) : (
            <button
              type="button"
              role="menuitem"
              onClick={signIn}
              className="flex w-full items-center gap-2 px-3 py-2 text-left text-xs text-foreground hover:bg-muted/60"
            >
              <LogIn className="size-3.5 shrink-0 text-muted-foreground" />
              Sign in
            </button>
          )}
        </div>
      ) : null}
    </div>
  );
}

function SettingsSidebarNav({
  active,
  onPick,
  onBackToChats,
}: {
  active: SettingsSectionId;
  onPick: (id: SettingsSectionId) => void;
  onBackToChats: () => void;
}) {
  return (
    <>
      <Button
        type="button"
        variant="ghost"
        size="sm"
        onClick={onBackToChats}
        className="mb-1 h-8 w-full justify-start gap-2 px-2 text-xs text-muted-foreground hover:text-foreground"
      >
        <ArrowLeft className="size-3.5 shrink-0" />
        Back to chats
      </Button>
      <SectionHeader label="Settings" />
      <nav
        className="mt-1 flex flex-col gap-0.5"
        role="navigation"
        aria-label="Settings categories"
      >
        {SETTINGS_NAV.map(({ id, label, icon: Icon }) => {
          const isActive = active === id;
          return (
            <button
              key={id}
              type="button"
              onClick={() => onPick(id)}
              className={cn(
                "flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-xs transition-colors",
                isActive
                  ? "bg-muted/90 font-medium text-foreground"
                  : "text-muted-foreground hover:bg-muted/50 hover:text-foreground",
              )}
              aria-current={isActive ? "page" : undefined}
            >
              <Icon className="size-3.5 shrink-0 opacity-85" strokeWidth={2} />
              <span className="truncate">{label}</span>
            </button>
          );
        })}
      </nav>
    </>
  );
}

function DocsSidebarNav({
  active,
  onPick,
  onBackToChats,
}: {
  active: DocSectionId;
  onPick: (id: DocSectionId) => void;
  onBackToChats: () => void;
}) {
  return (
    <>
      <Button
        type="button"
        variant="ghost"
        size="sm"
        onClick={onBackToChats}
        className="mb-1 h-8 w-full justify-start gap-2 px-2 text-xs text-muted-foreground hover:text-foreground"
      >
        <ArrowLeft className="size-3.5 shrink-0" />
        Back to chats
      </Button>
      <SectionHeader label="Docs" />
      <nav
        className="mt-1 flex flex-col gap-0.5"
        role="navigation"
        aria-label="Docs guides"
      >
        {DOCS_NAV.map(({ id, label, icon: Icon }) => {
          const isActive = active === id;
          return (
            <button
              key={id}
              type="button"
              onClick={() => onPick(id)}
              className={cn(
                "flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-xs transition-colors",
                isActive
                  ? "bg-muted/90 font-medium text-foreground"
                  : "text-muted-foreground hover:bg-muted/50 hover:text-foreground",
              )}
              aria-current={isActive ? "page" : undefined}
            >
              <Icon className="size-3.5 shrink-0 opacity-85" strokeWidth={2} />
              <span className="truncate">{label}</span>
            </button>
          );
        })}
      </nav>
    </>
  );
}

function Header() {
  return (
    <header className="window-drag flex h-11 min-w-0 shrink-0 items-center gap-2 px-3 pl-14">
      <img
        src="/logo.png"
        alt="ThinkingRoot logo"
        draggable={false}
        className="window-no-drag h-5 w-5 shrink-0 object-contain opacity-80"
      />
      <h1 className="window-no-drag min-w-0 shrink-0 text-sm font-medium tracking-tight whitespace-nowrap">
        ThinkingRoot
      </h1>
    </header>
  );
}

function PrimaryActions({
  onNewConversation,
  hasWorkspaces,
  activeWorkspace,
}: {
  surface: Surface;
  setSurface: (s: Surface) => void;
  activeWorkspace: string | null;
  hasWorkspaces: boolean;
  onNewConversation: () => void;
}) {
  return (
    <div className="flex flex-col">
      <Button
        variant="ghost"
        size="sm"
        onClick={onNewConversation}
        className="h-auto min-h-8 w-full min-w-0 justify-start gap-2 px-1.5 py-1.5 text-left text-xs font-medium text-foreground/80 hover:bg-muted/50 hover:text-foreground"
        disabled={!hasWorkspaces}
        title={
          activeWorkspace
            ? `New chat in ${activeWorkspace}`
            : hasWorkspaces
              ? "Picks the first workspace"
              : "Add a workspace first"
        }
      >
        <SquarePen className="size-4 shrink-0 text-muted-foreground" />
        <span className="min-w-0 whitespace-normal break-words leading-snug">
          New conversation
        </span>
      </Button>
    </div>
  );
}

function WorkspaceRow({
  workspace,
  surface,
  activeWorkspace,
  activeConv,
  onSelectWorkspace,
  onSelectConv,
  onNewChat,
}: {
  workspace: WorkspaceWithConvs;
  surface: Surface;
  activeWorkspace: string | null;
  activeConv: string | null;
  onSelectWorkspace: () => void;
  onSelectConv: (id: string) => void;
  onNewChat: () => void;
}) {
  const isActive = surface !== "settings" && workspace.name === activeWorkspace;
  const [showAllChats, setShowAllChats] = useState(false);
  const convs = workspace.conversations;
  const hiddenCount = Math.max(0, convs.length - MAX_PINNED_CONVS);

  useEffect(() => {
    if (convs.length <= MAX_PINNED_CONVS) setShowAllChats(false);
  }, [convs.length, workspace.name]);

  const displayed =
    showAllChats || hiddenCount === 0 ? convs : convs.slice(0, MAX_PINNED_CONVS);

  return (
    <li
      className={cn(
        "mb-3 box-border min-w-0 max-w-full rounded-lg px-1 py-0.5 last:mb-1",
        isActive && "mx-0.5 bg-muted/35",
      )}
    >
      <div className="group/workspace-head flex min-w-0 items-start gap-0.5">
        <button
          type="button"
          onClick={onSelectWorkspace}
          className={cn(
            "flex min-w-0 flex-1 items-start gap-2 rounded-md px-2 py-1.5 text-left text-[11px] font-medium tracking-tight transition-colors",
            isActive
              ? "text-foreground"
              : "text-muted-foreground hover:text-foreground/90",
          )}
          title={workspace.path}
        >
          <span className="min-w-0 flex-1 whitespace-normal break-words text-pretty">
            {workspace.name}
          </span>
          {!workspace.compiled ? (
            <span className="shrink-0 pt-0.5 text-[9px] font-normal uppercase tracking-wide text-muted-foreground/55">
              new
            </span>
          ) : null}
        </button>
        <button
          type="button"
          onClick={(e) => {
            e.stopPropagation();
            onNewChat();
          }}
          className={cn(
            "window-no-drag mt-0.5 flex size-6 shrink-0 items-center justify-center rounded-md",
            "text-muted-foreground transition-[opacity,background-color,color]",
            "opacity-0 hover:bg-muted hover:text-foreground",
            "group-hover/workspace-head:opacity-100 group-focus-within/workspace-head:opacity-100",
            "[@media(pointer:coarse)]:opacity-100",
          )}
          title={`New chat in ${workspace.name}`}
          aria-label={`New chat in ${workspace.name}`}
        >
          <Plus className="size-3.5" strokeWidth={2} />
        </button>
      </div>

      <ul className="mt-0.5 flex min-w-0 flex-col gap-0.5">
        {workspace.conversations.length === 0 ? (
          <li>
            <div className="rounded-sm py-1.5 pl-3 pr-2 text-left text-[11px] italic leading-snug text-muted-foreground/80">
              No conversations yet.
            </div>
          </li>
        ) : null}
        {displayed.map((c) => {
          const selected = surface !== "settings" && activeConv === c.id;
          return (
            <li key={c.id} className="min-w-0">
              <button
                type="button"
                onClick={() => onSelectConv(c.id)}
                aria-current={selected ? "true" : undefined}
                className={cn(
                  "w-full rounded-sm py-1 pl-3 pr-2.5 text-left transition-colors",
                  selected
                    ? "bg-muted/80 text-foreground"
                    : "text-muted-foreground hover:bg-muted/50 hover:text-foreground",
                )}
                title={c.title}
              >
                <span className="flex min-w-0 items-center gap-1.5">
                  <span
                    className={cn(
                      "shrink-0 text-[9px] leading-none",
                      selected
                        ? "text-muted-foreground/80"
                        : "text-muted-foreground/45",
                    )}
                    aria-hidden
                  >
                    {selected ? "●" : "○"}
                  </span>
                  <span
                    className={cn(
                      "min-w-0 truncate text-[10.5px] leading-snug",
                      selected ? "font-medium" : "font-normal",
                    )}
                  >
                    {c.title}
                  </span>
                </span>
              </button>
            </li>
          );
        })}
        {!showAllChats && hiddenCount > 0 ? (
          <li>
            <button
              type="button"
              onClick={() => setShowAllChats(true)}
              className="w-full rounded-md py-1.5 pl-3 pr-2 text-left text-[11px] leading-snug text-muted-foreground transition-colors hover:bg-muted/40 hover:text-foreground"
            >
              +{hiddenCount} more chats
            </button>
          </li>
        ) : null}
        {showAllChats && hiddenCount > 0 ? (
          <li>
            <button
              type="button"
              onClick={() => setShowAllChats(false)}
              className="w-full rounded-md py-1.5 pl-3 pr-2 text-left text-[11px] leading-snug text-muted-foreground/90 transition-colors hover:bg-muted/40 hover:text-foreground"
            >
              Show fewer
            </button>
          </li>
        ) : null}
      </ul>
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
    <div className="mt-4 flex min-h-6 w-full min-w-0 items-center justify-between gap-2 px-2">
      <span className="min-w-0 truncate text-[10px] font-semibold uppercase tracking-widest text-muted-foreground">
        {label}
      </span>
      {right ? (
        <div className="flex shrink-0 items-center gap-0.5">{right}</div>
      ) : null}
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
      className="flex size-5 shrink-0 items-center justify-center rounded text-muted-foreground/60 transition-colors hover:bg-muted/50 hover:text-foreground disabled:opacity-40"
      {...rest}
    >
      {children}
    </button>
  );
}

