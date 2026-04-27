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
  type LlmHealth,
} from "@/lib/tauri";
import type { ChatMessage } from "@/types";
import { SlashAutocomplete } from "./SlashAutocomplete";
import { runSlashCommand } from "./slashCommands";

export function ChatView() {
  const activeWorkspace = useApp((s) => s.activeWorkspace);
  const activeConv = useApp((s) => s.activeConversationId);
  const setActiveConv = useApp((s) => s.setActiveConversationId);
  const messagesByKey = useApp((s) => s.messages);
  const appendMessage = useApp((s) => s.appendMessage);
  const updateMessage = useApp((s) => s.updateMessage);
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
  useEffect(() => {
    if (!activeWorkspace || !activeConv) return;
    let cancelled = false;
    (async () => {
      try {
        const c = await conversationsGet(activeWorkspace, activeConv);
        if (cancelled) return;
        setMessages(
          activeWorkspace,
          activeConv,
          c.messages.map((m) => ({
            id: m.id,
            kind: m.role === "user" ? "user" : "assistant",
            body: m.content,
            at: new Date(m.created_at),
          })),
        );
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

  // Each turn is tagged with the workspace + conversation it
  // belongs to at send time. Streaming events are routed by *that*
  // tag, not by the user's current selection — so navigating away
  // mid-stream still persists the assistant reply to the correct
  // conversation on disk and never pollutes the new one.
  const turnCtxRef = useRef<
    Map<string, { workspace: string; convId: string }>
  >(new Map());

  // Wire the streaming events once.
  useEffect(() => {
    let unlisten: (() => void) | null = null;
    let running = true;
    onChatEvent((ev: ChatEvent) => {
      if (!running) return;
      const ctx = turnCtxRef.current.get(ev.turn_id);
      if (!ctx) return;

      const cur = useApp.getState();
      const userIsViewingThisTurn =
        cur.activeWorkspace === ctx.workspace &&
        cur.activeConversationId === ctx.convId;

      if (ev.type === "token") {
        // Visual streaming only when the user is still on this turn.
        if (userIsViewingThisTurn && cur.streaming?.turnId === ev.turn_id) {
          appendDelta(ev.text);
        }
      } else if (ev.type === "final") {
        // Always persist to the original conversation on disk.
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
        // Update UI cache only if the user is still watching.
        if (userIsViewingThisTurn) {
          appendMessage(ctx.workspace, ctx.convId, {
            id: `m-${Date.now()}-a`,
            kind: "assistant",
            body: ev.full_text,
            at: new Date(),
          });
          if (cur.streaming?.turnId === ev.turn_id) {
            setStreaming(null);
          }
        }
        turnCtxRef.current.delete(ev.turn_id);
      } else if (ev.type === "error") {
        if (userIsViewingThisTurn) {
          appendMessage(ctx.workspace, ctx.convId, {
            id: `m-${Date.now()}-e`,
            kind: "assistant",
            body: `⚠️ ${ev.message}`,
            at: new Date(),
          });
          if (cur.streaming?.turnId === ev.turn_id) {
            setStreaming(null);
          }
        }
        turnCtxRef.current.delete(ev.turn_id);
      }
    }).then((u) => {
      unlisten = u;
    });
    return () => {
      running = false;
      unlisten?.();
    };
  }, [appendDelta, appendMessage, setStreaming, updateMessage]);

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
        <div className="flex flex-1 flex-col items-center px-8 pt-[18vh]">
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
                  turnCtxRef.current.set(turnId, { workspace: ws, convId: cid });
                  setStreaming({
                    turnId,
                    partial: "",
                    startedAt: new Date(),
                    tokensIn: 0,
                    tokensOut: 0,
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
            <li>
              <MessageBubble
                msg={{
                  id: streaming.turnId,
                  kind: "assistant",
                  body: streaming.partial,
                  at: streaming.startedAt,
                }}
                pending
              />
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
        onStartTurn={(turnId) => {
          setStreaming({
            turnId,
            partial: "",
            startedAt: new Date(),
            tokensIn: 0,
            tokensOut: 0,
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

function MessageBubble({ msg, pending }: { msg: ChatMessage; pending?: boolean }) {
  const isUser = msg.kind === "user";
  return (
    <div className={cn("flex w-full", isUser && "justify-end")}>
      <div
        className={cn(
          "max-w-2xl whitespace-pre-wrap break-words rounded-2xl px-4 py-3 text-sm",
          isUser
            ? "bg-accent/15 text-foreground"
            : "bg-surface text-foreground",
          pending && "opacity-90",
        )}
      >
        {msg.body || (pending ? "…" : "")}
        {pending && <span className="ml-1 inline-block animate-pulse">▌</span>}
      </div>
    </div>
  );
}

function Composer({
  workspace,
  convId,
  disabled,
  autoFocus,
  onCancel,
  onCreateConvIfNeeded,
  onUserMessage,
  onStartTurn,
}: {
  workspace: string;
  convId: string | null;
  disabled: boolean;
  autoFocus?: boolean;
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
          <div className="flex items-center justify-end">
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
