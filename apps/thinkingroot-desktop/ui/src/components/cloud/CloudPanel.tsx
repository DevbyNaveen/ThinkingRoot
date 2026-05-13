import { useEffect, useState } from "react";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { CloudOff, LogIn, LogOut, Loader2, RefreshCw } from "lucide-react";

import { Button } from "@/components/ui/button";
import {
  type AuthState,
  type CloudStatusEventPayload,
  CLOUD_STATUS_EVENT,
  authState,
  cloudCreditsPoll,
  cloudLoginStart,
  cloudLogout,
  cloudRefreshMe,
} from "@/lib/tauri";

/**
 * Full Cloud settings panel. Subscribes to `cloud_status_changed` for
 * real-time state transitions and pulls an initial `auth_state` snapshot
 * on mount. Credits are polled every 60s while the tab is visible —
 * pull is silent on failure (the panel still shows last-known values).
 *
 * Honesty: no fabricated "signed in" placeholder. The four explicit
 * states map 1:1 to backend outcomes.
 *
 * Spec: docs/superpowers/specs/2026-05-13-oss-cloud-readiness-design.md §7.3.
 */
type UiState =
  | { kind: "loading" }
  | { kind: "signed_out" }
  | { kind: "logging_in"; manualUrl?: string }
  | { kind: "signed_in"; auth: AuthState }
  | { kind: "auth_expired" };

const POLL_INTERVAL_MS = 60_000;

export function CloudPanel() {
  const [ui, setUi] = useState<UiState>({ kind: "loading" });
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let mounted = true;
    let unlisten: UnlistenFn | null = null;
    let pollHandle: ReturnType<typeof setInterval> | null = null;

    (async () => {
      try {
        const state = await authState();
        if (!mounted) return;
        if (state.signed_in) {
          setUi({ kind: "signed_in", auth: state });
        } else {
          setUi({ kind: "signed_out" });
        }
      } catch (e) {
        if (mounted) setError(String(e));
      }

      unlisten = await listen<CloudStatusEventPayload>(CLOUD_STATUS_EVENT, (event) => {
        const p = event.payload;
        if (!mounted) return;
        if (p.status === "signed_in") {
          setUi({
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
          setError(null);
        } else if (p.status === "signed_out") {
          setUi({ kind: "signed_out" });
        } else if (p.status === "logging_in") {
          setUi({ kind: "logging_in", manualUrl: p.manual_url });
        } else if (p.status === "login_failed") {
          setUi({ kind: "signed_out" });
          setError(`Login failed: ${p.reason}${p.detail ? ` — ${p.detail}` : ""}`);
        } else if (p.status === "auth_expired") {
          setUi({ kind: "auth_expired" });
        } else if (p.status === "credits_updated") {
          setUi((prev) =>
            prev.kind === "signed_in"
              ? {
                  ...prev,
                  auth: {
                    ...prev.auth,
                    credits_remaining: p.remaining,
                    credits_total: p.total,
                  },
                }
              : prev,
          );
        }
      });
    })();

    const startPoll = () => {
      pollHandle = setInterval(() => {
        if (document.visibilityState !== "visible") return;
        cloudCreditsPoll().catch(() => {
          // honest: silent — UI shows last-known via existing state
        });
      }, POLL_INTERVAL_MS);
    };
    startPoll();

    return () => {
      mounted = false;
      if (unlisten) unlisten();
      if (pollHandle) clearInterval(pollHandle);
    };
  }, []);

  if (ui.kind === "loading") {
    return (
      <div className="flex items-center gap-2 text-muted-foreground">
        <Loader2 className="h-4 w-4 animate-spin" /> Loading auth state…
      </div>
    );
  }

  if (ui.kind === "signed_out") {
    return (
      <section className="space-y-4">
        <h2 className="text-lg font-semibold">Cloud</h2>
        <p className="text-sm text-muted-foreground">
          Sign in to push packs to the hub, use managed models, and compile on cloud GPUs.
        </p>
        {error && (
          <p className="text-sm text-destructive" role="alert">
            {error}
          </p>
        )}
        <Button onClick={() => cloudLoginStart()} className="gap-2">
          <LogIn className="h-4 w-4" /> Sign in to ThinkingRoot Cloud
        </Button>
      </section>
    );
  }

  if (ui.kind === "logging_in") {
    return (
      <section className="space-y-3">
        <div className="flex items-center gap-2 text-sm text-muted-foreground">
          <Loader2 className="h-4 w-4 animate-spin" /> Waiting for browser
          callback…
        </div>
        {ui.manualUrl && (
          <p className="text-xs">
            If the browser did not open,{" "}
            <a className="underline" href={ui.manualUrl}>
              click here
            </a>
            .
          </p>
        )}
      </section>
    );
  }

  if (ui.kind === "auth_expired") {
    return (
      <section className="space-y-4">
        <div className="flex items-center gap-2 text-sm text-destructive">
          <CloudOff className="h-4 w-4" /> Session expired
        </div>
        <Button onClick={() => cloudLoginStart()}>Sign in again</Button>
      </section>
    );
  }

  const a = ui.auth;
  return (
    <section className="space-y-4">
      <h2 className="text-lg font-semibold">Cloud</h2>
      <div className="rounded-md border p-3 text-sm space-y-1">
        <p>
          <span className="font-medium">@{a.handle ?? "?"}</span> ·{" "}
          <span className="text-muted-foreground">{a.tier ?? "free"} tier</span>
        </p>
        {a.credits_remaining != null && a.credits_total != null && (
          <p>
            {a.credits_remaining.toLocaleString()} / {a.credits_total.toLocaleString()} credits
            {a.period_end && (
              <span className="text-muted-foreground"> · resets {a.period_end.slice(0, 10)}</span>
            )}
          </p>
        )}
        {a.token_redacted && (
          <p className="text-xs text-muted-foreground">Token: {a.token_redacted}</p>
        )}
      </div>
      <div className="flex gap-2">
        <Button variant="outline" size="sm" onClick={() => cloudRefreshMe().catch(() => {})} className="gap-2">
          <RefreshCw className="h-3 w-3" /> Refresh
        </Button>
        <Button variant="outline" size="sm" onClick={() => cloudLogout()} className="gap-2">
          <LogOut className="h-3 w-3" /> Sign out
        </Button>
      </div>
    </section>
  );
}
