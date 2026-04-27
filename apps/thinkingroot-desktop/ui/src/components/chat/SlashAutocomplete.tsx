import { useEffect, useMemo, useState } from "react";
import { cn } from "@/lib/utils";

type Command = { cmd: string; args?: string; desc: string };

const COMMANDS: Command[] = [
  { cmd: "/branch", args: " <name>", desc: "fork a knowledge branch" },
  { cmd: "/branches", desc: "list branches" },
  { cmd: "/checkout", args: " <name>", desc: "switch HEAD" },
  { cmd: "/merge", args: " <name>", desc: "merge into main" },
  { cmd: "/compile", desc: "recompile the workspace" },
  { cmd: "/help", desc: "show command help" },
];

export function SlashAutocomplete({
  query,
  onSelect,
  onDismiss,
}: {
  query: string;
  onSelect: (insertion: string) => void;
  onDismiss: () => void;
}) {
  const matches = useMemo(() => {
    const q = query.slice(1).toLowerCase();
    return COMMANDS.filter((c) => c.cmd.slice(1).toLowerCase().startsWith(q));
  }, [query]);

  const [active, setActive] = useState(0);

  useEffect(() => {
    setActive(0);
  }, [query]);

  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      if (matches.length === 0) return;
      if (e.key === "ArrowDown") {
        e.preventDefault();
        setActive((i) => (i + 1) % matches.length);
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        setActive((i) => (i - 1 + matches.length) % matches.length);
      } else if (e.key === "Enter" || e.key === "Tab") {
        const m = matches[active];
        if (!m) return;
        e.preventDefault();
        e.stopPropagation();
        onSelect(m.cmd + (m.args ? " " : " "));
      } else if (e.key === "Escape") {
        e.preventDefault();
        onDismiss();
      }
    }
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [matches, active, onSelect, onDismiss]);

  if (matches.length === 0) return null;

  return (
    <div className="absolute top-full left-0 right-0 z-20 mt-2">
      <div className="overflow-hidden rounded-2xl border border-border/70 bg-surface">
        <ul className="max-h-72 overflow-y-auto py-1">
          {matches.map((c, i) => (
            <li key={c.cmd}>
              <button
                type="button"
                className={cn(
                  "flex w-full items-center justify-between gap-4 px-4 py-2 text-left text-sm transition-colors",
                  i === active
                    ? "bg-accent/15 text-foreground"
                    : "text-foreground hover:bg-accent/5",
                )}
                onMouseEnter={() => setActive(i)}
                onMouseDown={(e) => {
                  e.preventDefault();
                  onSelect(c.cmd + (c.args ? " " : " "));
                }}
              >
                <span className="font-mono text-[13px]">
                  <span>{c.cmd}</span>
                  {c.args && (
                    <span className="text-muted-foreground/70">{c.args}</span>
                  )}
                </span>
                <span className="truncate text-[11px] text-muted-foreground">
                  {c.desc}
                </span>
              </button>
            </li>
          ))}
        </ul>
      </div>
    </div>
  );
}
