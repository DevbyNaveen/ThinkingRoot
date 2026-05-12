import { create } from "zustand";
import { persist } from "zustand/middleware";
import type {
  AgentStep,
  ChatMessage,
  EngramActivationEntry,
  GapEntry,
  RightRailTab,
  SettingsSectionId,
  StreamState,
  Surface,
  Theme,
  TrustFilter,
} from "@/types";
import {
  SIDEBAR_MAX_WIDTH,
  SIDEBAR_MIN_WIDTH,
} from "@/lib/sidebar-layout";

const VALID_RAIL_TABS: RightRailTab[] = [
  "compile",
  "files",
  "brain",
  "builders",
  "browser",
  "privacy",
  "terminal",
];

function normalizeRightRailTab(tab: unknown): RightRailTab {
  if (tab === "readme" || tab === "branches") return tab === "readme" ? "files" : "compile";
  if (typeof tab === "string" && (VALID_RAIL_TABS as string[]).includes(tab)) {
    return tab as RightRailTab;
  }
  return "compile";
}

function normalizeSurface(surface: unknown): Surface {
  if (surface === "branches") return "chats";
  if (
    surface === "chats" ||
    surface === "settings" ||
    surface === "docs" ||
    surface === "brain" ||
    surface === "privacy"
  ) {
    return surface;
  }
  return "chats";
}

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
  // Compile Progress
  compileProgress: CompileProgress | null;
  setCompileProgress: (p: CompileProgress | null) => void;
  /** Absolute workspace root from `started` / `booting` events; cleared when compile ends. */
  compileRootPath: string | null;
  setCompileRootPath: (p: string | null) => void;

  // UI
  surface: Surface;
  setSurface: (s: Surface) => void;
  /** When `surface === "settings"`, the left sidebar lists these. */
  settingsSection: SettingsSectionId;
  setSettingsSection: (id: SettingsSectionId) => void;
  theme: Theme;
  setTheme: (t: Theme) => void;
  sidebarOpen: boolean;
  toggleSidebar: () => void;
  rightRailOpen: boolean;
  toggleRightRail: () => void;
  rightRailTab: RightRailTab;
  setRightRailTab: (tab: RightRailTab) => void;
  rightRailWidth: number;
  setRightRailWidth: (w: number) => void;
  sidebarWidth: number;
  setSidebarWidth: (w: number) => void;
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
  /** Stream E — remove a single message from the local cache. Used by
   *  the chat composer to roll back optimistic user-message inserts
   *  when `conversationsAppendMessage` fails (honesty rule #6). */
  removeMessage: (
    workspace: string,
    conversationId: string,
    messageId: string,
  ) => void;
  setMessages: (workspace: string, conversationId: string, msgs: ChatMessage[]) => void;
  streaming: StreamState | null;
  setStreaming: (s: StreamState | null) => void;
  appendStreamingDelta: (delta: string) => void;
  /** Append a fresh agent step in `proposed` state. Idempotent on
   *  duplicate id (e.g. if the SSE handler retries). */
  upsertAgentStep: (step: AgentStep) => void;
  /** Patch an existing agent step (status/output/isError). No-op if
   *  the step id isn't tracked yet. */
  patchAgentStep: (id: string, patch: Partial<AgentStep>) => void;
  /** Append one engram activation observed during the active turn.
   *  No-op when `streaming` is null (event arrived after final). */
  appendEngramActivation: (entry: EngramActivationEntry) => void;
  /** Append reflection gaps observed during the active turn.
   *  No-op when `streaming` is null. */
  appendGaps: (gaps: GapEntry[]) => void;

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

  // Slice 9 — Pack export sheet. When non-null, the sheet is open
  // pointing at this workspace. Cleared when the user dismisses.
  packExportTarget: { workspace: string; branch?: string } | null;
  setPackExportTarget: (
    target: { workspace: string; branch?: string } | null,
  ) => void;

  // Status-bar usage totals
  totalCostUsd: number;
  totalTokensIn: number;
  totalTokensOut: number;
  addTurnUsage: (inTok: number, outTok: number, costUsd: number) => void;
}

function key(workspace: string, conversationId: string): string {
  return `${workspace}::${conversationId}`;
}

import type { CompileProgress } from "@/lib/tauri";

export const useApp = create<AppStore>()(
  persist(
    (set, get) => ({
      compileProgress: null,
      setCompileProgress: (compileProgress) => set({ compileProgress }),
      compileRootPath: null,
      setCompileRootPath: (compileRootPath) => set({ compileRootPath }),

      surface: "chats",
      setSurface: (surface) => set({ surface: normalizeSurface(surface) }),
      settingsSection: "provider",
      setSettingsSection: (settingsSection) => set({ settingsSection }),
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
      rightRailTab: "compile",
      setRightRailTab: (rightRailTab) =>
        set({ rightRailTab: normalizeRightRailTab(rightRailTab) }),
      rightRailWidth: 450,
      setRightRailWidth: (rightRailWidth) => set({ rightRailWidth }),
      sidebarWidth: 232,
      setSidebarWidth: (sidebarWidth) => {
        const w = Number.isFinite(sidebarWidth) ? sidebarWidth : SIDEBAR_MIN_WIDTH;
        set({
          sidebarWidth: Math.min(
            SIDEBAR_MAX_WIDTH,
            Math.max(SIDEBAR_MIN_WIDTH, w),
          ),
        });
      },
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
      removeMessage: (workspace, conversationId, messageId) =>
        set((s) => {
          const k = key(workspace, conversationId);
          const current = s.messages[k] ?? [];
          return {
            messages: {
              ...s.messages,
              [k]: current.filter((m) => m.id !== messageId),
            },
          };
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
      upsertAgentStep: (step) =>
        set((s) => {
          if (!s.streaming) return {};
          const existingIdx = s.streaming.agentSteps.findIndex(
            (a) => a.id === step.id,
          );
          const next = [...s.streaming.agentSteps];
          const prior = existingIdx >= 0 ? next[existingIdx] : undefined;
          if (prior) {
            next[existingIdx] = { ...prior, ...step };
          } else {
            next.push(step);
          }
          return { streaming: { ...s.streaming, agentSteps: next } };
        }),
      patchAgentStep: (id, patch) =>
        set((s) => {
          if (!s.streaming) return {};
          const idx = s.streaming.agentSteps.findIndex((a) => a.id === id);
          if (idx < 0) return {};
          const prior = s.streaming.agentSteps[idx];
          if (!prior) return {};
          const next = [...s.streaming.agentSteps];
          next[idx] = { ...prior, ...patch };
          return { streaming: { ...s.streaming, agentSteps: next } };
        }),
      appendEngramActivation: (entry) =>
        set((s) => {
          if (!s.streaming) return {};
          return {
            streaming: {
              ...s.streaming,
              engramActivations: [...s.streaming.engramActivations, entry],
            },
          };
        }),
      appendGaps: (gaps) =>
        set((s) => {
          if (!s.streaming) return {};
          return {
            streaming: {
              ...s.streaming,
              gaps: [...s.streaming.gaps, ...gaps],
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

      packExportTarget: null,
      setPackExportTarget: (packExportTarget) => set({ packExportTarget }),

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
        rightRailTab: s.rightRailTab,
        rightRailWidth: s.rightRailWidth,
        sidebarWidth: s.sidebarWidth,
        trust: s.trust,
        recentCommandIds: s.recentCommandIds,
        onboardingDismissed: s.onboardingDismissed,
        activeWorkspace: s.activeWorkspace,
      }),
      merge: (persisted, current) => {
        const p =
          persisted && typeof persisted === "object"
            ? (persisted as Partial<AppStore>)
            : {};
        const merged: AppStore = { ...current, ...p };
        merged.rightRailTab = normalizeRightRailTab(merged.rightRailTab);
        merged.surface = normalizeSurface(merged.surface);
        return merged;
      },
    },
  ),
);
