// apps/thinkingroot-desktop/ui/src/components/chat/GapCards.tsx
//
// Inline "by the way" gap cards. Surfaces reflection gaps the agent
// discovered during the turn — pre-baked English from the engine's
// `thinkingroot_reflect::types::GapReport.reason` field renders
// unchanged.
//
// Wire path:
//   engine `gaps` MCP tool result
//     → rest.rs::parse_gaps_surfacing → SSE `gaps_surfaced` event
//     → sidecar ChatEvent::GapsSurfaced
//     → ChatView accumulator → this component
//
// Render contract:
//   - One card per gap, dismissable per-render-life (no persistence
//     yet — see honest-scope note).
//   - Confidence badge in the corner so users can weight the
//     suggestion.
//   - "Investigate" CTA inserts a `/probe <entity>` slash command
//     into the composer (best-effort — the user can edit before
//     sending).
//
// Honest scope (v1.0):
//   - Dismiss state is component-local. A page reload re-shows the
//     gaps. Persisting "user dismissed gap X for entity Y on
//     YYYY-MM-DD" needs a substrate-side gap_resolution table we
//     don't yet expose to the desktop. Acceptable v1.0 default
//     because gaps are shown alongside the assistant's reply, not in
//     a permanent feed.

import { useState } from "react";
import { Lightbulb, X, Compass } from "lucide-react";

import type { GapEntry } from "@/lib/tauri";
import { cn } from "@/lib/utils";

interface GapCardsProps {
  gaps: GapEntry[];
  /** Optional: invoked when the user clicks "Investigate" on a card.
   *  ChatView passes a handler that pre-fills the composer. */
  onInvestigate?: (entity: string) => void;
}

export function GapCards({ gaps, onInvestigate }: GapCardsProps) {
  const [dismissed, setDismissed] = useState<Set<string>>(() => new Set());

  const visible = gaps.filter((g) => {
    const key = `${g.entity_type}:${g.entity_name}:${g.expected_claim_type}`;
    return !dismissed.has(key);
  });

  if (visible.length === 0) return null;

  return (
    <div className="space-y-2">
      {visible.map((gap, idx) => {
        const key = `${gap.entity_type}:${gap.entity_name}:${gap.expected_claim_type}`;
        const confidencePct = Math.round(gap.confidence * 100);
        return (
          <div
            key={`${key}-${idx}`}
            className={cn(
              "rounded-lg border border-amber-200 bg-amber-50/70 p-3 text-xs",
              "dark:border-amber-900/60 dark:bg-amber-950/30",
            )}
            role="note"
            aria-label="Reflection gap"
          >
            <div className="mb-1.5 flex items-start justify-between gap-2">
              <div className="flex items-center gap-1.5 text-amber-700 dark:text-amber-300">
                <Lightbulb className="h-3.5 w-3.5" aria-hidden />
                <span className="text-[10px] font-semibold uppercase tracking-wide">
                  By the way
                </span>
              </div>
              <div className="flex items-center gap-1.5">
                <span
                  className="rounded bg-amber-100 px-1.5 py-0.5 text-[10px] font-mono text-amber-700 dark:bg-amber-900/60 dark:text-amber-300"
                  title={`Sample size: ${gap.sample_size}`}
                >
                  {confidencePct}%
                </span>
                <button
                  type="button"
                  onClick={() =>
                    setDismissed((prev) => {
                      const next = new Set(prev);
                      next.add(key);
                      return next;
                    })
                  }
                  className="rounded p-0.5 text-amber-700/70 hover:bg-amber-100 hover:text-amber-900 dark:text-amber-300/70 dark:hover:bg-amber-900/60 dark:hover:text-amber-100"
                  aria-label="Dismiss"
                >
                  <X className="h-3 w-3" aria-hidden />
                </button>
              </div>
            </div>

            <p className="text-amber-900 dark:text-amber-100">{gap.reason}</p>

            <div className="mt-2 flex items-center justify-between gap-2 text-[10px] text-amber-700/80 dark:text-amber-300/80">
              <span className="font-mono">
                {gap.entity_type}: {gap.entity_name}
              </span>
              {onInvestigate && (
                <button
                  type="button"
                  onClick={() => onInvestigate(gap.entity_name)}
                  className="inline-flex items-center gap-1 rounded border border-amber-300 bg-amber-100 px-1.5 py-0.5 text-amber-800 hover:bg-amber-200 dark:border-amber-700 dark:bg-amber-900/50 dark:text-amber-200 dark:hover:bg-amber-900/80"
                >
                  <Compass className="h-3 w-3" aria-hidden />
                  Investigate
                </button>
              )}
            </div>
          </div>
        );
      })}
    </div>
  );
}
