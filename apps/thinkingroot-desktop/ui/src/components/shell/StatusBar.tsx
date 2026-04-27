import {
  Zap,
  DollarSign,
  ShieldCheck,
  Users,
  Command,
  CircleSlash,
} from "lucide-react";
import { useApp } from "@/store/app";
import { formatCost, formatTokens } from "@/lib/utils";
import { Button } from "@/components/ui/button";

/** Bottom status bar — 10 segments, Antigravity-Code-style. */
export function StatusBar() {
  const totalCost = useApp((s) => s.totalCostUsd);
  const totalIn = useApp((s) => s.totalTokensIn);
  const totalOut = useApp((s) => s.totalTokensOut);
  const trust = useApp((s) => s.trust);
  const liveCapsules = useApp((s) => s.liveCapsules);
  const openCmd = useApp((s) => s.setCommandPaletteOpen);

  return (
    <footer className="flex h-7 shrink-0 items-center justify-between gap-3 border-t border-border bg-surface px-3 text-[11px] text-muted-foreground">
      <div className="flex items-center gap-4">
        <Segment Icon={ShieldCheck} label="covenant signed" tone="success" />
        <Segment Icon={Zap} label="azure · gpt-4.1-mini" />
        <Segment
          Icon={DollarSign}
          label={`${formatCost(totalCost)} today`}
          tone={totalCost > 5 ? "warn" : undefined}
        />
        <Segment label={`${formatTokens(totalIn)} in · ${formatTokens(totalOut)} out`} />
        <Segment Icon={Users} label="peers: laptop" />
        <Segment label={`trust: ${trust}`} />
        {liveCapsules.length > 0 ? (
          <Segment
            label={`${liveCapsules.length} capsule${liveCapsules.length === 1 ? "" : "s"} live`}
            tone="capsule"
          />
        ) : (
          <Segment Icon={CircleSlash} label="no live capsules" />
        )}
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

type Tone = "success" | "warn" | "capsule";

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
            : tone === "capsule"
              ? "flex items-center gap-1 text-capsule"
              : "flex items-center gap-1"
      }
    >
      {Icon ? <Icon className="size-3" /> : null}
      <span className="whitespace-nowrap">{label}</span>
    </span>
  );
}
