import { useEffect, useState } from "react";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import {
  CloudOff,
  Download,
  LogIn,
  LogOut,
  Loader2,
  RefreshCw,
  Send,
} from "lucide-react";

import { Button } from "@/components/ui/button";
import {
  type AuthState,
  type CloudStatusEventPayload,
  type PackOpResult,
  CLOUD_STATUS_EVENT,
  authState,
  cloudCreditsPoll,
  cloudLoginStart,
  cloudLogout,
  cloudPullPack,
  cloudPushWorkspace,
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
  const [pushBusy, setPushBusy] = useState(false);
  const [pushResult, setPushResult] = useState<PackOpResult | null>(null);
  const [pullRef, setPullRef] = useState("");
  const [pullBusy, setPullBusy] = useState(false);
  const [pullResult, setPullResult] = useState<PackOpResult | null>(null);

  const onPush = async () => {
    setPushBusy(true);
    setPushResult(null);
    try {
      // Use the desktop's current working directory as the workspace
      // path. The Tauri subprocess inherits the parent process's cwd
      // when no explicit path is passed; "." resolves there.
      const r = await cloudPushWorkspace(".");
      setPushResult(r);
    } catch (e) {
      setPushResult({ success: false, output: "", error: String(e) });
    } finally {
      setPushBusy(false);
    }
  };

  const onPull = async () => {
    if (!pullRef.trim()) return;
    setPullBusy(true);
    setPullResult(null);
    try {
      const r = await cloudPullPack(pullRef.trim());
      setPullResult(r);
    } catch (e) {
      setPullResult({ success: false, output: "", error: String(e) });
    } finally {
      setPullBusy(false);
    }
  };

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

      {/*
       * Packs section — subprocesses into `root push` / `root pull`
       * via the cloud_push_workspace / cloud_pull_pack Tauri
       * commands. Honest UX: stderr is surfaced verbatim on failure;
       * no fabricated "last pushed N ago" timestamps (those need a
       * hub-side endpoint that does not exist yet).
       */}
      <div className="rounded-md border p-3 text-sm space-y-3">
        <p className="font-medium">Packs</p>

        <div className="flex items-center gap-2">
          <Button
            size="sm"
            variant="outline"
            onClick={onPush}
            disabled={pushBusy}
            className="gap-1.5"
          >
            {pushBusy ? (
              <Loader2 className="h-3 w-3 animate-spin" />
            ) : (
              <Send className="h-3 w-3" />
            )}
            Push this workspace
          </Button>
        </div>
        {pushResult && (
          <p
            className={
              pushResult.success
                ? "text-xs text-green-600"
                : "text-xs text-destructive"
            }
          >
            {pushResult.success
              ? "Pushed."
              : `Push failed: ${pushResult.error ?? "unknown"}`}
          </p>
        )}

        <div className="flex items-center gap-2">
          <input
            type="text"
            placeholder="owner/slug or owner/slug@version"
            value={pullRef}
            onChange={(e) => setPullRef(e.target.value)}
            className="flex-1 px-2 py-1 border rounded text-xs"
          />
          <Button
            size="sm"
            variant="outline"
            onClick={onPull}
            disabled={pullBusy || !pullRef.trim()}
            className="gap-1.5"
          >
            {pullBusy ? (
              <Loader2 className="h-3 w-3 animate-spin" />
            ) : (
              <Download className="h-3 w-3" />
            )}
            Pull
          </Button>
        </div>
        {pullResult && (
          <p
            className={
              pullResult.success
                ? "text-xs text-green-600"
                : "text-xs text-destructive"
            }
          >
            {pullResult.success
              ? "Pulled."
              : `Pull failed: ${pullResult.error ?? "unknown"}`}
          </p>
        )}
      </div>
    </section>
  );
}
