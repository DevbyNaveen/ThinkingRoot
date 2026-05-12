/**
 * In-app browser for the right rail.
 *
 * Implementation choice: this is a real native child WebView via Tauri,
 * not an iframe. Iframes fail on most sites because of
 * `X-Frame-Options` and CSP. The React code here owns the *chrome*
 * (tabs, URL bar, find bar, downloads tray, etc.); Rust owns the
 * native WebView handles and emits lifecycle events.
 *
 * Keyboard model: shortcuts live on the document. They fire when the
 * React chrome has focus. When the WebView body has focus, the
 * WebView captures keys natively and our shortcuts don't run — the
 * user can press Escape or click the chrome to give focus back. A
 * proper global-accelerator wiring is a separate slice.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { openUrl, revealItemInDir } from "@tauri-apps/plugin-opener";
import {
  ArrowLeft,
  ArrowRight,
  Bug,
  Check,
  ChevronDown,
  Download,
  ExternalLink,
  FolderOpen,
  Globe2,
  Lock,
  MoreVertical,
  Pin,
  PinOff,
  Plus,
  Printer,
  RefreshCw,
  Save,
  Search,
  Star,
  X,
  ZoomIn,
  ZoomOut,
} from "lucide-react";

import { Button } from "@/components/ui/button";
import {
  browserBack,
  browserClose,
  browserDevtools,
  browserFind,
  browserFindClear,
  browserFocus,
  browserForward,
  browserHide,
  browserNavigate,
  browserOpen,
  browserPrint,
  browserReload,
  browserSavePage,
  browserSetBounds,
  browserShow,
  browserZoom,
  listenBrowserEvent,
  onWorkspacesChanged,
  workspaceList,
  type BrowserBounds,
  type BrowserEvent,
  type BrowserSavePageResult,
  type BrowserSessionInfo,
  type WorkspaceView,
} from "@/lib/tauri";
import { useApp } from "@/store/app";
import { toast } from "@/store/toast";
import { cn } from "@/lib/utils";

const DEFAULT_URL = "https://www.google.com";
const HISTORY_KEY = "thinkingroot.browser.history";
const BOOKMARKS_KEY = "thinkingroot.browser.bookmarks";
const SESSION_KEY = "thinkingroot.browser.session.v1";
const DOWNLOADS_KEY = "thinkingroot.browser.downloads.v1";
const DOWNLOADS_MAX = 50;
const ZOOM_STEP = 0.1;
const ZOOM_MIN = 0.25;
const ZOOM_MAX = 5;

interface BrowserTab {
  id: string;
  info: BrowserSessionInfo;
  title: string;
  url: string;
  input: string;
  loading: boolean;
  error: string | null;
  pinned: boolean;
  zoom: number;
}

interface StoredLink {
  title: string;
  url: string;
  at: number;
}

interface SessionTab {
  url: string;
  title: string;
  pinned: boolean;
  zoom: number;
}

interface SessionState {
  v: 1;
  tabs: SessionTab[];
}

interface DownloadEntry {
  url: string;
  path: string | null;
  filename: string;
  at: number;
  status: "in_progress" | "done" | "failed";
}

interface ContextMenuState {
  x: number;
  y: number;
  tabId: string;
}

function loadLinks(key: string): StoredLink[] {
  try {
    const raw = window.localStorage.getItem(key);
    if (!raw) return [];
    const parsed = JSON.parse(raw);
    return Array.isArray(parsed) ? parsed.slice(0, 100) : [];
  } catch {
    return [];
  }
}

function saveLinks(key: string, links: StoredLink[]) {
  try {
    window.localStorage.setItem(key, JSON.stringify(links.slice(0, 100)));
  } catch {
    // QuotaExceeded — fail open, we don't care about losing history
    // urgently. Bookmarks shouldn't ever be this big.
  }
}

function remember(key: string, link: StoredLink) {
  const next = [link, ...loadLinks(key).filter((l) => l.url !== link.url)];
  saveLinks(key, next);
}

function loadSession(): SessionState | null {
  try {
    const raw = window.localStorage.getItem(SESSION_KEY);
    if (!raw) return null;
    const parsed = JSON.parse(raw) as SessionState;
    if (parsed.v !== 1 || !Array.isArray(parsed.tabs)) return null;
    return parsed;
  } catch {
    return null;
  }
}

function saveSession(tabs: BrowserTab[]) {
  const payload: SessionState = {
    v: 1,
    tabs: tabs.map((t) => ({
      url: t.url,
      title: t.title,
      pinned: t.pinned,
      zoom: t.zoom,
    })),
  };
  try {
    window.localStorage.setItem(SESSION_KEY, JSON.stringify(payload));
  } catch {
    // fail open
  }
}

function loadDownloads(): DownloadEntry[] {
  try {
    const raw = window.localStorage.getItem(DOWNLOADS_KEY);
    if (!raw) return [];
    const parsed = JSON.parse(raw);
    return Array.isArray(parsed) ? parsed.slice(0, DOWNLOADS_MAX) : [];
  } catch {
    return [];
  }
}

function saveDownloads(entries: DownloadEntry[]) {
  try {
    window.localStorage.setItem(
      DOWNLOADS_KEY,
      JSON.stringify(entries.slice(0, DOWNLOADS_MAX)),
    );
  } catch {
    // fail open
  }
}

/** Leading icon for the omnibox from the *committed* navigation URL (not draft input). */
function OmniboxSecurityGlyph({ url }: { url: string }) {
  const raw = url.trim();
  if (!raw) {
    return <Search className="size-3.5 shrink-0 text-muted-foreground" aria-hidden />;
  }
  try {
    const href = raw.includes("://") ? raw : `https://${raw}`;
    const u = new URL(href);
    if (u.protocol === "https:") {
      return (
        <span title="Connection is secure (HTTPS)">
          <Lock className="size-3.5 shrink-0 text-emerald-500/85 dark:text-emerald-400/75" aria-hidden />
        </span>
      );
    }
    if (u.protocol === "http:") {
      return (
        <span title="Not secure (HTTP)">
          <Globe2 className="size-3.5 shrink-0 text-amber-500/90" aria-hidden />
        </span>
      );
    }
  } catch {
    /* treat as search / non-URL entry */
  }
  return <Search className="size-3.5 shrink-0 text-muted-foreground" aria-hidden />;
}

function filenameFromPath(path: string | null | undefined, url: string): string {
  if (path) {
    const parts = path.split(/[\\/]/);
    const last = parts[parts.length - 1];
    if (last) return last;
  }
  try {
    const u = new URL(url);
    const segs = u.pathname.split("/").filter(Boolean);
    return segs[segs.length - 1] || u.host;
  } catch {
    return "download";
  }
}

function displayTitle(tab: BrowserTab): string {
  if (tab.title && tab.title !== "New tab") return tab.title;
  try {
    return new URL(tab.url).host || tab.url;
  } catch {
    return tab.url || "New tab";
  }
}

function isBookmarked(url: string, bookmarks: StoredLink[]): boolean {
  return bookmarks.some((b) => b.url === url);
}

/**
 * Sort tabs so pinned ones come first while preserving the original
 * order *within* pinned and non-pinned groups. Stable so user
 * reordering doesn't get clobbered.
 */
function orderTabs(tabs: BrowserTab[]): BrowserTab[] {
  const pinned = tabs.filter((t) => t.pinned);
  const rest = tabs.filter((t) => !t.pinned);
  return [...pinned, ...rest];
}

export function BrowserPanel({ isActive }: { isActive: boolean }) {
  const viewportRef = useRef<HTMLDivElement | null>(null);
  const unlistenersRef = useRef<Map<string, () => void>>(new Map());
  const restoredRef = useRef(false);
  const [tabs, setTabs] = useState<BrowserTab[]>([]);
  const [activeId, setActiveId] = useState<string | null>(null);
  /** URL/search typed before any tab exists (omnibox is otherwise bound to `activeTab`). */
  const [urlDraft, setUrlDraft] = useState("");
  const [opening, setOpening] = useState(false);
  const [panelError, setPanelError] = useState<string | null>(null);
  const [history, setHistory] = useState<StoredLink[]>(() => loadLinks(HISTORY_KEY));
  const [bookmarks, setBookmarks] = useState<StoredLink[]>(() => loadLinks(BOOKMARKS_KEY));
  const [showLibrary, setShowLibrary] = useState<"history" | "bookmarks" | null>(null);

  // ── Save Page state ─────────────────────────────────────────────
  // The save target is the user's currently-active workspace (read
  // from the global app store, kept in sync by the sidebar). When
  // no workspace is active — first-launch path, or the user removed
  // every workspace — we fall back to "playground" which the boot
  // hook ensures is always registered. The workspace list itself is
  // cached locally so the Save button's tooltip can show the resolved
  // target without an IPC round-trip per render.
  const activeWorkspace = useApp((s) => s.activeWorkspace);
  const [workspaces, setWorkspaces] = useState<WorkspaceView[]>([]);
  const [saving, setSaving] = useState(false);
  /** Maps compile workspace label → toast id so the in-flight toast
   *  for that compile can be updated when the matching `Done` event
   *  arrives. */
  const saveToastsRef = useRef<Map<string, number>>(new Map());

  // Phase 0 additions
  const [findOpen, setFindOpen] = useState(false);
  const [findQuery, setFindQuery] = useState("");
  const [findCaseSensitive, setFindCaseSensitive] = useState(false);
  const findInputRef = useRef<HTMLInputElement | null>(null);
  const [devtoolsOpen, setDevtoolsOpen] = useState<Set<string>>(new Set());
  const [downloads, setDownloads] = useState<DownloadEntry[]>(() => loadDownloads());
  const [downloadsOpen, setDownloadsOpen] = useState(false);
  const [overflowOpen, setOverflowOpen] = useState(false);
  const [contextMenu, setContextMenu] = useState<ContextMenuState | null>(null);
  const [tabSearchOpen, setTabSearchOpen] = useState(false);
  const [tabSearchQuery, setTabSearchQuery] = useState("");

  const orderedTabs = useMemo(() => orderTabs(tabs), [tabs]);
  const activeTab = useMemo(
    () => tabs.find((t) => t.id === activeId) ?? null,
    [tabs, activeId],
  );

  // Persist tab list whenever it changes.
  useEffect(() => {
    if (!restoredRef.current) return;
    saveSession(tabs);
  }, [tabs]);

  const currentBounds = useCallback((): BrowserBounds => {
    const el = viewportRef.current;
    if (!el) return { x: 0, y: 0, width: 1, height: 1 };
    const rect = el.getBoundingClientRect();
    return {
      x: rect.left,
      y: rect.top,
      width: Math.max(1, rect.width),
      height: Math.max(1, rect.height),
    };
  }, []);

  const updateBounds = useCallback(() => {
    if (!activeId || !isActive) return;
    const bounds = currentBounds();
    void browserSetBounds(activeId, bounds).catch((err) => {
      setPanelError(err instanceof Error ? err.message : String(err));
    });
  }, [activeId, currentBounds, isActive]);

  const applyVisibility = useCallback(() => {
    for (const tab of tabs) {
      const shouldShow = isActive && tab.id === activeId;
      void (shouldShow ? browserShow(tab.id) : browserHide(tab.id)).catch(() => {});
      if (shouldShow) {
        void browserSetBounds(tab.id, currentBounds()).catch(() => {});
      }
    }
  }, [activeId, currentBounds, isActive, tabs]);

  const recordDownload = useCallback((event: Extract<BrowserEvent, { kind: "download" }>) => {
    setDownloads((prev) => {
      const filename = filenameFromPath(event.path, event.url);
      const existing = prev.find((d) => d.url === event.url);
      const status: DownloadEntry["status"] =
        event.success === true ? "done" : event.success === false ? "failed" : "in_progress";
      let next: DownloadEntry[];
      if (existing) {
        next = prev.map((d) =>
          d.url === event.url
            ? {
                ...d,
                path: event.path ?? d.path,
                filename: filename || d.filename,
                status,
              }
            : d,
        );
      } else {
        next = [
          {
            url: event.url,
            path: event.path ?? null,
            filename,
            at: Date.now(),
            status,
          },
          ...prev,
        ];
      }
      next = next.slice(0, DOWNLOADS_MAX);
      saveDownloads(next);
      return next;
    });
  }, []);

  const wireEvents = useCallback(
    async (session: BrowserSessionInfo) => {
      const unlisten = await listenBrowserEvent(session.event, (event: BrowserEvent) => {
        if (event.kind === "download") {
          recordDownload(event);
          // Surface a failed-download chip on the tab too so the user
          // notices without opening the tray.
          if (event.success === false) {
            setTabs((prev) =>
              prev.map((tab) =>
                tab.id === session.id
                  ? { ...tab, error: `Download failed: ${event.url}` }
                  : tab,
              ),
            );
          }
          return;
        }
        setTabs((prev) =>
          prev.map((tab) => {
            if (tab.id !== session.id) return tab;
            switch (event.kind) {
              case "loading":
                return { ...tab, url: event.url, input: event.url, loading: true, error: null };
              case "loaded": {
                const next = { ...tab, url: event.url, input: event.url, loading: false };
                remember(HISTORY_KEY, {
                  title: displayTitle(next),
                  url: event.url,
                  at: Date.now(),
                });
                setHistory(loadLinks(HISTORY_KEY));
                return next;
              }
              case "title":
                return { ...tab, title: event.title };
              case "navigation":
                return { ...tab, url: event.url, input: event.url };
              case "new_window":
                void openTab(event.url);
                return tab;
              default:
                return tab;
            }
          }),
        );
      });
      unlistenersRef.current.set(session.id, unlisten);
      // eslint-disable-next-line react-hooks/exhaustive-deps
    },
    [recordDownload],
  );

  const openTab = useCallback(
    async (
      url = DEFAULT_URL,
      opts: { pinned?: boolean; zoom?: number; activate?: boolean } = {},
    ) => {
      setOpening(true);
      setPanelError(null);
      try {
        const session = await browserOpen({
          url,
          bounds: currentBounds(),
          title: "New tab",
        });
        const tab: BrowserTab = {
          id: session.id,
          info: session,
          title: session.title,
          url: session.url,
          input: session.url,
          loading: true,
          error: null,
          pinned: opts.pinned ?? false,
          zoom: opts.zoom ?? 1,
        };
        await wireEvents(session);
        setTabs((prev) => [...prev, tab]);
        if (opts.activate !== false) setActiveId(session.id);
        setUrlDraft("");
        if (tab.zoom !== 1) {
          void browserZoom(session.id, tab.zoom).catch(() => {});
        }
        return tab;
      } catch (err) {
        setPanelError(err instanceof Error ? err.message : String(err));
        return null;
      } finally {
        setOpening(false);
      }
    },
    [currentBounds, wireEvents],
  );

  const closeTab = useCallback(
    (id: string) => {
      const tab = tabs.find((t) => t.id === id);
      if (tab?.pinned) return; // pinned tabs don't close via X (use context menu)
      unlistenersRef.current.get(id)?.();
      unlistenersRef.current.delete(id);
      void browserClose(id).catch(() => {});
      setTabs((prev) => {
        const next = prev.filter((t) => t.id !== id);
        setActiveId((current) => {
          if (current !== id) return current;
          return next[next.length - 1]?.id ?? null;
        });
        return next;
      });
    },
    [tabs],
  );

  const closeOtherTabs = useCallback(
    (keepId: string) => {
      const toClose = tabs.filter((t) => t.id !== keepId && !t.pinned);
      for (const tab of toClose) {
        unlistenersRef.current.get(tab.id)?.();
        unlistenersRef.current.delete(tab.id);
        void browserClose(tab.id).catch(() => {});
      }
      setTabs((prev) => prev.filter((t) => t.id === keepId || t.pinned));
      setActiveId(keepId);
    },
    [tabs],
  );

  const duplicateTab = useCallback(
    (id: string) => {
      const tab = tabs.find((t) => t.id === id);
      if (!tab) return;
      void openTab(tab.url, { pinned: false, zoom: tab.zoom, activate: true });
    },
    [tabs, openTab],
  );

  const togglePin = useCallback((id: string) => {
    setTabs((prev) =>
      prev.map((t) => (t.id === id ? { ...t, pinned: !t.pinned } : t)),
    );
  }, []);

  const navigateActive = useCallback(
    async (value?: string) => {
      if (!activeTab) return;
      const target = (value ?? activeTab.input).trim();
      if (!target) return;
      setTabs((prev) =>
        prev.map((t) =>
          t.id === activeTab.id ? { ...t, loading: true, error: null, input: target } : t,
        ),
      );
      try {
        const normalized = await browserNavigate(activeTab.id, target);
        setTabs((prev) =>
          prev.map((t) =>
            t.id === activeTab.id ? { ...t, url: normalized, input: normalized } : t,
          ),
        );
      } catch (err) {
        setTabs((prev) =>
          prev.map((t) =>
            t.id === activeTab.id
              ? {
                  ...t,
                  loading: false,
                  error: err instanceof Error ? err.message : String(err),
                }
              : t,
          ),
        );
      }
    },
    [activeTab],
  );

  const toggleBookmark = useCallback(() => {
    if (!activeTab) return;
    const existing = isBookmarked(activeTab.url, bookmarks);
    const next = existing
      ? bookmarks.filter((b) => b.url !== activeTab.url)
      : [
          {
            title: displayTitle(activeTab),
            url: activeTab.url,
            at: Date.now(),
          },
          ...bookmarks,
        ];
    setBookmarks(next);
    saveLinks(BOOKMARKS_KEY, next);
  }, [activeTab, bookmarks]);

  // ──────────────────── zoom ────────────────────
  const setZoom = useCallback(
    async (id: string, factor: number) => {
      const clamped = Math.max(ZOOM_MIN, Math.min(ZOOM_MAX, factor));
      try {
        const applied = await browserZoom(id, clamped);
        setTabs((prev) =>
          prev.map((t) => (t.id === id ? { ...t, zoom: applied } : t)),
        );
      } catch (err) {
        setPanelError(err instanceof Error ? err.message : String(err));
      }
    },
    [],
  );

  const zoomIn = useCallback(() => {
    if (!activeTab) return;
    void setZoom(activeTab.id, activeTab.zoom + ZOOM_STEP);
  }, [activeTab, setZoom]);

  const zoomOut = useCallback(() => {
    if (!activeTab) return;
    void setZoom(activeTab.id, activeTab.zoom - ZOOM_STEP);
  }, [activeTab, setZoom]);

  const zoomReset = useCallback(() => {
    if (!activeTab) return;
    void setZoom(activeTab.id, 1);
  }, [activeTab, setZoom]);

  // ──────────────────── devtools ────────────────────
  const toggleDevtools = useCallback(async () => {
    if (!activeTab) return;
    const isOpen = devtoolsOpen.has(activeTab.id);
    try {
      const after = await browserDevtools(activeTab.id, !isOpen);
      setDevtoolsOpen((prev) => {
        const next = new Set(prev);
        if (after) next.add(activeTab.id);
        else next.delete(activeTab.id);
        return next;
      });
    } catch (err) {
      setPanelError(err instanceof Error ? err.message : String(err));
    }
  }, [activeTab, devtoolsOpen]);

  // ──────────────────── find-in-page ────────────────────
  const findNext = useCallback(
    async (backwards = false) => {
      if (!activeTab || !findQuery) return;
      try {
        await browserFind(activeTab.id, findQuery, {
          caseSensitive: findCaseSensitive,
          backwards,
        });
      } catch (err) {
        setPanelError(err instanceof Error ? err.message : String(err));
      }
    },
    [activeTab, findQuery, findCaseSensitive],
  );

  const closeFind = useCallback(() => {
    setFindOpen(false);
    setFindQuery("");
    if (activeTab) void browserFindClear(activeTab.id).catch(() => {});
  }, [activeTab]);

  const openFind = useCallback(() => {
    setFindOpen(true);
    // focus after render
    requestAnimationFrame(() => findInputRef.current?.focus());
  }, []);

  // ──────────────────── print / save as PDF ────────────────────
  const printActive = useCallback(async () => {
    if (!activeTab) return;
    setOverflowOpen(false);
    try {
      await browserPrint(activeTab.id);
    } catch (err) {
      setPanelError(err instanceof Error ? err.message : String(err));
    }
  }, [activeTab]);

  // ──────────────────── save page → workspace ────────────────────
  //
  // The save target is the currently-active workspace, falling back
  // to "playground" — the auto-mounted scratchpad workspace the boot
  // hook guarantees is always present. The user switches targets by
  // switching workspaces in the sidebar before clicking Save. We
  // deliberately don't ship an in-panel workspace picker yet: it's
  // one more UI surface to maintain and the sidebar already exists
  // as the canonical workspace selector.

  const saveTarget = useMemo(() => {
    if (activeWorkspace && workspaces.some((w) => w.name === activeWorkspace)) {
      return activeWorkspace;
    }
    if (workspaces.some((w) => w.name === "playground")) {
      return "playground";
    }
    return workspaces[0]?.name ?? "playground";
  }, [activeWorkspace, workspaces]);

  const refreshWorkspaces = useCallback(async () => {
    try {
      const list = await workspaceList();
      setWorkspaces(list);
    } catch {
      // honest empty — sidecar may not be up yet; the next refresh
      // (sidebar's workspaces-changed event) will populate it.
    }
  }, []);

  useEffect(() => {
    void refreshWorkspaces();
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    onWorkspacesChanged(() => {
      if (!cancelled) void refreshWorkspaces();
    }).then((fn) => {
      if (cancelled) fn();
      else unlisten = fn;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [refreshWorkspaces]);

  const renderSaveResult = useCallback((r: BrowserSavePageResult) => {
    const shortHash = r.content_hash.slice(0, 8);
    switch (r.status) {
      case "already_saved":
        toast("Already saved", {
          kind: "info",
          body: `${r.workspace} · no changes since the last save`,
        });
        return;
      case "updated":
        toast("Saved updated version", {
          kind: "success",
          body: `${r.workspace} · compiling… · ${shortHash}`,
          durationMs: 4500,
        });
        return;
      case "saved":
      default:
        toast("Page saved", {
          kind: "success",
          body: `${r.workspace} · compiling… · ${shortHash}`,
          durationMs: 4500,
        });
    }
  }, []);

  const savePage = useCallback(async () => {
    if (!activeTab) return;
    if (saving) return;
    if (!saveTarget) {
      toast("No workspace available", {
        kind: "error",
        body: "Add a workspace or wait for Playground to finish initialising.",
      });
      return;
    }
    setSaving(true);
    const startedAt = Date.now();
    toast("Saving page…", {
      kind: "info",
      body: `→ ${saveTarget}`,
      durationMs: 2000,
    });
    try {
      const result = await browserSavePage(activeTab.id, saveTarget);
      renderSaveResult(result);
    } catch (err) {
      const elapsedMs = Date.now() - startedAt;
      toast("Save failed", {
        kind: "error",
        body: err instanceof Error ? err.message : String(err),
        durationMs: 6000 + Math.min(elapsedMs / 10, 4000),
      });
    } finally {
      setSaving(false);
    }
  }, [activeTab, saveTarget, saving, renderSaveResult]);

  // Compile-progress listener: when the post-save compile finishes,
  // surface the Witness count in a follow-up toast so the user gets
  // the canonical "what did this page turn into" signal without
  // needing to glance at the right-rail. We only fire on `Done` (not
  // every tick) to keep the toast lane quiet.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    listen<{ phase: string; [k: string]: unknown }>(
      "workspace_compile_progress",
      (event) => {
        if (cancelled) return;
        const payload = event.payload;
        if (payload?.phase !== "done") return;
        // Only emit a witness-count toast when we actually triggered
        // the compile via Save. `saving` was already cleared by the
        // command's `finally` block, so we use `saveToastsRef` as a
        // weak signal: if no save happened in the last 8 seconds,
        // stay silent (it's the user's manual recompile, not ours).
        const lastSave = saveToastsRef.current.get("__last_save_at__");
        if (typeof lastSave !== "number") return;
        if (Date.now() - lastSave > 8000) return;
        saveToastsRef.current.delete("__last_save_at__");
        const witnesses =
          typeof payload.claims === "number" ? (payload.claims as number) : null;
        if (witnesses === null) return;
        toast("Page compiled", {
          kind: "success",
          body: `${witnesses} witness${witnesses === 1 ? "" : "es"} extracted`,
          durationMs: 4000,
        });
      },
    ).then((fn) => {
      if (cancelled) fn();
      else unlisten = fn;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

  // Stamp the save timestamp so the compile listener above knows
  // whether the next `done` event is ours.
  useEffect(() => {
    if (saving) {
      saveToastsRef.current.set("__last_save_at__", Date.now());
    }
  }, [saving]);

  // ──────────────────── session restore ────────────────────
  useEffect(() => {
    if (restoredRef.current) return;
    restoredRef.current = true;
    const session = loadSession();
    if (!session || session.tabs.length === 0) return;
    void (async () => {
      let firstId: string | null = null;
      for (const t of session.tabs) {
        const created = await openTab(t.url, {
          pinned: t.pinned,
          zoom: t.zoom,
          activate: false,
        });
        if (created && firstId === null) firstId = created.id;
      }
      if (firstId) setActiveId(firstId);
    })();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // ──────────────────── keyboard shortcuts ────────────────────
  useEffect(() => {
    if (!isActive) return;
    const onKey = (e: KeyboardEvent) => {
      const meta = e.metaKey || e.ctrlKey;
      if (!meta) {
        if (e.key === "Escape") {
          if (tabSearchOpen) {
            setTabSearchOpen(false);
            return;
          }
          if (findOpen) {
            closeFind();
            return;
          }
          if (contextMenu) {
            setContextMenu(null);
            return;
          }
        }
        return;
      }
      // Cmd+T / Cmd+N — new tab
      if ((e.key === "t" || e.key === "T") && !e.shiftKey) {
        e.preventDefault();
        void openTab(
          tabs.length === 0 && urlDraft.trim() ? urlDraft.trim() : undefined,
        );
        return;
      }
      // Cmd+W — close current tab
      if (e.key === "w" || e.key === "W") {
        e.preventDefault();
        if (activeId) closeTab(activeId);
        return;
      }
      // Cmd+L — focus URL bar
      if (e.key === "l" || e.key === "L") {
        e.preventDefault();
        const input = document.getElementById("browser-url-input") as HTMLInputElement | null;
        input?.focus();
        input?.select();
        return;
      }
      // Cmd+R — reload
      if (e.key === "r" || e.key === "R") {
        e.preventDefault();
        if (activeTab) void browserReload(activeTab.id);
        return;
      }
      // Cmd+F — find
      if (e.key === "f" || e.key === "F") {
        e.preventDefault();
        openFind();
        return;
      }
      // Cmd+= / Cmd++ — zoom in
      if (e.key === "=" || e.key === "+") {
        e.preventDefault();
        zoomIn();
        return;
      }
      // Cmd+- — zoom out
      if (e.key === "-") {
        e.preventDefault();
        zoomOut();
        return;
      }
      // Cmd+0 — zoom reset
      if (e.key === "0") {
        e.preventDefault();
        zoomReset();
        return;
      }
      // Cmd+Shift+A — tab search
      if (e.shiftKey && (e.key === "a" || e.key === "A")) {
        e.preventDefault();
        setTabSearchOpen(true);
        setTabSearchQuery("");
        return;
      }
      // Cmd+Opt+I — devtools
      if (e.altKey && (e.key === "i" || e.key === "I")) {
        e.preventDefault();
        void toggleDevtools();
        return;
      }
      // Cmd+P — print
      if (e.key === "p" || e.key === "P") {
        e.preventDefault();
        void printActive();
        return;
      }
      // Cmd+S — save current page to active workspace (or playground)
      if (e.key === "s" || e.key === "S") {
        e.preventDefault();
        void savePage();
        return;
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [
    isActive,
    activeId,
    activeTab,
    tabs.length,
    urlDraft,
    findOpen,
    tabSearchOpen,
    contextMenu,
    closeFind,
    closeTab,
    openTab,
    openFind,
    zoomIn,
    zoomOut,
    zoomReset,
    toggleDevtools,
    printActive,
    savePage,
  ]);

  // ──────────────────── visibility / bounds effects ────────────────────
  useEffect(() => {
    if (!isActive) {
      applyVisibility();
      return;
    }
    applyVisibility();
    if (activeId) {
      void browserFocus(activeId).catch(() => {});
    }
  }, [activeId, applyVisibility, isActive]);

  useEffect(() => {
    const el = viewportRef.current;
    if (!el) return;
    const observer = new ResizeObserver(updateBounds);
    observer.observe(el);
    window.addEventListener("resize", updateBounds);
    return () => {
      observer.disconnect();
      window.removeEventListener("resize", updateBounds);
    };
  }, [updateBounds]);

  useEffect(() => {
    return () => {
      for (const unlisten of unlistenersRef.current.values()) unlisten();
      for (const tab of tabs) void browserClose(tab.id).catch(() => {});
      unlistenersRef.current.clear();
    };
    // Full cleanup only on unmount; tab close handles normal removal.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Close context menu when clicking anywhere.
  useEffect(() => {
    if (!contextMenu) return;
    const onClick = () => setContextMenu(null);
    document.addEventListener("click", onClick, { once: true });
    return () => document.removeEventListener("click", onClick);
  }, [contextMenu]);

  // ──────────────────── derived UI ────────────────────
  const inProgressDownloads = downloads.filter((d) => d.status === "in_progress").length;
  const failedDownloads = downloads.filter((d) => d.status === "failed").length;

  const filteredTabSearchResults = useMemo(() => {
    const q = tabSearchQuery.trim().toLowerCase();
    if (!q) return tabs;
    return tabs.filter(
      (t) =>
        t.title.toLowerCase().includes(q) || t.url.toLowerCase().includes(q),
    );
  }, [tabs, tabSearchQuery]);

  const onTabContextMenu = useCallback(
    (e: React.MouseEvent, tabId: string) => {
      e.preventDefault();
      setContextMenu({ x: e.clientX, y: e.clientY, tabId });
    },
    [],
  );

  return (
    <div className="flex h-full min-h-0 flex-col bg-background/40">
      {/* Tab strip */}
      <div className="flex h-9 shrink-0 items-stretch gap-0.5 overflow-x-auto border-b border-border/60 bg-surface/40 px-1">
        {orderedTabs.map((tab) => (
          <button
            key={tab.id}
            type="button"
            className={cn(
              "group my-1 flex h-7 shrink-0 items-center gap-1.5 rounded-md px-2 text-[11px] transition-colors",
              tab.pinned ? "max-w-[40px] justify-center" : "max-w-[180px]",
              tab.id === activeId
                ? "bg-muted text-foreground"
                : "text-muted-foreground/70 hover:bg-muted/50 hover:text-foreground",
              tab.loading && tab.id === activeId && "ring-1 ring-accent/25 ring-offset-0",
            )}
            onClick={() => setActiveId(tab.id)}
            onContextMenu={(e) => onTabContextMenu(e, tab.id)}
            title={tab.url}
          >
            {tab.pinned ? (
              <Pin className="size-3 shrink-0" />
            ) : (
              <Globe2 className="size-3 shrink-0" />
            )}
            {!tab.pinned && <span className="truncate">{displayTitle(tab)}</span>}
            {!tab.pinned && (
              <span
                role="button"
                tabIndex={0}
                className="ml-0.5 rounded p-0.5 opacity-0 transition-opacity hover:bg-background/60 group-hover:opacity-100"
                onClick={(e) => {
                  e.stopPropagation();
                  closeTab(tab.id);
                }}
                onKeyDown={(e) => {
                  if (e.key === "Enter" || e.key === " ") closeTab(tab.id);
                }}
                aria-label="Close browser tab"
              >
                <X className="size-3" />
              </span>
            )}
          </button>
        ))}
        <Button
          variant="ghost"
          size="icon"
          className="my-1 h-7 w-7 shrink-0 self-center text-muted-foreground/70 hover:text-foreground"
          onClick={() => void openTab(tabs.length === 0 && urlDraft.trim() ? urlDraft.trim() : undefined)}
          disabled={opening}
          aria-label="New browser tab"
          title="New browser tab (Cmd+T)"
        >
          <Plus className="size-3.5" />
        </Button>
      </div>

      {/* URL bar */}
      <form
        className="relative flex h-10 shrink-0 items-center gap-1 border-b border-border/50 bg-muted/[0.08] px-2 dark:bg-muted/10"
        onSubmit={(e) => {
          e.preventDefault();
          if (activeTab) {
            void navigateActive();
            return;
          }
          const raw = urlDraft.trim();
          if (!raw || opening) return;
          void openTab(raw);
        }}
      >
        <Button
          type="button"
          variant="ghost"
          size="icon"
          className="h-7 w-7"
          disabled={!activeTab}
          onClick={() => activeTab && void browserBack(activeTab.id)}
          aria-label="Back"
        >
          <ArrowLeft className="size-3.5" />
        </Button>
        <Button
          type="button"
          variant="ghost"
          size="icon"
          className="h-7 w-7"
          disabled={!activeTab}
          onClick={() => activeTab && void browserForward(activeTab.id)}
          aria-label="Forward"
        >
          <ArrowRight className="size-3.5" />
        </Button>
        <Button
          type="button"
          variant="ghost"
          size="icon"
          className="h-7 w-7"
          disabled={!activeTab}
          onClick={() => activeTab && void browserReload(activeTab.id)}
          aria-label="Reload"
        >
          <RefreshCw className={cn("size-3.5", activeTab?.loading && "animate-spin")} />
        </Button>
        <div
          className={cn(
            "flex min-w-0 flex-1 items-center gap-1.5 rounded-lg border px-2.5 transition-colors",
            activeTab?.loading
              ? "border-accent/25 bg-background/55"
              : "border-border/45 bg-background/60 dark:bg-background/40",
          )}
        >
          {activeTab ? (
            <OmniboxSecurityGlyph url={activeTab.url} />
          ) : (
            <Search className="size-3.5 shrink-0 text-muted-foreground" aria-hidden />
          )}
          <input
            id="browser-url-input"
            className="h-7 min-w-0 flex-1 bg-transparent font-mono text-xs leading-none outline-none placeholder:text-muted-foreground/55 disabled:opacity-50"
            value={activeTab ? activeTab.input : urlDraft}
            disabled={opening}
            placeholder="Search Google or enter URL"
            onChange={(e) => {
              const value = e.target.value;
              if (activeTab && activeId) {
                setTabs((prev) =>
                  prev.map((t) => (t.id === activeId ? { ...t, input: value } : t)),
                );
              } else {
                setUrlDraft(value);
              }
            }}
          />
          {/* zoom indicator (only when not 100%) */}
          {activeTab && Math.abs(activeTab.zoom - 1) > 0.001 && (
            <button
              type="button"
              onClick={zoomReset}
              className="rounded px-1 font-mono text-[10px] text-muted-foreground hover:bg-muted hover:text-foreground"
              title="Reset zoom (Cmd+0)"
            >
              {Math.round(activeTab.zoom * 100)}%
            </button>
          )}
        </div>
        {/* zoom out / in */}
        <Button
          type="button"
          variant="ghost"
          size="icon"
          className="h-7 w-7"
          disabled={!activeTab}
          onClick={zoomOut}
          aria-label="Zoom out (Cmd+-)"
          title="Zoom out (Cmd+-)"
        >
          <ZoomOut className="size-3.5" />
        </Button>
        <Button
          type="button"
          variant="ghost"
          size="icon"
          className="h-7 w-7"
          disabled={!activeTab}
          onClick={zoomIn}
          aria-label="Zoom in (Cmd+=)"
          title="Zoom in (Cmd+=)"
        >
          <ZoomIn className="size-3.5" />
        </Button>
        {/* Save page → workspace (the headline Witness-Mesh affordance) */}
        <Button
          type="button"
          variant="ghost"
          size="icon"
          className={cn(
            "h-7 w-7",
            saving && "text-blue-400",
          )}
          disabled={!activeTab || saving || !saveTarget}
          onClick={() => void savePage()}
          aria-label={`Save to ${saveTarget} (Cmd+S)`}
          title={
            saving
              ? `Saving to ${saveTarget}…`
              : `Save page to ${saveTarget} (Cmd+S)`
          }
        >
          {saving ? (
            <RefreshCw className="size-3.5 animate-spin" />
          ) : (
            <Save className="size-3.5" />
          )}
        </Button>
        {/* bookmark */}
        <Button
          type="button"
          variant="ghost"
          size="icon"
          className={cn(
            "h-7 w-7",
            activeTab && isBookmarked(activeTab.url, bookmarks) && "text-amber-400",
          )}
          disabled={!activeTab}
          onClick={toggleBookmark}
          aria-label="Bookmark"
        >
          <Star className="size-3.5" />
        </Button>
        {/* devtools */}
        <Button
          type="button"
          variant="ghost"
          size="icon"
          className={cn(
            "h-7 w-7",
            activeTab && devtoolsOpen.has(activeTab.id) && "text-blue-400",
          )}
          disabled={!activeTab}
          onClick={toggleDevtools}
          aria-label="DevTools (Cmd+Opt+I)"
          title="Inspect (Cmd+Opt+I)"
        >
          <Bug className="size-3.5" />
        </Button>
        {/* downloads */}
        <div className="relative">
          <Button
            type="button"
            variant="ghost"
            size="icon"
            className={cn(
              "h-7 w-7",
              inProgressDownloads > 0 && "text-blue-400",
              failedDownloads > 0 && "text-rose-400",
            )}
            onClick={() => setDownloadsOpen((v) => !v)}
            aria-label="Downloads"
            title="Downloads"
          >
            <Download className="size-3.5" />
            {downloads.length > 0 && (
              <span className="absolute -right-0.5 -top-0.5 flex h-3 min-w-3 items-center justify-center rounded-full bg-blue-500 px-1 text-[8px] font-bold text-white">
                {downloads.length > 9 ? "9+" : downloads.length}
              </span>
            )}
          </Button>
        </div>
        {/* overflow */}
        <div className="relative">
          <Button
            type="button"
            variant="ghost"
            size="icon"
            className="h-7 w-7"
            onClick={() => setOverflowOpen((v) => !v)}
            aria-label="More"
            title="More"
          >
            <MoreVertical className="size-3.5" />
          </Button>
          {overflowOpen && (
            <div className="absolute right-0 top-8 z-30 w-44 rounded-md border border-border/60 bg-background/95 p-1 shadow-lg backdrop-blur">
              <button
                type="button"
                className="flex w-full items-center gap-2 rounded px-2 py-1.5 text-left text-[11px] hover:bg-muted disabled:opacity-50"
                disabled={!activeTab}
                onClick={() => {
                  setOverflowOpen(false);
                  openFind();
                }}
              >
                <Search className="size-3" /> Find in page
                <span className="ml-auto font-mono text-[9px] text-muted-foreground">⌘F</span>
              </button>
              <button
                type="button"
                className="flex w-full items-center gap-2 rounded px-2 py-1.5 text-left text-[11px] hover:bg-muted disabled:opacity-50"
                disabled={!activeTab || saving || !saveTarget}
                onClick={() => {
                  setOverflowOpen(false);
                  void savePage();
                }}
                title={`Save to ${saveTarget}`}
              >
                <Save className="size-3" /> Save to {saveTarget}
                <span className="ml-auto font-mono text-[9px] text-muted-foreground">⌘S</span>
              </button>
              <button
                type="button"
                className="flex w-full items-center gap-2 rounded px-2 py-1.5 text-left text-[11px] hover:bg-muted disabled:opacity-50"
                disabled={!activeTab}
                onClick={printActive}
              >
                <Printer className="size-3" /> Print / Save as PDF
                <span className="ml-auto font-mono text-[9px] text-muted-foreground">⌘P</span>
              </button>
              <button
                type="button"
                className="flex w-full items-center gap-2 rounded px-2 py-1.5 text-left text-[11px] hover:bg-muted"
                onClick={() => {
                  setOverflowOpen(false);
                  setTabSearchOpen(true);
                  setTabSearchQuery("");
                }}
              >
                <Search className="size-3" /> Search tabs
                <span className="ml-auto font-mono text-[9px] text-muted-foreground">⌘⇧A</span>
              </button>
              <button
                type="button"
                className="flex w-full items-center gap-2 rounded px-2 py-1.5 text-left text-[11px] hover:bg-muted disabled:opacity-50"
                disabled={!activeTab}
                onClick={() => {
                  setOverflowOpen(false);
                  if (activeTab) void openUrl(activeTab.url);
                }}
              >
                <ExternalLink className="size-3" /> Open externally
              </button>
            </div>
          )}
        </div>
      </form>

      {/* Find-in-page bar */}
      {findOpen && (
        <div className="flex h-8 shrink-0 items-center gap-1 border-b border-border/60 bg-surface/50 px-2">
          <input
            ref={findInputRef}
            className="h-6 min-w-0 flex-1 rounded border border-border/60 bg-background/70 px-2 font-mono text-[11px] outline-none focus:border-blue-500"
            value={findQuery}
            onChange={(e) => {
              setFindQuery(e.target.value);
              if (activeTab) void browserFindClear(activeTab.id).catch(() => {});
            }}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                void findNext(e.shiftKey);
              } else if (e.key === "Escape") {
                e.preventDefault();
                closeFind();
              }
            }}
            placeholder="Find in page"
          />
          <button
            type="button"
            onClick={() => setFindCaseSensitive((v) => !v)}
            className={cn(
              "rounded px-1.5 font-mono text-[10px]",
              findCaseSensitive
                ? "bg-blue-500/20 text-blue-300"
                : "text-muted-foreground hover:bg-muted",
            )}
            title="Match case"
          >
            Aa
          </button>
          <Button
            type="button"
            variant="ghost"
            size="icon"
            className="h-6 w-6"
            onClick={() => findNext(true)}
            disabled={!findQuery}
            aria-label="Previous match"
          >
            <ChevronDown className="size-3 rotate-180" />
          </Button>
          <Button
            type="button"
            variant="ghost"
            size="icon"
            className="h-6 w-6"
            onClick={() => findNext(false)}
            disabled={!findQuery}
            aria-label="Next match"
          >
            <ChevronDown className="size-3" />
          </Button>
          <Button
            type="button"
            variant="ghost"
            size="icon"
            className="h-6 w-6"
            onClick={closeFind}
            aria-label="Close find"
          >
            <X className="size-3" />
          </Button>
        </div>
      )}

      {/* Error chip */}
      {(panelError || activeTab?.error) && (
        <div className="flex shrink-0 items-center gap-2 border-b border-rose-500/30 bg-rose-500/10 px-3 py-1.5 text-[11px] text-rose-300">
          <span className="font-medium">Browser:</span>
          <span className="truncate font-mono">{panelError ?? activeTab?.error}</span>
          <Button
            variant="ghost"
            size="icon"
            className="ml-auto h-5 w-5 text-rose-300 hover:text-rose-100"
            onClick={() => {
              setPanelError(null);
              if (activeTab) {
                setTabs((prev) =>
                  prev.map((t) => (t.id === activeTab.id ? { ...t, error: null } : t)),
                );
              }
            }}
            aria-label="Dismiss"
          >
            <X className="size-3" />
          </Button>
        </div>
      )}

      {/* Library toolbar */}
      <div className="flex h-7 shrink-0 items-center gap-1 border-b border-border/40 bg-surface/20 px-2">
        <button
          type="button"
          className={cn(
            "rounded px-2 py-0.5 text-[10px]",
            showLibrary === "history" ? "bg-muted text-foreground" : "text-muted-foreground",
          )}
          onClick={() => setShowLibrary(showLibrary === "history" ? null : "history")}
        >
          History
        </button>
        <button
          type="button"
          className={cn(
            "rounded px-2 py-0.5 text-[10px]",
            showLibrary === "bookmarks" ? "bg-muted text-foreground" : "text-muted-foreground",
          )}
          onClick={() => setShowLibrary(showLibrary === "bookmarks" ? null : "bookmarks")}
        >
          Bookmarks
        </button>
        <span className="ml-auto truncate font-mono text-[10px] text-muted-foreground/70">
          {activeTab?.url ?? "No tab"}
        </span>
      </div>

      {/* Library drawer */}
      {showLibrary && (
        <div className="max-h-36 shrink-0 overflow-y-auto border-b border-border/50 bg-surface/50 p-2">
          {(showLibrary === "history" ? history : bookmarks).length === 0 ? (
            <p className="px-1 py-2 text-[11px] text-muted-foreground">
              No {showLibrary} yet.
            </p>
          ) : (
            <div className="flex flex-col gap-1">
              {(showLibrary === "history" ? history : bookmarks).map((item) => (
                <button
                  key={`${item.url}:${item.at}`}
                  type="button"
                  className="rounded-lg px-2 py-1.5 text-left text-[11px] hover:bg-muted/50"
                  onClick={() => {
                    setShowLibrary(null);
                    if (activeTab) void navigateActive(item.url);
                    else void openTab(item.url);
                  }}
                >
                  <div className="truncate font-medium text-foreground">{item.title}</div>
                  <div className="truncate font-mono text-[10px] text-muted-foreground">
                    {item.url}
                  </div>
                </button>
              ))}
            </div>
          )}
        </div>
      )}

      {/* Viewport (native WebView lives on top of this) */}
      <div ref={viewportRef} className="relative min-h-0 flex-1 bg-background">
        {tabs.length === 0 && (
          <div className="flex h-full flex-col items-center justify-center gap-3 px-6 text-center">
            <Globe2 className="size-8 text-muted-foreground/50" />
            <div className="space-y-1">
              <p className="text-xs font-medium text-foreground">No browser tab open</p>
              <p className="text-[11px] text-muted-foreground">
                Type a URL or query above and press Enter, or start here. Docs and Hub open in-panel
                beside chat.
              </p>
            </div>
            <Button
              variant="outline"
              size="sm"
              className="h-8 gap-1.5 rounded-xl px-3 text-xs"
              disabled={opening}
              onClick={() =>
                void openTab(urlDraft.trim() ? urlDraft.trim() : undefined)
              }
            >
              <Plus className="size-3" />
              New browser tab
            </Button>
          </div>
        )}
      </div>

      {/* Downloads tray */}
      {downloadsOpen && (
        <div className="absolute right-2 top-20 z-40 w-72 rounded-md border border-border/60 bg-background/95 shadow-xl backdrop-blur">
          <div className="flex items-center justify-between border-b border-border/60 px-3 py-1.5">
            <span className="text-[11px] font-medium">Downloads</span>
            <div className="flex gap-1">
              {downloads.length > 0 && (
                <button
                  type="button"
                  className="rounded px-1.5 text-[9px] text-muted-foreground hover:bg-muted hover:text-foreground"
                  onClick={() => {
                    setDownloads([]);
                    saveDownloads([]);
                  }}
                >
                  Clear
                </button>
              )}
              <Button
                variant="ghost"
                size="icon"
                className="h-5 w-5"
                onClick={() => setDownloadsOpen(false)}
                aria-label="Close"
              >
                <X className="size-3" />
              </Button>
            </div>
          </div>
          <div className="max-h-64 overflow-y-auto p-1">
            {downloads.length === 0 ? (
              <p className="px-2 py-3 text-center text-[11px] text-muted-foreground">
                No downloads yet
              </p>
            ) : (
              downloads.map((d) => (
                <div
                  key={`${d.url}:${d.at}`}
                  className="flex items-center gap-2 rounded px-2 py-1.5 hover:bg-muted/50"
                >
                  <div className="min-w-0 flex-1">
                    <div className="truncate text-[11px] font-medium">{d.filename}</div>
                    <div className="truncate font-mono text-[9px] text-muted-foreground">
                      {d.status === "in_progress" && "Downloading…"}
                      {d.status === "done" && d.path}
                      {d.status === "failed" && "Failed"}
                    </div>
                  </div>
                  {d.status === "done" && d.path && (
                    <Button
                      variant="ghost"
                      size="icon"
                      className="h-6 w-6"
                      onClick={() => {
                        if (d.path) void revealItemInDir(d.path).catch(() => {});
                      }}
                      aria-label="Show in folder"
                      title="Show in folder"
                    >
                      <FolderOpen className="size-3" />
                    </Button>
                  )}
                </div>
              ))
            )}
          </div>
        </div>
      )}

      {/* Tab search palette */}
      {tabSearchOpen && (
        <div
          className="absolute inset-0 z-40 flex items-start justify-center bg-black/40 backdrop-blur-sm"
          onClick={() => setTabSearchOpen(false)}
        >
          <div
            className="mt-16 w-full max-w-md rounded-lg border border-border/60 bg-background shadow-2xl"
            onClick={(e) => e.stopPropagation()}
          >
            <div className="flex items-center gap-2 border-b border-border/60 px-3 py-2">
              <Search className="size-4 text-muted-foreground" />
              <input
                autoFocus
                className="h-7 flex-1 bg-transparent text-[12px] outline-none"
                placeholder="Search tabs by title or URL…"
                value={tabSearchQuery}
                onChange={(e) => setTabSearchQuery(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Escape") {
                    e.preventDefault();
                    setTabSearchOpen(false);
                  } else if (e.key === "Enter") {
                    e.preventDefault();
                    const first = filteredTabSearchResults[0];
                    if (first) {
                      setActiveId(first.id);
                      setTabSearchOpen(false);
                    }
                  }
                }}
              />
            </div>
            <div className="max-h-72 overflow-y-auto p-1">
              {filteredTabSearchResults.length === 0 ? (
                <p className="px-2 py-3 text-center text-[11px] text-muted-foreground">
                  No tabs match
                </p>
              ) : (
                filteredTabSearchResults.map((t) => (
                  <button
                    key={t.id}
                    type="button"
                    className={cn(
                      "flex w-full items-center gap-2 rounded px-2 py-1.5 text-left text-[11px] hover:bg-muted",
                      t.id === activeId && "bg-muted",
                    )}
                    onClick={() => {
                      setActiveId(t.id);
                      setTabSearchOpen(false);
                    }}
                  >
                    {t.pinned ? (
                      <Pin className="size-3 shrink-0 text-amber-400" />
                    ) : (
                      <Globe2 className="size-3 shrink-0 text-muted-foreground" />
                    )}
                    <div className="min-w-0 flex-1">
                      <div className="truncate">{displayTitle(t)}</div>
                      <div className="truncate font-mono text-[9px] text-muted-foreground">
                        {t.url}
                      </div>
                    </div>
                    {t.id === activeId && <Check className="size-3 text-blue-400" />}
                  </button>
                ))
              )}
            </div>
          </div>
        </div>
      )}

      {/* Tab context menu */}
      {contextMenu && (() => {
        const tab = tabs.find((t) => t.id === contextMenu.tabId);
        if (!tab) return null;
        return (
          <div
            className="fixed z-50 w-48 rounded-md border border-border/60 bg-background/95 p-1 shadow-xl backdrop-blur"
            style={{ left: contextMenu.x, top: contextMenu.y }}
          >
            <button
              type="button"
              className="flex w-full items-center gap-2 rounded px-2 py-1.5 text-left text-[11px] hover:bg-muted"
              onClick={() => {
                togglePin(tab.id);
                setContextMenu(null);
              }}
            >
              {tab.pinned ? <PinOff className="size-3" /> : <Pin className="size-3" />}
              {tab.pinned ? "Unpin tab" : "Pin tab"}
            </button>
            <button
              type="button"
              className="flex w-full items-center gap-2 rounded px-2 py-1.5 text-left text-[11px] hover:bg-muted"
              onClick={() => {
                duplicateTab(tab.id);
                setContextMenu(null);
              }}
            >
              <Plus className="size-3" /> Duplicate
            </button>
            <button
              type="button"
              className="flex w-full items-center gap-2 rounded px-2 py-1.5 text-left text-[11px] hover:bg-muted"
              onClick={() => {
                void navigator.clipboard?.writeText(tab.url);
                setContextMenu(null);
              }}
            >
              <ExternalLink className="size-3" /> Copy URL
            </button>
            <button
              type="button"
              className="flex w-full items-center gap-2 rounded px-2 py-1.5 text-left text-[11px] hover:bg-muted"
              onClick={() => {
                void openUrl(tab.url);
                setContextMenu(null);
              }}
            >
              <ExternalLink className="size-3" /> Open externally
            </button>
            <div className="my-1 h-px bg-border/40" />
            <button
              type="button"
              className="flex w-full items-center gap-2 rounded px-2 py-1.5 text-left text-[11px] hover:bg-muted disabled:opacity-50"
              disabled={tabs.filter((t) => t.id !== tab.id && !t.pinned).length === 0}
              onClick={() => {
                closeOtherTabs(tab.id);
                setContextMenu(null);
              }}
            >
              <X className="size-3" /> Close other tabs
            </button>
            <button
              type="button"
              className="flex w-full items-center gap-2 rounded px-2 py-1.5 text-left text-[11px] text-rose-300 hover:bg-rose-500/10 disabled:opacity-50"
              disabled={tab.pinned}
              onClick={() => {
                closeTab(tab.id);
                setContextMenu(null);
              }}
            >
              <X className="size-3" /> Close tab
            </button>
          </div>
        );
      })()}
    </div>
  );
}
