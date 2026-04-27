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

/** Surfaces in the new tree-sidebar layout. Conversations is the
 * home screen — workspaces and their conversations live as nested
 * tree entries under it. Brain, Privacy, and Settings each get a
 * full-pane workbench. Trace + Satellites tabs are dropped in
 * favour of context-aware right rails. */
export type Surface = "chats" | "brain" | "privacy" | "settings";

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

/** In-flight streaming state. */
export interface StreamState {
  turnId: string;
  partial: string;
  startedAt: Date;
  tokensIn: number;
  tokensOut: number;
}

/** Live capsule rendered in the footer + toast deck. */
export interface LiveCapsule {
  id: string;
  operation: string;
  kind: string;
  graceEndsAt: Date;
}
