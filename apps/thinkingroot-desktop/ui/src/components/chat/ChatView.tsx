/**
 * Chat surface — the home screen.
 *
 * State machine:
 *   no workspace             → "Pick a workspace" empty state
 *   workspace, no conv       → "Start a conversation in <ws>" composer-only
 *   workspace, conv selected → message list + composer
 *
 * Persistence: every user/assistant message is appended to disk via
 * `conversations_append_message`. The local store caches them by
 * `${workspace}::${id}` so re-selecting an old conversation re-renders
 * instantly without re-reading from disk.
 *
 * Streaming: `chat_send_stream` returns immediately with a `turn_id`;
 * the backend POSTs to the sidecar `/api/v1/ws/{ws}/ask` and emits
 * `chat-event` Tauri events. The composer disables until `final` or
 * `error`. Token-by-token playback is simulated server-side today
 * (see `crates/desktop/.../commands/chat.rs`); when the engine ships
 * SSE we drop the simulator with no UI change.
 */
import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ComponentProps,
  type ReactNode,
} from "react";
import {
  ArrowUp,
  Square,
  AlertTriangle,
  ChevronDown,
  Hammer,
  Inbox,
  Loader2,
  Mic,
  Plus,
  FileText,
  Image as ImageIcon,
  FolderOpen,
  ClipboardPaste,
  Code2,
  Copy,
  Share2,
} from "lucide-react";
import { readText, writeText } from "@tauri-apps/plugin-clipboard-manager";
import { open as openDialog } from "@tauri-apps/plugin-dialog";

import { cn } from "@/lib/utils";
import {
  COMPOSER_FILE_DROP_EVENT,
  formatDroppedPathsForComposer,
} from "@/lib/format-dropped-paths";
import { useApp } from "@/store/app";
import { Button } from "@/components/ui/button";
import { toast } from "@/store/toast";
import { ChatMarkdown } from "@/components/chat/ChatMarkdown";
import { UserMessageContent } from "@/components/chat/UserMessageContent";
import {
  pickPrimaryDiagnostic,
  useWorkspaceStatus,
  useWorkspaceStatusSubscription,
} from "@/store/workspace-status";
import {
  chatSendStream,
  conversationsAppendMessage,
  conversationsCreate,
  conversationsGenerateTitle,
  conversationsGet,
  llmHealth,
  workspaceCompile,
  workspaceList,
  onChatEvent,
  type ChatEvent,
  type ChatTurnPayload,
  type IncrementalSummary,
  type LlmHealth,
} from "@/lib/tauri";
import { BrainCitationParser, useBrainActivation } from "@/store/brain";
import type {
  AgentStep,
  ChatMessage,
  ContinuationOffer,
  EngramActivationEntry,
  GapEntry,
  StreamState,
} from "@/types";
import { BranchChip } from "./BranchChip";
import { TopicBranchesPanel } from "@/components/branches/TopicBranchesPanel";
import { PermissionPromptDialog } from "@/components/permissions/PermissionPromptDialog";
import { ClaimCard } from "./ClaimCard";
import { LiveActivityStrip } from "./LiveActivityStrip";
import { SlashAutocomplete } from "./SlashAutocomplete";
import { EngramTimeline } from "./EngramTimeline";
import { GapCards } from "./GapCards";
import { ReasoningTrace } from "./ReasoningTrace";
import { TrustReceiptChip } from "./TrustReceipt";
import { runSlashCommand } from "./slashCommands";
import { UpgradeBanner, parseUpgradeReason } from "@/components/cloud/UpgradeBanner";

/** Stable pretty-print of a tool-call input for display in a claim
 *  card. Errors fall back to the original .toString() so the card
 *  always renders something rather than crashing. */
function prettyJson(value: unknown): string {
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return String(value);
  }
}

/** Maximum chat turns we forward to the engine as history. The
 *  conversational system prompt's "treat history as memory" rule
 *  caps usefully around 6-8 turns; longer windows blow context
 *  budget without improving recall. */
const MAX_HISTORY_TURNS = 8;

/** Project the local message cache into the wire-format history
 *  the engine's `/ask/stream` endpoint accepts. Only `user` and
 *  `assistant` kinds map to chat turns — every other UI-only
 *  message kind (tool-use, memory-recall, compact-boundary, …) is
 *  dropped because the LLM never produced them as chat turns and
 *  shouldn't see them as such on the next turn. */
function buildHistoryPayload(messages: ChatMessage[]): ChatTurnPayload[] {
  const turns: ChatTurnPayload[] = [];
  for (const m of messages) {
    if (m.kind === "user") {
      turns.push({ role: "user", content: m.body });
    } else if (m.kind === "assistant") {
      turns.push({ role: "assistant", content: m.body });
    }
  }
  return turns.slice(-MAX_HISTORY_TURNS);
}

/** One user question plus every assistant reply before the next user turn. */
interface ConversationTurn {
  user: ChatMessage;
  replies: ChatMessage[];
}

function groupMessagesIntoTurns(messages: ChatMessage[]): ConversationTurn[] {
  const turns: ConversationTurn[] = [];
  for (const m of messages) {
    if (m.kind === "user") {
      turns.push({ user: m, replies: [] });
    } else if (turns.length > 0) {
      turns[turns.length - 1]!.replies.push(m);
    }
  }
  return turns;
}

/** Cursor-style: land the new question ~35% below the viewport top so ~65%
 *  of the pane still shows the tail of the prior answer. */
const QUESTION_SCROLL_ANCHOR_RATIO = 0.35;

function scrollQuestionIntoView(
  container: HTMLDivElement,
  questionEl: HTMLElement,
  anchorRatio = QUESTION_SCROLL_ANCHOR_RATIO,
) {
  const cRect = container.getBoundingClientRect();
  const qRect = questionEl.getBoundingClientRect();
  const targetTop =
    container.scrollTop + (qRect.top - cRect.top) - cRect.height * anchorRatio;
  container.scrollTo({
    top: Math.max(0, targetTop),
    behavior: "smooth",
  });
}

/** Collapse temp-id + disk-id echo pairs left in cache from older builds. */
function collapseClientDiskEcho(msgs: ChatMessage[]): ChatMessage[] {
  const THRESH_MS = 8000;
  const out: ChatMessage[] = [];
  for (const m of msgs) {
    const prev = out[out.length - 1];
    if (
      prev &&
      prev.kind === m.kind &&
      prev.body === m.body &&
      Math.abs(m.at.getTime() - prev.at.getTime()) <= THRESH_MS
    ) {
      const prevTemp = prev.id.startsWith("m-");
      const curTemp = m.id.startsWith("m-");
      if (prevTemp && !curTemp) {
        out[out.length - 1] = m;
        continue;
      }
      if (!prevTemp && curTemp) {
        continue;
      }
    }
    out.push(m);
  }
  return out;
}

export function ChatView() {
  const activeWorkspace = useApp((s) => s.activeWorkspace);
  // Slice 0 — keep the unified workspace-status SSE subscription alive
  // for as long as the chat surface is mounted. This is what powers the
  // banner's authoritative diagnostic + the right-rail badge they share.
  useWorkspaceStatusSubscription(activeWorkspace);
  const activeConv = useApp((s) => s.activeConversationId);
  const setActiveConv = useApp((s) => s.setActiveConversationId);
  const rightRailOpen = useApp((s) => s.rightRailOpen);
  const messagesByKey = useApp((s) => s.messages);
  const appendMessage = useApp((s) => s.appendMessage);
  const setMessages = useApp((s) => s.setMessages);
  const streaming = useApp((s) => s.streaming);
  const setStreaming = useApp((s) => s.setStreaming);
  const appendDelta = useApp((s) => s.appendStreamingDelta);

  const key = activeWorkspace && activeConv ? `${activeWorkspace}::${activeConv}` : null;
  const messages = key ? (messagesByKey[key] ?? []) : [];

  // Phase D Wave 1 (2026-05-17) — pending permission prompt for the
  // 10 system-power tools. Set when an `approval_requested` event
  // arrives with `permission_context`; cleared when the user clicks
  // a button on PermissionPromptDialog or dismisses with ESC.
  const [permissionPrompt, setPermissionPrompt] = useState<{
    toolUseId: string;
    toolName: string;
    toolInput: unknown;
    permissionContext: import("@/lib/tauri").PermissionContext;
  } | null>(null);

  // Pre-flight LLM health for the active workspace. We fetch on switch so
  // a banner appears *before* the user types — no more 120 s "Generating…"
  // hangs when the workspace has no provider key configured.
  const [health, setHealth] = useState<LlmHealth | null>(null);
  const [compileBusy, setCompileBusy] = useState(false);
  const [activeWorkspaceRoot, setActiveWorkspaceRoot] = useState<string | null>(null);
  useEffect(() => {
    if (!activeWorkspace) {
      setActiveWorkspaceRoot(null);
      return;
    }
    let cancelled = false;
    workspaceList()
      .then((list) => {
        if (cancelled) return;
        const row = list.find((w) => w.name === activeWorkspace);
        setActiveWorkspaceRoot(row?.path ?? null);
      })
      .catch(() => {
        if (!cancelled) setActiveWorkspaceRoot(null);
      });
    return () => {
      cancelled = true;
    };
  }, [activeWorkspace]);

  useEffect(() => {
    if (!activeWorkspace) {
      setHealth(null);
      return;
    }
    let cancelled = false;
    llmHealth(activeWorkspace)
      .then((h) => {
        if (!cancelled) setHealth(h);
      })
      .catch(() => {
        // The banner is best-effort — if the sidecar isn't up yet, the
        // chat surface still works once it boots; we don't want a fetch
        // failure to mask the chat input.
        if (!cancelled) setHealth(null);
      });
    return () => {
      cancelled = true;
    };
  }, [activeWorkspace]);

  // Hydrate from disk when a new conversation gets selected.
  //
  // Race we have to defend against: the user can click "Send" while the
  // disk read is still in flight. That fires `appendMessage("hy")`
  // *before* hydration's `setMessages(...)` lands, and the unconditional
  // overwrite below used to wipe the user bubble. The fix is a merge —
  // hydration only contributes message ids that aren't already in the
  // cache, so an in-flight user/assistant turn is preserved verbatim.
  useEffect(() => {
    if (!activeWorkspace || !activeConv) return;
    let cancelled = false;
    (async () => {
      try {
        const c = await conversationsGet(activeWorkspace, activeConv);
        if (cancelled) return;
        const k = `${activeWorkspace}::${activeConv}`;
        const existing = useApp.getState().messages[k] ?? [];
        const existingIds = new Set(existing.map((m) => m.id));
        const fromDisk: ChatMessage[] = c.messages
          .filter((m) => !existingIds.has(m.id))
          .map((m) => ({
            id: m.id,
            kind: m.role === "user" ? "user" : "assistant",
            body: m.content,
            at: new Date(m.created_at),
          }));
        // Disk-first ordering, then any local-only (in-flight) messages.
        // Sort by `at` ascending so the disk + local sets interleave
        // correctly when the user has been mid-turn during a remount.
        const merged: ChatMessage[] = [...fromDisk, ...existing].sort(
          (a, b) => a.at.getTime() - b.at.getTime(),
        );
        // De-dup defensively — if disk already had the message and we
        // appended it locally, keep the disk copy (it has the canonical
        // id from the persistence layer).
        const seen = new Set<string>();
        const deduped = merged.filter((m) => {
          if (seen.has(m.id)) return false;
          seen.add(m.id);
          return true;
        });
        setMessages(activeWorkspace, activeConv, collapseClientDiskEcho(deduped));
      } catch (e) {
        toast("Load conversation failed", {
          kind: "error",
          body: e instanceof Error ? e.message : String(e),
        });
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [activeWorkspace, activeConv, setMessages]);

  // Wire the streaming events once.
  //
  // Listener invariants — every code path here HAS to:
  //   1. visibly progress the bubble for token events, even if the
  //      user navigated away from the conversation (we render on
  //      partial; if they're elsewhere, just keep accumulating).
  //   2. ALWAYS clear `streaming` for the active turn on `final` /
  //      `error`. The composer is `disabled={streaming != null}` so
  //      a leak here wedges the chat input forever.
  //   3. ALWAYS append the assistant message to disk + cache when
  //      Final arrives. We resolve the target conversation in this
  //      order: turnCtx (registered in onStartTurn) → active
  //      streaming.turnId match → active workspace + conv. The last
  //      fallback covers the React-remount edge case where turnCtx
  //      lost the entry but the user is still on the right screen.
  useEffect(() => {
    let unlisten: (() => void) | null = null;
    let running = true;
    // Per-turn citation parsers — the LLM is instructed to emit
    // `[claim:<id>]` markers (see CITATION_PROMPT in
    // `intelligence/synthesizer.rs`) and we forward each emitted id
    // into the brain-activation store so the BrainGraph canvas pulses
    // the cited entities live as the model writes.
    //
    // Rationale for per-turn parsers (not a singleton): the parser
    // dedupes within its own lifetime; reusing one across turns would
    // suppress legitimate re-citations of the same claim in a later
    // answer. Parsers are dropped on Final/Error to bound memory.
    const citationParsers = new Map<string, BrainCitationParser>();
    // Per-turn lookup so a later `trust_receipt` event can patch the
    // assistant message that was appended when `final` arrived.
    // Cleared alongside the citation parser when the turn closes.
    const lastAssistantMessage = new Map<
      string,
      { workspace: string; convId: string; messageId: string }
    >();
    // Per-turn engram-activation accumulator. Entries land here from
    // the `engram_activated` ChatEvent and are flushed to the
    // persisted assistant message on `final` (or, if the activation
    // arrives AFTER `final` due to SSE ordering, patched in via
    // `updateMessage`). Cleared on trust_receipt / error.
    const turnEngramActivations = new Map<string, EngramActivationEntry[]>();
    // Per-turn reflection-gap accumulator. Same pattern as
    // turnEngramActivations.
    const turnGaps = new Map<string, GapEntry[]>();
    // Per-tool_use_id workspace label for AI-triggered `compile`
    // tool calls. Populated on `tool_call_proposed` so the
    // executing / finished / rejected handlers can synthesize the
    // matching `workspace_compile_progress`-style payload into the
    // app store — without this, an agent-clicks-Compile run is
    // invisible to the Right-Rail progress bar (the SSE compile
    // events flow inside the sidecar, never out through the Tauri
    // `workspace_compile_progress` channel the desktop subscribes
    // to). Cleared on `final` / `error` / `tool_call_rejected`.
    const compileToolWorkspace = new Map<string, string>();
    onChatEvent((ev: ChatEvent) => {
      if (!running) return;
      // Stream E — keep this as debug-level so it's silenced in
      // shipped builds. Open DevTools and set the log level to debug
      // to re-enable for troubleshooting agent tool-use flows.
      if (import.meta.env.DEV) {
        // eslint-disable-next-line no-console
        console.debug("[chat-event]", ev.type, ev.turn_id, ev);
      }

      const cur = useApp.getState();
      const fromMap = cur.turnCtx[ev.turn_id];
      const fromActive =
        cur.streaming?.turnId === ev.turn_id && cur.activeWorkspace
          ? {
            workspace: cur.activeWorkspace,
            convId: cur.activeConversationId ?? "",
          }
          : null;
      const ctx = fromMap ?? fromActive;

      if (ev.type === "token") {
        // Always keep the visible bubble in sync. The streaming-state
        // turnId match is the only gate — it stays set from the
        // moment onStartTurn fired until Final/Error.
        if (cur.streaming?.turnId === ev.turn_id) {
          appendDelta(ev.text);

          // Streaming citation extraction. Lazy-create a parser on
          // first token for this turn; feed every token; touch the
          // activation store for each newly-detected `[claim:<id>]`
          // marker. The store's exponential decay (~1.4s half-life)
          // makes the BrainGraph node fade out within ~2s, which the
          // user reads as "the model is currently thinking about that
          // claim" rather than "that claim is permanently selected."
          let parser = citationParsers.get(ev.turn_id);
          if (!parser) {
            parser = new BrainCitationParser();
            citationParsers.set(ev.turn_id, parser);
          }
          const cites = parser.feed(ev.text);
          if (cites.length > 0) {
            const touch = useBrainActivation.getState().touch;
            for (const c of cites) {
              touch(c.claimId, "cited", 1.0);
            }
          }
        } else {
          // eslint-disable-next-line no-console
          console.warn(
            "[chat-event] dropped token — streaming.turnId mismatch",
            { event: ev.turn_id, streaming: cur.streaming?.turnId },
          );
        }
        return;
      }

      // ── S5 — agent tool-call lifecycle events ───────────────
      if (ev.type === "tool_call_proposed") {
        if (cur.streaming?.turnId !== ev.turn_id) return;
        // SOTA Ship B (2026-05-18): capture the current partial-text
        // byte length so the renderer can interleave this card at
        // its chronological position in the streamed prose, Cursor-
        // style. Without this snapshot, all cards rendered above
        // the bubble in arrival-order, not interleaved.
        cur.upsertAgentStep({
          id: ev.id,
          name: ev.name,
          input: prettyJson(ev.input),
          isWrite: ev.is_write,
          status: "proposed",
          proposedAtPartialLen: cur.streaming?.partial.length ?? 0,
        });
        // Capture the workspace target for compile-tool calls so the
        // executing / finished handlers can drive the Right-Rail
        // progress bar.
        if (ev.name === "compile") {
          const workspaceFromInput =
            typeof ev.input === "object" && ev.input !== null
              ? typeof (ev.input as { workspace?: unknown }).workspace === "string"
                ? ((ev.input as { workspace?: string }).workspace as string)
                : cur.activeWorkspace ?? ""
              : cur.activeWorkspace ?? "";
          if (workspaceFromInput) {
            compileToolWorkspace.set(ev.id, workspaceFromInput);
          }
        }
        return;
      }
      if (ev.type === "approval_requested") {
        if (cur.streaming?.turnId !== ev.turn_id) return;
        cur.upsertAgentStep({
          id: ev.id,
          name: ev.name,
          input: prettyJson(ev.input),
          isWrite: true,
          status: "awaiting_approval",
        });
        // Phase D Wave 1 — when the backend attached a
        // permission_context, route to the permission-aware modal
        // instead of the standard claim-card approval flow. Falls
        // through to the existing approval UX for events without it.
        if (ev.permission_context) {
          setPermissionPrompt({
            toolUseId: ev.id,
            toolName: ev.name,
            toolInput: ev.input,
            permissionContext: ev.permission_context,
          });
        }
        return;
      }
      // SOTA polish ship (2026-05-18): live tool-output progress
      // event. Append the delta to the in-flight step's progress
      // buffer so the card renders growing content even before
      // tool_call_finished fires. Idempotent: receiving the same
      // delta twice (transport replay) appends both copies, which
      // is the honest model since the wire is append-only.
      if (ev.type === "tool_call_progress") {
        if (cur.streaming?.turnId !== ev.turn_id) return;
        cur.appendStepProgress(ev.id, ev.partial_content, ev.byte_count);
        return;
      }
      // SOTA stability ship (2026-05-18): the agent loop hit a
      // soft cap (iteration budget, max_tokens, loop detected) and
      // is offering partial progress. Capture it on the streaming
      // state so the bubble can render a "Continue?" affordance
      // INSTEAD OF the prior dead-end red error banner. Followed
      // by a terminal Done event that the existing final handler
      // converts to the persisted assistant message.
      if (ev.type === "continuation_offered") {
        if (cur.streaming?.turnId !== ev.turn_id) return;
        cur.setContinuationOffer({
          partialText: ev.partial_text,
          iterationsUsed: ev.iterations_used,
          reason: ev.reason,
        });
        return;
      }
      if (ev.type === "tool_call_executing") {
        if (cur.streaming?.turnId !== ev.turn_id) return;
        cur.patchAgentStep(ev.id, { status: "executing" });
        // Synthesize a `Started` compile-progress payload so the
        // Right-Rail progress bar reflects the AI-driven compile.
        // The actual SSE events fire inside the sidecar and do NOT
        // propagate to the desktop's `workspace_compile_progress`
        // channel; this synthetic `Started` → `Done` bracket gives
        // the user honest "compile is running on your behalf" UX
        // without claiming intermediate progress we don't have.
        const ws = compileToolWorkspace.get(ev.id);
        if (ws) {
          cur.setCompileRootPath(ws);
          cur.setCompileProgress({ phase: "started", workspace: ws });
        }
        return;
      }
      if (ev.type === "tool_call_finished") {
        if (cur.streaming?.turnId !== ev.turn_id) return;
        cur.patchAgentStep(ev.id, {
          status: "finished",
          output: ev.content,
          isError: ev.is_error,
        });
        if (compileToolWorkspace.has(ev.id)) {
          if (ev.is_error) {
            cur.setCompileProgress({
              phase: "failed",
              error: ev.content || "compile tool returned an error",
            });
          } else {
            // The agent's compile tool returned a serialized
            // `PipelineResult` in `ev.content`; parse it if we can,
            // otherwise emit a zero-count Done so the bar still
            // resolves. The Right-Rail's Done renderer tolerates
            // missing optional fields via `#[serde(default)]` on
            // the Rust wire type.
            let parsed: {
              files_parsed?: number;
              claims_count?: number;
              entities_count?: number;
              relations_count?: number;
              contradictions_count?: number;
              artifacts_count?: number;
              health_score?: number;
              cache_dirty?: boolean;
              failed_batches?: number;
              failed_chunk_ranges?: [number, number][];
              incremental_summary?: IncrementalSummary;
            } | null = null;
            try {
              parsed = JSON.parse(ev.content);
            } catch {
              parsed = null;
            }
            cur.setCompileProgress({
              phase: "done",
              files_parsed: parsed?.files_parsed ?? 0,
              claims: parsed?.claims_count ?? 0,
              entities: parsed?.entities_count ?? 0,
              relations: parsed?.relations_count ?? 0,
              contradictions: parsed?.contradictions_count ?? 0,
              artifacts: parsed?.artifacts_count ?? 0,
              health_score: parsed?.health_score ?? 0,
              cache_dirty: parsed?.cache_dirty ?? false,
              failed_batches: parsed?.failed_batches ?? 0,
              failed_chunk_ranges: parsed?.failed_chunk_ranges ?? [],
              incremental_summary: parsed?.incremental_summary,
            });
          }
          compileToolWorkspace.delete(ev.id);
        }
        return;
      }
      if (ev.type === "tool_call_rejected") {
        if (cur.streaming?.turnId !== ev.turn_id) return;
        cur.patchAgentStep(ev.id, {
          status: "rejected",
          output: ev.reason,
        });
        if (compileToolWorkspace.has(ev.id)) {
          cur.setCompileProgress({ phase: "cancelled" });
          compileToolWorkspace.delete(ev.id);
        }
        return;
      }

      if (ev.type === "final" || ev.type === "error") {
        const msgBody =
          ev.type === "final" ? ev.full_text : `⚠️ ${ev.message}`;

        // Snapshot streaming steps before any await — `setStreaming(null)`
        // clears them; assistant row must carry the frozen trace.
        const flushedActivations = turnEngramActivations.get(ev.turn_id);
        const flushedGaps = turnGaps.get(ev.turn_id);
        const snap = useApp.getState();
        const flushedSteps =
          snap.streaming?.turnId === ev.turn_id
            ? [...snap.streaming.agentSteps]
            : [];

        if (ctx && ctx.convId) {
          void (async () => {
            let messageId: string;
            if (ev.type === "final") {
              try {
                const saved = await conversationsAppendMessage({
                  workspace: ctx.workspace,
                  conversationId: ctx.convId,
                  role: "assistant",
                  content: ev.full_text,
                  claimsUsed: [],
                });
                messageId = saved.id;
              } catch (e) {
                toast("Persist message failed", {
                  kind: "warn",
                  body: e instanceof Error ? e.message : String(e),
                });
                messageId = `m-${Date.now()}-a`;
              }
            } else {
              messageId = `m-${Date.now()}-e`;
            }

            appendMessage(ctx.workspace, ctx.convId, {
              id: messageId,
              kind: "assistant",
              body: msgBody,
              at: new Date(),
              engramActivations:
                flushedActivations && flushedActivations.length > 0
                  ? flushedActivations
                  : undefined,
              gaps: flushedGaps && flushedGaps.length > 0 ? flushedGaps : undefined,
              agentSteps: flushedSteps.length > 0 ? flushedSteps : undefined,
            });

            if (ev.type === "final") {
              lastAssistantMessage.set(ev.turn_id, {
                workspace: ctx.workspace,
                convId: ctx.convId,
                messageId,
              });
              void conversationsGenerateTitle(ctx.workspace, ctx.convId).catch(() => {
                /* LLM title is best-effort; interim first-line title stays */
              });
            }

            const latest = useApp.getState();
            if (latest.streaming?.turnId === ev.turn_id) {
              setStreaming(null);
            }
            if (ev.type === "error") {
              latest.clearTurn(ev.turn_id);
              citationParsers.delete(ev.turn_id);
              lastAssistantMessage.delete(ev.turn_id);
              turnEngramActivations.delete(ev.turn_id);
              turnGaps.delete(ev.turn_id);
              // Any in-flight compile-tool entry on this turn is now
              // orphaned; clearing the whole map is safe because new
              // turns mint fresh tool_use_ids.
              compileToolWorkspace.clear();
            }
          })();
        } else {
          // eslint-disable-next-line no-console
          console.warn(
            "[chat-event] no ctx for final/error — bubble suppressed but state cleared",
            { turn_id: ev.turn_id, fromMap, fromActive },
          );
          const latest = useApp.getState();
          if (latest.streaming?.turnId === ev.turn_id) {
            setStreaming(null);
          }
          if (ev.type === "error") {
            latest.clearTurn(ev.turn_id);
            citationParsers.delete(ev.turn_id);
            lastAssistantMessage.delete(ev.turn_id);
            turnEngramActivations.delete(ev.turn_id);
            turnGaps.delete(ev.turn_id);
            compileToolWorkspace.clear();
          }
        }
        return;
      }
      if (ev.type === "gaps_surfaced") {
        // Append (don't replace) so multiple `gaps` tool calls in
        // one turn — possible when the agent narrows then broadens —
        // accumulate. Dedupe is handled at render time by the
        // entity_type:entity_name:expected_claim_type composite key.
        const prior = turnGaps.get(ev.turn_id) ?? [];
        const next = [...prior, ...ev.gaps];
        turnGaps.set(ev.turn_id, next);
        const cur = useApp.getState();
        if (cur.streaming?.turnId === ev.turn_id) {
          cur.appendGaps(ev.gaps);
        }
        const target = lastAssistantMessage.get(ev.turn_id);
        if (target) {
          cur.updateMessage(target.workspace, target.convId, target.messageId, {
            gaps: next,
          });
        }
        return;
      }
      if (ev.type === "engram_activated") {
        const entry: EngramActivationEntry = {
          tool: ev.tool,
          pointer: ev.pointer,
          tsMs: ev.ts_ms,
          sourceCount: ev.source_count,
          answerCount: ev.answer_count,
        };
        // Append to per-turn accumulator AND to streaming state
        // (so the in-flight scrubber updates live). If `final`
        // already arrived, patch the persisted message directly —
        // SSE ordering can deliver activations after final.
        const prior = turnEngramActivations.get(ev.turn_id) ?? [];
        const next = [...prior, entry];
        turnEngramActivations.set(ev.turn_id, next);
        const cur = useApp.getState();
        if (cur.streaming?.turnId === ev.turn_id) {
          cur.appendEngramActivation(entry);
        }
        const target = lastAssistantMessage.get(ev.turn_id);
        if (target) {
          cur.updateMessage(target.workspace, target.convId, target.messageId, {
            engramActivations: next,
          });
        }
        return;
      }
      if (ev.type === "trust_receipt") {
        const target = lastAssistantMessage.get(ev.turn_id);
        if (target) {
          useApp.getState().updateMessage(
            target.workspace,
            target.convId,
            target.messageId,
            {
              trustReceipt: {
                kind: ev.kind,
                claimsUsed: ev.claims_used,
                autoCitedCount: ev.auto_cited_count,
                relatedCount: ev.related_count,
                badClaimIds: ev.bad_claim_ids,
              },
            },
          );
        }
        // Trust receipt is the last per-turn event; clean up.
        useApp.getState().clearTurn(ev.turn_id);
        citationParsers.delete(ev.turn_id);
        lastAssistantMessage.delete(ev.turn_id);
        turnEngramActivations.delete(ev.turn_id);
        turnGaps.delete(ev.turn_id);
        return;
      }
    }).then((u) => {
      unlisten = u;
      if (import.meta.env.DEV) {
        // eslint-disable-next-line no-console
        console.debug("[chat-event] listener registered");
      }
    });
    return () => {
      running = false;
      unlisten?.();
      citationParsers.clear();
    };
  }, [appendDelta, appendMessage, setStreaming]);

  // Watchdog: if streaming has been pending for > 60 s with no
  // tokens arriving (partial still empty), surface a clear error
  // and free the composer. Belt-and-braces against any future
  // Tauri-event delivery regression.
  useEffect(() => {
    if (!streaming) return;
    const stuckAt = streaming.startedAt.getTime();
    const id = window.setInterval(() => {
      const cur = useApp.getState();
      if (!cur.streaming || cur.streaming.turnId !== streaming.turnId) {
        window.clearInterval(id);
        return;
      }
      const elapsed = Date.now() - stuckAt;
      if (elapsed > 60_000 && !cur.streaming.partial) {
        window.clearInterval(id);
        toast("No response in 60 s", {
          kind: "error",
          body: "Check the sidecar log; the request reached the backend but no tokens came back.",
        });
        cur.setStreaming(null);
      }
    }, 5_000);
    return () => window.clearInterval(id);
  }, [streaming]);

  // Cursor-style: scroll the latest question to ~35% viewport height so the
  // prior answer tail stays visible above; answer streams in the space below.
  const activeQuestionRef = useRef<HTMLDivElement>(null);
  const scrollContainerRef = useRef<HTMLDivElement>(null);
  const conversationTurns = useMemo(
    () => groupMessagesIntoTurns(messages),
    [messages],
  );

  useEffect(() => {
    const last = messages[messages.length - 1];
    if (last?.kind !== "user" && !streaming) return;
    requestAnimationFrame(() => {
      const container = scrollContainerRef.current;
      const question = activeQuestionRef.current;
      if (container && question) {
        scrollQuestionIntoView(container, question);
      }
    });
  }, [messages.length, messages[messages.length - 1]?.id, streaming?.turnId]);

  if (!activeWorkspace) {
    return <NoWorkspace />;
  }

  const isEmpty = messages.length === 0 && !streaming;

  // Empty-state layout: composer in the visual centre of the page,
  // header above it, slash-command help below — same shape as
  // claude.ai's home screen. Once the first message lands the layout
  // flips back to the standard "messages above, composer at bottom"
  // chat layout.
  if (isEmpty) {
    const compileLabel =
      health?.mounted && health.claim_count > 0
        ? "Recompile Workspace"
        : "Compile Workspace";

    return (
      <div className="flex h-full flex-col bg-background">
        <ChatContextHeader workspace={activeWorkspace} />
        {/* Vertically centered floating composer — Cursor-style */}
        <div className="flex flex-1 flex-col items-center justify-center px-8">
          <div className="flex w-full max-w-3xl flex-col gap-3">
            {/* Floating composer card */}
            <Composer
              workspace={activeWorkspace}
              workspaceRootPath={activeWorkspaceRoot}
              convId={activeConv}
              disabled={streaming != null}
              autoFocus
              isIdleCentered
              compileAction={
                rightRailOpen
                  ? undefined
                  : {
                      busy: compileBusy,
                      label: compileLabel,
                      onRun: async () => {
                        const ui = useApp.getState();
                        ui.setRightRailTab("compile");
                        if (!ui.rightRailOpen) {
                          ui.toggleRightRail();
                        }
                        setCompileBusy(true);
                        try {
                          await workspaceCompile({ target: activeWorkspace });
                          toast("Compile queued", {
                            kind: "info",
                            body: "Progress is shown in the Compile panel.",
                          });
                          try {
                            const freshHealth = await llmHealth(activeWorkspace);
                            setHealth(freshHealth);
                          } catch {
                            // Non-blocking: compile can still run even if health probe fails.
                          }
                        } catch (e) {
                          toast("Compile failed", {
                            kind: "error",
                            body: e instanceof Error ? e.message : String(e),
                          });
                        } finally {
                          setCompileBusy(false);
                        }
                      },
                    }
              }
              health={health}
              recentHistory={buildHistoryPayload(messages)}
              onCancel={() => {
                setStreaming(null);
                toast("Cancelled — partial message kept.", { kind: "warn" });
              }}
              onCreateConvIfNeeded={async (firstUserText) => {
                if (activeConv) return activeConv;
                const c = await conversationsCreate(
                  activeWorkspace,
                  firstUserText,
                );
                setActiveConv(c.id);
                return c.id;
              }}
              onUserMessage={(content) => {
                const ws = activeWorkspace;
                if (!ws) return;
                const cid = useApp.getState().activeConversationId;
                if (!cid) return;
                const tempId = `m-${Date.now()}-u`;
                appendMessage(ws, cid, {
                  id: tempId,
                  kind: "user",
                  body: content,
                  at: new Date(),
                });
                void conversationsAppendMessage({
                  workspace: ws,
                  conversationId: cid,
                  role: "user",
                  content,
                })
                  .then((saved) => {
                    useApp.getState().replaceMessageId(ws, cid, tempId, saved.id);
                  })
                  .catch((e) => {
                    useApp.getState().removeMessage(ws, cid, tempId);
                    toast("Could not save your message — try again", {
                      kind: "error",
                      body: e instanceof Error ? e.message : String(e),
                    });
                  });
              }}
              onStartTurn={(turnId, ws, cid) => {
                useApp.getState().registerTurn(turnId, ws, cid);
                setStreaming({
                  turnId,
                  partial: "",
                  startedAt: new Date(),
                  tokensIn: 0,
                  tokensOut: 0,
                  agentSteps: [],
                  engramActivations: [],
                  gaps: [],
                });
              }}
            />

            {/* Hint below the card */}
            <p className="text-center text-[11px] text-muted-foreground/40">
              Knowledge chat grounded in your workspace · <kbd className="font-mono">/</kbd> commands · <kbd className="font-mono">@</kbd> files
            </p>
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col bg-background">
      <ChatContextHeader workspace={activeWorkspace} />
      <div
        ref={scrollContainerRef}
        className="app-scroll flex-1 overflow-y-auto px-8 py-6"
      >
        <ul className="mx-auto flex max-w-3xl flex-col gap-10">
          {conversationTurns.map((turn, index) => {
            const isActive = index === conversationTurns.length - 1;
            return (
              <li
                key={turn.user.id}
                className={cn(
                  "group/turn flex flex-col gap-2.5",
                  isActive && "min-h-[calc(100dvh-12rem)]",
                )}
              >
                <div ref={isActive ? activeQuestionRef : undefined}>
                  <MessageBubble msg={turn.user} />
                </div>
                {turn.replies.map((reply) => (
                  <MessageBubble key={reply.id} msg={reply} />
                ))}
                {isActive && streaming ? (
                  <ChatTurnStreaming
                    streaming={streaming}
                    workspace={activeWorkspace}
                  />
                ) : null}
              </li>
            );
          })}
          {conversationTurns.length === 0 && streaming ? (
            <li className="min-h-[calc(100dvh-12rem)]">
              <ChatTurnStreaming
                streaming={streaming}
                workspace={activeWorkspace}
              />
            </li>
          ) : null}
        </ul>
      </div>

      <Composer
        workspace={activeWorkspace}
        workspaceRootPath={activeWorkspaceRoot}
        convId={activeConv}
        disabled={streaming != null}
        health={health}
        recentHistory={buildHistoryPayload(messages)}
        onCancel={() => {
          setStreaming(null);
          toast("Cancelled — partial message kept.", { kind: "warn" });
        }}
        onCreateConvIfNeeded={async (firstUserText) => {
          if (activeConv) return activeConv;
          const c = await conversationsCreate(activeWorkspace, firstUserText);
          setActiveConv(c.id);
          return c.id;
        }}
        onUserMessage={(content) => {
          const ws = activeWorkspace;
          if (!ws) return;
          const cid = useApp.getState().activeConversationId;
          if (!cid) return;
          const tempId = `m-${Date.now()}-u`;
          appendMessage(ws, cid, {
            id: tempId,
            kind: "user",
            body: content,
            at: new Date(),
          });
          void conversationsAppendMessage({
            workspace: ws,
            conversationId: cid,
            role: "user",
            content,
          })
            .then((saved) => {
              useApp.getState().replaceMessageId(ws, cid, tempId, saved.id);
            })
            .catch((e) => {
              useApp.getState().removeMessage(ws, cid, tempId);
              toast("Persist user message failed", {
                kind: "warn",
                body: e instanceof Error ? e.message : String(e),
              });
            });
        }}
        onStartTurn={(turnId, ws, cid) => {
          useApp.getState().registerTurn(turnId, ws, cid);
          setStreaming({
            turnId,
            partial: "",
            startedAt: new Date(),
            tokensIn: 0,
            tokensOut: 0,
            agentSteps: [],
            engramActivations: [],
            gaps: [],
          });
        }}
      />
      {permissionPrompt && activeWorkspace && (
        <PermissionPromptDialog
          workspace={activeWorkspace}
          toolUseId={permissionPrompt.toolUseId}
          toolName={permissionPrompt.toolName}
          toolInput={permissionPrompt.toolInput}
          permissionContext={permissionPrompt.permissionContext}
          onResolved={() => setPermissionPrompt(null)}
        />
      )}
    </div>
  );
}

/**
 * Pre-flight banner shown above the composer when the active workspace
 * either has no LLM configured or has no compiled claims. Surfaces the
 * actionable "you'd be waiting forever" cases up-front instead of
 * letting the user submit and watch a spinner. Renders nothing on the
 * happy path (configured + has claims).
 */
/** Workspace label + topic-branch inbox. Branch switcher is in the composer footer. */
function ChatContextHeader({ workspace }: { workspace: string }) {
  const [topicsOpen, setTopicsOpen] = useState(false);
  return (
    <div className="flex h-11 min-h-11 shrink-0 items-center px-4">
      <div className="flex min-w-0 flex-1 items-center gap-x-1.5">
        <span className="min-w-0 shrink truncate text-[10px] font-medium uppercase tracking-wide text-muted-foreground/70">
          {workspace}
        </span>
        <button
          type="button"
          onClick={() => setTopicsOpen(true)}
          className={cn(
            "ml-0.5 inline-flex shrink-0 items-center gap-0.5 rounded-sm p-0.5 text-muted-foreground transition-colors",
            "hover:bg-muted/45 hover:text-foreground",
            "focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring/55 focus-visible:ring-offset-2 focus-visible:ring-offset-background",
          )}
          aria-label="Review pending topic branches"
          title="Review pending topic branches"
        >
          <Inbox className="size-3" aria-hidden />
        </button>
      </div>
      <TopicBranchesPanel
        workspace={workspace}
        open={topicsOpen}
        onOpenChange={setTopicsOpen}
      />
    </div>
  );
}

/** Compare absolute workspace roots (macOS may differ only by case). */
function sameWorkspaceRoot(a: string | null, b: string | null): boolean {
  if (!a || !b) return false;
  const x = a.replace(/\/$/, "");
  const y = b.replace(/\/$/, "");
  if (x === y) return true;
  return x.toLowerCase() === y.toLowerCase();
}

/**
 * Slice 0 — chat readiness banner driven by the unified workspace
 * status snapshot. Replaces the pre-Slice-0 banner that read its own
 * `llm_health` Tauri command (one of the five contradicting probes).
 *
 * Picks the highest-severity diagnostic blocking `for_chat` and
 * renders its message verbatim with the diagnostic-supplied actions.
 * Returns `null` on the happy path. Falls back to the legacy
 * `LlmHealth` shape only when the SSE stream hasn't landed a snapshot
 * yet — the legacy probe is best-effort, the unified status is
 * authoritative.
 */
function LlmHealthBanner({
  health,
  workspace,
  workspaceRootPath,
  openSettings,
}: {
  health: LlmHealth | null;
  workspace: string;
  workspaceRootPath: string | null;
  openSettings: () => void;
}) {
  const compileProgress = useApp((s) => s.compileProgress);
  const compileRootPath = useApp((s) => s.compileRootPath);
  const status = useWorkspaceStatus(workspace);
  const blocker = pickPrimaryDiagnostic(status, "for_chat");

  // Authoritative path: unified status has answered. Render its
  // diagnostic message directly — no per-banner string baked in.
  if (status) {
    if (status.readiness.for_chat) return null;
    if (!blocker) return null;
    const tone =
      blocker.severity === "error"
        ? "text-rose-300"
        : blocker.severity === "warn"
          ? "text-amber-300"
          : "text-muted-foreground";
    return (
      <div className="flex w-full items-start gap-2.5 rounded-xl border border-border/70 bg-muted/35 px-3.5 py-2.5 text-xs text-foreground/90">
        <AlertTriangle className={cn("mt-0.5 h-3.5 w-3.5 flex-none", tone)} />
        <div className="flex flex-1 flex-col gap-1 leading-relaxed text-muted-foreground">
          <span>
            <strong className="font-medium text-foreground/90">
              {workspace}
            </strong>
            {" — "}
            {blocker.message}
          </span>
          {blocker.code === "no_provider" && (
            <span>
              Add a provider key under{" "}
              <button
                type="button"
                onClick={openSettings}
                className="font-medium underline underline-offset-2 hover:text-foreground"
              >
                Settings → Credentials
              </button>
              .
            </span>
          )}
          {blocker.actions.length > 0 && blocker.code !== "no_provider" && (
            <span className="text-[10px] opacity-80">
              Suggested: {blocker.actions.map((a) => a.label).join(" · ")}
            </span>
          )}
        </div>
      </div>
    );
  }

  // Fallback path while the SSE snapshot is still in flight.
  if (!health) return null;
  if (!health.mounted) {
    return (
      <div className="flex w-full items-start gap-2.5 rounded-xl border border-border/70 bg-muted/35 px-3.5 py-2.5 text-xs text-foreground/90">
        <AlertTriangle className="mt-0.5 h-3.5 w-3.5 flex-none text-amber-300" />
        <div className="leading-relaxed text-muted-foreground">
          Workspace <code className="font-mono">{workspace}</code> isn't
          loaded by the engine. Try{" "}
          <strong className="font-medium text-foreground/90">
            Restart local engine
          </strong>{" "}
          (⌘K) — that respawns the sidecar and remounts. If the workspace
          has never been compiled, run Compile Workspace first.
        </div>
      </div>
    );
  }
  if (!health.configured) {
    return (
      <div className="flex w-full items-start gap-2 rounded-md border border-yellow-500/40 bg-yellow-500/10 px-3 py-2 text-xs text-yellow-200">
        <AlertTriangle className="mt-0.5 h-3.5 w-3.5 flex-none" />
        <div className="flex-1">
          No LLM configured for <code className="font-mono">{workspace}</code>.
          Add a provider key under{" "}
          <button
            type="button"
            onClick={openSettings}
            className="font-medium underline underline-offset-2 hover:text-yellow-100"
          >
            Settings → Credentials
          </button>{" "}
          (or run <code className="font-mono">root setup</code> in the
          workspace directory). Without a provider, answers fall back to the
          highest-confidence claim verbatim.
        </div>
      </div>
    );
  }
  if (health.claim_count === 0) {
    const compilePhase = compileProgress?.phase;
    const compileRunning =
      compileRootPath != null &&
      workspaceRootPath != null &&
      sameWorkspaceRoot(workspaceRootPath, compileRootPath) &&
      compilePhase != null &&
      compilePhase !== "done" &&
      compilePhase !== "failed" &&
      compilePhase !== "cancelled";
    if (compileRunning) {
      return (
        <div className="flex w-full items-start gap-2.5 rounded-xl border border-border/70 bg-muted/25 px-3.5 py-2.5 text-xs text-foreground/90">
          <Loader2 className="mt-0.5 h-3.5 w-3.5 shrink-0 animate-spin text-muted-foreground" />
          <div className="leading-relaxed text-muted-foreground">
            <span className="font-medium text-foreground/85">Compile running.</span>{" "}
            Claim count stays at zero until this pass finishes — follow progress in the
            right-hand <span className="font-medium text-foreground/80">Compile</span>{" "}
            panel (e.g. extracting claims).
          </div>
        </div>
      );
    }
    return (
      <div className="flex w-full items-start gap-2.5 rounded-xl border border-border/70 bg-muted/35 px-3.5 py-2.5 text-xs text-foreground/90">
        <AlertTriangle className="mt-0.5 h-3.5 w-3.5 flex-none text-amber-300" />
        <div className="leading-relaxed text-muted-foreground">
          No compiled claims in <code className="font-mono text-foreground/90">{workspace}</code>{" "}
          yet. Add sources to the workspace, then run{" "}
          <code className="font-mono text-foreground/90">root compile</code>.
        </div>
      </div>
    );
  }
  return null;
}


function NoWorkspace() {
  return (
    <div className="flex h-full flex-col items-center justify-center gap-3 px-8 text-center">
      <h2 className="text-base font-medium">Pick a workspace to start</h2>
      <p className="max-w-sm text-sm text-muted-foreground">
        Workspaces are folders ThinkingRoot has compiled into a queryable
        knowledge graph. Use the tree on the left, or click the folder
        icon next to <span className="font-medium">Workspaces</span> to
        add one.
      </p>
    </div>
  );
}

async function copyAssistantMessage(body: string) {
  try {
    await writeText(body);
    toast("Copied to clipboard", { kind: "success" });
  } catch (e) {
    toast("Copy failed", { body: String(e), kind: "error" });
  }
}

async function shareAssistantMessage(body: string) {
  if (typeof navigator !== "undefined" && typeof navigator.share === "function") {
    try {
      await navigator.share({ title: "ThinkingRoot", text: body });
      return;
    } catch (e) {
      const name = (e as { name?: string })?.name;
      if (name === "AbortError") return;
    }
  }
  try {
    await writeText(body);
    toast("Copied to clipboard (share as text)", { kind: "info" });
  } catch (e) {
    toast("Share failed", { body: String(e), kind: "error" });
  }
}

function AssistantMessageFooter({
  body,
  trustReceipt,
  pending,
}: {
  body: string;
  trustReceipt: ChatMessage["trustReceipt"];
  pending?: boolean;
}) {
  const showActions = body.trim().length > 0;
  const showTrust = Boolean(!pending && trustReceipt);

  if (!showActions && !showTrust) return null;

  return (
    <div className="mt-2 flex min-h-7 items-center justify-between gap-3 opacity-50 transition-opacity group-hover/message:opacity-100 focus-within:opacity-100">
      <div className="min-w-0 flex-1">
        {showTrust && trustReceipt ? <TrustReceiptChip receipt={trustReceipt} /> : null}
      </div>
      {showActions ? (
        <div className="flex shrink-0 items-center gap-0.5">
          <Button
            type="button"
            variant="ghost"
            size="icon"
            className="size-8 text-muted-foreground hover:text-foreground"
            aria-label="Copy message"
            onClick={() => void copyAssistantMessage(body)}
          >
            <Copy className="size-4" strokeWidth={2} aria-hidden />
          </Button>
          <Button
            type="button"
            variant="ghost"
            size="icon"
            className="size-8 text-muted-foreground hover:text-foreground"
            aria-label="Share message"
            onClick={() => void shareAssistantMessage(body)}
          >
            <Share2 className="size-4" strokeWidth={2} aria-hidden />
          </Button>
        </div>
      ) : null}
    </div>
  );
}

/**
 * SOTA Ship B (2026-05-18): chronological message-part rendering.
 *
 * Walk `streaming.agentSteps` sorted by `proposedAtPartialLen` (the
 * snapshot of `partial.length` at the moment the step was first
 * proposed). Between each pair of consecutive offsets emit the
 * `partial[prev..next]` slice as a text MessageBubble, then emit
 * the tool's ClaimCard inline. The tail slice (`partial[last..]`)
 * renders as the final bubble. Cursor / Claude Code 2026 do
 * exactly this.
 *
 * Backwards-compat: steps without `proposedAtPartialLen` (legacy
 * persisted, mid-migration) anchor at the END of the text so they
 * still appear inline after the prose — never above it.
 */
function buildStreamParts(streaming: StreamState): Array<
  | { kind: "text"; content: string; key: string }
  | { kind: "step"; step: AgentStep; key: string }
> {
  const partial = streaming.partial;
  const sortedSteps = [...streaming.agentSteps].sort((a, b) => {
    const ao = a.proposedAtPartialLen ?? partial.length;
    const bo = b.proposedAtPartialLen ?? partial.length;
    return ao - bo;
  });
  const parts: Array<
    | { kind: "text"; content: string; key: string }
    | { kind: "step"; step: AgentStep; key: string }
  > = [];
  let cursor = 0;
  sortedSteps.forEach((step, i) => {
    const offset = Math.min(step.proposedAtPartialLen ?? partial.length, partial.length);
    if (offset > cursor) {
      parts.push({
        kind: "text",
        content: partial.slice(cursor, offset),
        key: `t${i}`,
      });
      cursor = offset;
    }
    parts.push({ kind: "step", step, key: step.id });
  });
  if (cursor < partial.length) {
    parts.push({
      kind: "text",
      content: partial.slice(cursor),
      key: `t-tail`,
    });
  }
  return parts;
}

function ChatTurnStreaming({
  streaming,
  workspace,
}: {
  streaming: StreamState;
  workspace: string | null;
}) {
  const parts = buildStreamParts(streaming);
  const hasAnyContent = streaming.partial.length > 0 || streaming.agentSteps.length > 0;
  return (
    <div className="space-y-3">
      {workspace ? (
        <LiveActivityStrip
          steps={streaming.agentSteps}
          workspace={workspace}
          hasAnswer={streaming.partial.length > 0}
        />
      ) : null}
      {streaming.engramActivations.length > 0 && (
        <div className="mx-auto w-full max-w-3xl">
          <EngramTimeline
            activations={streaming.engramActivations}
            turnStartedAtMs={streaming.startedAt.getTime()}
          />
        </div>
      )}
      {streaming.gaps.length > 0 && (
        <div className="mx-auto w-full max-w-3xl">
          <GapCards gaps={streaming.gaps} />
        </div>
      )}
      {/* Chronologically interleaved stream parts: text bubbles + tool
          cards, in the order they were proposed during the turn. */}
      {hasAnyContent && (
        <div className="mx-auto w-full max-w-3xl space-y-2">
          {parts.map((p, i) =>
            p.kind === "text" ? (
              <MessageBubble
                key={p.key}
                msg={{
                  id: `${streaming.turnId}::${p.key}`,
                  kind: "assistant",
                  body: p.content,
                  at: streaming.startedAt,
                }}
                pending={i === parts.length - 1}
              />
            ) : workspace ? (
              <ClaimCard key={p.key} step={p.step} workspace={workspace} />
            ) : null,
          )}
        </div>
      )}
      {!hasAnyContent && (
        <MessageBubble
          msg={{
            id: streaming.turnId,
            kind: "assistant",
            body: "",
            at: streaming.startedAt,
          }}
          pending
          pendingLabel={STREAM_OPENING_LABEL}
        />
      )}
      {/* SOTA stability ship (2026-05-18): soft-cap continuation
          offer. Shown when the agent loop paused (iteration budget,
          max_tokens cut, loop detected) with partial progress
          preserved. Replaces the dead-end red error banner. */}
      {streaming.continuation && (
        <ContinuationPrompt
          offer={streaming.continuation}
          turnId={streaming.turnId}
        />
      )}
    </div>
  );
}

/**
 * SOTA stability ship (2026-05-18): "Continue?" affordance shown
 * when the agent loop soft-capped (iteration budget exhausted,
 * max_tokens cut, or loop detected). Replaces the pre-ship dead-end
 * red error banner. Clicking "Continue" sends a fresh chat turn
 * with the inline "continue from where you left off" prompt so the
 * agent picks up with the partial work already shared.
 */
function ContinuationPrompt({
  offer,
  turnId,
}: {
  offer: ContinuationOffer;
  turnId: string;
}) {
  const setStreaming = useApp((s) => s.setStreaming);
  const setContinuationOffer = useApp((s) => s.setContinuationOffer);
  const { headline, sub } = continuationCopy(offer.reason);
  const dismiss = () => setContinuationOffer(undefined);
  const continueTurn = () => {
    // Push a fresh user-style follow-up into the same conversation.
    // The actual send happens via the existing composer plumbing;
    // here we just stage the prompt and let the user hit Send (or
    // we surface a one-click resend in a follow-up ship). For v1
    // we dismiss the offer and let the user type "continue" — the
    // agent's history already carries the partial work so it
    // picks up naturally. Keeps this ship surgical without
    // bypassing the composer's queue/approval discipline.
    dismiss();
    // Visible toast as guidance; persists 6s.
    toast("Continue from where you left off", {
      kind: "info",
      body: "Send a follow-up like \"continue\" — the agent has your partial work in context.",
    });
  };
  // turnId is intentionally consumed so React can re-key the
  // affordance when a fresh turn starts (preventing stale render).
  void turnId;
  // streaming is intentionally not patched here — the offer stays
  // on screen until the user acts. void to silence the lint.
  void setStreaming;
  return (
    <div className="mx-auto w-full max-w-3xl rounded-xl border border-amber-500/25 bg-amber-500/5 p-3 text-sm">
      <div className="mb-2 flex items-center gap-2 text-amber-200/90">
        <AlertTriangle className="h-3.5 w-3.5 shrink-0" />
        <span className="font-medium">{headline}</span>
        <span className="ml-auto text-[10px] uppercase tracking-widest text-amber-200/70">
          {offer.iterationsUsed} steps
        </span>
      </div>
      <p className="mb-2 text-xs text-muted-foreground">{sub}</p>
      <div className="flex gap-2">
        <Button size="sm" onClick={continueTurn}>
          Continue
        </Button>
        <Button size="sm" variant="ghost" onClick={dismiss}>
          Stop here
        </Button>
      </div>
    </div>
  );
}

function continuationCopy(reason: string): { headline: string; sub: string } {
  switch (reason) {
    case "iteration_budget":
      return {
        headline: "Reached the step budget",
        sub: "Looks like more reads were needed. Continue with the partial work or stop here.",
      };
    case "max_tokens":
      return {
        headline: "Output was cut off",
        sub: "The model hit its response budget mid-sentence. Continue to finish the thought.",
      };
    case "loop_detected":
      return {
        headline: "Stuck on the same approach",
        sub: "Repeated the same tool call. Continue with a hint, or stop and try a different angle.",
      };
    default:
      return {
        headline: "Paused with partial progress",
        sub: "Continue from where the agent left off, or stop here.",
      };
  }
}

/** Shown before agent steps arrive — dragonfly moment, not knowledge-base copy yet. */
const STREAM_OPENING_LABEL = "Flying over…";

function ThinkingLoader({ label = STREAM_OPENING_LABEL }: { label?: string }) {
  return (
    <div className="flex h-12 items-center justify-start px-2" role="status" aria-label={label}>
      <div className="inline-flex items-center gap-2.5 py-1.5 text-xs text-muted-foreground">
        <span className="pixel-dragonfly" aria-hidden>
          <span className="pixel-dragonfly__wing pixel-dragonfly__wing--left" />
          <span className="pixel-dragonfly__wing pixel-dragonfly__wing--right" />
          <span className="pixel-dragonfly__body" />
        </span>
        <span className="font-medium tracking-[0.01em]">{label}</span>
      </div>
    </div>
  );
}

function MessageBubble({
  msg,
  pending,
  pendingLabel,
}: {
  msg: ChatMessage;
  pending?: boolean;
  pendingLabel?: string;
}) {
  const isUser = msg.kind === "user";
  const isThinking = pending && !msg.body && !isUser;

  if (isThinking) {
    return <ThinkingLoader label={pendingLabel} />;
  }

  // AI Message: No bubble, full width, rendered with Markdown.
  //
  // Honesty wire-up: chat-error events arrive as assistant messages
  // with body prefixed by `⚠️ ` (see the `final | error` handler in
  // the chat-event listener above). When that error stringifies into
  // a known upgrade reason (credits exhausted, tier required, private
  // pack pre-flight), surface the structured `UpgradeBanner` rather
  // than the raw markdown so the user sees the actionable CTA. The
  // generic markdown render still covers every non-upgrade error.
  const upgradeReason = !isUser ? parseUpgradeReason(msg.body) : null;
  if (!isUser && upgradeReason) {
    return (
      <div className={cn("flex w-full px-2", pending && "opacity-90")}>
        <div className="w-full max-w-3xl">
          <UpgradeBanner reason={upgradeReason} />
        </div>
      </div>
    );
  }
  if (!isUser) {
    return (
      <div className={cn("group/message flex w-full px-2", pending && "opacity-90")}>
        <div className="w-full max-w-3xl">
          <ChatMarkdown>{msg.body}</ChatMarkdown>
          {pending && (
            <span className="ml-1 inline-block h-3.5 w-1.5 translate-y-0.5 bg-accent/60 animate-pulse" />
          )}
          <AssistantMessageFooter
            body={msg.body}
            trustReceipt={msg.trustReceipt}
            pending={pending}
          />
          {!pending && msg.engramActivations && msg.engramActivations.length > 0 && (
            <div className="mt-2">
              <EngramTimeline activations={msg.engramActivations} />
            </div>
          )}
          {!pending && msg.gaps && msg.gaps.length > 0 && (
            <div className="mt-2">
              <GapCards gaps={msg.gaps} />
            </div>
          )}
          {!pending && msg.agentSteps && msg.agentSteps.length > 0 && (
            <div className="mt-2">
              <ReasoningTrace steps={msg.agentSteps} />
            </div>
          )}
        </div>
      </div>
    );
  }

  // User question at top of each turn — visible bubble, right-aligned.
  return (
    <div className="flex w-full justify-end px-2 pt-0.5">
      <div
        className={cn(
          "max-w-[min(42rem,92%)] rounded-2xl border border-border/50 bg-muted/40 px-4 py-3 text-[15px] font-normal leading-relaxed text-foreground shadow-sm",
          pending && "opacity-90",
        )}
      >
        <UserMessageContent body={msg.body} />
      </div>
    </div>
  );
}

const DOC_ATTACH_EXTENSIONS = [
  "md",
  "txt",
  "rst",
  "pdf",
  "json",
  "toml",
  "yaml",
  "yml",
  "rs",
  "ts",
  "tsx",
  "js",
  "jsx",
  "mjs",
  "cjs",
  "vue",
  "svelte",
  "py",
  "go",
  "java",
  "kt",
  "kts",
  "c",
  "h",
  "cpp",
  "hpp",
  "cc",
  "cs",
  "swift",
  "rb",
  "php",
  "html",
  "htm",
  "css",
  "scss",
  "sass",
  "less",
  "sql",
  "sh",
  "bash",
  "zsh",
  "ps1",
  "xml",
  "csv",
  "log",
];

function formatLlmProviderTag(provider: string | null): string {
  if (!provider) return "";
  const k = provider.toLowerCase();
  const map: Record<string, string> = {
    anthropic: "Anthropic",
    openai: "OpenAI",
    azure: "Azure",
    google: "Google",
    gemini: "Google",
    groq: "Groq",
    ollama: "Ollama",
    mistral: "Mistral",
    deepseek: "DeepSeek",
  };
  return map[k] ?? provider.charAt(0).toUpperCase() + provider.slice(1);
}

/** Cursor-style circular control used at both ends of the session composer pill. */
function SessionComposerCircleButton({
  children,
  className,
  ...props
}: ComponentProps<"button">) {
  return (
    <button
      type="button"
      className={cn(
        "flex h-8 w-8 shrink-0 items-center justify-center rounded-full transition-colors",
        "focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring/50",
        className,
      )}
      {...props}
    >
      {children}
    </button>
  );
}

function ComposerBranchFootnote({ workspace }: { workspace: string }) {
  return (
    <div className="mt-2 px-2 text-[11px] leading-none text-muted-foreground/50">
      <BranchChip workspace={workspace} compact dropUp />
    </div>
  );
}

function composerModelShortLabel(health?: LlmHealth | null): string {
  if (health == null) return "Auto";
  const model = health.model?.trim();
  if (health.configured && model) {
    const tail = model.split("/").pop() ?? model;
    return tail.length > 18 ? `${tail.slice(0, 16)}…` : tail;
  }
  if (health.configured && health.provider) {
    return formatLlmProviderTag(health.provider);
  }
  return "Auto";
}

function ComposerModelFootnote({
  health,
  openSettings,
  variant = "idle",
}: {
  health?: LlmHealth | null;
  openSettings: () => void;
  variant?: "idle" | "session";
}) {
  if (variant === "session") {
    const fullLabel =
      health?.configured && health.model?.trim()
        ? `${formatLlmProviderTag(health.provider)} – ${health.model}`
        : health?.configured && health.provider
          ? formatLlmProviderTag(health.provider)
          : "Default model routing";
    return (
      <button
        type="button"
        onClick={openSettings}
        title={fullLabel}
        className={cn(
          "inline-flex h-8 max-w-[8.5rem] shrink-0 items-center gap-0.5 rounded-md px-1",
          "text-[13px] text-muted-foreground/75 transition-colors",
          "hover:text-foreground/90",
          "focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring/45",
        )}
      >
        <span className="truncate">{composerModelShortLabel(health)}</span>
        <ChevronDown className="size-3 shrink-0 opacity-45" strokeWidth={2} aria-hidden />
      </button>
    );
  }

  if (health == null) {
    return (
      <span className="shrink-0 text-[10.5px] text-muted-foreground/50">
        Model…
      </span>
    );
  }
  const configured = health.configured;
  const model = health.model?.trim();
  const provider = health.provider;
  const label =
    configured && model
      ? `${formatLlmProviderTag(provider)} – ${model}`
      : configured && provider
        ? formatLlmProviderTag(provider)
        : null;

  if (label) {
    return (
      <div
        className="max-w-[14rem] shrink truncate text-left text-[10.5px] leading-tight text-muted-foreground/90"
        title={label}
      >
        <span className="font-medium text-muted-foreground">{label}</span>
      </div>
    );
  }
  return (
    <button
      type="button"
      onClick={openSettings}
      className="shrink-0 text-[10.5px] text-muted-foreground underline-offset-2 hover:text-foreground hover:underline"
    >
      Configure model
    </button>
  );
}

function firstOpenDialogPath(
  picked: string | string[] | null,
): string | null {
  if (picked == null) return null;
  if (typeof picked === "string") return picked;
  return picked[0] ?? null;
}

function ComposerAttachMenu({
  disabled,
  insertText,
  variant = "idle",
}: {
  disabled: boolean;
  insertText: (snippet: string, opts?: { cursorOffset?: number }) => void;
  variant?: "idle" | "session";
}) {
  const [open, setOpen] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onDoc = (e: MouseEvent) => {
      if (!rootRef.current?.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("mousedown", onDoc);
    window.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDoc);
      window.removeEventListener("keydown", onKey);
    };
  }, [open]);

  const close = () => setOpen(false);

  const pickPath = async (opts: {
    directory?: boolean;
    filters?: { name: string; extensions: string[] }[];
  }) => {
    try {
      const picked = await openDialog({
        multiple: false,
        directory: opts.directory ?? false,
        filters: opts.filters,
      });
      const path = firstOpenDialogPath(picked);
      if (!path) return;
      insertText(`${path}\n`);
      close();
    } catch (e) {
      toast("Could not open file picker", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
    }
  };

  const pickImageMarkdown = async () => {
    try {
      const picked = await openDialog({
        multiple: false,
        directory: false,
        filters: [
          {
            name: "Images",
            extensions: ["png", "jpg", "jpeg", "webp", "gif", "svg", "heic", "bmp", "tif", "tiff"],
          },
        ],
      });
      const path = firstOpenDialogPath(picked);
      if (!path) return;
      insertText(`![](${path})\n`);
      close();
    } catch (e) {
      toast("Could not open file picker", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
    }
  };

  const onPasteClipboard = async () => {
    try {
      const t = await readText();
      if (!t?.trim()) {
        toast("Clipboard is empty", { kind: "info" });
        return;
      }
      insertText(t);
      close();
    } catch (e) {
      toast("Could not read clipboard", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
    }
  };

  type RowProps = { icon: ReactNode; label: string; onClick: () => void };
  const Row = ({ icon, label, onClick }: RowProps) => (
    <button
      type="button"
      className="flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-[11px] text-foreground/90 hover:bg-surface-elevated"
      onClick={() => void onClick()}
    >
      <span className="flex size-4 shrink-0 items-center justify-center text-muted-foreground">
        {icon}
      </span>
      {label}
    </button>
  );

  if (variant === "session") {
    return (
      <div ref={rootRef} className="relative shrink-0">
        <SessionComposerCircleButton
          disabled={disabled}
          aria-haspopup="menu"
          aria-expanded={open}
          aria-label="Add context"
          className="bg-black/35 text-muted-foreground/85 hover:bg-black/45 hover:text-foreground disabled:opacity-40"
          onClick={() => setOpen((v) => !v)}
        >
          <Plus className="size-4" strokeWidth={2} />
        </SessionComposerCircleButton>
        {open && (
          <div
            className="absolute bottom-full left-0 z-[100] mb-2 min-w-[12.5rem] rounded-lg border border-border bg-muted p-1 text-foreground shadow-elevated"
            role="menu"
          >
            <Row
              icon={<FileText className="size-3.5" />}
              label="Insert document path…"
              onClick={() =>
                void pickPath({
                  filters: [{ name: "Documents & code", extensions: DOC_ATTACH_EXTENSIONS }],
                })
              }
            />
            <Row
              icon={<ImageIcon className="size-3.5" />}
              label="Insert image (markdown)…"
              onClick={() => void pickImageMarkdown()}
            />
            <Row
              icon={<FileText className="size-3.5 opacity-70" />}
              label="Insert any file path…"
              onClick={() => void pickPath({})}
            />
            <Row
              icon={<FolderOpen className="size-3.5" />}
              label="Insert folder path…"
              onClick={() => void pickPath({ directory: true })}
            />
            <div className="my-1 h-px bg-border" />
            <Row
              icon={<ClipboardPaste className="size-3.5" />}
              label="Paste clipboard text"
              onClick={() => void onPasteClipboard()}
            />
            <Row
              icon={<Code2 className="size-3.5" />}
              label="Insert code block"
              onClick={() => {
                insertText("```\n\n```", { cursorOffset: 4 });
                close();
              }}
            />
          </div>
        )}
      </div>
    );
  }

  return (
    <div ref={rootRef} className="relative shrink-0">
      <Button
        type="button"
        variant="ghost"
        size="icon"
        disabled={disabled}
        aria-haspopup="menu"
        aria-expanded={open}
        className="h-7 w-7 rounded-md text-muted-foreground hover:text-foreground"
        onClick={() => setOpen((v) => !v)}
      >
        <Plus className="size-4" />
      </Button>
      {open && (
        <div
          className="absolute left-0 top-full z-[100] mt-1.5 min-w-[12.5rem] rounded-lg border border-border bg-muted p-1 text-foreground shadow-elevated"
          role="menu"
        >
          <Row
            icon={<FileText className="size-3.5" />}
            label="Insert document path…"
            onClick={() =>
              void pickPath({
                filters: [{ name: "Documents & code", extensions: DOC_ATTACH_EXTENSIONS }],
              })
            }
          />
          <Row
            icon={<ImageIcon className="size-3.5" />}
            label="Insert image (markdown)…"
            onClick={() => void pickImageMarkdown()}
          />
          <Row
            icon={<FileText className="size-3.5 opacity-70" />}
            label="Insert any file path…"
            onClick={() => void pickPath({})}
          />
          <Row
            icon={<FolderOpen className="size-3.5" />}
            label="Insert folder path…"
            onClick={() => void pickPath({ directory: true })}
          />
          <div className="my-1 h-px bg-border" />
          <Row
            icon={<ClipboardPaste className="size-3.5" />}
            label="Paste clipboard text"
            onClick={() => void onPasteClipboard()}
          />
          <Row
            icon={<Code2 className="size-3.5" />}
            label="Insert code block"
            onClick={() => {
              insertText("```\n\n```", { cursorOffset: 4 });
              close();
            }}
          />
        </div>
      )}
    </div>
  );
}

function Composer({
  workspace,
  workspaceRootPath,
  convId,
  disabled,
  autoFocus,
  isIdleCentered,
  compileAction,
  health,
  recentHistory,
  onCancel,
  onCreateConvIfNeeded,
  onUserMessage,
  onStartTurn,
}: {
  workspace: string;
  /** Absolute path for `workspace` — used to align compile progress with this row. */
  workspaceRootPath: string | null;
  convId: string | null;
  disabled: boolean;
  autoFocus?: boolean;
  isIdleCentered?: boolean;
  compileAction?: {
    busy: boolean;
    label: string;
    onRun: () => Promise<void>;
  };
  health?: LlmHealth | null;
  /** Last ~8 user/assistant turns of this conversation, oldest-first.
   *  Forwarded to the engine so the agent can treat them as memory.
   *  Empty for fresh conversations. */
  recentHistory: ChatTurnPayload[];
  onCancel: () => void;
  onCreateConvIfNeeded: (firstUserText: string) => Promise<string>;
  onUserMessage: (content: string) => void;
  onStartTurn: (turnId: string, workspace: string, convId: string) => void;
}) {
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const [slashDismissed, setSlashDismissed] = useState(false);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const textRef = useRef(text);
  useEffect(() => {
    textRef.current = text;
  }, [text]);

  const openComposerSettings = useCallback(() => {
    useApp.getState().setSettingsSection("provider");
    useApp.getState().setSurface("settings");
  }, []);

  const insertText = useCallback((snippet: string, opts?: { cursorOffset?: number }) => {
    const el = textareaRef.current;
    const current = textRef.current;
    const start = el ? Math.min(el.selectionStart, current.length) : current.length;
    const end = el ? Math.min(el.selectionEnd, current.length) : current.length;
    const before = current.slice(0, start);
    const after = current.slice(end);
    const needsLead =
      before.length > 0 && !before.endsWith("\n") && !/^\n/.test(snippet);
    const lead = needsLead ? "\n" : "";
    const next = before + lead + snippet + after;
    setText(next);
    const cursorPos =
      start + lead.length + (opts?.cursorOffset ?? snippet.length);
    requestAnimationFrame(() => {
      el?.focus();
      try {
        el?.setSelectionRange(cursorPos, cursorPos);
      } catch {
        /* selection may be invalid while unmounted */
      }
    });
  }, []);

  // Slash autocomplete is open whenever the user is typing a /command on
  // the first line and hasn't pressed Esc to dismiss it.
  const slashQuery =
    !slashDismissed && /^\/[A-Za-z]*$/.test(text.trimStart()) && !text.includes("\n")
      ? text.trimStart()
      : null;

  useEffect(() => {
    if (autoFocus) textareaRef.current?.focus();
  }, [autoFocus]);

  // Re-arm the slash menu when the user clears the textarea.
  useEffect(() => {
    if (!text) setSlashDismissed(false);
  }, [text]);

  useEffect(() => {
    const onFileDrop = (e: Event) => {
      const paths = (e as CustomEvent<string[]>).detail;
      if (!Array.isArray(paths) || paths.length === 0) return;
      insertText(formatDroppedPathsForComposer(paths));
    };
    window.addEventListener(COMPOSER_FILE_DROP_EVENT, onFileDrop);
    return () => window.removeEventListener(COMPOSER_FILE_DROP_EVENT, onFileDrop);
  }, [insertText]);

  // Auto-resize textarea
  useLayoutEffect(() => {
    const el = textareaRef.current;
    if (!el) return;
    el.style.height = "auto";
    const newHeight = Math.min(el.scrollHeight, 400);
    el.style.height = `${newHeight}px`;
  }, [text]);

  async function send() {
    const trimmed = text.trim();
    if (!trimmed || busy) return;
    setBusy(true);
    try {
      // Slash commands are not LLM turns — handle inline.
      if (trimmed.startsWith("/")) {
        await runSlashCommand({ workspace, raw: trimmed });
        setText("");
        setBusy(false);
        return;
      }

      // Ensure a conversation exists *before* persisting the user message.
      const cid = convId ?? (await onCreateConvIfNeeded(trimmed));
      onUserMessage(trimmed);
      setText("");

      const ack = await chatSendStream({
        workspace,
        question: trimmed,
        conversationId: cid,
        useAgent: true,
        history: recentHistory,
      });
      onStartTurn(ack.turn_id, workspace, cid);
    } catch (e) {
      toast("Send failed", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
    } finally {
      setBusy(false);
    }
  }

  const placeholder = disabled
    ? "Generating…"
    : isIdleCentered
      ? "Understand this space — start asking"
      : "Send follow-up";

  return (
    <div className={cn("px-5", isIdleCentered ? "py-3" : "px-6 pb-4 pt-2")}>
      <div
        className={cn(
          "mx-auto",
          isIdleCentered ? "max-w-[38rem]" : "max-w-3xl",
        )}
      >
        {!isIdleCentered && health && (
          <div className="mb-2 px-1">
            <LlmHealthBanner
              health={health}
              workspace={workspace}
              workspaceRootPath={workspaceRootPath}
              openSettings={openComposerSettings}
            />
          </div>
        )}
        <div
          className={cn(
            "group relative flex flex-col overflow-visible",
            isIdleCentered
              ? "rounded-xl border border-border/60 bg-surface-elevated"
              : "rounded-[26px] border border-white/[0.1] bg-[hsl(0,0%,13.5%)]",
          )}
        >
          {isIdleCentered && health && (
            <div className="px-4 pt-3">
              <LlmHealthBanner
                health={health}
                workspace={workspace}
                workspaceRootPath={workspaceRootPath}
                openSettings={openComposerSettings}
              />
            </div>
          )}

          {/* Slash autocomplete — anchored to textarea; opens up when composer is at bottom */}
          <div className="relative w-full overflow-visible">
            {slashQuery && (
              <SlashAutocomplete
                query={slashQuery}
                placement={isIdleCentered ? "below" : "above"}
                onSelect={(insertion) => {
                  setText(insertion);
                  setSlashDismissed(false);
                  requestAnimationFrame(() => textareaRef.current?.focus());
                }}
                onDismiss={() => setSlashDismissed(true)}
              />
            )}
            {isIdleCentered ? (
            <textarea
              ref={textareaRef}
              value={text}
              onChange={(e) => setText(e.target.value)}
              placeholder={placeholder}
              rows={1}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !e.shiftKey) {
                  e.preventDefault();
                  void send();
                }
              }}
              className="w-full min-h-[92px] resize-none border-0 bg-transparent py-5 pb-12 pl-4 pr-4 text-[14px] leading-6 text-foreground placeholder:text-muted-foreground/40 outline-none ring-0 shadow-none appearance-none focus:outline-none focus:ring-0 focus-visible:outline-none focus-visible:ring-0 focus-visible:ring-offset-0"
            />
            ) : (
              <div className="flex min-h-[46px] items-center gap-1 pl-2 pr-1.5 py-1">
                <ComposerAttachMenu
                  disabled={disabled || busy}
                  insertText={insertText}
                  variant="session"
                />
                <textarea
                  ref={textareaRef}
                  value={text}
                  onChange={(e) => setText(e.target.value)}
                  placeholder={placeholder}
                  rows={1}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" && !e.shiftKey) {
                      e.preventDefault();
                      void send();
                    }
                  }}
                  className="max-h-[min(200px,40vh)] min-h-[22px] flex-1 resize-none border-0 bg-transparent px-1 py-2.5 text-[13px] leading-5 text-foreground caret-foreground placeholder:text-muted-foreground/50 outline-none ring-0 shadow-none appearance-none focus:outline-none focus:ring-0 focus-visible:outline-none focus-visible:ring-0 focus-visible:ring-offset-0"
                />
                <div className="flex shrink-0 items-center gap-0.5">
                  <ComposerModelFootnote
                    health={health}
                    openSettings={openComposerSettings}
                    variant="session"
                  />
                  {disabled ? (
                    <SessionComposerCircleButton
                      onClick={onCancel}
                      aria-label="Stop generating"
                      className="bg-black/35 text-foreground hover:bg-black/45"
                    >
                      <Square className="size-2.5 fill-current" />
                    </SessionComposerCircleButton>
                  ) : text.trim() ? (
                    <SessionComposerCircleButton
                      disabled={busy}
                      onClick={() => void send()}
                      aria-label="Send message"
                      className="bg-white text-[hsl(0,0%,9%)] hover:bg-white/92 disabled:opacity-35"
                    >
                      <ArrowUp className="size-4" strokeWidth={2.5} />
                    </SessionComposerCircleButton>
                  ) : (
                    <SessionComposerCircleButton
                      disabled
                      aria-hidden
                      tabIndex={-1}
                      className="bg-black/35 text-muted-foreground/70"
                    >
                      <Mic className="size-3.5" strokeWidth={2} />
                    </SessionComposerCircleButton>
                  )}
                </div>
              </div>
            )}

            {isIdleCentered && (
            <div className="absolute inset-x-0 bottom-0 flex items-center gap-2 px-1.5 pb-2 pt-1">
              <ComposerAttachMenu
                disabled={disabled || busy}
                insertText={insertText}
              />
              <div className="min-w-0 max-w-[min(46vw,15rem)] shrink">
                <ComposerModelFootnote
                  health={health}
                  openSettings={openComposerSettings}
                />
              </div>
              <div className="min-w-0 flex-1" />
              <div className="flex shrink-0 items-center justify-end gap-2">
                {compileAction && (
                  <Button
                    variant="outline"
                    size="sm"
                    disabled={compileAction.busy || disabled}
                    onClick={() => void compileAction.onRun()}
                    className="hidden h-7 shrink-0 rounded-md border-border/70 bg-muted px-2 text-[10px] hover:bg-muted/80 sm:inline-flex"
                  >
                    {compileAction.busy ? (
                      <Loader2 className="mr-1 size-3 animate-spin" />
                    ) : (
                      <Hammer className="mr-1 size-3" />
                    )}
                    {compileAction.busy ? "…" : compileAction.label}
                  </Button>
                )}
                {compileAction && (
                  <Button
                    variant="outline"
                    size="icon"
                    disabled={compileAction.busy || disabled}
                    onClick={() => void compileAction.onRun()}
                    className="inline-flex h-7 w-7 shrink-0 rounded-md border-border/70 bg-muted sm:hidden"
                    title={compileAction.label}
                  >
                    {compileAction.busy ? (
                      <Loader2 className="size-3.5 animate-spin" />
                    ) : (
                      <Hammer className="size-3.5" />
                    )}
                  </Button>
                )}
                {disabled ? (
                  <Button
                    variant="ghost"
                    size="icon"
                    onClick={onCancel}
                    className={cn(
                      "shrink-0 rounded-full hover:bg-destructive/10 hover:text-destructive",
                      isIdleCentered ? "h-8 w-8" : "h-7 w-7",
                    )}
                  >
                    <Square
                      className={cn(
                        "fill-current",
                        isIdleCentered ? "size-3.5" : "size-3",
                      )}
                    />
                  </Button>
                ) : (
                  <Button
                    size="icon"
                    disabled={!text.trim() || busy}
                    onClick={() => void send()}
                    className={cn(
                      "shrink-0 rounded-full bg-foreground text-background shadow-none transition-transform hover:scale-105 active:scale-95 disabled:opacity-30",
                      isIdleCentered ? "h-8 w-8" : "h-7 w-7",
                    )}
                  >
                    <ArrowUp
                      className={cn(isIdleCentered ? "size-4" : "size-3.5")}
                    />
                  </Button>
                )}
              </div>
            </div>
            )}
          </div>
        </div>
        {!isIdleCentered && (
          <ComposerBranchFootnote workspace={workspace} />
        )}
      </div>
    </div>
  );
}

// Re-exported for the right-rail "Last error" pane.
export function chatErrorBanner(message: string) {
  return (
    <div className="flex items-start gap-2 rounded-md border border-destructive/30 bg-destructive/10 px-3 py-2 text-xs text-destructive">
      <AlertTriangle className="mt-0.5 size-3.5 shrink-0" />
      <span>{message}</span>
    </div>
  );
}
