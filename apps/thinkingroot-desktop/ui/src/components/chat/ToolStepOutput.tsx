import { useState, type ReactNode } from "react";
import { Terminal } from "lucide-react";

import { cn } from "@/lib/utils";
import type { AgentStep } from "@/types";
import {
  formatShellOutput,
  isShellTool,
  parseJsonLoose,
  TOOL_OUTPUT_EXPAND_CHARS,
  TOOL_OUTPUT_PREVIEW_CHARS,
  truncateText,
} from "./tool-step-present";

interface ToolStepOutputProps {
  step: AgentStep;
  /** Live progress while executing (before final output). */
  live?: boolean;
}

export function ToolStepOutput({ step, live = false }: ToolStepOutputProps) {
  const raw = live ? (step.progress ?? "") : (step.output ?? "");
  if (!raw.trim()) return null;

  if (isShellTool(step.name)) {
    return <ShellOutputBlock raw={raw} isError={step.isError} live={live} />;
  }

  return <GenericOutputBlock raw={raw} isError={step.isError} live={live} />;
}

function ShellOutputBlock({
  raw,
  isError,
  live,
}: {
  raw: string;
  isError?: boolean;
  live?: boolean;
}) {
  const { summary, body } = formatShellOutput(raw);
  if (!body && live) {
    return (
      <pre className="max-h-40 overflow-auto whitespace-pre-wrap break-words rounded-md border border-border/40 bg-background/50 p-2.5 font-mono text-[11px] leading-relaxed text-muted-foreground">
        {raw}
        <span className="ml-0.5 inline-block h-3 w-1 animate-pulse bg-muted-foreground/40 align-middle" aria-hidden />
      </pre>
    );
  }
  if (!body) return null;

  return (
    <CollapsibleCode
      badge={
        <span className="inline-flex items-center gap-1.5 text-[10px] text-muted-foreground/80">
          <Terminal className="size-3 shrink-0" aria-hidden />
          {summary}
          {live ? " · streaming" : null}
        </span>
      }
      body={body}
      isError={isError}
      languageHint="shell"
    />
  );
}

function GenericOutputBlock({
  raw,
  isError,
  live,
}: {
  raw: string;
  isError?: boolean;
  live?: boolean;
}) {
  const parsed = parseJsonLoose(raw);
  let body = raw;
  let badge: string | null = null;

  if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
    try {
      body = JSON.stringify(parsed, null, 2);
      const keys = Object.keys(parsed as Record<string, unknown>);
      badge = `${keys.length} field${keys.length === 1 ? "" : "s"}`;
    } catch {
      body = raw;
    }
  } else if (parsed && Array.isArray(parsed)) {
    body = JSON.stringify(parsed, null, 2);
    badge = `${parsed.length} item${parsed.length === 1 ? "" : "s"}`;
  } else {
    const lines = raw.split("\n").length;
    badge = `${lines} line${lines === 1 ? "" : "s"}`;
  }

  return (
    <CollapsibleCode
      badge={
        <span className="text-[10px] text-muted-foreground/80">
          {badge}
          {live ? " · streaming" : null}
        </span>
      }
      body={body}
      isError={isError}
    />
  );
}

function CollapsibleCode({
  badge,
  body,
  isError,
  languageHint,
}: {
  badge: ReactNode;
  body: string;
  isError?: boolean;
  languageHint?: string;
}) {
  const [showAll, setShowAll] = useState(false);
  const limit = showAll ? TOOL_OUTPUT_EXPAND_CHARS : TOOL_OUTPUT_PREVIEW_CHARS;
  const { text, truncated, totalChars } = truncateText(body, limit);

  return (
    <div className="space-y-1.5">
      <div className="flex flex-wrap items-center justify-between gap-2">{badge}</div>
      <pre
        className={cn(
          "max-h-56 overflow-auto whitespace-pre-wrap break-words rounded-md border p-2.5 font-mono text-[11px] leading-relaxed",
          isError
            ? "border-destructive/30 bg-destructive/10 text-destructive"
            : "border-border/40 bg-background/50 text-muted-foreground",
        )}
        data-language={languageHint}
      >
        {text}
      </pre>
      {truncated && (
        <button
          type="button"
          onClick={() => setShowAll((v) => !v)}
          className="text-[10px] font-medium text-muted-foreground/90 underline-offset-2 hover:text-foreground hover:underline"
        >
          {showAll
            ? "Show less"
            : `Show full output (${totalChars.toLocaleString()} chars)`}
        </button>
      )}
    </div>
  );
}

