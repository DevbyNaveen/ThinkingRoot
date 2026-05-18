/** Theme identifiers mirror `thinkingroot-tui::app::state::Theme`. */
export type Theme = "auto" | "dark" | "light";

/** Trust filter matches `TrustFilter` in thinkingroot-tui. */
export type TrustFilter = "any" | "rooted" | "attested";

/** Right-rail panel tabs. */
export type RightRailTab =
  | "compile"
  | "files"
  | "brain"
  | "browser"
  | "privacy"
  | "terminal";

/** Sub-page inside the right-rail Workspace (files) inspector — readme first, folder tree second. */
export type WorkspaceInspectorPage = "readme" | "folder";

/** Surfaces in the layout.
 *
 * Conceptually `"chats"` and `"settings"` are the only full main-pane
 * surfaces post-Stream-F; Knowledge (brain), Privacy, and the
 * inspector rail are coordinated from here for palette shortcuts.
 * Legacy persisted value `"branches"` is normalized to `"chats"` on
 * rehydrate — branch tooling lives on the Compile rail tab. */
export type Surface =
  | "chats"
  | "playground"
  | "settings"
  | "docs"
  | "brain"
  | "privacy";

/** Left-rail categories when the main pane is on Settings. */
export type SettingsSectionId =
  | "provider"
  | "workspace"
  | "appearance"
  | "mcp"
  | "channels"
  | "cloud";

/** Left-rail categories when the main pane is on Docs. */
export type DocSectionId =
  | "overview"
  | "cursor"
  | "node"
  | "python"
  | "curl"
  | "lovable"
  | "export";

/** One entry in the conversations sidebar. */
export interface ConversationSummary {
  id: string;
  title: string;
  lastMessageAt: Date;
  pinned?: boolean;
}

/** Message kinds rendered in the chat surface. */
export type MessageKind =
  | "user"
  | "assistant"
  | "assistant-thinking"
  | "tool-use"
  | "tool-result"
  | "compact-boundary"
  | "memory-recall"
  | "rooting-progress"
  | "blindspot";

export interface ChatMessage {
  id: string;
  kind: MessageKind;
  body: string;
  at: Date;
  provenance?: Provenance[];
  tool?: { name: string; args: unknown; ok?: boolean };
  /** Post-stream verifier verdict, attached when the engine emits
   *  the `trust_receipt` SSE event after `final`. Only meaningful
   *  on kind === "assistant" messages. */
  trustReceipt?: TrustReceipt;
  /** Engrams the agent activated while producing this message,
   *  copied from the StreamState's `engramActivations` at `final`.
   *  Only meaningful on kind === "assistant" messages. */
  engramActivations?: EngramActivationEntry[];
  /** Reflection gaps the agent surfaced via the `gaps` MCP tool
   *  during this turn. Renders as inline "by the way" cards under
   *  the assistant body. Only meaningful on kind === "assistant". */
  gaps?: GapEntry[];
  /** Tool calls the agent made while producing this message — the
   *  reasoning trace. Copied from StreamState.agentSteps at `final`.
   *  Only meaningful on kind === "assistant"; renders as a collapsed
   *  accordion below the body. */
  agentSteps?: AgentStep[];
}

/** Re-export of the wire-shape gap entry for component prop types.
 *  Kept here so component files can `import type { GapEntry }` from
 *  a single source. The canonical definition lives at
 *  `lib/tauri.ts::GapEntry` (keep them aligned). */
export interface GapEntry {
  entity_name: string;
  entity_type: string;
  expected_claim_type: string;
  confidence: number;
  sample_size: number;
  reason: string;
}

export interface Provenance {
  claimId: string;
  tier: "rooted" | "attested" | "unknown";
  confidence: number;
  source: string;
  /** Statement text — populated from the live `provenance_claim`
   * event; older/persisted claims may have it undefined. */
  statement?: string;
}

/** One agent tool-call rendered as an inline claim card during a
 *  streaming agent turn. The card transitions:
 *
 *    proposed → executing → finished | rejected
 *
 *  `awaiting_approval` is a synthetic UI state set when a write
 *  tool's `approval_requested` event arrives — Approve / Reject
 *  buttons are surfaced and the card stays in this state until the
 *  user clicks one (which triggers `chat_approve` and the next
 *  event).
 */
export interface AgentStep {
  id: string;
  name: string;
  /** JSON.stringified tool input, pretty-printed for the card. */
  input: string;
  isWrite: boolean;
  status:
    | "proposed"
    | "awaiting_approval"
    | "executing"
    | "finished"
    | "rejected";
  /** Tool output (when finished) or rejection reason (when rejected). */
  output?: string;
  /** True when the tool reported a runtime error. */
  isError?: boolean;
}

/** In-flight streaming state. */
export interface StreamState {
  turnId: string;
  partial: string;
  startedAt: Date;
  tokensIn: number;
  tokensOut: number;
  /** Agent tool-call steps emitted during this turn. Empty for
   *  legacy non-agent streams. */
  agentSteps: AgentStep[];
  /** Engram activations during this turn — populated from
   *  `ChatEvent::EngramActivated`. Drives the EngramTimeline
   *  scrubber while the turn is in flight; copied to the persisted
   *  ChatMessage on `final`. */
  engramActivations: EngramActivationEntry[];
  /** Reflection gaps surfaced during this turn (from the engine's
   *  `gaps_surfaced` SSE event). Copied to the assistant
   *  ChatMessage on `final`. */
  gaps: GapEntry[];
}

/** One engram activation observed during a turn. Mirrors the
 *  rest.rs `engram_activated` SSE shape; matches what
 *  `EngramTimeline.tsx` consumes. */
export interface EngramActivationEntry {
  tool: string;
  pointer: string;
  tsMs: number;
  sourceCount?: number;
  answerCount?: number;
}

/** Live capsule rendered in the footer + toast deck. */
export interface LiveCapsule {
  id: string;
  operation: string;
  kind: string;
  graceEndsAt: Date;
}

/** Verifier verdict — one wire kind per `Verdict` variant on the
 *  engine side (intelligence/verifier.rs). The UI switches on `kind`
 *  to render colour + tooltip; the optional fields are populated
 *  per variant. */
export type TrustReceiptKind =
  | "fully_grounded"
  | "partially_grounded"
  | "unverified_citations"
  | "skipped_chitchat"
  | "skipped_rejection"
  | "skipped_bench";

/** Post-stream trust receipt attached to an assistant message.
 *  Arrives via the `chat-event` Tauri channel as `ChatEvent::TrustReceipt`
 *  (apps/thinkingroot-desktop/src-tauri/src/commands/chat.rs). */
export interface TrustReceipt {
  kind: TrustReceiptKind;
  /** Distinct claim_ids the response credits (may be empty for
   *  skip variants). Stable order from the verifier. */
  claimsUsed: string[];
  /** Present only when kind === "fully_grounded". */
  autoCitedCount?: number;
  /** Present only when kind === "partially_grounded". */
  relatedCount?: number;
  /** Present only when kind === "unverified_citations" — claim_ids
   *  the agent emitted that DON'T resolve in substrate. */
  badClaimIds?: string[];
}
