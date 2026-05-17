// apps/thinkingroot-desktop/ui/src/components/permissions/PermissionPromptDialog.tsx
//
// Phase D Wave 1 (2026-05-17) — permission-aware approval modal
// for the 10 system-power tools (file_read, file_write, file_edit,
// glob, grep, shell_exec, clipboard_*, open_in_default, trash).
//
// Wire path:
//   - Backend emits an `approval_requested` SSE event carrying
//     `permission_context` (rest.rs::build_permission_context_for_tool).
//   - commands/chat.rs decodes the event, includes `permission_context`
//     on the Tauri `ChatEvent::ApprovalRequested` payload.
//   - ChatView listens for `approval_requested` events and renders
//     this dialog when `permission_context` is present.
//   - User clicks Allow once / Allow always / Deny once / Deny always.
//   - Component calls `chatApprove` with optional `persistRule`.
//
// Architectural invariants enforced by this component:
//   - When `permission_context.default_deny_matched === true`, the
//     UI MUST hide the Allow buttons. The path is protected by
//     ThinkingRoot's hardcoded security policy and the backend will
//     reject any `allow_*` rule that overlaps DEFAULT_DENY anyway —
//     hiding the buttons here is honest UX, not just a safety net.
//   - "Once" decisions omit `persistRule`; "Always" decisions
//     attach it. The backend persists to permissions.toml BEFORE
//     resolving the oneshot, so the next turn's PermissionsGate
//     sees the new rule immediately.

import { useCallback, useEffect, useRef, useState } from "react";
import { AlertTriangle, Check, Loader2, Lock, Shield, X } from "lucide-react";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { toast } from "@/store/toast";
import {
  chatApprove,
  type PermissionContext,
  type PersistRule,
} from "@/lib/tauri";

interface Props {
  workspace: string;
  toolUseId: string;
  toolName: string;
  toolInput: unknown;
  permissionContext: PermissionContext;
  /** Called after the user has acted (approve/deny) — parent should
   *  unmount the dialog. */
  onResolved: () => void;
}

type ActionKind = "allow_once" | "allow_always" | "deny_once" | "deny_always";

export function PermissionPromptDialog({
  workspace,
  toolUseId,
  toolName,
  toolInput,
  permissionContext,
  onResolved,
}: Props) {
  const [pending, setPending] = useState<ActionKind | null>(null);
  const closeBtnRef = useRef<HTMLButtonElement | null>(null);
  const defaultDenyMatched = permissionContext.default_deny_matched === true;

  // Focus close on open; ESC defaults to "deny_once" (fail-safe).
  useEffect(() => {
    closeBtnRef.current?.focus();
    function onKey(ev: KeyboardEvent) {
      if (ev.key === "Escape" && pending == null) {
        void resolve("deny_once");
      }
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [pending]);

  const resolve = useCallback(
    async (kind: ActionKind) => {
      if (pending != null) return;
      setPending(kind);
      const isAllow = kind === "allow_once" || kind === "allow_always";
      const isAlways = kind === "allow_always" || kind === "deny_always";

      const persistRule: PersistRule | undefined =
        isAlways && permissionContext.suggested_pattern
          ? {
              kind: toolName === "shell_exec" ? "command" : "path",
              pattern:
                toolName === "shell_exec" && permissionContext.command
                  ? deriveCommandPattern(permissionContext.command)
                  : permissionContext.suggested_pattern,
              decision: isAllow ? "allow" : "deny",
            }
          : undefined;

      try {
        await chatApprove({
          workspace,
          toolUseId,
          approve: isAllow,
          reason: !isAllow ? "user denied via permission prompt" : undefined,
          persistRule,
        });
        if (isAlways && persistRule) {
          toast(
            `Rule persisted: ${persistRule.decision} ${persistRule.pattern}`,
            { kind: "info" },
          );
        }
      } catch (e) {
        toast("Permission decision failed", {
          kind: "error",
          body: e instanceof Error ? e.message : String(e),
        });
      } finally {
        setPending(null);
        onResolved();
      }
    },
    [
      pending,
      permissionContext.suggested_pattern,
      permissionContext.command,
      toolName,
      workspace,
      toolUseId,
      onResolved,
    ],
  );

  const subject =
    permissionContext.canonical_path ??
    permissionContext.raw_path ??
    permissionContext.command ??
    JSON.stringify(toolInput);

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label={`Permission required for ${toolName}`}
      className="fixed inset-0 z-[60] flex items-center justify-center bg-background/70 backdrop-blur-sm"
      onClick={(e) => {
        if (e.target === e.currentTarget && pending == null) {
          void resolve("deny_once");
        }
      }}
    >
      <div className="flex w-full max-w-lg flex-col overflow-hidden rounded-xl border border-border bg-surface-elevated shadow-elevated">
        <header className="flex items-start gap-3 border-b border-border/60 px-5 py-3.5">
          <div
            className={cn(
              "flex size-8 shrink-0 items-center justify-center rounded-md",
              defaultDenyMatched
                ? "bg-destructive/15 text-destructive"
                : "bg-primary/12 text-primary",
            )}
          >
            {defaultDenyMatched ? (
              <Shield className="size-4" aria-hidden />
            ) : (
              <Lock className="size-4" aria-hidden />
            )}
          </div>
          <div className="min-w-0 flex-1">
            <h3 className="text-sm font-medium tracking-tight">
              {defaultDenyMatched
                ? `Blocked by security policy: ${toolName}`
                : `Allow ${toolName}?`}
            </h3>
            <p className="mt-0.5 text-[11px] text-muted-foreground">
              The AI wants to{" "}
              <span className="font-medium text-foreground">{toolName}</span>{" "}
              on:
            </p>
          </div>
          <button
            ref={closeBtnRef}
            type="button"
            disabled={pending != null}
            onClick={() => void resolve("deny_once")}
            className="rounded-md p-1.5 text-muted-foreground transition-colors hover:bg-muted/45 hover:text-foreground focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring/55"
            aria-label="Deny once and close"
          >
            <X className="size-4" aria-hidden />
          </button>
        </header>

        <div className="space-y-3 px-5 py-3.5">
          <div className="rounded-md border border-border/55 bg-background/40 px-3 py-2 font-mono text-[11px] leading-relaxed text-foreground">
            <span className="break-all">{subject}</span>
          </div>

          {defaultDenyMatched && (
            <div className="rounded-md border border-destructive/30 bg-destructive/8 px-3 py-2 text-[11px] leading-relaxed text-destructive">
              <div className="flex items-start gap-2">
                <AlertTriangle className="size-3.5 shrink-0" aria-hidden />
                <p>
                  This path is protected by ThinkingRoot's hardcoded security
                  policy (e.g. SSH keys, AWS credentials, browser profiles,
                  <code className="mx-0.5 rounded bg-destructive/15 px-1 py-0.5 text-[10px]">
                    .env
                  </code>{" "}
                  files). No user click can override this rule.
                </p>
              </div>
            </div>
          )}

          {permissionContext.suggested_pattern && !defaultDenyMatched && (
            <p className="text-[11px] leading-relaxed text-muted-foreground">
              "Always" decisions persist a rule for{" "}
              <code className="rounded bg-muted/45 px-1 py-0.5 font-mono text-[10px] text-foreground">
                {permissionContext.suggested_pattern}
              </code>{" "}
              under your user-level{" "}
              <code className="rounded bg-muted/45 px-1 py-0.5 font-mono text-[10px] text-foreground">
                permissions.toml
              </code>
              .
            </p>
          )}
        </div>

        <footer className="flex flex-wrap items-center justify-end gap-1.5 border-t border-border/60 bg-background/35 px-5 py-2.5">
          {!defaultDenyMatched && (
            <>
              <Button
                type="button"
                size="sm"
                variant="default"
                disabled={pending != null}
                onClick={() => void resolve("allow_once")}
              >
                {pending === "allow_once" ? (
                  <Loader2 className="size-3 animate-spin" aria-hidden />
                ) : (
                  <Check className="size-3" aria-hidden />
                )}
                <span className="ml-1 text-[11px]">Allow once</span>
              </Button>
              {permissionContext.suggested_pattern && (
                <Button
                  type="button"
                  size="sm"
                  variant="default"
                  disabled={pending != null}
                  onClick={() => void resolve("allow_always")}
                >
                  {pending === "allow_always" ? (
                    <Loader2 className="size-3 animate-spin" aria-hidden />
                  ) : (
                    <Check className="size-3" aria-hidden />
                  )}
                  <span className="ml-1 text-[11px]">
                    Allow always for{" "}
                    <code className="font-mono">
                      {permissionContext.suggested_pattern}
                    </code>
                  </span>
                </Button>
              )}
            </>
          )}
          <Button
            type="button"
            size="sm"
            variant="ghost"
            disabled={pending != null}
            onClick={() => void resolve("deny_once")}
          >
            {pending === "deny_once" ? (
              <Loader2 className="size-3 animate-spin" aria-hidden />
            ) : (
              <X className="size-3" aria-hidden />
            )}
            <span className="ml-1 text-[11px]">Deny once</span>
          </Button>
          {permissionContext.suggested_pattern && (
            <Button
              type="button"
              size="sm"
              variant="ghost"
              disabled={pending != null}
              onClick={() => void resolve("deny_always")}
            >
              {pending === "deny_always" ? (
                <Loader2 className="size-3 animate-spin" aria-hidden />
              ) : (
                <X className="size-3" aria-hidden />
              )}
              <span className="ml-1 text-[11px]">
                Deny always for{" "}
                <code className="font-mono">
                  {permissionContext.suggested_pattern}
                </code>
              </span>
            </Button>
          )}
        </footer>
      </div>
    </div>
  );
}

/**
 * For shell_exec, take the user's full command line and produce a
 * glob-friendly pattern. Example: `git push origin main` →
 * `git push *`. Conservative: only keeps the first 2 tokens
 * literal and replaces the rest with `*`. The user can always edit
 * the persisted rule by hand in `permissions.toml` if they want
 * different scoping.
 */
function deriveCommandPattern(command: string): string {
  const trimmed = command.trim();
  if (trimmed.length === 0) return command;
  const tokens = trimmed.split(/\s+/);
  if (tokens.length <= 1) return tokens[0]!;
  if (tokens.length === 2) return tokens.join(" ");
  return `${tokens[0]} ${tokens[1]} *`;
}
