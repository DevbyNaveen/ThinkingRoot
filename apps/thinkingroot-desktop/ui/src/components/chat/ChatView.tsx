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
  Folder,
  AlertTriangle,
} from "lucide-react";

import { cn } from "@/lib/utils";
import { useApp } from "@/store/app";
import { Button } from "@/components/ui/button";
import { toast } from "@/store/toast";
import {
  chatSendStream,
  conversationsAppendMessage,
  conversationsCreate,
  conversationsGet,
  llmHealth,
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
  const activeConv = useApp((s) => s.activeConversationId);
  const setActiveConv = useApp((s) => s.setActiveConversationId);
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
      // eslint-disable-next-line no-console
      console.log("[chat-event]", ev.type, ev.turn_id, ev);

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
      // eslint-disable-next-line no-console
      console.log("[chat-event] listener registered");
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
    return (
      <div className="flex h-full flex-col bg-background">
        <ChatHeader workspace={activeWorkspace} convTitle="New conversation" />
        <div className="flex flex-1 flex-col items-center px-8 pt-[24vh]">
          <div className="flex w-full max-w-2xl flex-col items-center gap-4">
            <h2 className="text-center text-lg font-medium">
              New conversation in{" "}
              <span className="text-accent">{activeWorkspace}</span>
            </h2>
            <p className="max-w-md text-center text-xs text-muted-foreground">
              Ask anything about your compiled sources, or use{" "}
              <code className="rounded bg-muted px-1 py-0.5 font-mono">/</code>{" "}
              for slash commands.
            </p>
            <LlmHealthBanner health={health} workspace={activeWorkspace} />
            <div className="w-full">
              <Composer
                workspace={activeWorkspace}
                convId={activeConv}
                disabled={streaming != null}
                autoFocus
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
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col bg-background">
      <ChatHeader workspace={activeWorkspace} convTitle="" />

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
                <div className="mx-auto max-w-3xl space-y-2">
                  {streaming.agentSteps.map((step) => (
                    <ClaimCard
                      key={step.id}
                      step={step}
                      workspace={activeWorkspace}
                    />
                  ))}
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

      <div className="mx-auto w-full max-w-3xl px-8">
        <LlmHealthBanner health={health} workspace={activeWorkspace} />
      </div>
      <Composer
        workspace={activeWorkspace}
        convId={activeConv}
        disabled={streaming != null}
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
function LlmHealthBanner({
  health,
  workspace,
}: {
  health: LlmHealth | null;
  workspace: string;
}) {
  if (!health) return null;
  if (!health.mounted) {
    return (
      <div className="flex w-full items-start gap-2 rounded-md border border-yellow-500/30 bg-yellow-500/5 px-3 py-2 text-xs text-yellow-200">
        <AlertTriangle className="mt-0.5 h-3.5 w-3.5 flex-none" />
        <div>
          Workspace <code className="font-mono">{workspace}</code> isn't
          mounted in the engine yet. Compile it from the Workspaces panel
          before chatting.
        </div>
      </div>
    );
  }
  if (!health.configured) {
    return (
      <div className="flex w-full items-start gap-2 rounded-md border border-yellow-500/40 bg-yellow-500/10 px-3 py-2 text-xs text-yellow-200">
        <AlertTriangle className="mt-0.5 h-3.5 w-3.5 flex-none" />
        <div>
          No LLM configured for <code className="font-mono">{workspace}</code>.
          Set <code className="font-mono">ANTHROPIC_API_KEY</code> (or your
          provider's key) and restart, or run{" "}
          <code className="font-mono">root setup</code> in the workspace
          directory. Without a provider, answers fall back to the
          highest-confidence claim verbatim.
        </div>
      </div>
    );
  }
  if (health.claim_count === 0) {
    return (
      <div className="flex w-full items-start gap-2 rounded-md border border-blue-500/40 bg-blue-500/10 px-3 py-2 text-xs text-blue-100">
        <AlertTriangle className="mt-0.5 h-3.5 w-3.5 flex-none" />
        <div>
          No compiled claims in <code className="font-mono">{workspace}</code>{" "}
          yet. The {health.provider} {health.model} model is wired, but
          there's nothing to ground answers against — drop sources into the
          workspace and run <code className="font-mono">root compile</code>.
        </div>
      </div>
    );
  }
  return null;
}

function ChatHeader({
  workspace,
  convTitle,
}: {
  workspace: string;
  convTitle: string;
}) {
  return (
    <header className="flex h-11 shrink-0 items-center gap-2 border-b border-border px-4">
      <Folder className="size-4 text-muted-foreground" />
      <span className="text-sm font-medium">{workspace}</span>
      {convTitle && (
        <>
          <span className="text-muted-foreground">·</span>
          <span className="text-xs text-muted-foreground">{convTitle}</span>
        </>
      )}
    </header>
  );
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
      <div className="relative h-6 w-6 animate-scale-pulse">
        <img
          src="/logo_white.png"
          alt="Thinking"
          className="h-full w-full object-contain opacity-80"
          style={{
            filter: 'drop-shadow(0 0 8px hsl(var(--accent) / 0.5))'
          }}
        />
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

  return (
    <div className={cn("flex w-full", isUser && "justify-end")}>
      <div
        className={cn(
          "max-w-2xl whitespace-pre-wrap break-words rounded-2xl px-4 py-3 text-sm transition-all duration-300",
          isUser
            ? "bg-accent/15 text-foreground"
            : "bg-surface text-foreground shadow-sm border border-border/40",
          pending && "opacity-90"
        )}
      >
        {msg.body}
        {pending && !isUser && (
          <span className="ml-1 inline-block h-3.5 w-1 translate-y-0.5 bg-accent/60 animate-pulse" />
        )}
      </div>
    </div>
  );
}

function Composer({
  workspace,
  convId,
  disabled,
  autoFocus,
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
    <div className="px-4 py-4">
      <div className="mx-auto max-w-3xl">
        <div className="group relative flex flex-col gap-2 rounded-3xl border border-border/60 bg-muted/40 px-4 pt-3 pb-2 transition-colors focus-within:border-border">
          {slashQuery && (
            <SlashAutocomplete
              query={slashQuery}
              onSelect={(insertion) => {
                setText(insertion);
                setSlashDismissed(false);
                requestAnimationFrame(() => textareaRef.current?.focus());
              }}
              onDismiss={() => setSlashDismissed(true)}
            />
          )}
          <textarea
            ref={textareaRef}
            value={text}
            onChange={(e) => setText(e.target.value)}
            placeholder={
              disabled
                ? "Generating…"
                : "Ask anything · / for commands · @ to mention"
            }
            rows={1}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                void send();
              }
            }}
            className="w-full resize-none border-0 bg-transparent text-[14px] leading-6 text-foreground placeholder:text-muted-foreground/50 outline-none ring-0 shadow-none appearance-none focus:outline-none focus:ring-0 focus-visible:outline-none focus-visible:ring-0 focus-visible:ring-offset-0"
          />
          <div className="flex items-center justify-end -mr-2">
            {disabled ? (
              <Button
                variant="ghost"
                size="icon"
                onClick={onCancel}
                className="h-8 w-8 rounded-full hover:bg-destructive/10 hover:text-destructive"
              >
                <Square className="size-3.5 fill-current" />
              </Button>
            ) : (
              <Button
                size="icon"
                disabled={!text.trim() || busy}
                onClick={() => void send()}
                className="h-8 w-8 rounded-full bg-foreground text-background shadow-none transition-transform hover:scale-105 active:scale-95 disabled:bg-muted disabled:text-muted-foreground/50"
              >
                <ArrowUp className="size-4" />
              </Button>
            )}
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
