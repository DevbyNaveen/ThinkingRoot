import { Zap, DollarSign, Command } from "lucide-react";
import { useApp } from "@/store/app";
import { formatCost, formatTokens } from "@/lib/utils";
import { Button } from "@/components/ui/button";

/** Bottom status bar — usage totals + cmd-palette shortcut. */
export function StatusBar() {
  const totalCost = useApp((s) => s.totalCostUsd);
  const totalIn = useApp((s) => s.totalTokensIn);
  const totalOut = useApp((s) => s.totalTokensOut);
  const trust = useApp((s) => s.trust);
  const openCmd = useApp((s) => s.setCommandPaletteOpen);

  return (
    <footer className="flex h-7 shrink-0 items-center justify-between gap-3 border-t border-border bg-surface px-3 text-[11px] text-muted-foreground">
      <div className="flex items-center gap-4">
        <Segment Icon={Zap} label="local sidecar" />
        <Segment
          Icon={DollarSign}
          label={`${formatCost(totalCost)} today`}
          tone={totalCost > 5 ? "warn" : undefined}
        />
        <Segment label={`${formatTokens(totalIn)} in · ${formatTokens(totalOut)} out`} />
        <Segment label={`trust: ${trust}`} />
      </div>
      <div className="flex items-center gap-1">
        <Button
          size="sm"
          variant="ghost"
          className="h-6 gap-1.5 px-2 text-[11px] text-muted-foreground hover:text-foreground"
          onClick={() => openCmd(true)}
        >
          <Command className="size-3" />
          <span className="font-mono">K</span>
          <span className="hidden md:inline">to search</span>
        </Button>
      </div>
    </footer>
  );
}

type Tone = "success" | "warn";

function Segment({
  Icon,
  label,
  tone,
}: {
  Icon?: typeof Zap;
  label: string;
  tone?: Tone;
}) {
  return (
    <span
      className={
        tone === "success"
          ? "flex items-center gap-1 text-success"
          : tone === "warn"
            ? "flex items-center gap-1 text-warn"
            : "flex items-center gap-1"
      }
    >
      {Icon ? <Icon className="size-3" /> : null}
      <span className="whitespace-nowrap">{label}</span>
    </span>
  );
}
