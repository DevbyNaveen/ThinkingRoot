import { useState } from "react";
import { AlertTriangle, ChevronRight } from "lucide-react";

import type { AgentStep } from "@/types";
import { cn } from "@/lib/utils";
import { ClaimCard } from "./ClaimCard";

interface LiveActivityStripProps {
  steps: AgentStep[];
  workspace: string;
  hasAnswer: boolean;
}

export function LiveActivityStrip({
  steps,
  workspace,
  hasAnswer,
}: LiveActivityStripProps) {
  const [open, setOpen] = useState(false);
  if (steps.length === 0) return null;

  const needsApproval = steps.filter((s) => s.status === "awaiting_approval");
  const failed = steps.filter((s) => s.isError || s.status === "rejected").length;
  const finished = steps.filter((s) => s.status === "finished" && !s.isError).length;
  const current = currentActivityText(steps, hasAnswer);

  return (
    <div className="mx-auto w-full max-w-3xl space-y-2 px-2">
      {/* Dragonfly + status + evidence control — no circular spinner (dragonfly is enough) */}
      <div
        className="flex min-h-12 w-full items-center gap-2.5 text-xs text-muted-foreground"
        role="status"
        aria-label={current}
      >
        <span className="pixel-dragonfly shrink-0" aria-hidden>
          <span className="pixel-dragonfly__wing pixel-dragonfly__wing--left" />
          <span className="pixel-dragonfly__wing pixel-dragonfly__wing--right" />
          <span className="pixel-dragonfly__body" />
        </span>
        {failed > 0 ? (
          <AlertTriangle className="size-3 shrink-0 text-amber-300" aria-hidden />
        ) : null}
        <span className="min-w-0 flex-1 truncate font-medium tracking-[0.01em] text-muted-foreground">
          {current}
        </span>
        {/* Minimal text control — no pill/box; expands detail on click */}
        <button
          type="button"
          onClick={() => setOpen((v) => !v)}
          className="group inline-flex shrink-0 items-center gap-1.5 rounded-sm py-0.5 pl-1 text-[11px] tabular-nums text-muted-foreground/90 transition-colors hover:text-foreground focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring/60"
          aria-expanded={open}
          aria-label={open ? "Hide evidence details" : "Show evidence details"}
        >
          <span className="font-medium text-foreground/80">{steps.length}</span>
          <span className="font-normal text-muted-foreground/65">evidence</span>
          {finished > 0 && (
            <>
              <span className="text-muted-foreground/40" aria-hidden>
                ·
              </span>
              <span className="text-muted-foreground/70">
                {finished}/{steps.length} ok
              </span>
            </>
          )}
          {failed > 0 && (
            <>
              <span className="text-muted-foreground/40" aria-hidden>
                ·
              </span>
              <span className="text-amber-200/90">{failed} issue{failed === 1 ? "" : "s"}</span>
            </>
          )}
          <ChevronRight
            className={cn(
              "size-3.5 shrink-0 text-muted-foreground/45 transition-transform duration-200 group-hover:text-muted-foreground/70",
              open && "rotate-90",
            )}
            aria-hidden
          />
        </button>
      </div>

      {needsApproval.length > 0 && (
        <div className="space-y-1.5 rounded-xl border border-amber-500/25 bg-amber-500/5 p-2">
          <div className="flex items-center gap-1.5 px-1 text-[10px] font-semibold uppercase tracking-widest text-amber-200/90">
            <AlertTriangle className="size-3" />
            Action needed
          </div>
          {needsApproval.map((step) => (
            <ClaimCard key={step.id} step={step} workspace={workspace} />
          ))}
        </div>
      )}

      {open && (
        <div className="space-y-1.5 rounded-xl border border-border/50 bg-muted/10 p-2">
          {steps.map((step) => (
            <ClaimCard key={step.id} step={step} workspace={workspace} />
          ))}
        </div>
      )}
    </div>
  );
}

export function currentActivityText(steps: AgentStep[], hasAnswer: boolean): string {
  const active =
    steps.find((s) => s.status === "awaiting_approval") ??
    [...steps].reverse().find((s) => s.status === "executing" || s.status === "proposed");

  if (active) {
    const verb =
      active.status === "awaiting_approval" ? "Waiting for approval" : labelForTool(active.name);
    return `${verb}...`;
  }

  if (hasAnswer) {
    return "Answering with verified local evidence...";
  }

  const recent = [...steps].reverse();
  const searchStep = recent.find((s) => {
    const n = s.name.toLowerCase();
    return (
      n.includes("search") ||
      n.includes("query") ||
      n.includes("witness") ||
      n.includes("relation")
    );
  });
  if (searchStep && searchStep.status !== "rejected") {
    return "Searching your knowledge base...";
  }

  if (steps.length > 0) {
    return "Putting it together...";
  }

  return "Flying over...";
}

export function labelForTool(name: string): string {
  const n = name.toLowerCase();
  if (n.includes("witness")) return "Checking witnesses";
  if (n.includes("relation") || n.includes("graph")) return "Reading graph context";
  if (n.includes("search") || n.includes("query")) return "Searching knowledge base";
  if (n.includes("claim") || n.includes("read")) return "Reading relevant claims";
  if (n.includes("summar") || n.includes("synth")) return "Composing answer";
  return name.replace(/_/g, " ");
}
