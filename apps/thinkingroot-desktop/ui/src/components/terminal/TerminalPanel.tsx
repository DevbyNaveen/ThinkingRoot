/**
 * Right-rail Terminal panel.
 *
 * Owns one or more PTY-backed shell sessions surfaced as a tab strip.
 * Each session is held in a `TerminalController` instance that persists
 * for the lifetime of this panel (across rail-tab switches and rail
 * width drags), so xterm scrollback and the live shell process survive
 * the user toggling between Compile / Brain / Terminal tabs.
 *
 * Layout
 * ──────
 *
 *   ┌──────────────────────────────────────────────────────┐
 *   │  zsh · thinkingroot  ×  │ + │       (tab strip)      │
 *   ├──────────────────────────────────────────────────────┤
 *   │                                                      │
 *   │           xterm canvas (active session)              │
 *   │                                                      │
 *   └──────────────────────────────────────────────────────┘
 *
 * Inactive sessions stay mounted with `display: none` so their xterm
 * canvas is not torn down when the user switches tabs.
 *
 * Honesty rules enforced (CLAUDE.md):
 *  - PTY spawn errors surface verbatim in the panel header — no toast
 *    that pretends success.
 *  - When a shell exits the tab gets an "exited" pill and a
 *    "Restart" action; we never let a dead PTY look alive.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Plus, RefreshCw, Terminal as TerminalIcon, X } from "lucide-react";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { useApp } from "@/store/app";
import { TerminalController } from "@/lib/terminal";
import { workspaceList } from "@/lib/tauri";

interface OpenTab {
  id: string;
  controller: TerminalController;
  /** Tracks the title shown in the tab strip — updated from OSC 0/2. */
  title: string;
  /** True after the underlying shell exited; the tab stays in place
   *  so the user can read final output / scrollback. */
  exited: boolean;
}

interface Props {
  /** Whether the rail tab is currently the active one. Used purely
   *  for `focus()` after attach so the user can start typing
   *  immediately when they open the Terminal tab. */
  isActive: boolean;
}

export function TerminalPanel({ isActive }: Props) {
  const activeWorkspace = useApp((s) => s.activeWorkspace);
  const theme = useApp((s) => s.theme);

  const [tabs, setTabs] = useState<OpenTab[]>([]);
  const [activeId, setActiveId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [opening, setOpening] = useState(false);

  // Map from session id → DOM container element; the controller's
  // `attach(container)` is called once we have both.
  const containersRef = useRef<Map<string, HTMLDivElement>>(new Map());
  // Tracks which controllers we have already attached to DOM, so a
  // re-render does not re-attach (xterm rejects double-open).
  const attachedRef = useRef<Set<string>>(new Set());
  // Tracks the currently-known cwd so a workspace switch can suggest
  // (but does not auto-create) a fresh terminal.
  const lastCwdRef = useRef<string | null>(null);

  /** Resolve the active workspace's absolute path. */
  const resolveCwd = useCallback(async (): Promise<string | null> => {
    if (!activeWorkspace) return null;
    try {
      const list = await workspaceList();
      return list.find((w) => w.name === activeWorkspace)?.path ?? null;
    } catch (err) {
      console.warn("[terminal] workspaceList failed", err);
      return null;
    }
  }, [activeWorkspace]);

  /** Open a new shell session in the active workspace's cwd. */
  const openTab = useCallback(async () => {
    setError(null);
    setOpening(true);
    try {
      const cwd = await resolveCwd();
      lastCwdRef.current = cwd;
      const controller = await TerminalController.spawn({ cwd });
      controller.setEventHandlers({
        onTitle: (title) => {
          setTabs((prev) =>
            prev.map((t) => (t.id === controller.session.id ? { ...t, title } : t)),
          );
        },
        onExit: () => {
          setTabs((prev) =>
            prev.map((t) =>
              t.id === controller.session.id ? { ...t, exited: true } : t,
            ),
          );
        },
      });
      const next: OpenTab = {
        id: controller.session.id,
        controller,
        title: controller.session.title,
        exited: false,
      };
      setTabs((prev) => [...prev, next]);
      setActiveId(controller.session.id);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setOpening(false);
    }
  }, [resolveCwd]);

  /** Close + dispose a tab. */
  const closeTab = useCallback((id: string) => {
    setTabs((prev) => {
      const target = prev.find((t) => t.id === id);
      if (!target) return prev;
      void target.controller.dispose();
      attachedRef.current.delete(id);
      containersRef.current.delete(id);
      const next = prev.filter((t) => t.id !== id);
      // Move focus to a neighbour tab if we just closed the active one.
      setActiveId((current) => {
        if (current !== id) return current;
        const fallback = next[next.length - 1];
        return fallback ? fallback.id : null;
      });
      return next;
    });
  }, []);

  /** Restart a dead session: dispose old controller, spawn new with same cwd. */
  const restartTab = useCallback(async (id: string) => {
    const target = tabs.find((t) => t.id === id);
    if (!target) return;
    const cwd = target.controller.session.cwd;
    closeTab(id);
    try {
      const controller = await TerminalController.spawn({ cwd });
      controller.setEventHandlers({
        onTitle: (title) => {
          setTabs((prev) =>
            prev.map((t) => (t.id === controller.session.id ? { ...t, title } : t)),
          );
        },
        onExit: () => {
          setTabs((prev) =>
            prev.map((t) =>
              t.id === controller.session.id ? { ...t, exited: true } : t,
            ),
          );
        },
      });
      const next: OpenTab = {
        id: controller.session.id,
        controller,
        title: controller.session.title,
        exited: false,
      };
      setTabs((prev) => [...prev, next]);
      setActiveId(controller.session.id);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }, [tabs, closeTab]);

  // Auto-spawn the first session the very first time the user enters
  // the Terminal tab with a workspace selected. Avoids an empty
  // "click + to begin" state for the common case while still letting
  // power users open multiple shells via the + button.
  useEffect(() => {
    if (!isActive) return;
    if (tabs.length > 0) return;
    if (opening) return;
    void openTab();
    // We deliberately do not depend on `tabs.length` to avoid an open
    // loop if openTab errors — `opening` flips while in flight.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [isActive]);

  // Attach controllers to their containers once both sides are ready.
  // Runs after every render so a freshly-mounted container picks up
  // its controller on the next pass.
  useEffect(() => {
    for (const tab of tabs) {
      if (attachedRef.current.has(tab.id)) continue;
      const container = containersRef.current.get(tab.id);
      if (!container) continue;
      tab.controller.attach(container);
      attachedRef.current.add(tab.id);
    }
  }, [tabs]);

  // Push focus into the active session whenever the rail tab becomes
  // active so the user can start typing immediately.
  useEffect(() => {
    if (!isActive) return;
    if (!activeId) return;
    const tab = tabs.find((t) => t.id === activeId);
    if (!tab) return;
    // requestAnimationFrame so the visibility flip has flushed first.
    const handle = requestAnimationFrame(() => tab.controller.focus());
    return () => cancelAnimationFrame(handle);
  }, [isActive, activeId, tabs]);

  // Re-fit the active terminal whenever the panel becomes visible —
  // ResizeObserver does not fire when only `display` toggles.
  useEffect(() => {
    if (!isActive || !activeId) return;
    const tab = tabs.find((t) => t.id === activeId);
    tab?.controller.fitToContainer();
  }, [isActive, activeId, tabs]);

  // Re-apply the xterm theme when the app theme switches.
  useEffect(() => {
    for (const tab of tabs) tab.controller.applyTheme();
  }, [theme, tabs]);

  // Dispose every controller on unmount. Tauri's `terminal_close`
  // kills the underlying child shell so no `claude` / `root serve`
  // outlives this panel.
  useEffect(() => {
    return () => {
      for (const tab of tabs) {
        void tab.controller.dispose();
      }
      attachedRef.current.clear();
      containersRef.current.clear();
    };
    // We intentionally drop tabs from deps — we want this cleanup to
    // run only on unmount, not on every tab change.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const activeTab = useMemo(() => tabs.find((t) => t.id === activeId) ?? null, [tabs, activeId]);

  return (
    <div className="flex h-full min-h-0 flex-col bg-background/40">
      {/* Tab strip */}
      <div className="flex h-9 shrink-0 items-stretch gap-0.5 overflow-x-auto border-b border-border/60 bg-surface/40 px-1">
        {tabs.map((tab) => (
          <TabHandle
            key={tab.id}
            tab={tab}
            active={tab.id === activeId}
            onSelect={() => setActiveId(tab.id)}
            onClose={() => closeTab(tab.id)}
            onRestart={() => restartTab(tab.id)}
          />
        ))}
        <Button
          variant="ghost"
          size="icon"
          className="my-1 h-7 w-7 shrink-0 self-center text-muted-foreground/70 hover:text-foreground"
          onClick={openTab}
          disabled={opening}
          aria-label="New terminal"
          title="New terminal"
        >
          <Plus className="size-3.5" />
        </Button>
      </div>

      {/* Error banner */}
      {error && (
        <div className="flex shrink-0 items-center gap-2 border-b border-rose-500/30 bg-rose-500/10 px-3 py-1.5 text-[11px] text-rose-300">
          <span className="font-medium">Terminal error:</span>
          <span className="truncate font-mono">{error}</span>
          <Button
            variant="ghost"
            size="icon"
            className="ml-auto h-5 w-5 text-rose-300 hover:text-rose-100"
            onClick={() => setError(null)}
            aria-label="Dismiss"
          >
            <X className="size-3" />
          </Button>
        </div>
      )}

      {/* Empty state — shown only when the user closed every tab */}
      {tabs.length === 0 && !opening && (
        <div className="flex min-h-0 flex-1 flex-col items-center justify-center gap-3 px-6 text-center">
          <TerminalIcon className="size-7 text-muted-foreground/50" />
          <div className="space-y-1">
            <p className="text-xs font-medium text-foreground">No terminal open</p>
            <p className="text-[11px] text-muted-foreground">
              Spawns your default login shell in
              {activeWorkspace ? ` the active workspace` : " your home directory"}.
            </p>
          </div>
          <Button
            variant="outline"
            size="sm"
            className="h-8 gap-1.5 rounded-xl px-3 text-xs"
            onClick={openTab}
            disabled={opening}
          >
            <Plus className="size-3" />
            New terminal
          </Button>
        </div>
      )}

      {/* xterm containers — one per session, only the active one is
          visible. Keeping them all mounted preserves scrollback and
          avoids re-attaching the renderer on every tab switch. */}
      <div className="relative min-h-0 flex-1">
        {tabs.map((tab) => (
          <div
            key={tab.id}
            ref={(el) => {
              if (el) {
                containersRef.current.set(tab.id, el);
              } else {
                containersRef.current.delete(tab.id);
              }
            }}
            className={cn(
              "absolute inset-0 px-1 pb-1 pt-1",
              tab.id === activeId ? "block" : "hidden",
            )}
            // xterm steals focus on click; ensure clicks elsewhere in
            // the rail still work by stopping at the container.
          />
        ))}
      </div>

      {/* Footer status — exited / shell / pid */}
      {activeTab && (
        <footer className="flex shrink-0 items-center gap-2 border-t border-border/60 bg-surface/40 px-3 py-1 text-[10px] text-muted-foreground/80">
          <span className="font-mono">
            {activeTab.controller.session.shell}
            {activeTab.controller.session.pid !== null
              ? ` · pid ${activeTab.controller.session.pid}`
              : ""}
          </span>
          {activeTab.exited && (
            <span className="rounded-full bg-amber-500/15 px-1.5 py-0.5 font-mono uppercase tracking-wider text-amber-400">
              exited
            </span>
          )}
          <span className="ml-auto truncate font-mono" title={activeTab.controller.session.cwd}>
            {activeTab.controller.session.cwd.replace(
              /^\/Users\/[^/]+|^\/home\/[^/]+/,
              "~",
            )}
          </span>
        </footer>
      )}
    </div>
  );
}

interface TabHandleProps {
  tab: OpenTab;
  active: boolean;
  onSelect: () => void;
  onClose: () => void;
  onRestart: () => void;
}

function TabHandle({ tab, active, onSelect, onClose, onRestart }: TabHandleProps) {
  return (
    <div
      className={cn(
        "group flex h-7 shrink-0 cursor-pointer select-none items-center gap-1.5 self-center rounded-md px-2 text-[11px] transition-colors",
        active
          ? "bg-muted text-foreground"
          : "text-muted-foreground/70 hover:bg-muted/50 hover:text-foreground",
      )}
      onClick={onSelect}
      title={tab.controller.session.cwd}
    >
      <TerminalIcon className="size-3 shrink-0" />
      <span className="max-w-[140px] truncate font-mono">
        {tab.title}
      </span>
      {tab.exited && (
        <button
          type="button"
          className="text-muted-foreground/70 hover:text-emerald-400"
          onClick={(e) => {
            e.stopPropagation();
            void onRestart();
          }}
          title="Restart shell"
          aria-label="Restart shell"
        >
          <RefreshCw className="size-3" />
        </button>
      )}
      <button
        type="button"
        className="text-muted-foreground/60 opacity-0 transition-opacity group-hover:opacity-100 hover:text-foreground"
        onClick={(e) => {
          e.stopPropagation();
          onClose();
        }}
        title="Close terminal"
        aria-label="Close terminal"
      >
        <X className="size-3" />
      </button>
    </div>
  );
}
