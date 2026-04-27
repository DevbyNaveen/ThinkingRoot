import { useEffect, useRef } from "react";
import { X, Plus } from "lucide-react";
import { cn } from "@/lib/utils";

export interface TabDescriptor {
  id: string;
  title: string;
}

interface TabBarProps {
  tabs: TabDescriptor[];
  activeId: string | null;
  onSelect: (id: string) => void;
  onClose: (id: string) => void;
  onNew: () => void;
}

/** Tab bar at the top of the main pane — Obsidian-style. */
export function TabBar({ tabs, activeId, onSelect, onClose, onNew }: TabBarProps) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const activeRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    if (activeRef.current) {
      // Allow a tiny delay for layout to settle if a new tab was just mounted.
      setTimeout(() => {
        activeRef.current?.scrollIntoView({
          behavior: "smooth",
          block: "nearest",
          inline: "nearest",
        });
      }, 50);
    }
  }, [activeId, tabs.length]);

  return (
    <div className="window-drag flex h-11 items-end border-b border-border bg-surface pl-2 pr-1">
      <div
        ref={scrollRef}
        role="tablist"
        aria-label="Open tabs"
        className="window-no-drag flex h-11 flex-1 items-end gap-0.5 overflow-x-auto overflow-y-hidden scrollbar-width-none [&::-webkit-scrollbar]:hidden"
      >
        {tabs.map((tab) => {
          const active = tab.id === activeId;
          return (
            <button
              key={tab.id}
              ref={active ? activeRef : null}
              role="tab"
              type="button"
              aria-selected={active}
              onClick={() => onSelect(tab.id)}
              className={cn(
                "group relative flex h-8 shrink-0 items-center gap-2 rounded-t-md px-3 text-xs transition-colors",
                active
                  ? "bg-surface-elevated text-foreground"
                  : "text-muted-foreground hover:bg-muted/60 hover:text-foreground",
              )}
            >
              <span className="max-w-[160px] truncate">{tab.title}</span>
              <span
                role="button"
                tabIndex={-1}
                aria-label={`Close ${tab.title}`}
                onClick={(e) => {
                  e.stopPropagation();
                  onClose(tab.id);
                }}
                className={cn(
                  "flex size-4 items-center justify-center rounded-sm transition-opacity",
                  "opacity-0 group-hover:opacity-100 hover:bg-muted",
                  active && "opacity-60 hover:opacity-100",
                )}
              >
                <X className="size-3" />
              </span>
              {active && (
                <span className="absolute inset-x-2 -bottom-px h-px bg-accent" />
              )}
            </button>
          );
        })}
      </div>
      <div className="flex shrink-0 items-end px-1 pb-1">
        <button
          type="button"
          onClick={onNew}
          aria-label="New tab"
          className="window-no-drag flex size-7 items-center justify-center rounded text-muted-foreground transition-colors hover:bg-muted/60 hover:text-foreground"
        >
          <Plus className="size-3.5" />
        </button>
      </div>
    </div>
  );
}
