import { useEffect, useMemo, useRef, useState } from "react";

import type { AgentStep } from "@/types";
import {
  extractFilePath,
  extractShellCommand,
  isThinkTool,
  stepActivityLabel,
} from "./tool-step-present";

/** Last in-flight hop — steps are append-only during a turn. */
export function findLastActiveStep(steps: AgentStep[]): AgentStep | null {
  for (let i = steps.length - 1; i >= 0; i--) {
    const s = steps[i]!;
    if (
      s.status === "executing" ||
      s.status === "proposed" ||
      s.status === "awaiting_approval"
    ) {
      return s;
    }
  }
  return null;
}

export function isStepTerminal(step: AgentStep): boolean {
  return step.status === "finished" || step.status === "rejected";
}

export function allStepsTerminal(steps: AgentStep[]): boolean {
  return steps.length > 0 && steps.every(isStepTerminal);
}

const HOP_HOLD_MS = 480;

/**
 * Keeps the hero hop visible across micro-gaps between tool_call_finished
 * and the next tool_call_proposed so the UI does not flash "landed".
 */
export function useStickyActiveHop(visible: AgentStep[]) {
  const rawActive = useMemo(() => findLastActiveStep(visible), [visible]);
  const [held, setHeld] = useState<AgentStep | null>(null);
  const holdTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    if (holdTimer.current) {
      clearTimeout(holdTimer.current);
      holdTimer.current = null;
    }

    if (rawActive) {
      setHeld(rawActive);
      return;
    }

    holdTimer.current = setTimeout(() => {
      setHeld(null);
      holdTimer.current = null;
    }, HOP_HOLD_MS);

    return () => {
      if (holdTimer.current) {
        clearTimeout(holdTimer.current);
        holdTimer.current = null;
      }
    };
  }, [rawActive, rawActive?.id, rawActive?.status]);

  const display = rawActive ?? held;
  const isHolding = rawActive == null && held != null;

  return { display, rawActive, isHolding };
}

function trimHopDetail(text: string, max = 88): string {
  const flat = text.replace(/\s+/g, " ").trim();
  if (flat.length <= max) return flat;
  return flat.length > max ? `${flat.slice(0, max - 1)}…` : flat;
}

/** Path / command labels stick per step id so labels never flicker mid-hop. */
export function useStableHopDetail(step: AgentStep): string {
  const cache = useRef(new Map<string, string>());

  if (!cache.current.has(step.id)) {
    const path = extractFilePath(step.input);
    if (path) {
      cache.current.set(
        step.id,
        path.length > 88 ? `…${path.slice(-86)}` : path,
      );
    } else {
      const cmd = extractShellCommand(step.input);
      if (cmd) {
        cache.current.set(step.id, trimHopDetail(cmd));
      } else if (step.status !== "proposed") {
        const live = stepActivityLabel(step);
        if (live && live !== "Shell command") {
          cache.current.set(step.id, live);
        }
      }
    }
  }

  const cached = cache.current.get(step.id);
  if (cached) return cached;

  if (step.status === "proposed") {
    return "Preparing…";
  }

  return stepActivityLabel(step);
}

export function visibleEvidenceSteps(steps: AgentStep[]): AgentStep[] {
  return steps.filter((s) => !isThinkTool(s.name));
}
