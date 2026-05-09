// apps/thinkingroot-desktop/ui/src/components/chat/EngramTimeline.tsx
//
// Per-turn engram-activation timeline. Right-rail / inline scrubber
// that shows which engrams the agent activated during the current
// turn and when.
//
// Wire path:
//   engine `agent_stream_response` parses ToolCallFinished for
//     `materialize_engram` / `probe_engram` (rest.rs::parse_engram_activation)
//     → emits `event: engram_activated` SSE
//     → sidecar parses into `ChatEvent::EngramActivated`
//     → ChatView passes activations down here
//   render: horizontal lane with one ▣ per activation, hovered cell
//     shows pointer + tool + age
//
// Honest scope (v1.0):
//   - Per-turn timeline only. Cross-turn / historical scrubbing
//     (drag to "rewind to past state") is NOT implemented — that
//     would need a substrate-side engram-history endpoint we don't
//     yet expose. Surfacing the per-turn footprint is the
//     load-bearing UX win; cross-turn rewind is v1.1.
//   - The `summary` payload from the engine is opaque (`unknown`)
//     here — we read pointer + tool + counts only. Full summary
//     drilldown happens via probe_engram, not via this scrubber.

import { useMemo, useState } from "react";
import { Brain, Layers, MessageCircleQuestion } from "lucide-react";

import { cn } from "@/lib/utils";

export interface EngramActivationEntry {
  /** "materialize_engram" or "probe_engram"; may be other strings if
   *  future tools also emit engram_activated. */
  tool: string;
  pointer: string;
  /** Wall-clock ms at which the SSE relay observed the activation. */
  tsMs: number;
  /** materialize_engram only. */
  sourceCount?: number;
  /** probe_engram only. */
  answerCount?: number;
}

interface EngramTimelineProps {
  /** Ordered oldest → newest. */
  activations: EngramActivationEntry[];
  /** Wall-clock ms the turn started, used to label the leftmost
   *  position. If absent, the first activation's tsMs is used. */
  turnStartedAtMs?: number;
  /** Optional title; defaults to "Engrams". */
  title?: string;
  /** When false, the title row is hidden (panelMode for sidebars). */
  showHeader?: boolean;
}

function ageLabel(deltaMs: number): string {
  if (deltaMs < 0) return "0s";
  const seconds = Math.floor(deltaMs / 1000);
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  return `${minutes}m`;
}

function toolGlyph(tool: string) {
  if (tool === "materialize_engram") return Layers;
  if (tool === "probe_engram") return MessageCircleQuestion;
  return Brain;
}

export function EngramTimeline({
  activations,
  turnStartedAtMs,
  title = "Engrams",
  showHeader = true,
}: EngramTimelineProps) {
  const [hovered, setHovered] = useState<number | null>(null);

  const start = useMemo(() => {
    if (typeof turnStartedAtMs === "number") return turnStartedAtMs;
    if (activations.length > 0) return activations[0]!.tsMs;
    return Date.now();
  }, [activations, turnStartedAtMs]);

  const end = useMemo(() => {
    if (activations.length === 0) return start + 1_000;
    return Math.max(start + 1_000, activations[activations.length - 1]!.tsMs);
  }, [activations, start]);

  // Distinct pointers in this turn. Engrams persist across activations
  // (materialize then probe-multiple-times) — the timeline collapses
  // re-uses to the same lane row but renders distinct cells.
  const distinctPointers = useMemo(() => {
    const set = new Set<string>();
    for (const a of activations) set.add(a.pointer);
    return Array.from(set);
  }, [activations]);

  if (activations.length === 0) {
    return (
      <div className="rounded border border-dashed border-border/60 bg-muted/10 p-3 text-xs text-muted-foreground">
        No engrams activated this turn yet.
      </div>
    );
  }

  const totalSpan = Math.max(1, end - start);

  return (
    <div className="rounded border border-border/60 bg-muted/10 p-2">
      {showHeader && (
        <div className="mb-1.5 flex items-center justify-between">
          <div className="flex items-center gap-1.5">
            <Brain className="h-3 w-3 text-muted-foreground" aria-hidden />
            <span className="text-[10px] font-semibold uppercase tracking-wide text-muted-foreground">
              {title}
            </span>
            <span className="text-[10px] text-muted-foreground/70">
              {activations.length} activation
              {activations.length === 1 ? "" : "s"} · {distinctPointers.length}{" "}
              pointer{distinctPointers.length === 1 ? "" : "s"}
            </span>
          </div>
          <span
            className="text-[10px] text-muted-foreground/70"
            title={`Span: ${ageLabel(totalSpan)}`}
          >
            {ageLabel(totalSpan)}
          </span>
        </div>
      )}

      <div className="relative h-7 rounded bg-background/40">
        {/* Tick marks at 0/25/50/75/100% of the span. */}
        {[0, 0.25, 0.5, 0.75, 1].map((frac) => (
          <div
            key={frac}
            className="absolute top-0 bottom-0 border-l border-border/40"
            style={{ left: `${frac * 100}%` }}
            aria-hidden
          />
        ))}
        {activations.map((a, idx) => {
          const Glyph = toolGlyph(a.tool);
          const frac = (a.tsMs - start) / totalSpan;
          const left = Math.max(0, Math.min(0.98, frac));
          const isHovered = hovered === idx;
          return (
            <button
              key={`${a.tsMs}-${idx}`}
              type="button"
              onMouseEnter={() => setHovered(idx)}
              onMouseLeave={() => setHovered((h) => (h === idx ? null : h))}
              onFocus={() => setHovered(idx)}
              onBlur={() => setHovered((h) => (h === idx ? null : h))}
              className={cn(
                "absolute top-1/2 -translate-x-1/2 -translate-y-1/2 inline-flex h-4 w-4 items-center justify-center rounded-full border transition-transform",
                a.tool === "materialize_engram"
                  ? "border-emerald-300 bg-emerald-50 text-emerald-700 dark:border-emerald-700 dark:bg-emerald-950/40 dark:text-emerald-300"
                  : a.tool === "probe_engram"
                    ? "border-blue-300 bg-blue-50 text-blue-700 dark:border-blue-700 dark:bg-blue-950/40 dark:text-blue-300"
                    : "border-zinc-300 bg-zinc-50 text-zinc-700 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-300",
                isHovered && "z-20 scale-125 shadow",
              )}
              style={{ left: `${left * 100}%` }}
              aria-label={`${a.tool} ${a.pointer} at ${ageLabel(a.tsMs - start)}`}
              title={`${a.tool}\npointer: ${a.pointer}\nt+${ageLabel(a.tsMs - start)}${
                typeof a.sourceCount === "number"
                  ? `\nsources: ${a.sourceCount}`
                  : ""
              }${
                typeof a.answerCount === "number"
                  ? `\nanswers: ${a.answerCount}`
                  : ""
              }`}
            >
              <Glyph className="h-2.5 w-2.5" aria-hidden />
            </button>
          );
        })}
      </div>

      {/* Pointer lane: each distinct pointer rendered once with its
          last-known counts. Compact list so the right-rail can render
          this in 200px width. */}
      <ul className="mt-2 space-y-0.5">
        {distinctPointers.map((ptr) => {
          const last = [...activations].reverse().find((a) => a.pointer === ptr);
          if (!last) return null;
          return (
            <li
              key={ptr}
              className="flex items-center gap-1.5 text-[10px] font-mono text-muted-foreground"
            >
              <span className="text-foreground">{ptr}</span>
              {typeof last.sourceCount === "number" && (
                <span title="Source count">·{last.sourceCount} src</span>
              )}
              {typeof last.answerCount === "number" && (
                <span title="Probe answers">·{last.answerCount} ans</span>
              )}
            </li>
          );
        })}
      </ul>
    </div>
  );
}
