import {
  MessageSquareText,
  Cpu,
  Orbit,
  Activity,
  ShieldCheck,
  SlidersHorizontal,
} from "lucide-react";
import { motion } from "framer-motion";
import { cn } from "@/lib/utils";
import { useApp } from "@/store/app";
import type { Surface } from "@/types";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";

interface RailItem {
  id: Surface;
  label: string;
  Icon: typeof MessageSquareText;
  hint?: string;
}

const TOP: RailItem[] = [
  { id: "chats", label: "Conversations", Icon: MessageSquareText, hint: "⌘1" },
  { id: "brain", label: "Brain", Icon: Cpu, hint: "⌘2" },
  { id: "satellites", label: "Satellites", Icon: Orbit, hint: "⌘3" },
  { id: "trace", label: "Trace", Icon: Activity, hint: "⌘4" },
  { id: "privacy", label: "Privacy", Icon: ShieldCheck, hint: "⌘5" },
];

const BOTTOM: RailItem[] = [
  { id: "settings", label: "Settings", Icon: SlidersHorizontal, hint: "⌘," },
];

/**
 * Left-most vertical rail of five surface icons. Active surface gets a
 * subtle accent pill + bold icon. Keyboard shortcuts are surfaced in
 * tooltips.
 */
export function IconRail() {
  const surface = useApp((s) => s.surface);
  const setSurface = useApp((s) => s.setSurface);

  return (
    <aside
      className="flex h-full w-14 shrink-0 flex-col items-center border-r border-border bg-surface"
      aria-label="Primary navigation"
    >
      <div className="window-drag flex h-11 w-full shrink-0 items-center justify-center border-b border-transparent">
        <img
          src="/logo.png"
          alt="ThinkingRoot logo"
          draggable={false}
          className="window-no-drag h-7 w-7 object-contain opacity-80"
        />
      </div>
      <div className="flex w-full flex-1 flex-col items-center justify-between pb-3 pt-2">
        <nav className="flex flex-col items-center gap-2">
          {TOP.map((item) => (
            <RailButton
              key={item.id}
              item={item}
              active={surface === item.id}
              onClick={() => setSurface(item.id)}
            />
          ))}
        </nav>
        <div className="flex flex-col items-center gap-2">
          {BOTTOM.map((item) => (
            <RailButton
              key={item.id}
              item={item}
              active={surface === item.id}
              onClick={() => setSurface(item.id)}
            />
          ))}
        </div>
      </div>
    </aside>
  );
}

function RailButton({
  item,
  active,
  onClick,
}: {
  item: RailItem;
  active: boolean;
  onClick: () => void;
}) {
  const { Icon } = item;
  return (
    <Tooltip delayDuration={200}>
      <TooltipTrigger asChild>
        <button
          type="button"
          onClick={onClick}
          aria-label={item.label}
          aria-current={active ? "page" : undefined}
          className={cn(
            "window-no-drag relative flex h-9 w-9 items-center justify-center rounded-md transition-colors",
            "hover:bg-muted/60 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
            active ? "text-accent" : "text-muted-foreground",
          )}
        >
          {active && (
            <motion.span
              layoutId="rail-active"
              className="absolute inset-0 rounded-md bg-accent/10"
              transition={{ type: "spring", stiffness: 500, damping: 35 }}
            />
          )}
          <Icon className="relative z-10 size-4" strokeWidth={active ? 2 : 1.5} />
        </button>
      </TooltipTrigger>
      <TooltipContent side="right" className="flex items-center gap-2">
        <span>{item.label}</span>
        {item.hint && (
          <kbd className="rounded bg-muted px-1.5 py-0.5 font-mono text-[10px] text-muted-foreground">
            {item.hint}
          </kbd>
        )}
      </TooltipContent>
    </Tooltip>
  );
}
