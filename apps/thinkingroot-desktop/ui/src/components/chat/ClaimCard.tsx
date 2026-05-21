/**
 * Claim card — inline visualisation of one agent tool call inside a
 * streaming chat turn.
 *
 * Lifecycle:
 *
 *   proposed              → "AI is preparing X"
 *   awaiting_approval     → write tool needs human consent;
 *                           Approve / Reject buttons render
 *   executing             → tool is running (auto for reads, post-
 *                           approve for writes)
 *   finished              → output rendered, optionally as an error
 *   rejected              → reason rendered
 *
 * The `chatApprove` Tauri command resolves the matching pending
 * oneshot in the engine's `state.pending_approvals` map; the agent
 * unblocks and emits the next event (executing → finished, or
 * rejected).
 */
import { useState } from "react";
import {
  Check,
  X,
  Loader2,
  CheckCircle2,
  AlertTriangle,
  ChevronRight,
  ChevronDown,
  Lightbulb,
} from "lucide-react";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { chatApprove } from "@/lib/tauri";
import { toast } from "@/store/toast";
import type { AgentStep } from "@/types";
import { ToolStepOutput } from "./ToolStepOutput";
import {
  extractShellCommand,
  friendlyToolTitle,
  isShellTool,
  isThinkTool,
  shouldDefaultExpandStep,
  stepActivityLabel,
} from "./tool-step-present";

interface ClaimCardProps {
  step: AgentStep;
  workspace: string;
  /** Open details when embedded from the flight path. */
  startExpanded?: boolean;
}

/**
 * SOTA polish ship (2026-05-18): special-case render for the
 * Anthropic `think` tool. The tool is a no-op reasoning scratchpad,
 * NOT a substrate query — rendering it as a normal ClaimCard with
 * "checking witnesses" / "queued" / etc. is wrong. Render as a
 * compact collapsed "Thought ▸" block matching Anthropic's UX.
 */
function ThoughtBlock({ step }: { step: AgentStep }) {
  const [expanded, setExpanded] = useState(false);
  // The thought text lives in `step.input` as `{"thought": "..."}`
  // pre-execution, and as the ToolHandler's "noted: ..." echo in
  // `step.output` post-execution. Prefer the echo (it's what the
  // model actually saw); fall back to the input.
  let thoughtText = "";
  if (step.output) {
    thoughtText = step.output.replace(/^noted:\s*/, "");
  } else if (step.input) {
    try {
      const parsed = JSON.parse(step.input);
      if (parsed && typeof parsed === "object" && typeof parsed.thought === "string") {
        thoughtText = parsed.thought;
      }
    } catch {
      // ignore — show empty
    }
  }
  const isThinking = step.status === "proposed" || step.status === "executing";
  return (
    <div className="my-1 flex items-start gap-2 text-xs">
      <button
        type="button"
        onClick={() => setExpanded((v) => !v)}
        className="group inline-flex items-center gap-1 rounded px-1.5 py-0.5 text-muted-foreground hover:bg-muted/50 hover:text-foreground"
        aria-expanded={expanded}
      >
        {isThinking ? (
          <Loader2 className="h-3 w-3 animate-spin" aria-hidden />
        ) : (
          <Lightbulb className="h-3 w-3 text-amber-400/80" aria-hidden />
        )}
        <span className="italic">
          {isThinking ? "Thinking…" : expanded ? "Thought" : "Thought ▸"}
        </span>
      </button>
      {expanded && thoughtText && (
        <div className="ml-1 max-w-prose whitespace-pre-wrap text-muted-foreground/90">
          {thoughtText}
        </div>
      )}
    </div>
  );
}

export function ClaimCard({ step, workspace, startExpanded }: ClaimCardProps) {
  const [busy, setBusy] = useState<"approve" | "reject" | null>(null);
  const [rejectReason, setRejectReason] = useState("");
  const [showRejectInput, setShowRejectInput] = useState(false);
  const initialExpanded = startExpanded ?? shouldDefaultExpandStep(step);
  const [expanded, setExpanded] = useState(initialExpanded);

  if (isThinkTool(step.name)) {
    return <ThoughtBlock step={step} />;
  }

  const hasInput = !!step.input && step.input !== "{}";
  const hasOutput = !!step.output || !!step.progress;
  const hasDetails = hasInput || hasOutput;
  const friendlyName = friendlyToolTitle(step.name);
  const collapsedHint = stepActivityLabel(step);

  const onApprove = async () => {
    if (busy) return;
    setBusy("approve");
    try {
      await chatApprove({
        workspace,
        toolUseId: step.id,
        approve: true,
      });
    } catch (e) {
      toast("Approve failed", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
      setBusy(null);
    }
  };

  const onReject = async () => {
    if (busy) return;
    setBusy("reject");
    try {
      await chatApprove({
        workspace,
        toolUseId: step.id,
        approve: false,
        reason: rejectReason.trim() || undefined,
      });
    } catch (e) {
      toast("Reject failed", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
      setBusy(null);
    }
  };

  return (
    <div
      className={cn(
        "rounded-lg border border-border/45 bg-background/25 text-sm transition-colors",
        expanded ? "px-2.5 py-2" : "px-2.5 py-1.5",
        step.status === "finished" && !step.isError && "border-emerald-500/25",
        (step.status === "rejected" || (step.status === "finished" && step.isError)) &&
          "border-destructive/40",
      )}
    >
      <div className="flex min-w-0 items-center gap-2">
        <StatusIcon status={step.status} isError={step.isError} />
        <span className="shrink-0 text-xs font-medium text-foreground">
          {friendlyName}
        </span>
        <span className="text-[10px] text-muted-foreground">{statusText(step.status)}</span>
        {step.isWrite && (
          <span className="shrink-0 rounded bg-amber-500/15 px-1.5 py-0.5 font-mono text-[10px] uppercase tracking-wide text-amber-700 dark:text-amber-300">
            write
          </span>
        )}
        {!expanded && hasDetails ? (
          <span className="min-w-0 flex-1 truncate text-[11px] text-muted-foreground/75">
            {collapsedHint}
          </span>
        ) : (
          <span className="flex-1" />
        )}
        {hasDetails && (
          <button
            type="button"
            onClick={() => setExpanded((v) => !v)}
            className="ml-auto inline-flex shrink-0 items-center gap-1 rounded px-1.5 py-0.5 text-[10px] text-muted-foreground hover:bg-muted/50 hover:text-foreground"
          >
            {expanded ? <ChevronDown className="h-3 w-3" /> : <ChevronRight className="h-3 w-3" />}
            {expanded ? "Hide" : "Details"}
          </button>
        )}
      </div>

      {step.status === "awaiting_approval" && (
        <div className="mt-3 flex flex-col gap-2">
          {showRejectInput ? (
            <div className="flex flex-col gap-2">
              <input
                type="text"
                value={rejectReason}
                onChange={(e) => setRejectReason(e.target.value)}
                placeholder="Reason (optional)"
                className="rounded border bg-background px-2 py-1 text-xs"
              />
              <div className="flex gap-2">
                <Button
                  size="sm"
                  variant="destructive"
                  onClick={onReject}
                  disabled={busy != null}
                >
                  {busy === "reject" ? (
                    <Loader2 className="mr-1 h-3 w-3 animate-spin" />
                  ) : (
                    <X className="mr-1 h-3 w-3" />
                  )}
                  Reject
                </Button>
                <Button
                  size="sm"
                  variant="ghost"
                  onClick={() => {
                    setShowRejectInput(false);
                    setRejectReason("");
                  }}
                  disabled={busy != null}
                >
                  Cancel
                </Button>
              </div>
            </div>
          ) : (
            <div className="flex gap-2">
              <Button size="sm" onClick={onApprove} disabled={busy != null}>
                {busy === "approve" ? (
                  <Loader2 className="mr-1 h-3 w-3 animate-spin" />
                ) : (
                  <Check className="mr-1 h-3 w-3" />
                )}
                Approve
              </Button>
              <Button
                size="sm"
                variant="outline"
                onClick={() => setShowRejectInput(true)}
                disabled={busy != null}
              >
                Reject
              </Button>
            </div>
          )}
        </div>
      )}

      {step.status === "rejected" && step.output && (
        <p className="mt-2 text-xs italic text-muted-foreground">
          Declined: {step.output}
        </p>
      )}

      {expanded && (
        <div className="mt-2 space-y-2">
          {hasInput && (
            <ToolStepInputView
              input={step.input}
              isWrite={step.isWrite}
              toolName={step.name}
            />
          )}
          {step.status === "executing" && step.progress && (
            <div className="space-y-1">
              <ToolStepOutput step={step} live />
              {typeof step.progressBytes === "number" && step.progressBytes > 0 && (
                <span className="text-[10px] text-muted-foreground/60">
                  {humanBytes(step.progressBytes)} streamed
                </span>
              )}
            </div>
          )}
          {step.status === "finished" && step.output && (
            <ToolStepOutput step={step} />
          )}
        </div>
      )}
    </div>
  );
}

/**
 * SOTA Ship B (2026-05-18): inline JSON view for tool args. For
 * write tools the rendering highlights each top-level field as a
 * coloured row so the user can quickly scan what's about to be
 * mutated — closer to a diff than a raw `<pre>` dump. Read tools
 * use a quieter monospaced render since the args are usually a
 * short query string.
 */
function ToolStepInputView({
  input,
  isWrite,
  toolName,
}: {
  input: string;
  isWrite: boolean;
  toolName: string;
}) {
  if (isShellTool(toolName)) {
    const command = extractShellCommand(input);
    if (command) {
      return (
        <div className="rounded-md border border-border/40 bg-background/50 px-2.5 py-2">
          <p className="mb-1 text-[10px] font-medium uppercase tracking-wide text-muted-foreground/70">
            Command
          </p>
          <pre className="max-h-32 overflow-auto whitespace-pre-wrap break-words font-mono text-[11px] leading-relaxed text-foreground/90">
            {command}
          </pre>
        </div>
      );
    }
  }

  let parsed: Record<string, unknown> | null = null;
  try {
    const v = JSON.parse(input);
    if (v && typeof v === "object" && !Array.isArray(v)) {
      parsed = v as Record<string, unknown>;
    }
  } catch {
    parsed = null;
  }

  // Fallback: opaque JSON or array → render as a quieter <pre>.
  if (!parsed) {
    return (
      <pre className="overflow-x-auto whitespace-pre-wrap break-words rounded bg-background/60 p-2 font-mono text-[11px] leading-snug text-muted-foreground">
        {input}
      </pre>
    );
  }

  const entries = Object.entries(parsed);
  if (entries.length === 0) {
    return (
      <div className="rounded bg-background/60 px-2 py-1 text-[11px] italic text-muted-foreground">
        (no arguments)
      </div>
    );
  }

  return (
    <div
      className={cn(
        "rounded p-2",
        isWrite ? "border border-amber-500/20 bg-amber-500/[0.04]" : "bg-background/60",
      )}
    >
      <div className="space-y-0.5">
        {entries.map(([k, v]) => (
          <div
            key={k}
            className="flex items-baseline gap-2 font-mono text-[11px] leading-snug"
          >
            <span
              className={cn(
                "shrink-0 font-semibold",
                isWrite ? "text-amber-300/90" : "text-muted-foreground/80",
              )}
            >
              {k}:
            </span>
            <span className="min-w-0 break-words text-muted-foreground">
              {formatFieldValue(v)}
            </span>
          </div>
        ))}
      </div>
    </div>
  );
}

function formatFieldValue(v: unknown): string {
  if (typeof v === "string") return v;
  if (typeof v === "number" || typeof v === "boolean" || v === null) {
    return String(v);
  }
  try {
    return JSON.stringify(v, null, 2);
  } catch {
    return String(v);
  }
}

function humanBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

function statusText(status: AgentStep["status"]): string {
  switch (status) {
    case "proposed":
      return "queued";
    case "awaiting_approval":
      return "needs approval";
    case "executing":
      return "running";
    case "finished":
      return "done";
    case "rejected":
      return "rejected";
  }
}

function StatusIcon({
  status,
  isError,
}: {
  status: AgentStep["status"];
  isError?: boolean;
}) {
  switch (status) {
    case "proposed":
    case "awaiting_approval":
      return (
        <span className="h-2 w-2 rounded-full bg-amber-500" aria-hidden />
      );
    case "executing":
      return (
        <Loader2 className="h-3 w-3 animate-spin text-muted-foreground" />
      );
    case "finished":
      return isError ? (
        <AlertTriangle className="h-3 w-3 text-destructive" />
      ) : (
        <CheckCircle2 className="h-3 w-3 text-emerald-500" />
      );
    case "rejected":
      return <X className="h-3 w-3 text-destructive" />;
  }
}
