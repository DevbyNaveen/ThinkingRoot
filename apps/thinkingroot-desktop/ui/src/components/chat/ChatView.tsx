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
  useRef,
  useState,
  type ReactNode,
} from "react";
import {
  ArrowUp,
  Square,
  AlertTriangle,
  Hammer,
  Loader2,
  Plus,
  FileText,
  Image as ImageIcon,
  FolderOpen,
  ClipboardPaste,
  Code2,
} from "lucide-react";
import { readText, writeText } from "@tauri-apps/plugin-clipboard-manager";
import { open as openDialog } from "@tauri-apps/plugin-dialog";

import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { Prism as SyntaxHighlighter } from "react-syntax-highlighter";
import { vscDarkPlus } from "react-syntax-highlighter/dist/esm/styles/prism";

import { cn } from "@/lib/utils";
import { useApp } from "@/store/app";
import { Button } from "@/components/ui/button";
import { toast } from "@/store/toast";
import {
  pickPrimaryDiagnostic,
  useWorkspaceStatus,
  useWorkspaceStatusSubscription,
} from "@/store/workspace-status";
import {
  chatSendStream,
  conversationsAppendMessage,
  conversationsCreate,
  conversationsGet,
  llmHealth,
  workspaceCompile,
  workspaceList,
  onChatEvent,
  type ChatEvent,
  type ChatTurnPayload,
  type LlmHealth,
} from "@/lib/tauri";
import { BrainCitationParser, useBrainActivation } from "@/store/brain";
import type { ChatMessage, EngramActivationEntry, GapEntry } from "@/types";
import { BranchChip } from "./BranchChip";
import { LiveActivityStrip } from "./LiveActivityStrip";
import { SlashAutocomplete } from "./SlashAutocomplete";
import { EngramTimeline } from "./EngramTimeline";
import { GapCards } from "./GapCards";
import { ReasoningTrace } from "./ReasoningTrace";
import { TrustReceiptChip } from "./TrustReceipt";
import { runSlashCommand } from "./slashCommands";

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
        setMessages(activeWorkspace, activeConv, deduped);
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
        cur.upsertAgentStep({
          id: ev.id,
          name: ev.name,
          input: prettyJson(ev.input),
          isWrite: ev.is_write,
          status: "proposed",
        });
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
        return;
      }
      if (ev.type === "tool_call_executing") {
        if (cur.streaming?.turnId !== ev.turn_id) return;
        cur.patchAgentStep(ev.id, { status: "executing" });
        return;
      }
      if (ev.type === "tool_call_finished") {
        if (cur.streaming?.turnId !== ev.turn_id) return;
        cur.patchAgentStep(ev.id, {
          status: "finished",
          output: ev.content,
          isError: ev.is_error,
        });
        return;
      }
      if (ev.type === "tool_call_rejected") {
        if (cur.streaming?.turnId !== ev.turn_id) return;
        cur.patchAgentStep(ev.id, {
          status: "rejected",
          output: ev.reason,
        });
        return;
      }

      if (ev.type === "final" || ev.type === "error") {
        const msgBody =
          ev.type === "final" ? ev.full_text : `⚠️ ${ev.message}`;

        // Persist to disk + UI cache when we know which conversation
        // to write to. The on-disk write is fire-and-forget; UI cache
        // is what makes the bubble appear.
        if (ctx && ctx.convId) {
          if (ev.type === "final") {
            conversationsAppendMessage({
              workspace: ctx.workspace,
              conversationId: ctx.convId,
              role: "assistant",
              content: ev.full_text,
              claimsUsed: [],
            }).catch((e) => {
              toast("Persist message failed", {
                kind: "warn",
                body: e instanceof Error ? e.message : String(e),
              });
            });
          }
          const messageId = `m-${Date.now()}-${ev.type === "final" ? "a" : "e"}`;
          const flushedActivations = turnEngramActivations.get(ev.turn_id);
          const flushedGaps = turnGaps.get(ev.turn_id);
          // Reasoning-trace snapshot: copy the streaming.agentSteps
          // off the active StreamState (read BEFORE setStreaming(null)
          // below clears it). Empty arrays drop to undefined so the
          // accordion is hidden when there's nothing to expand.
          const flushedSteps =
            cur.streaming?.turnId === ev.turn_id
              ? [...cur.streaming.agentSteps]
              : [];
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
          // Remember this message id for the matching trust_receipt
          // event that arrives shortly after `final` on the same SSE
          // stream. Only meaningful for `final` (errors don't carry
          // a verifier verdict).
          if (ev.type === "final") {
            lastAssistantMessage.set(ev.turn_id, {
              workspace: ctx.workspace,
              convId: ctx.convId,
              messageId,
            });
          }
        } else {
          // eslint-disable-next-line no-console
          console.warn(
            "[chat-event] no ctx for final/error — bubble suppressed but state cleared",
            { turn_id: ev.turn_id, fromMap, fromActive },
          );
        }

        if (cur.streaming?.turnId === ev.turn_id) {
          setStreaming(null);
        }
        // For `final`, do NOT clearTurn — the trust_receipt may still
        // be in flight. We clear in the trust_receipt handler below,
        // and also after a short timeout below in case the stream
        // closes without one. For `error`, clear immediately.
        if (ev.type === "error") {
          cur.clearTurn(ev.turn_id);
          citationParsers.delete(ev.turn_id);
          lastAssistantMessage.delete(ev.turn_id);
          turnEngramActivations.delete(ev.turn_id);
          turnGaps.delete(ev.turn_id);
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

  // Auto-scroll on append.
  const bottomRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [messages.length, streaming?.partial]);

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
        {/* Vertically centered floating composer — Cursor-style */}
        <div className="flex flex-1 flex-col items-center justify-center px-8">
          <div className="flex w-full max-w-3xl flex-col gap-3">
            {/* Subtle heading above the card */}
            <div className="mb-1 text-center">
              <p className="text-[11px] uppercase tracking-widest text-muted-foreground/50">
                {activeWorkspace}
              </p>
            </div>

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
                const id = `m-${Date.now()}-u`;
                appendMessage(ws, cid, {
                  id,
                  kind: "user",
                  body: content,
                  at: new Date(),
                });
                conversationsAppendMessage({
                  workspace: ws,
                  conversationId: cid,
                  role: "user",
                  content,
                }).catch((e) => {
                  useApp.getState().removeMessage(ws, cid, id);
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
              Ask anything · <kbd className="font-mono">/</kbd> for commands · <kbd className="font-mono">@</kbd> to mention
            </p>
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col bg-background">
      <div className="border-b border-border/40 bg-background/80 px-8 py-2 backdrop-blur">
        <div className="mx-auto flex min-w-0 max-w-3xl items-center gap-1.5">
          <span className="min-w-0 truncate text-[10px] font-semibold uppercase tracking-widest text-muted-foreground/70">
            {activeWorkspace}
          </span>
          <span
            className="shrink-0 select-none text-[10px] text-muted-foreground/40"
            aria-hidden
          >
            /
          </span>
          <div className="min-w-0 shrink">
            <BranchChip workspace={activeWorkspace} />
          </div>
        </div>
      </div>
      <div className="flex-1 overflow-y-auto px-8 py-6">
        <ul className="mx-auto flex max-w-3xl flex-col gap-6">
          {messages.map((m) => (
            <li key={m.id}>
              <MessageBubble msg={m} />
            </li>
          ))}
          {streaming && (
            <li className="space-y-3">
              <LiveActivityStrip
                steps={streaming.agentSteps}
                workspace={activeWorkspace}
                hasAnswer={streaming.partial.length > 0}
              />
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
              {streaming.partial.length > 0 && (
                <MessageBubble
                  msg={{
                    id: streaming.turnId,
                    kind: "assistant",
                    body: streaming.partial,
                    at: streaming.startedAt,
                  }}
                  pending
                />
              )}
              {/* Option A: agent steps use dragonfly row inside LiveActivityStrip only — no duplicate ThinkingLoader */}
              {streaming.partial.length === 0 && streaming.agentSteps.length === 0 && (
                <MessageBubble
                  msg={{
                    id: streaming.turnId,
                    kind: "assistant",
                    body: streaming.partial,
                    at: streaming.startedAt,
                  }}
                  pending
                  pendingLabel="Searching your knowledge base..."
                />
              )}
            </li>
          )}
          <div ref={bottomRef} />
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
          const id = `m-${Date.now()}-u`;
          appendMessage(ws, cid, {
            id,
            kind: "user",
            body: content,
            at: new Date(),
          });
          conversationsAppendMessage({
            workspace: ws,
            conversationId: cid,
            role: "user",
            content,
          }).catch((e) => {
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

function AssistantMessageActions({ body }: { body: string }) {
  if (!body.trim()) return null;
  return (
    <div className="mt-3 flex flex-wrap items-center gap-1 border-t border-border/30 pt-2">
      <Button
        type="button"
        variant="ghost"
        size="sm"
        className="h-7 px-2 text-[11px] text-muted-foreground hover:text-foreground"
        onClick={() => void copyAssistantMessage(body)}
      >
        Copy
      </Button>
      <Button
        type="button"
        variant="ghost"
        size="sm"
        className="h-7 px-2 text-[11px] text-muted-foreground hover:text-foreground"
        onClick={() => void shareAssistantMessage(body)}
      >
        Share
      </Button>
    </div>
  );
}

function ThinkingLoader({ label = "Thinking" }: { label?: string }) {
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

  // AI Message: No bubble, full width, rendered with Markdown
  if (!isUser) {
    return (
      <div className={cn("flex w-full px-2", pending && "opacity-90")}>
        <div className="w-full max-w-3xl text-[15px] leading-7 text-foreground">
          <ReactMarkdown
            remarkPlugins={[remarkGfm]}
            components={{
              code({ node, inline, className, children, ...props }: any) {
                const match = /language-(\w+)/.exec(className || "");
                return !inline && match ? (
                  <div className="my-4 overflow-hidden rounded-md border border-border/50">
                    <div className="flex items-center justify-between bg-muted/50 px-4 py-1.5 text-xs font-medium text-muted-foreground">
                      <span>{match[1]}</span>
                    </div>
                    <SyntaxHighlighter
                      style={vscDarkPlus as any}
                      language={match[1]}
                      PreTag="div"
                      customStyle={{ margin: 0, background: "transparent", padding: "16px" }}
                      {...props}
                    >
                      {String(children).replace(/\n$/, "")}
                    </SyntaxHighlighter>
                  </div>
                ) : (
                  <code className="rounded bg-muted/80 px-1.5 py-0.5 text-[13px] font-mono text-foreground" {...props}>
                    {children}
                  </code>
                );
              },
              p: ({ children }) => <p className="mb-4 last:mb-0 leading-relaxed">{children}</p>,
              ul: ({ children }) => <ul className="mb-4 list-disc pl-6 last:mb-0 space-y-1">{children}</ul>,
              ol: ({ children }) => <ol className="mb-4 list-decimal pl-6 last:mb-0 space-y-1">{children}</ol>,
              li: ({ children }) => <li className="mb-1 leading-relaxed">{children}</li>,
              h1: ({ children }) => <h1 className="mb-4 mt-6 text-2xl font-bold">{children}</h1>,
              h2: ({ children }) => <h2 className="mb-4 mt-6 text-xl font-bold border-b border-border/50 pb-2">{children}</h2>,
              h3: ({ children }) => <h3 className="mb-4 mt-4 text-lg font-bold">{children}</h3>,
              a: ({ href, children }) => (
                <a href={href} target="_blank" rel="noopener noreferrer" className="text-primary underline underline-offset-4">
                  {children}
                </a>
              ),
              blockquote: ({ children }) => (
                <blockquote className="border-l-4 border-muted pl-4 italic text-muted-foreground my-4">
                  {children}
                </blockquote>
              ),
            }}
          >
            {msg.body}
          </ReactMarkdown>
          {pending && (
            <span className="ml-1 inline-block h-3.5 w-1.5 translate-y-0.5 bg-accent/60 animate-pulse" />
          )}
          <AssistantMessageActions body={msg.body} />
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
          {!pending && msg.trustReceipt && (
            <div className="mt-2">
              <TrustReceiptChip receipt={msg.trustReceipt} />
            </div>
          )}
        </div>
      </div>
    );
  }

  // User Message: Right-aligned bubble
  return (
    <div className="flex w-full justify-end px-2">
      <div
        className={cn(
          "max-w-2xl whitespace-pre-wrap break-words rounded-2xl bg-accent/15 px-4 py-3 text-[15px] text-foreground transition-all duration-300",
          pending && "opacity-90"
        )}
      >
        {msg.body}
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

function ComposerModelFootnote({
  health,
  openSettings,
}: {
  health?: LlmHealth | null;
  openSettings: () => void;
}) {
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
}: {
  disabled: boolean;
  insertText: (snippet: string, opts?: { cursorOffset?: number }) => void;
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

  return (
    <div className="px-5 py-3">
      <div
        className={cn(
          "mx-auto",
          isIdleCentered ? "max-w-[38rem]" : "max-w-3xl",
        )}
      >
        {/* Floating card — large radius, subtle shadow, gentle border */}
        <div
          className={cn(
            "group relative flex flex-col overflow-visible border border-border/60 bg-surface-elevated shadow-[0_2px_16px_rgba(0,0,0,0.25)] transition-shadow",
            isIdleCentered ? "rounded-xl" : "rounded-2xl",
          )}
        >

          {/* Health banner inside the card, above the textarea */}
          {health && (
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
            {/* Textarea */}
            <textarea
              ref={textareaRef}
              value={text}
              onChange={(e) => setText(e.target.value)}
              placeholder={
                disabled
                  ? "Generating…"
                  : "Plan, Build, / for commands, @ for context"
              }
              rows={1}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !e.shiftKey) {
                  e.preventDefault();
                  void send();
                }
              }}
              className={cn(
                "w-full resize-none border-0 bg-transparent pl-4 pr-4 text-[14px] leading-6 text-foreground placeholder:text-muted-foreground/40 outline-none ring-0 shadow-none appearance-none focus:outline-none focus:ring-0 focus-visible:outline-none focus-visible:ring-0 focus-visible:ring-offset-0",
                isIdleCentered
                  ? "min-h-[92px] py-5 pb-12"
                  : "py-2.5 pb-10",
              )}
            />

            {/* Bottom row: + · model (left) · compile + send (right) */}
            <div
              className={cn(
                "absolute inset-x-0 bottom-0 flex items-center gap-2 px-1.5",
                isIdleCentered ? "pb-2 pt-1" : "pb-1.5 pt-0.5",
              )}
            >
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
                {isIdleCentered && compileAction && (
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
                {isIdleCentered && compileAction && (
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
          </div>
        </div>
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
