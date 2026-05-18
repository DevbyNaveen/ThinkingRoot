import { useEffect, useState } from "react";
import { Plug } from "lucide-react";

import { cn } from "@/lib/utils";
import { mcpListConnected, type McpServerRow } from "@/lib/tauri";

/**
 * Live catalog from the sidecar `/.well-known/mcp` manifest — same rows
 * the sidebar used to show under "MCP Tools".
 */
export function McpConnectedToolsList() {
  const [rows, setRows] = useState<McpServerRow[]>([]);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      setLoading(true);
      try {
        const m = await mcpListConnected();
        if (!cancelled) setRows(m);
      } catch {
        if (!cancelled) setRows([]);
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  if (loading) {
    return (
      <p className="px-4 py-4 text-[13px] text-muted-foreground sm:px-5">
        Loading tools from sidecar…
      </p>
    );
  }

  if (rows.length === 0) {
    return (
      <p className="px-4 py-4 text-[13px] leading-relaxed text-muted-foreground sm:px-5">
        Sidecar starting or unreachable — tools appear when the local MCP server
        is running.
      </p>
    );
  }

  return (
    <ul className="flex max-h-[min(50vh,24rem)] flex-col gap-0.5 overflow-y-auto p-4 sm:p-5">
      {rows.map((row, i) => (
        <li
          key={`${row.name}-${i}`}
          className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 rounded-md px-2 py-1.5 text-xs"
          title={row.description ?? row.name}
        >
          <span
            className={cn(
              "size-1.5 shrink-0 rounded-full",
              row.status === "running"
                ? "bg-emerald-500"
                : row.status === "configured"
                  ? "bg-amber-500/90"
                  : row.status === "unhealthy"
                    ? "bg-amber-500"
                    : "bg-zinc-500",
            )}
            aria-hidden
          />
          <Plug className="size-3.5 shrink-0 text-muted-foreground" aria-hidden />
          <span className="min-w-0 flex-1 basis-0 break-words text-foreground/80">
            {row.name}
          </span>
          <span className="shrink-0 rounded bg-muted/60 px-1 py-0.5 font-mono text-[9px] uppercase tracking-wider text-muted-foreground">
            {row.transport}
          </span>
        </li>
      ))}
    </ul>
  );
}
