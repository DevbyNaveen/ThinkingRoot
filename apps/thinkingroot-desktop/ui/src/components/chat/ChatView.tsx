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
import { useEffect, useLayoutEffect, useRef, useState } from "react";
import {
  ArrowUp,
  Square,
  AlertTriangle,
  Hammer,
  Loader2,
} from "lucide-react";

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
  onChatEvent,
  type ChatEvent,
  type ChatTurnPayload,
  type LlmHealth,
} from "@/lib/tauri";
import type { ChatMessage } from "@/types";
import { ClaimCard } from "./ClaimCard";
import { SlashAutocomplete } from "./SlashAutocomplete";
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
          appendMessage(ctx.workspace, ctx.convId, {
            id: `m-${Date.now()}-${ev.type === "final" ? "a" : "e"}`,
            kind: "assistant",
            body: msgBody,
            at: new Date(),
          });
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
        cur.clearTurn(ev.turn_id);
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
      <div className="flex-1 overflow-y-auto px-8 py-6">
        <ul className="mx-auto flex max-w-3xl flex-col gap-6">
          {messages.map((m) => (
            <li key={m.id}>
              <MessageBubble msg={m} />
            </li>
          ))}
          {streaming && (
            <li className="space-y-3">
              {streaming.agentSteps.length > 0 && (
                <div className="mx-auto w-full max-w-3xl">
                  <div className="rounded-xl border border-border/60 bg-muted/20 p-2.5">
                    <div className="mb-2 text-[10px] font-semibold uppercase tracking-widest text-muted-foreground/70">
                      Activity
                    </div>
                    <div className="space-y-1.5">
                      {streaming.agentSteps.map((step) => (
                        <ClaimCard
                          key={step.id}
                          step={step}
                          workspace={activeWorkspace}
                        />
                      ))}
                    </div>
                  </div>
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
              {streaming.partial.length === 0 &&
                streaming.agentSteps.length === 0 && (
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
            </li>
          )}
          <div ref={bottomRef} />
        </ul>
      </div>

      <Composer
        workspace={activeWorkspace}
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
  openSettings,
}: {
  health: LlmHealth | null;
  workspace: string;
  openSettings: () => void;
}) {
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

function ThinkingLoader() {
  return (
    <div className="flex h-12 items-center justify-start px-2">
      <div className="inline-flex items-center gap-2 rounded-full bg-muted/35 px-3 py-1.5 text-xs text-muted-foreground">
        <span>Thinking</span>
        <span className="inline-flex items-center gap-1">
          <span className="size-1.5 animate-pulse rounded-full bg-muted-foreground/70 [animation-delay:0ms]" />
          <span className="size-1.5 animate-pulse rounded-full bg-muted-foreground/70 [animation-delay:180ms]" />
          <span className="size-1.5 animate-pulse rounded-full bg-muted-foreground/70 [animation-delay:360ms]" />
        </span>
      </div>
    </div>
  );
}

function MessageBubble({ msg, pending }: { msg: ChatMessage; pending?: boolean }) {
  const isUser = msg.kind === "user";
  const isThinking = pending && !msg.body && !isUser;

  if (isThinking) {
    return <ThinkingLoader />;
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

function Composer({
  workspace,
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
            "group relative flex flex-col border border-border/50 bg-muted/30 shadow-[0_2px_16px_rgba(0,0,0,0.25)] transition-shadow",
            isIdleCentered ? "rounded-xl" : "rounded-2xl",
          )}
        >

          {/* Health banner inside the card, above the textarea */}
          {health && (
            <div className="px-4 pt-3">
              <LlmHealthBanner
                health={health}
                workspace={workspace}
                openSettings={() => useApp.getState().setSurface("settings")}
              />
            </div>
          )}

          {/* Slash autocomplete */}
          {slashQuery && (
            <div className="px-4 pt-3">
              <SlashAutocomplete
                query={slashQuery}
                onSelect={(insertion) => {
                  setText(insertion);
                  setSlashDismissed(false);
                  requestAnimationFrame(() => textareaRef.current?.focus());
                }}
                onDismiss={() => setSlashDismissed(true)}
              />
            </div>
          )}

          <div className="relative w-full">
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
                "w-full resize-none border-0 bg-transparent pl-4 pr-12 text-[14px] leading-6 text-foreground placeholder:text-muted-foreground/40 outline-none ring-0 shadow-none appearance-none focus:outline-none focus:ring-0 focus-visible:outline-none focus-visible:ring-0 focus-visible:ring-offset-0",
                isIdleCentered ? "min-h-[92px] py-6" : "py-2.5",
              )}
            />

            {/* Inline action button */}
            <div
              className={cn(
                "absolute right-2 flex items-center justify-end",
                isIdleCentered ? "bottom-2.5" : "bottom-1.5",
              )}
            >
              {isIdleCentered && compileAction && (
                <Button
                  variant="outline"
                  size="sm"
                  disabled={compileAction.busy || disabled}
                  onClick={() => void compileAction.onRun()}
                  className="mr-2 h-8 rounded-lg border-border/70 bg-background/40 px-2.5 text-[11px] hover:bg-muted/40"
                >
                  {compileAction.busy ? (
                    <Loader2 className="mr-1 size-3 animate-spin" />
                  ) : (
                    <Hammer className="mr-1 size-3" />
                  )}
                  {compileAction.busy ? "Compiling..." : compileAction.label}
                </Button>
              )}
              {disabled ? (
                <Button
                  variant="ghost"
                  size="icon"
                  onClick={onCancel}
                  className={cn(
                    "rounded-full hover:bg-destructive/10 hover:text-destructive",
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
                    "rounded-full bg-foreground text-background shadow-none transition-transform hover:scale-105 active:scale-95 disabled:opacity-30",
                    isIdleCentered ? "h-8 w-8" : "h-7 w-7",
                  )}
                >
                  <ArrowUp className={cn(isIdleCentered ? "size-4" : "size-3.5")} />
                </Button>
              )}
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
