import { useEffect, useMemo, useRef, useState } from "react";

import { cn } from "@/lib/utils";
import { ChatMarkdown } from "./ChatMarkdown";
import { splitStreamingMarkdown } from "./streaming-markdown";

const COMMIT_THROTTLE_MS = 96;

function useThrottledMarkdownCommit(committed: string, active: boolean): string {
  const [displayed, setDisplayed] = useState(committed);
  const latest = useRef(committed);

  latest.current = committed;

  useEffect(() => {
    if (!active) {
      setDisplayed(committed);
      return;
    }
    if (
      committed.endsWith("\n\n") ||
      committed.endsWith("```\n") ||
      committed.endsWith("```")
    ) {
      setDisplayed(committed);
      return;
    }
    const id = window.setTimeout(() => {
      setDisplayed(latest.current);
    }, COMMIT_THROTTLE_MS);
    return () => window.clearTimeout(id);
  }, [committed, active]);

  return active ? displayed : committed;
}

export function StreamingAssistantContent({
  body,
  className,
}: {
  body: string;
  className?: string;
}) {
  const split = useMemo(() => splitStreamingMarkdown(body), [body]);
  const displayedCommitted = useThrottledMarkdownCommit(split.committed, true);
  const hasCommitted = displayedCommitted.length > 0;
  const hasTail = split.tail.length > 0;

  return (
    <div
      className={cn("stream-assistant relative", className)}
      aria-live="polite"
      aria-busy="true"
    >
      {hasCommitted ? (
        <div className="stream-assistant__committed">
          <ChatMarkdown>{displayedCommitted}</ChatMarkdown>
        </div>
      ) : null}
      {hasTail ? (
        <pre
          className={cn(
            "stream-assistant__tail",
            hasCommitted && "stream-assistant__tail--continued",
          )}
        >
          <code>{split.tail}</code>
          <span className="stream-caret" aria-hidden />
        </pre>
      ) : (
        <span className="stream-caret stream-caret--solo" aria-hidden />
      )}
    </div>
  );
}
