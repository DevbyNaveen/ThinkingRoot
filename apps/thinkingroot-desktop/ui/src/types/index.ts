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

/** Surfaces available in the icon rail. Branches were folded into
 * Satellites (each compiled folder shows its own branches inline)
 * after Phase D-15. `privacy` ships in Step 13 of the OSS v0.1
 * plan — every locally-stored datum is enumerable + redactable
 * from this surface. */
export type Surface =
  | "chats"
  | "brain"
  | "satellites"
  | "trace"
  | "privacy"
  | "settings";

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
