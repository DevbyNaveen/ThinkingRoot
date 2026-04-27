import { useEffect, useMemo, useState } from "react";
import { Command } from "cmdk";
import { Search, ArrowRight, Clock } from "lucide-react";
import { useApp } from "@/store/app";
import { useHotkey } from "@/hooks/useHotkeys";
import { buildCatalog, GROUP_ORDER, type CommandDef } from "./catalog";
import type { Surface } from "@/types";
import { cn } from "@/lib/utils";

/**
 * Raycast-style ⌘K palette backed by cmdk. Renders the full 60-
 * command catalog, surfaces recent + argument prompts, and honours
 * ⌘1–⌘5 / ⌘, surface-navigation shortcuts regardless of whether
 * the palette is open.
 */
export function CommandPalette() {
  const open = useApp((s) => s.commandPaletteOpen);
  const setOpen = useApp((s) => s.setCommandPaletteOpen);
  const setSurface = useApp((s) => s.setSurface);
  const setTheme = useApp((s) => s.setTheme);
  const setTrust = useApp((s) => s.setTrust);
  const toggleSidebar = useApp((s) => s.toggleSidebar);
  const toggleRightRail = useApp((s) => s.toggleRightRail);
  const setRightRailOpen = (o: boolean) => {
    const current = useApp.getState().rightRailOpen;
    if (current !== o) useApp.getState().toggleRightRail();
  };
  const recordCommand = useApp((s) => s.recordCommand);
  const recentIds = useApp((s) => s.recentCommandIds);

  // ⌘K toggle
  useHotkey("mod+k", (e) => {
    e.preventDefault();
    setOpen(!open);
  });

  // ⌘1..⌘4 / ⌘, — surface navigation (work even when palette closed)
  const surfaceShortcuts: Array<[string, Surface]> = [
    ["mod+1", "chats"],
    ["mod+2", "brain"],
    ["mod+3", "satellites"],
    ["mod+4", "trace"],
  ];
  for (const [combo, s] of surfaceShortcuts) {
    // eslint-disable-next-line react-hooks/rules-of-hooks
    useHotkey(combo, (e) => {
      e.preventDefault();
      setSurface(s);
    });
  }
  useHotkey("mod+,", (e) => {
    e.preventDefault();
    setSurface("settings");
  });

  const close = () => {
    setOpen(false);
    setPendingArg(null);
  };

  const [pendingArg, setPendingArg] = useState<CommandDef | null>(null);
  const [argValue, setArgValue] = useState("");

  const ctx = useMemo(
    () => ({
      setSurface,
      setTheme,
      setTrust,
      setRightRailOpen,
      toggleSidebar,
      toggleRightRail,
      close,
    }),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [],
  );

  const catalog = useMemo(() => buildCatalog(ctx), [ctx]);

  // Reset drill-down state when the palette closes.
  useEffect(() => {
    if (!open) {
      setPendingArg(null);
      setArgValue("");
    }
  }, [open]);

  // Close on background mousedown.
  useEffect(() => {
    if (!open) return;
    const onMouse = (e: MouseEvent) => {
      const t = e.target as HTMLElement | null;
      if (!t?.closest("[cmdk-root]")) close();
    };
    window.addEventListener("mousedown", onMouse);
    return () => window.removeEventListener("mousedown", onMouse);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  async function runCommand(item: CommandDef, arg?: string) {
    recordCommand(item.id);
    await Promise.resolve(item.run(ctx, arg));
  }

  if (!open) return null;

  const grouped = groupByBand(catalog);
  const recent = recentIds
    .map((id) => catalog.find((c) => c.id === id))
    .filter((c): c is CommandDef => Boolean(c));

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label="Command palette"
      className="fixed inset-0 z-50 flex items-start justify-center bg-background/60 pt-[12vh] backdrop-blur-sm"
      onClick={(e) => {
        if (e.target === e.currentTarget) close();
      }}
    >
      <Command
        label="Command palette"
        className="w-full max-w-xl overflow-hidden rounded-xl border border-border bg-surface-elevated shadow-elevated"
        shouldFilter={pendingArg === null}
      >
        {pendingArg === null ? (
          <>
            <div className="flex items-center gap-2 border-b border-border px-3">
              <Search className="size-4 text-muted-foreground" />
              <Command.Input
                autoFocus
                placeholder="Type a command or search…"
                className="h-11 w-full bg-transparent text-sm text-foreground placeholder:text-muted-foreground focus:outline-none"
              />
              <kbd className="hidden shrink-0 rounded bg-muted px-1.5 py-0.5 font-mono text-[10px] text-muted-foreground sm:block">
                Esc
              </kbd>
            </div>
            <Command.List className="max-h-[60vh] overflow-y-auto p-1.5">
              <Command.Empty className="px-3 py-8 text-center text-sm text-muted-foreground">
                No matching command.
              </Command.Empty>

              {recent.length > 0 && (
                <Section heading="Recent" icon={<Clock className="size-3" />}>
                  {recent.map((item) => (
                    <CommandRow
                      key={`recent-${item.id}`}
                      item={item}
                      onSelect={() => handleSelect(item)}
                    />
                  ))}
                </Section>
              )}

              {GROUP_ORDER.map((group) => {
                const items = grouped.get(group);
                if (!items || items.length === 0) return null;
                return (
                  <Section key={group} heading={group}>
                    {items.map((item) => (
                      <CommandRow
                        key={item.id}
                        item={item}
                        onSelect={() => handleSelect(item)}
                      />
                    ))}
                  </Section>
                );
              })}
            </Command.List>
          </>
        ) : (
          <ArgDrillDown
            command={pendingArg}
            value={argValue}
            onValue={setArgValue}
            onCancel={() => {
              setPendingArg(null);
              setArgValue("");
            }}
            onSubmit={async () => {
              const trimmed = argValue.trim();
              if (!trimmed) return;
              const cmd = pendingArg;
              setPendingArg(null);
              setArgValue("");
              await runCommand(cmd, trimmed);
            }}
          />
        )}
      </Command>
    </div>
  );

  function handleSelect(item: CommandDef) {
    if (item.argLabel) {
      setPendingArg(item);
      setArgValue("");
    } else {
      void runCommand(item);
    }
  }
}

function groupByBand(items: CommandDef[]): Map<string, CommandDef[]> {
  const groups = new Map<string, CommandDef[]>();
  for (const it of items) {
    const bucket = groups.get(it.group) ?? [];
    bucket.push(it);
    groups.set(it.group, bucket);
  }
  return groups;
}

function Section({
  heading,
  icon,
  children,
}: {
  heading: string;
  icon?: React.ReactNode;
  children: React.ReactNode;
}) {
  return (
    <Command.Group
      heading={heading}
      className={cn(
        "[&_[cmdk-group-heading]]:flex [&_[cmdk-group-heading]]:items-center [&_[cmdk-group-heading]]:gap-1",
        "[&_[cmdk-group-heading]]:px-2 [&_[cmdk-group-heading]]:pb-1 [&_[cmdk-group-heading]]:pt-2",
        "[&_[cmdk-group-heading]]:text-[10px] [&_[cmdk-group-heading]]:font-semibold",
        "[&_[cmdk-group-heading]]:uppercase [&_[cmdk-group-heading]]:tracking-widest",
        "[&_[cmdk-group-heading]]:text-muted-foreground",
      )}
    >
      {icon && <span className="sr-only">{heading}</span>}
      {children}
    </Command.Group>
  );
}

function CommandRow({
  item,
  onSelect,
}: {
  item: CommandDef;
  onSelect: () => void;
}) {
  const value = [item.label, item.id, item.group, ...(item.keywords ?? [])]
    .join(" ")
    .toLowerCase();
  return (
    <Command.Item
      value={value}
      onSelect={onSelect}
      className={cn(
        "flex cursor-pointer items-center gap-2.5 rounded-md px-2.5 py-1.5 text-sm text-foreground",
        "data-[selected=true]:bg-accent/15 data-[selected=true]:text-accent",
      )}
    >
      <item.Icon className="size-4 shrink-0 text-muted-foreground" />
      <span className="flex-1 truncate">{item.label}</span>
      {item.argLabel && (
        <span className="text-[10px] text-muted-foreground">needs arg</span>
      )}
      {item.hint && (
        <kbd className="rounded bg-muted px-1.5 py-0.5 font-mono text-[10px] text-muted-foreground">
          {item.hint}
        </kbd>
      )}
    </Command.Item>
  );
}

function ArgDrillDown({
  command,
  value,
  onValue,
  onCancel,
  onSubmit,
}: {
  command: CommandDef;
  value: string;
  onValue: (s: string) => void;
  onCancel: () => void;
  onSubmit: () => void;
}) {
  return (
    <form
      className="flex flex-col"
      onSubmit={(e) => {
        e.preventDefault();
        onSubmit();
      }}
    >
      <div className="flex items-center gap-2 border-b border-border px-3">
        <ArrowRight className="size-4 text-accent" />
        <span className="flex-1 truncate text-sm text-foreground">
          {command.label}
        </span>
        <button
          type="button"
          onClick={onCancel}
          className="rounded px-1.5 py-0.5 text-[10px] text-muted-foreground hover:bg-muted hover:text-foreground"
        >
          back
        </button>
      </div>
      <div className="flex items-center gap-2 px-3 py-3">
        <input
          autoFocus
          value={value}
          onChange={(e) => onValue(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Escape") {
              e.preventDefault();
              onCancel();
            }
          }}
          placeholder={command.argPlaceholder ?? command.argLabel}
          className={cn(
            "h-10 w-full rounded-md border border-input bg-background px-3 text-sm",
            "placeholder:text-muted-foreground focus:border-accent focus:outline-none focus:ring-1 focus:ring-accent/40",
          )}
        />
        <button
          type="submit"
          disabled={!value.trim()}
          className={cn(
            "rounded-md bg-accent px-3 py-2 text-xs font-medium text-accent-foreground",
            "disabled:opacity-50",
          )}
        >
          Run ↵
        </button>
      </div>
      <footer className="px-3 pb-3 text-[10px] text-muted-foreground">
        {command.argLabel}
      </footer>
    </form>
  );
}
