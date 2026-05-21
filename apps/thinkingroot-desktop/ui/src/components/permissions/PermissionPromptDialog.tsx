// Permission approval sheet — expands upward from the chat composer
// (Cursor-style), not a centered modal with backdrop blur.

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

export interface PermissionPromptProps {
  workspace: string;
  toolUseId: string;
  toolName: string;
  toolInput: unknown;
  permissionContext: PermissionContext;
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
  variant = "session",
}: PermissionPromptProps & {
  /** Match the active composer shell (bottom session vs idle centered). */
  variant?: "session" | "idle";
}) {
  const [pending, setPending] = useState<ActionKind | null>(null);
  const panelRef = useRef<HTMLDivElement | null>(null);
  const defaultDenyMatched = permissionContext.default_deny_matched === true;

  useEffect(() => {
    panelRef.current?.focus();
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
      ref={panelRef}
      role="region"
      aria-label={`Permission required for ${toolName}`}
      tabIndex={-1}
      className={cn(
        "permission-sheet-enter flex w-full max-h-[min(42vh,320px)] flex-col overflow-hidden border border-b-0",
        "shadow-[0_-4px_24px_-8px_rgba(0,0,0,0.35)]",
        variant === "idle"
          ? "rounded-t-xl border-border/60 bg-surface-elevated"
          : "rounded-t-[26px] border-white/[0.1] bg-[hsl(0,0%,13.5%)]",
      )}
    >
        <div
          className={cn(
            "flex shrink-0 items-start gap-2.5 border-b px-3 py-2.5",
            variant === "idle" ? "border-border/50" : "border-white/[0.08]",
          )}
        >
        <div
          className={cn(
            "flex size-7 shrink-0 items-center justify-center rounded-md",
            defaultDenyMatched
              ? "bg-destructive/15 text-destructive"
              : "bg-warn/15 text-warn",
          )}
        >
          {defaultDenyMatched ? (
            <Shield className="size-3.5" aria-hidden />
          ) : (
            <Lock className="size-3.5" aria-hidden />
          )}
        </div>
        <div className="min-w-0 flex-1">
          <p className="text-[13px] font-medium leading-snug text-foreground">
            {defaultDenyMatched
              ? `Blocked: ${toolName}`
              : `Allow ${toolName}?`}
          </p>
          <p
            className="mt-0.5 truncate font-mono text-[11px] text-muted-foreground"
            title={subject}
          >
            {subject}
          </p>
        </div>
        <button
          type="button"
          disabled={pending != null}
          onClick={() => void resolve("deny_once")}
          className="shrink-0 rounded-md p-1 text-muted-foreground transition-colors hover:bg-muted/40 hover:text-foreground"
          aria-label="Deny once"
        >
          <X className="size-3.5" aria-hidden />
        </button>
      </div>

      {defaultDenyMatched && (
        <div
          className={cn(
            "flex items-start gap-2 border-b px-3 py-2 text-[11px] leading-relaxed text-destructive",
            variant === "idle" ? "border-border/40" : "border-white/[0.06]",
          )}
        >
          <AlertTriangle className="mt-0.5 size-3 shrink-0" aria-hidden />
          <p>
            Protected by security policy — cannot be overridden.
          </p>
        </div>
      )}

      {permissionContext.suggested_pattern && !defaultDenyMatched && (
        <p
          className={cn(
            "border-b px-3 py-1.5 text-[10px] leading-relaxed text-muted-foreground",
            variant === "idle" ? "border-border/40" : "border-white/[0.06]",
          )}
        >
          &quot;Always&quot; saves{" "}
          <code className="font-mono text-foreground/80">
            {permissionContext.suggested_pattern}
          </code>{" "}
          to permissions.toml
        </p>
      )}

      <div className="flex shrink-0 flex-wrap items-center gap-1.5 overflow-y-auto px-3 py-2">
        {!defaultDenyMatched && (
          <>
            <Button
              type="button"
              size="sm"
              variant="default"
              disabled={pending != null}
              className="h-7 text-[11px]"
              onClick={() => void resolve("allow_once")}
            >
              {pending === "allow_once" ? (
                <Loader2 className="size-3 animate-spin" aria-hidden />
              ) : (
                <Check className="size-3" aria-hidden />
              )}
              <span className="ml-1">Allow once</span>
            </Button>
            {permissionContext.suggested_pattern && (
              <Button
                type="button"
                size="sm"
                variant="secondary"
                disabled={pending != null}
                className="h-7 max-w-[14rem] truncate text-[11px]"
                title={`Allow always for ${permissionContext.suggested_pattern}`}
                onClick={() => void resolve("allow_always")}
              >
                {pending === "allow_always" ? (
                  <Loader2 className="size-3 shrink-0 animate-spin" aria-hidden />
                ) : (
                  <Check className="size-3 shrink-0" aria-hidden />
                )}
                <span className="ml-1 truncate">Always allow</span>
              </Button>
            )}
          </>
        )}
        <Button
          type="button"
          size="sm"
          variant="ghost"
          disabled={pending != null}
          className="h-7 text-[11px]"
          onClick={() => void resolve("deny_once")}
        >
          {pending === "deny_once" ? (
            <Loader2 className="size-3 animate-spin" aria-hidden />
          ) : (
            <X className="size-3" aria-hidden />
          )}
          <span className="ml-1">Deny</span>
        </Button>
        {permissionContext.suggested_pattern && (
          <Button
            type="button"
            size="sm"
            variant="ghost"
            disabled={pending != null}
            className="h-7 max-w-[12rem] truncate text-[11px]"
            title={`Deny always for ${permissionContext.suggested_pattern}`}
            onClick={() => void resolve("deny_always")}
          >
            {pending === "deny_always" ? (
              <Loader2 className="size-3 shrink-0 animate-spin" aria-hidden />
            ) : null}
            <span className="ml-1 truncate">Always deny</span>
          </Button>
        )}
      </div>
    </div>
  );
}

function deriveCommandPattern(command: string): string {
  const trimmed = command.trim();
  if (trimmed.length === 0) return command;
  const tokens = trimmed.split(/\s+/);
  if (tokens.length <= 1) return tokens[0]!;
  if (tokens.length === 2) return tokens.join(" ");
  return `${tokens[0]} ${tokens[1]} *`;
}
