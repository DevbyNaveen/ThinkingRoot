import { useMemo } from "react";

import type { AgentStep } from "@/types";
import { cn } from "@/lib/utils";
import {
  useStableHopDetail,
  useStickyActiveHop,
  visibleEvidenceSteps,
} from "./evidence-hop-state";
import { shortToolVerb } from "./tool-step-present";

interface EvidenceFlightPathProps {
  steps: AgentStep[];
  workspace: string;
  statusLabel: string;
  finishedCount: number;
  hasAnswer: boolean;
}

/**
 * Single-row HUD: dragonfly + phase (“Flying over…”) + grounded count.
 * One secondary line shows only the current hop (file path pulses).
 */
export function EvidenceFlightPath({
  steps,
  statusLabel,
  finishedCount,
  variant = "full",
}: EvidenceFlightPathProps & { variant?: "full" | "hop-only" }) {
  const visible = useMemo(() => visibleEvidenceSteps(steps), [steps]);
  const { display: displayStep, rawActive } = useStickyActiveHop(visible);

  if (visible.length === 0) return null;

  const isLive = rawActive != null;

  if (variant === "hop-only") {
    return displayStep ? (
      <HopLine step={displayStep} isLive={isLive} />
    ) : null;
  }

  return (
    <section className="agent-hud" aria-label="Agent workspace trace">
      <div className="agent-hud__row" role="status" aria-live="polite">
        <span className="pixel-dragonfly shrink-0" aria-hidden>
          <span className="pixel-dragonfly__wing pixel-dragonfly__wing--left" />
          <span className="pixel-dragonfly__wing pixel-dragonfly__wing--right" />
          <span className="pixel-dragonfly__body" />
        </span>
        <span className="agent-hud__phase min-w-0 flex-1 truncate">{statusLabel}</span>
        <span className="agent-hud__count shrink-0 tabular-nums">
          <span className="agent-hud__count-num">{finishedCount}</span>
          <span className="agent-hud__count-sep">/</span>
          <span className="agent-hud__count-num">{visible.length}</span>
          <span className="agent-hud__count-label"> grounded</span>
        </span>
      </div>

      {displayStep ? (
        <HopLine step={displayStep} isLive={isLive} />
      ) : null}
    </section>
  );
}

function HopLine({ step, isLive }: { step: AgentStep; isLive: boolean }) {
  const hopTarget = useStableHopDetail(step);
  const hopVerb = shortToolVerb(step.name);
  const showVerb = hopTarget !== hopVerb && !hopTarget.startsWith(hopVerb);

  if (!hopTarget || hopTarget === "Preparing…") {
    return null;
  }

  return (
    <p
      className={cn("agent-hud__hop", isLive && "agent-hud__hop--live")}
      title={hopTarget}
    >
      {showVerb ? <span className="agent-hud__hop-verb">{hopVerb}</span> : null}
      <span
        className={cn(
          "agent-hud__hop-target font-mono",
          isLive && "agent-hud__hop-target--pulse",
        )}
      >
        {hopTarget}
      </span>
    </p>
  );
}
