import { useState } from "react";
import { AlertTriangle, ChevronRight } from "lucide-react";

import type { AgentStep } from "@/types";
import { cn } from "@/lib/utils";
import { ClaimCard } from "./ClaimCard";
import { EvidenceFlightPath } from "./EvidenceFlightPath";
import { labelForTool, stepActivityLabel } from "./tool-step-present";

interface LiveActivityStripProps {
  steps: AgentStep[];
  workspace: string;
  hasAnswer: boolean;
  /** Tool steps render on the flight path — not as duplicate inline cards. */
  inlineEvidence?: boolean;
}

export function LiveActivityStrip({
  steps,
  workspace,
  hasAnswer,
  inlineEvidence = false,
}: LiveActivityStripProps) {
  const [open, setOpen] = useState(false);
  if (steps.length === 0) return null;

  const needsApproval = steps.filter((s) => s.status === "awaiting_approval");
  const failed = steps.filter((s) => s.isError || s.status === "rejected").length;
  const finished = steps.filter((s) => s.status === "finished" && !s.isError).length;
  const current = currentActivityText(steps, hasAnswer);
  const toolsRunning = steps.some(
    (s) =>
      s.status === "executing" ||
      s.status === "proposed" ||
      s.status === "awaiting_approval",
  );

  return (
    <div className="mx-auto w-full max-w-3xl px-2">
      <div>
        <div className="px-2.5 py-2">
          <div
            className="flex min-h-12 w-full items-center gap-2.5 text-xs text-muted-foreground"
            role="status"
            aria-live="polite"
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
            <button
              type="button"
              onClick={() => setOpen((v) => !v)}
              className="group inline-flex shrink-0 items-center gap-1.5 rounded-sm py-0.5 pl-1 text-[11px] tabular-nums text-muted-foreground/90 transition-colors hover:text-foreground focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring/60"
              aria-expanded={open}
              aria-label={
                open ? "Hide what the agent is doing" : "Show what the agent is doing"
              }
            >
              <span className="font-medium text-foreground/80">{steps.length}</span>
              <span className="font-normal text-muted-foreground/65">
                {toolsRunning ? "steps" : "evidence"}
              </span>
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
                  <span className="text-amber-200/90">
                    {failed} issue{failed === 1 ? "" : "s"}
                  </span>
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

          {!open && inlineEvidence ? (
            <EvidenceFlightPath
              steps={steps}
              workspace={workspace}
              statusLabel={current}
              finishedCount={finished}
              hasAnswer={hasAnswer}
              variant="hop-only"
            />
          ) : null}
        </div>

        {open ? (
          <div className="space-y-1.5 p-2">
            {inlineEvidence ? (
              steps.map((step) => (
                <ClaimCard key={step.id} step={step} workspace={workspace} />
              ))
            ) : (
              steps.map((step) => (
                <ActivityStepSummary key={step.id} step={step} />
              ))
            )}
          </div>
        ) : null}
      </div>

      {needsApproval.length > 0 && (
        <div className="mb-2 mt-2 space-y-1.5 rounded-xl border border-amber-500/25 bg-amber-500/5 p-2">
          <div className="flex items-center gap-1.5 px-1 text-[10px] font-semibold uppercase tracking-widest text-amber-200/90">
            <AlertTriangle className="size-3" />
            Action needed
          </div>
          {needsApproval.map((step) => (
            <ClaimCard key={step.id} step={step} workspace={workspace} />
          ))}
        </div>
      )}
    </div>
  );
}

/** Compact row when tool cards already render inline in the message stream. */
function ActivityStepSummary({ step }: { step: AgentStep }) {
  const status =
    step.status === "awaiting_approval"
      ? "needs approval"
      : step.status === "executing"
        ? "running"
        : step.status === "finished"
          ? step.isError
            ? "failed"
            : "done"
          : step.status === "rejected"
            ? "declined"
            : step.status;

  return (
    <div className="rounded-lg border border-border/40 bg-muted/10 px-2.5 py-2 text-[11px]">
      <div className="flex items-center gap-2">
        <span className="min-w-0 flex-1 font-medium text-foreground/85">
          {labelForTool(step.name)}
        </span>
        <span className="shrink-0 text-muted-foreground/70">{status}</span>
      </div>
      {step.status === "executing" && step.progress ? (
        <p className="mt-1 truncate font-mono text-[10px] text-muted-foreground/55">
          {stepActivityLabel(step)}
        </p>
      ) : null}
    </div>
  );
}

export function currentActivityText(steps: AgentStep[], hasAnswer: boolean): string {
  const active =
    [...steps]
      .reverse()
      .find(
        (s) =>
          s.status === "awaiting_approval" ||
          s.status === "executing" ||
          s.status === "proposed",
      ) ?? null;

  const toolsStillRunning = steps.some(
    (s) =>
      s.status === "executing" ||
      s.status === "proposed" ||
      s.status === "awaiting_approval",
  );

  if (active) {
    const verb =
      active.status === "awaiting_approval" ? "Waiting for approval" : labelForTool(active.name);
    return `${verb}…`;
  }

  if (hasAnswer && !toolsStillRunning) {
    return "Answering with verified local evidence…";
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
    return "Searching your knowledge base…";
  }

  if (steps.length > 0) {
    return "Putting it together…";
  }

  return "Flying over…";
}
