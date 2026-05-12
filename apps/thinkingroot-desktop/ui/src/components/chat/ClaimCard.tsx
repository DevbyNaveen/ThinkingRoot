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
} from "lucide-react";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { chatApprove } from "@/lib/tauri";
import { toast } from "@/store/toast";
import type { AgentStep } from "@/types";

interface ClaimCardProps {
  step: AgentStep;
  workspace: string;
}

export function ClaimCard({ step, workspace }: ClaimCardProps) {
  const [busy, setBusy] = useState<"approve" | "reject" | null>(null);
  const [rejectReason, setRejectReason] = useState("");
  const [showRejectInput, setShowRejectInput] = useState(false);
  const [expanded, setExpanded] = useState(false);

  const hasInput = !!step.input && step.input !== "{}";
  const hasOutput = !!step.output;
  const hasDetails = hasInput || hasOutput;
  const friendlyName = getFriendlyStepName(step.name);

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
        "rounded-lg border border-border/60 bg-background/30 px-2.5 py-2 text-sm transition-colors",
        step.status === "finished" && !step.isError && "border-emerald-500/30",
        (step.status === "rejected" || (step.status === "finished" && step.isError)) &&
          "border-destructive/40",
      )}
    >
      <div className="flex items-center gap-2">
        <StatusIcon status={step.status} isError={step.isError} />
        <span className="text-xs font-medium text-foreground">{friendlyName}</span>
        <span className="text-[10px] text-muted-foreground">{statusText(step.status)}</span>
        {step.isWrite && (
          <span className="ml-auto rounded bg-amber-500/15 px-1.5 py-0.5 font-mono text-[10px] uppercase tracking-wide text-amber-700 dark:text-amber-300">
            write
          </span>
        )}
        {hasDetails && (
          <button
            type="button"
            onClick={() => setExpanded((v) => !v)}
            className="ml-1 inline-flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px] text-muted-foreground hover:bg-muted/50 hover:text-foreground"
          >
            {expanded ? <ChevronDown className="h-3 w-3" /> : <ChevronRight className="h-3 w-3" />}
            {expanded ? "Hide details" : "Show details"}
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
            <pre className="overflow-x-auto whitespace-pre-wrap break-words rounded bg-background/60 p-2 font-mono text-[11px] leading-snug text-muted-foreground">
              {step.input}
            </pre>
          )}
          {step.status === "finished" && step.output && (
            <pre
              className={cn(
                "overflow-x-auto whitespace-pre-wrap break-words rounded p-2 font-mono text-[11px] leading-snug",
                step.isError
                  ? "bg-destructive/10 text-destructive"
                  : "bg-background/60 text-muted-foreground",
              )}
            >
              {step.output}
            </pre>
          )}
        </div>
      )}
    </div>
  );
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

function getFriendlyStepName(name: string): string {
  const n = name.toLowerCase();
  if (n.includes("witness")) return "Checking witnesses";
  if (n.includes("relation") || n.includes("graph")) return "Reading graph context";
  if (n.includes("search") || n.includes("query")) return "Searching knowledge base";
  if (n.includes("claim") || n.includes("read")) return "Reading relevant claims";
  if (n.includes("summar") || n.includes("synth")) return "Composing answer";
  return name.replace(/_/g, " ");
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
