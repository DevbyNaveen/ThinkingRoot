/** Theme identifiers mirror `thinkingroot-tui::app::state::Theme`. */
export type Theme =
  | "auto"
  | "dark"
  | "light"
  | "daltonized-protanopia"
  | "daltonized-deuteranopia"
  | "daltonized-tritanopia";

/** Trust filter matches `TrustFilter` in thinkingroot-tui. */
export type TrustFilter = "any" | "rooted" | "attested";

/** Right-rail panel tabs. */
export type RightRailTab =
  | "compile"
  | "files"
  | "brain"
  | "readme"
  | "branches"
  | "privacy";

/** Surfaces in the layout.
 *
 * Conceptually `"chats"` and `"settings"` are the only full main-pane
 * surfaces post-Stream-F; Brain/Branches/Privacy have moved to the
 * right-rail tab panel. The two legacy values stay in the union
 * because the command palette + IconRail still navigate via Surface
 * tags as a side-effect ("show me Brain" → flips the right-rail tab
 * to brain). Removing them would force every call site through a
 * separate "rail target" type for no real gain. */
export type Surface = "chats" | "settings" | "brain" | "privacy" | "branches";

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
}

/** Live capsule rendered in the footer + toast deck. */
export interface LiveCapsule {
  id: string;
  operation: string;
  kind: string;
  graceEndsAt: Date;
}
