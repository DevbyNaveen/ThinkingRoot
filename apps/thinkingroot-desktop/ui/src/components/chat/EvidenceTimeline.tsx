import {
  AlertTriangle,
  CheckCircle2,
  Loader2,
  Wrench,
  XCircle,
} from "lucide-react";

import type { AgentStep } from "@/types";
import {
  formatShellOutput,
  friendlyToolTitle,
  isShellTool,
  isThinkTool,
  stepActivityLabel,
} from "./tool-step-present";

/** Compact evidence list — no raw dumps (details stay inline in the thread). */
export function EvidenceTimeline({ steps }: { steps: AgentStep[] }) {
  const visible = steps.filter((s) => !isThinkTool(s.name));
  if (visible.length === 0) return null;

  return (
    <ol className="space-y-0.5 rounded-lg border border-border/35 bg-background/25 px-1 py-1">
      {visible.map((step, idx) => (
        <li
          key={step.id}
          className="flex min-w-0 items-center gap-2 rounded-md px-2 py-1.5 text-[11px]"
        >
          <StepStatusIcon step={step} />
          <span className="shrink-0 tabular-nums text-muted-foreground/50">
            {idx + 1}
          </span>
          <span className="shrink-0 font-medium text-foreground/85">
            {friendlyToolTitle(step.name)}
          </span>
          <span className="min-w-0 flex-1 truncate text-muted-foreground/70">
            {collapsedDetail(step)}
          </span>
          <span className="shrink-0 text-[10px] uppercase tracking-wide text-muted-foreground/45">
            {step.status === "finished"
              ? "done"
              : step.status === "executing"
                ? "run"
                : step.status === "awaiting_approval"
                  ? "approve"
                  : step.status}
          </span>
        </li>
      ))}
    </ol>
  );
}

function collapsedDetail(step: AgentStep): string {
  if (step.status === "rejected" && step.output) {
    return step.output.length > 80 ? `${step.output.slice(0, 78)}…` : step.output;
  }
  if (step.isError && step.output) {
    const first = step.output.split("\n")[0] ?? "Error";
    return first.length > 80 ? `${first.slice(0, 78)}…` : first;
  }
  if (isShellTool(step.name) && step.output) {
    const { summary } = formatShellOutput(step.output);
    const cmd = stepActivityLabel(step);
    return `${cmd} · ${summary}`;
  }
  return stepActivityLabel(step);
}

function StepStatusIcon({ step }: { step: AgentStep }) {
  if (step.status === "rejected") {
    return <XCircle className="size-3 shrink-0 text-rose-400" aria-hidden />;
  }
  if (step.isError) {
    return (
      <AlertTriangle className="size-3 shrink-0 text-amber-300" aria-hidden />
    );
  }
  if (step.status === "finished") {
    return (
      <CheckCircle2 className="size-3 shrink-0 text-emerald-500/90" aria-hidden />
    );
  }
  if (step.status === "executing") {
    return (
      <Loader2
        className="size-3 shrink-0 animate-spin text-muted-foreground"
        aria-hidden
      />
    );
  }
  return <Wrench className="size-3 shrink-0 text-muted-foreground/60" aria-hidden />;
}
