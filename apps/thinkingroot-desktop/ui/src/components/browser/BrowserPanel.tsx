/**
 * Manual browser panel for the right rail.
 *
 * This is intentionally a user-operated browser, not an agentic tool:
 * address/search bar, tab strip, back/forward/reload, bookmarks,
 * history, and open-in-system-browser. The page body is a native Tauri
 * child WebView placed over `viewportRef`; the React app only renders
 * the browser chrome.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  ArrowLeft,
  ArrowRight,
  ExternalLink,
  Globe2,
  Plus,
  RefreshCw,
  Search,
  Star,
  X,
} from "lucide-react";

import { Button } from "@/components/ui/button";
import {
  browserBack,
  browserClose,
  browserFocus,
  browserForward,
  browserHide,
  browserNavigate,
  browserOpen,
  browserReload,
  browserSetBounds,
  browserShow,
  listenBrowserEvent,
  type BrowserBounds,
  type BrowserEvent,
  type BrowserSessionInfo,
} from "@/lib/tauri";
import { cn } from "@/lib/utils";

const DEFAULT_URL = "https://duckduckgo.com";
const HISTORY_KEY = "thinkingroot.browser.history";
const BOOKMARKS_KEY = "thinkingroot.browser.bookmarks";

interface BrowserTab {
  id: string;
  info: BrowserSessionInfo;
  title: string;
  url: string;
  input: string;
  loading: boolean;
  error: string | null;
}

interface StoredLink {
  title: string;
  url: string;
  at: number;
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
  window.localStorage.setItem(key, JSON.stringify(links.slice(0, 100)));
}

function remember(key: string, link: StoredLink) {
  const next = [link, ...loadLinks(key).filter((l) => l.url !== link.url)];
  saveLinks(key, next);
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

export function BrowserPanel({ isActive }: { isActive: boolean }) {
  const viewportRef = useRef<HTMLDivElement | null>(null);
  const unlistenersRef = useRef<Map<string, () => void>>(new Map());
  const [tabs, setTabs] = useState<BrowserTab[]>([]);
  const [activeId, setActiveId] = useState<string | null>(null);
  const [opening, setOpening] = useState(false);
  const [panelError, setPanelError] = useState<string | null>(null);
  const [history, setHistory] = useState<StoredLink[]>(() => loadLinks(HISTORY_KEY));
  const [bookmarks, setBookmarks] = useState<StoredLink[]>(() => loadLinks(BOOKMARKS_KEY));
  const [showLibrary, setShowLibrary] = useState<"history" | "bookmarks" | null>(null);

  const activeTab = useMemo(
    () => tabs.find((t) => t.id === activeId) ?? null,
    [tabs, activeId],
  );

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

  const wireEvents = useCallback(async (session: BrowserSessionInfo) => {
    const unlisten = await listenBrowserEvent(session.event, (event: BrowserEvent) => {
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
            case "download":
              if (event.success === false) {
                return {
                  ...tab,
                  error: `Download failed: ${event.url}`,
                };
              }
              return tab;
            default:
              return tab;
          }
        }),
      );
    });
    unlistenersRef.current.set(session.id, unlisten);
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const openTab = useCallback(async (url = DEFAULT_URL) => {
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
      };
      await wireEvents(session);
      setTabs((prev) => [...prev, tab]);
      setActiveId(session.id);
    } catch (err) {
      setPanelError(err instanceof Error ? err.message : String(err));
    } finally {
      setOpening(false);
    }
  }, [currentBounds, wireEvents]);

  const closeTab = useCallback((id: string) => {
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
  }, []);

  const navigateActive = useCallback(async (value?: string) => {
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
  }, [activeTab]);

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

  return (
    <div className="flex h-full min-h-0 flex-col bg-background/40">
      <div className="flex h-9 shrink-0 items-stretch gap-0.5 overflow-x-auto border-b border-border/60 bg-surface/40 px-1">
        {tabs.map((tab) => (
          <button
            key={tab.id}
            type="button"
            className={cn(
              "group my-1 flex h-7 max-w-[180px] shrink-0 items-center gap-1.5 rounded-md px-2 text-[11px] transition-colors",
              tab.id === activeId
                ? "bg-muted text-foreground"
                : "text-muted-foreground/70 hover:bg-muted/50 hover:text-foreground",
            )}
            onClick={() => setActiveId(tab.id)}
            title={tab.url}
          >
            <Globe2 className="size-3 shrink-0" />
            <span className="truncate">{displayTitle(tab)}</span>
            {tab.loading && <RefreshCw className="size-3 shrink-0 animate-spin" />}
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
          </button>
        ))}
        <Button
          variant="ghost"
          size="icon"
          className="my-1 h-7 w-7 shrink-0 self-center text-muted-foreground/70 hover:text-foreground"
          onClick={() => void openTab()}
          disabled={opening}
          aria-label="New browser tab"
          title="New browser tab"
        >
          <Plus className="size-3.5" />
        </Button>
      </div>

      <form
        className="flex h-10 shrink-0 items-center gap-1.5 border-b border-border/60 bg-surface/30 px-2"
        onSubmit={(e) => {
          e.preventDefault();
          void navigateActive();
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
        <div className="flex min-w-0 flex-1 items-center gap-1 rounded-xl border border-border/60 bg-background/70 px-2">
          <Search className="size-3.5 shrink-0 text-muted-foreground" />
          <input
            className="h-7 min-w-0 flex-1 bg-transparent font-mono text-[11px] outline-none placeholder:text-muted-foreground/60"
            value={activeTab?.input ?? ""}
            disabled={!activeTab}
            placeholder="Search or enter URL"
            onChange={(e) => {
              const value = e.target.value;
              setTabs((prev) =>
                prev.map((t) => (t.id === activeId ? { ...t, input: value } : t)),
              );
            }}
          />
        </div>
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
        <Button
          type="button"
          variant="ghost"
          size="icon"
          className="h-7 w-7"
          disabled={!activeTab}
          onClick={() => activeTab && void openUrl(activeTab.url)}
          aria-label="Open externally"
        >
          <ExternalLink className="size-3.5" />
        </Button>
      </form>

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

      <div ref={viewportRef} className="relative min-h-0 flex-1 bg-background">
        {tabs.length === 0 && (
          <div className="flex h-full flex-col items-center justify-center gap-3 px-6 text-center">
            <Globe2 className="size-8 text-muted-foreground/50" />
            <div className="space-y-1">
              <p className="text-xs font-medium text-foreground">No browser tab open</p>
              <p className="text-[11px] text-muted-foreground">
                Open docs, research, or Hub pages beside chat without leaving ThinkingRoot.
              </p>
            </div>
            <Button
              variant="outline"
              size="sm"
              className="h-8 gap-1.5 rounded-xl px-3 text-xs"
              disabled={opening}
              onClick={() => void openTab()}
            >
              <Plus className="size-3" />
              New browser tab
            </Button>
          </div>
        )}
      </div>
    </div>
  );
}
