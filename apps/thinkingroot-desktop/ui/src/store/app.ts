import { create } from "zustand";
import { persist } from "zustand/middleware";
import type {
  ChatMessage,
  StreamState,
  Surface,
  Theme,
  TrustFilter,
} from "@/types";

/**
 * App-wide UI store.
 *
 * Conversations + messages are persisted on disk (per-workspace
 * `.thinkingroot/conversations/`). The store keeps:
 *   - in-flight UI state (current surface, active workspace, etc.)
 *   - per-conversation message *cache* keyed by `${workspace}:${id}`,
 *     hydrated lazily from `conversations_get`.
 * That keeps localStorage small and lets the disk be the source of
 * truth — no fixture data, no MOCK_*.
 */
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

  // Active selection
  activeWorkspace: string | null;
  setActiveWorkspace: (name: string | null) => void;
  activeConversationId: string | null;
  setActiveConversationId: (id: string | null) => void;

  // Cached chat messages: key = `${workspace}::${conversationId}`
  messages: Record<string, ChatMessage[]>;
  appendMessage: (workspace: string, conversationId: string, msg: ChatMessage) => void;
  updateMessage: (
    workspace: string,
    conversationId: string,
    messageId: string,
    patch: Partial<ChatMessage>,
  ) => void;
  setMessages: (workspace: string, conversationId: string, msgs: ChatMessage[]) => void;
  streaming: StreamState | null;
  setStreaming: (s: StreamState | null) => void;
  appendStreamingDelta: (delta: string) => void;

  // Per-turn routing context. Survives component re-mounts (React
  // Strict Mode, Vite HMR) so SSE Final/Error events that arrive AFTER
  // ChatView's useEffect has cleaned up still find the right
  // workspace+conversation to write the assistant reply to. Without
  // this, a stale `useRef<Map>` would lose the entry on remount and
  // the streaming state would never clear — leaving the composer
  // disabled forever.
  turnCtx: Record<string, { workspace: string; convId: string }>;
  registerTurn: (turnId: string, workspace: string, convId: string) => void;
  resolveTurn: (turnId: string) => { workspace: string; convId: string } | null;
  clearTurn: (turnId: string) => void;

  // Trust filter (used by Brain table)
  trust: TrustFilter;
  setTrust: (t: TrustFilter) => void;

  // Right-rail provenance pill
  selectedClaimId: string | null;
  setSelectedClaimId: (id: string | null) => void;

  // Command palette LRU
  recentCommandIds: string[];
  recordCommand: (id: string) => void;

  // Onboarding overlay
  onboardingOpen: boolean;
  setOnboardingOpen: (open: boolean) => void;
  onboardingDismissed: boolean;
  setOnboardingDismissed: (dismissed: boolean) => void;

  // Status-bar usage totals
  totalCostUsd: number;
  totalTokensIn: number;
  totalTokensOut: number;
  addTurnUsage: (inTok: number, outTok: number, costUsd: number) => void;
}

function key(workspace: string, conversationId: string): string {
  return `${workspace}::${conversationId}`;
}

export const useApp = create<AppStore>()(
  persist(
    (set, get) => ({
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

      activeWorkspace: null,
      setActiveWorkspace: (activeWorkspace) =>
        set({
          activeWorkspace,
          activeConversationId: null,
          streaming: null,
        }),
      activeConversationId: null,
      setActiveConversationId: (activeConversationId) =>
        set({ activeConversationId, streaming: null }),

      messages: {},
      appendMessage: (workspace, conversationId, msg) =>
        set((s) => {
          const k = key(workspace, conversationId);
          return {
            messages: {
              ...s.messages,
              [k]: [...(s.messages[k] ?? []), msg],
            },
          };
        }),
      updateMessage: (workspace, conversationId, messageId, patch) =>
        set((s) => {
          const k = key(workspace, conversationId);
          const current = s.messages[k] ?? [];
          const next = current.map((m) =>
            m.id === messageId ? { ...m, ...patch } : m,
          );
          return { messages: { ...s.messages, [k]: next } };
        }),
      setMessages: (workspace, conversationId, msgs) =>
        set((s) => ({
          messages: { ...s.messages, [key(workspace, conversationId)]: msgs },
        })),
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

      turnCtx: {},
      registerTurn: (turnId, workspace, convId) =>
        set((s) => ({
          turnCtx: { ...s.turnCtx, [turnId]: { workspace, convId } },
        })),
      resolveTurn: (turnId) => get().turnCtx[turnId] ?? null,
      clearTurn: (turnId) =>
        set((s) => {
          if (!s.turnCtx[turnId]) return {};
          const next = { ...s.turnCtx };
          delete next[turnId];
          return { turnCtx: next };
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

      onboardingOpen: false,
      setOnboardingOpen: (onboardingOpen) => set({ onboardingOpen }),
      onboardingDismissed: false,
      setOnboardingDismissed: (onboardingDismissed) => set({ onboardingDismissed }),

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
        activeWorkspace: s.activeWorkspace,
      }),
    },
  ),
);
