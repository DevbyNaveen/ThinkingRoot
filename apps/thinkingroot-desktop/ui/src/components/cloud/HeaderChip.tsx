import { useEffect, useState } from "react";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { Cloud, CloudOff, LogIn, Loader2 } from "lucide-react";

import { cn } from "@/lib/utils";
import {
  type AuthState,
  type CloudStatusEventPayload,
  CLOUD_STATUS_EVENT,
  authState,
  cloudLoginStart,
} from "@/lib/tauri";

/**
 * Compact top-strip status chip. Same event-source as CloudPanel, but
 * a click here either kicks off a login (when signed-out / expired)
 * or invokes the supplied `onClick` callback (typically nav to the
 * Cloud settings tab).
 *
 * Spec: docs/superpowers/specs/2026-05-13-oss-cloud-readiness-design.md §7.4.
 */
type ChipState =
  | { kind: "loading" }
  | { kind: "signed_out" }
  | { kind: "logging_in" }
  | { kind: "signed_in"; auth: AuthState }
  | { kind: "auth_expired" };

export function HeaderChip({ onClick }: { onClick?: () => void }) {
  const [state, setState] = useState<ChipState>({ kind: "loading" });

  useEffect(() => {
    let mounted = true;
    let unlisten: UnlistenFn | null = null;

    (async () => {
      try {
        const s = await authState();
        if (!mounted) return;
        setState(s.signed_in ? { kind: "signed_in", auth: s } : { kind: "signed_out" });
      } catch {
        if (mounted) setState({ kind: "signed_out" });
      }

      unlisten = await listen<CloudStatusEventPayload>(CLOUD_STATUS_EVENT, (event) => {
        const p = event.payload;
        if (!mounted) return;
        if (p.status === "signed_in") {
          setState({
            kind: "signed_in",
            auth: {
              signed_in: true,
              handle: p.handle,
              tier: p.tier,
              credits_remaining: p.credits_remaining,
              credits_total: p.credits_total,
              period_end: p.period_end,
              server: "",
            },
          });
        } else if (p.status === "signed_out" || p.status === "login_failed") {
          setState({ kind: "signed_out" });
        } else if (p.status === "logging_in") {
          setState({ kind: "logging_in" });
        } else if (p.status === "auth_expired") {
          setState({ kind: "auth_expired" });
        } else if (p.status === "credits_updated") {
          setState((prev) =>
            prev.kind === "signed_in"
              ? { ...prev, auth: { ...prev.auth, credits_remaining: p.remaining, credits_total: p.total } }
              : prev,
          );
        }
      });
    })();

    return () => {
      mounted = false;
      if (unlisten) unlisten();
    };
  }, []);

  const baseCls =
    "inline-flex items-center gap-1.5 rounded-full border px-2.5 py-1 text-xs whitespace-nowrap";

  if (state.kind === "loading") {
    return (
      <div className={cn(baseCls, "text-muted-foreground")}>
        <Loader2 className="h-3 w-3 animate-spin" />
      </div>
    );
  }
  if (state.kind === "logging_in") {
    return (
      <div className={cn(baseCls, "text-muted-foreground")}>
        <Loader2 className="h-3 w-3 animate-spin" /> Signing in…
      </div>
    );
  }
  if (state.kind === "signed_out") {
    return (
      <button onClick={() => cloudLoginStart()} className={cn(baseCls, "hover:bg-muted")}>
        <LogIn className="h-3 w-3" /> Sign in
      </button>
    );
  }
  if (state.kind === "auth_expired") {
    return (
      <button onClick={() => cloudLoginStart()} className={cn(baseCls, "border-destructive text-destructive")}>
        <CloudOff className="h-3 w-3" /> Session expired
      </button>
    );
  }
  const a = state.auth;
  return (
    <button onClick={onClick} className={cn(baseCls, "hover:bg-muted")}>
      <Cloud className="h-3 w-3" />
      <span className="font-medium">@{a.handle ?? "?"}</span>
      <span className="text-muted-foreground">· {a.tier ?? "free"}</span>
      {a.credits_remaining != null && (
        <span className="text-muted-foreground">· {compactCredits(a.credits_remaining)}</span>
      )}
    </button>
  );
}

function compactCredits(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(0)}k`;
  return n.toString();
}
