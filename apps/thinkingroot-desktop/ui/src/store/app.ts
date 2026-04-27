import { create } from "zustand";
import { persist } from "zustand/middleware";
import type {
  ChatMessage,
  ConversationSummary,
  LiveCapsule,
  StreamState,
  Surface,
  Theme,
  TrustFilter,
} from "@/types";

/** App-wide store. Persists theme + surface selection to localStorage. */
interface AppStore {
  // UI
  surface: Surface;
  setSurface: (s: Surface) => void;
  theme: Theme;
  setTheme: (t: Theme) => void;
  sidebarOpen: boolean;
  toggleSidebar: () => void;
  rightRailOpen: boolean;
  toggleRightRail: () => void;
  commandPaletteOpen: boolean;
  setCommandPaletteOpen: (open: boolean) => void;

  // Conversations
  conversations: ConversationSummary[];
  activeConversationId: string | null;
  setActiveConversationId: (id: string | null) => void;
  /** Insert or refresh a conversation entry. Used by the chat surface
   * on every submit so the sidebar history reflects what the main pane
   * already shows. The first user message becomes the initial title;
   * subsequent calls only refresh `lastMessageAt`. */
  upsertConversation: (id: string, title: string, lastMessageAt: Date) => void;

  // Chat
  messages: Record<string, ChatMessage[]>;
  appendMessage: (conversationId: string, msg: ChatMessage) => void;
  updateMessage: (
    conversationId: string,
    messageId: string,
    patch: Partial<ChatMessage>,
  ) => void;
  streaming: StreamState | null;
  setStreaming: (s: StreamState | null) => void;
  appendStreamingDelta: (delta: string) => void;

  // Filters
  trust: TrustFilter;
  setTrust: (t: TrustFilter) => void;

  // Selected provenance pill (for the right-rail drawer)
  selectedClaimId: string | null;
  setSelectedClaimId: (id: string | null) => void;

  // Recent command palette invocations (LRU, max 8)
  recentCommandIds: string[];
  recordCommand: (id: string) => void;

  covenantOpen: boolean;
  setCovenantOpen: (open: boolean) => void;

  onboardingOpen: boolean;
  setOnboardingOpen: (open: boolean) => void;
  onboardingDismissed: boolean;
  setOnboardingDismissed: (dismissed: boolean) => void;

  // Moat
  liveCapsules: LiveCapsule[];
  setLiveCapsules: (c: LiveCapsule[]) => void;

  // Cost / token totals for the status bar
  totalCostUsd: number;
  totalTokensIn: number;
  totalTokensOut: number;
  addTurnUsage: (inTok: number, outTok: number, costUsd: number) => void;
}

export const useApp = create<AppStore>()(
  persist(
    (set) => ({
      surface: "chats",
      setSurface: (surface) => set({ surface }),
      theme: "dark",
      setTheme: (theme) => {
        set({ theme });
        if (typeof document !== "undefined") {
          document.documentElement.dataset.theme =
            theme === "auto"
              ? (window.matchMedia("(prefers-color-scheme: light)").matches
                  ? "light"
                  : "dark")
              : theme;
        }
      },
      sidebarOpen: true,
      toggleSidebar: () => set((s) => ({ sidebarOpen: !s.sidebarOpen })),
      rightRailOpen: true,
      toggleRightRail: () => set((s) => ({ rightRailOpen: !s.rightRailOpen })),
      commandPaletteOpen: false,
      setCommandPaletteOpen: (commandPaletteOpen) => set({ commandPaletteOpen }),

      conversations: [],
      activeConversationId: null,
      setActiveConversationId: (activeConversationId) =>
        set({ activeConversationId }),
      upsertConversation: (id, title, lastMessageAt) =>
        set((s) => {
          const existing = s.conversations.find((c) => c.id === id);
          if (existing) {
            return {
              conversations: s.conversations.map((c) =>
                c.id === id ? { ...c, lastMessageAt } : c,
              ),
            };
          }
          // New conversation: keep the first user line (truncated) as
          // the sidebar title — matches how every chat product titles
          // an unnamed conversation. The user can rename later.
          const fallbackTitle = title.trim().slice(0, 60) || "Untitled";
          return {
            conversations: [
              { id, title: fallbackTitle, lastMessageAt },
              ...s.conversations,
            ],
          };
        }),

      messages: {},
      appendMessage: (conversationId, msg) =>
        set((s) => ({
          messages: {
            ...s.messages,
            [conversationId]: [...(s.messages[conversationId] ?? []), msg],
          },
        })),
      updateMessage: (conversationId, messageId, patch) =>
        set((s) => {
          const current = s.messages[conversationId] ?? [];
          const next = current.map((m) =>
            m.id === messageId ? { ...m, ...patch } : m,
          );
          return {
            messages: { ...s.messages, [conversationId]: next },
          };
        }),
      streaming: null,
      setStreaming: (streaming) => set({ streaming }),
      appendStreamingDelta: (delta) =>
        set((s) => {
          if (!s.streaming) return {};
          return {
            streaming: {
              ...s.streaming,
              partial: s.streaming.partial + delta,
            },
          };
        }),

      trust: "any",
      setTrust: (trust) => set({ trust }),

      selectedClaimId: null,
      setSelectedClaimId: (selectedClaimId) => set({ selectedClaimId }),

      recentCommandIds: [],
      recordCommand: (id) =>
        set((s) => ({
          recentCommandIds: [
            id,
            ...s.recentCommandIds.filter((x) => x !== id),
          ].slice(0, 8),
        })),

      covenantOpen: false,
      setCovenantOpen: (covenantOpen) => set({ covenantOpen }),

      onboardingOpen: false,
      setOnboardingOpen: (onboardingOpen) => set({ onboardingOpen }),
      onboardingDismissed: false,
      setOnboardingDismissed: (onboardingDismissed) => set({ onboardingDismissed }),

      liveCapsules: [],
      setLiveCapsules: (liveCapsules) => set({ liveCapsules }),

      totalCostUsd: 0,
      totalTokensIn: 0,
      totalTokensOut: 0,
      addTurnUsage: (inTok, outTok, costUsd) =>
        set((s) => ({
          totalTokensIn: s.totalTokensIn + inTok,
          totalTokensOut: s.totalTokensOut + outTok,
          totalCostUsd: s.totalCostUsd + costUsd,
        })),
    }),
    {
      name: "thinkingroot-desktop-ui",
      partialize: (s) => ({
        theme: s.theme,
        surface: s.surface,
        sidebarOpen: s.sidebarOpen,
        rightRailOpen: s.rightRailOpen,
        trust: s.trust,
        recentCommandIds: s.recentCommandIds,
        onboardingDismissed: s.onboardingDismissed,
      }),
    },
  ),
);
