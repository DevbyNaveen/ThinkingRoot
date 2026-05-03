/**
 * ThinkingRoot — the secondary brain for AI agents.
 *
 * This package is the canonical TypeScript SDK for talking to a
 * `root serve` daemon.  The high-level entry point is the
 * {@link Brain} class:
 *
 * ```ts
 * import { Brain } from "thinkingroot";
 *
 * const brain = await Brain.connect();              // cortex-aware
 * const { pointer } = await brain.materializeEngram("auth flow");
 * const answer = await brain.probe(pointer, "what changed?");
 * ```
 *
 * For the cortex.lock discovery primitives directly, import from
 * `"thinkingroot/cortex"`.
 */

export { Brain } from "./brain.js";
export type { BrainInfo, BrainOptions } from "./brain.js";

export { Client } from "./client.js";
export type { ClientOptions } from "./client.js";

export {
  ApiError,
  ConnectionError,
  CortexError,
  IncompatibleLockSchema,
} from "./errors.js";

export type {
  AnswerRow,
  ApiEnvelope,
  Claim,
  Entity,
  EngramRef,
  EngramScope,
  EngramSummary,
  HybridResponse,
  MaterializeResponse,
  MountSummary,
  ProbeAnswer,
  RetrievalHit,
  RetrievalRequest,
  ScoreBreakdown,
  SearchResult,
  WorkspaceInfo,
} from "./types.js";
