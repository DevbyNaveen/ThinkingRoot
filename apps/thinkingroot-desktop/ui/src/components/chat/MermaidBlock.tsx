import { useEffect, useId, useRef, useState } from "react";

import { cn } from "@/lib/utils";

let mermaidInit: Promise<void> | null = null;

async function ensureMermaid() {
  if (!mermaidInit) {
    mermaidInit = import("mermaid").then(({ default: mermaid }) => {
      mermaid.initialize({
        startOnLoad: false,
        theme: "dark",
        securityLevel: "strict",
        fontFamily: "ui-sans-serif, system-ui, sans-serif",
      });
    });
  }
  return mermaidInit;
}

export function MermaidBlock({ code }: { code: string }) {
  const containerRef = useRef<HTMLDivElement>(null);
  const [error, setError] = useState<string | null>(null);
  const renderId = useId().replace(/:/g, "");

  useEffect(() => {
    let cancelled = false;
    const source = code.trim();
    if (!source) return;

    void (async () => {
      setError(null);
      try {
        const { default: mermaid } = await import("mermaid");
        await ensureMermaid();
        const { svg } = await mermaid.render(`tr-mermaid-${renderId}`, source);
        if (!cancelled && containerRef.current) {
          containerRef.current.innerHTML = svg;
        }
      } catch (e) {
        if (!cancelled) {
          setError(e instanceof Error ? e.message : String(e));
        }
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [code, renderId]);

  return (
    <div className="my-5 overflow-hidden rounded-xl border border-border/55 bg-[hsl(0,0%,11%)]">
      <div className="border-b border-border/40 px-3 py-1.5 text-[11px] font-medium uppercase tracking-wide text-muted-foreground/70">
        Diagram
      </div>
      {error ? (
        <div className="space-y-2 p-3">
          <p className="text-xs text-destructive/90">
            Could not render diagram: {error}
          </p>
          <pre className="overflow-x-auto rounded-lg bg-muted/30 p-3 font-mono text-[12px] leading-relaxed text-muted-foreground">
            {code.trim()}
          </pre>
        </div>
      ) : (
        <div
          ref={containerRef}
          className={cn(
            "min-h-[4rem] overflow-x-auto p-4",
            "[&_svg]:mx-auto [&_svg]:max-w-full [&_svg]:h-auto",
          )}
        />
      )}
    </div>
  );
}
