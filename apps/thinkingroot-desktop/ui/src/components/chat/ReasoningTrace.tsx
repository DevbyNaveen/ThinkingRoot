// apps/thinkingroot-desktop/ui/src/components/chat/ReasoningTrace.tsx
//
// Collapsed accordion under each assistant message that expands to
// show the agent's reasoning trace — every tool call + result, in
// order. Mirrors the live ClaimCard activity panel that renders
// during streaming, but as a static after-the-fact summary.
//
// Wire path:
//   StreamState.agentSteps (live during turn)
//     → copied to ChatMessage.agentSteps at `final`
//     → this component, via `<MessageBubble>` props
//
// Render contract:
//   - Header summary: "N tool calls · M failed · click to expand"
//   - Expanded body: each step as a row with tool name, status,
//     truncated input + output, error highlight if `isError`
//   - Default collapsed; user click toggles
//
// Honest scope (v1.0):
//   - We don't surface the engine's hash-chained `TraceEntry` (the
//     ed25519-signed audit log at `.thinkingroot/traces/{conv}.jsonl`).
//     That's the auditability surface; this is the "what just
//     happened" UI surface. Cross-referencing the two is a v1.1
//     feature ("verify trace" button → fetches signed log → checks
//     prev-hash chain).
//   - Long tool outputs are truncated client-side at 800 chars to
//     keep the accordion responsive. Full output is on the
//     ClaimCard surface during streaming.

import { useState } from "react";
import {
  AlertCircle,
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  ListTree,
  XCircle,
  Wrench,
} from "lucide-react";

import type { AgentStep } from "@/types";
import { cn } from "@/lib/utils";

interface ReasoningTraceProps {
  steps: AgentStep[];
}

const MAX_PREVIEW_CHARS = 800;

function truncate(text: string, max = MAX_PREVIEW_CHARS): string {
  if (text.length <= max) return text;
  return `${text.slice(0, max)}\n…[truncated, ${text.length - max} more chars]`;
}

function statusIcon(step: AgentStep) {
  if (step.status === "rejected") return XCircle;
  if (step.isError) return AlertCircle;
  if (step.status === "finished") return CheckCircle2;
  return Wrench;
}

function statusColour(step: AgentStep): string {
  if (step.status === "rejected") return "text-rose-600 dark:text-rose-400";
  if (step.isError) return "text-amber-600 dark:text-amber-400";
  if (step.status === "finished") return "text-emerald-600 dark:text-emerald-400";
  return "text-muted-foreground";
}

export function ReasoningTrace({ steps }: ReasoningTraceProps) {
  const [open, setOpen] = useState(false);

  if (steps.length === 0) return null;

  const failedCount = steps.filter((s) => s.isError || s.status === "rejected").length;
  const finishedCount = steps.filter((s) => s.status === "finished" && !s.isError).length;

  return (
    <div className="rounded border border-border/60 bg-muted/10">
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        className="flex w-full items-center gap-2 px-2.5 py-1.5 text-left text-xs hover:bg-muted/20"
        aria-expanded={open}
        aria-label={
          open ? "Hide reasoning trace" : "Show reasoning trace"
        }
      >
        {open ? (
          <ChevronDown className="h-3 w-3 flex-shrink-0" aria-hidden />
        ) : (
          <ChevronRight className="h-3 w-3 flex-shrink-0" aria-hidden />
        )}
        <ListTree className="h-3 w-3 flex-shrink-0 text-muted-foreground" aria-hidden />
        <span className="font-semibold text-muted-foreground">Reasoning trace</span>
        <span className="text-muted-foreground/70">
          {steps.length} tool call{steps.length === 1 ? "" : "s"} ·{" "}
          {finishedCount} ok
          {failedCount > 0 && (
            <span className="text-rose-600 dark:text-rose-400">
              {" "}
              · {failedCount} failed
            </span>
          )}
        </span>
      </button>

      {open && (
        <ol className="space-y-1.5 border-t border-border/60 px-2.5 py-2 text-xs">
          {steps.map((step, idx) => {
            const Icon = statusIcon(step);
            return (
              <li key={step.id} className="rounded bg-background/40 p-2">
                <div className="mb-1 flex items-center gap-1.5">
                  <Icon
                    className={cn("h-3 w-3 flex-shrink-0", statusColour(step))}
                    aria-hidden
                  />
                  <span className="text-[10px] text-muted-foreground/70">
                    #{idx + 1}
                  </span>
                  <span className="font-mono font-semibold">{step.name}</span>
                  <span className="text-[10px] uppercase tracking-wide text-muted-foreground/70">
                    {step.status}
                  </span>
                  {step.isWrite && (
                    <span className="rounded bg-amber-100 px-1 text-[10px] font-medium uppercase text-amber-700 dark:bg-amber-950/40 dark:text-amber-300">
                      write
                    </span>
                  )}
                </div>

                <details className="space-y-1">
                  <summary className="cursor-pointer text-[10px] text-muted-foreground/80">
                    input
                  </summary>
                  <pre className="mt-1 max-h-40 overflow-auto rounded bg-muted/40 p-1.5 font-mono text-[10px] text-muted-foreground">
                    {truncate(step.input, 600)}
                  </pre>
                </details>

                {step.output != null && (
                  <details className="mt-1 space-y-1" open={step.isError}>
                    <summary
                      className={cn(
                        "cursor-pointer text-[10px]",
                        step.isError
                          ? "text-rose-600 dark:text-rose-400"
                          : "text-muted-foreground/80",
                      )}
                    >
                      {step.isError ? "error" : step.status === "rejected" ? "rejection" : "output"}
                    </summary>
                    <pre
                      className={cn(
                        "mt-1 max-h-60 overflow-auto rounded p-1.5 font-mono text-[10px]",
                        step.isError
                          ? "bg-rose-50 text-rose-900 dark:bg-rose-950/40 dark:text-rose-200"
                          : "bg-muted/40 text-foreground/80",
                      )}
                    >
                      {truncate(step.output)}
                    </pre>
                  </details>
                )}
              </li>
            );
          })}
        </ol>
      )}
    </div>
  );
}
