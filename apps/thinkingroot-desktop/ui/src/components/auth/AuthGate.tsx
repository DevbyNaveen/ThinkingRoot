import { useEffect, useRef, useState, type ReactNode } from "react";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { openUrl } from "@tauri-apps/plugin-opener";
import { Loader2 } from "lucide-react";

import { Button } from "@/components/ui/button";
import { ThinkingRootGlyph } from "@/components/shell/ThinkingRootGlyph";
import {
  authState,
  cloudLoginStart,
  CLOUD_STATUS_EVENT,
  type CloudStatusEventPayload,
} from "@/lib/tauri";
import { useApp } from "@/store/app";
import { cn } from "@/lib/utils";

interface AuthGateProps {
  children: ReactNode;
}

/**
 * Hybrid welcome gate — first launch only. Once the user picks local
 * BYOK or signs in, in-dashboard sign-in is handled by Sidebar /
 * HeaderChip and must not be hijacked here.
 */
export function AuthGate({ children }: AuthGateProps) {
  const skippedCloudSignIn = useApp((s) => s.skippedCloudSignIn);
  const setSkippedCloudSignIn = useApp((s) => s.setSkippedCloudSignIn);
  const showWelcomeScreen = useApp((s) => s.showWelcomeScreen);
  const setShowWelcomeScreen = useApp((s) => s.setShowWelcomeScreen);
  const skippedRef = useRef(skippedCloudSignIn);
  skippedRef.current = skippedCloudSignIn;

  const [storeHydrated, setStoreHydrated] = useState(() =>
    useApp.persist.hasHydrated(),
  );
  const [booting, setBooting] = useState(true);
  const [signedIn, setSignedIn] = useState(false);
  const signedInRef = useRef(false);
  signedInRef.current = signedIn;

  const [welcomeBusy, setWelcomeBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [manualUrl, setManualUrl] = useState<string | null>(null);

  useEffect(() => {
    if (useApp.persist.hasHydrated()) {
      setStoreHydrated(true);
      return;
    }
    return useApp.persist.onFinishHydration(() => setStoreHydrated(true));
  }, []);

  // Dev / QA: http://localhost:1420/?welcome=1
  useEffect(() => {
    const params = new URLSearchParams(window.location.search);
    if (params.get("welcome") === "1") {
      setSkippedCloudSignIn(false);
      setShowWelcomeScreen(true);
    }
  }, [setSkippedCloudSignIn, setShowWelcomeScreen]);

  useEffect(() => {
    let mounted = true;
    let unlisten: UnlistenFn | null = null;

    (async () => {
      try {
        const state = await authState();
        if (!mounted) return;
        setSignedIn(state.signed_in);
      } catch (e) {
        if (mounted) {
          setError(e instanceof Error ? e.message : String(e));
        }
      } finally {
        if (mounted) setBooting(false);
      }

      unlisten = await listen<CloudStatusEventPayload>(CLOUD_STATUS_EVENT, (event) => {
        if (!mounted) return;
        const p = event.payload;

        if (p.status === "signed_in") {
          setSignedIn(true);
          setShowWelcomeScreen(false);
          setWelcomeBusy(false);
          setManualUrl(null);
          setError(null);
          return;
        }

        if (p.status === "signed_out") {
          setSignedIn(false);
          return;
        }

        // Welcome-screen only — never take over the dashboard after BYOK skip.
        const onWelcome =
          !skippedRef.current && !signedInRef.current;
        if (!onWelcome) return;

        if (p.status === "logging_in") {
          setWelcomeBusy(true);
          setManualUrl(p.manual_url ?? null);
          setError(null);
        } else if (p.status === "login_failed") {
          setWelcomeBusy(false);
          setManualUrl(null);
          setError(p.detail ?? `Sign-in failed (${p.reason})`);
        }
      });
    })();

    return () => {
      mounted = false;
      unlisten?.();
    };
  }, [setShowWelcomeScreen]);

  const showWelcome =
    storeHydrated &&
    !booting &&
    (showWelcomeScreen || (!signedIn && !skippedCloudSignIn));

  const onLogIn = async () => {
    setError(null);
    setWelcomeBusy(true);
    try {
      await cloudLoginStart();
    } catch (e) {
      setWelcomeBusy(false);
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const onSignUp = async () => {
    setError(null);
    try {
      const state = await authState();
      const base = state.server.replace(/\/$/, "");
      await openUrl(`${base}/signup`);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const onContinueLocal = () => {
    setSkippedCloudSignIn(true);
    setShowWelcomeScreen(false);
  };

  if (!showWelcome) {
    return !storeHydrated || booting ? null : <>{children}</>;
  }

  return (
    <div className="fixed inset-0 z-[70] flex flex-col items-center justify-center bg-background px-6">
      <div className="flex w-full max-w-[300px] flex-col items-center text-center">
        <ThinkingRootGlyph className="mb-6 size-16" aria-hidden />
        <h1 className="text-[22px] font-semibold tracking-[0.2em] text-foreground">
          THINKINGROOT
        </h1>
        <p className="mt-2 text-sm text-muted-foreground">
          Byte-grounded knowledge for AI agents
        </p>

        <div className="mt-10 flex w-full flex-col gap-2.5">
          {welcomeBusy ? (
            <div className="flex h-10 w-full items-center justify-center gap-2 rounded-md bg-accent/90 text-sm font-medium text-accent-foreground">
              <Loader2 className="size-4 animate-spin" />
              Waiting for browser…
            </div>
          ) : (
            <>
              <Button
                size="lg"
                className="h-10 w-full rounded-md bg-[hsl(213,94%,68%)] text-sm font-medium text-[hsl(220,20%,10%)] hover:bg-[hsl(213,94%,62%)]"
                onClick={() => void onLogIn()}
              >
                Log In
              </Button>
              <Button
                size="lg"
                variant="secondary"
                className="h-10 w-full rounded-md bg-muted/80 text-sm font-medium text-foreground hover:bg-muted"
                onClick={() => void onSignUp()}
              >
                Sign Up
              </Button>
            </>
          )}

          <button
            type="button"
            onClick={onContinueLocal}
            disabled={welcomeBusy}
            className={cn(
              "mt-2 text-xs text-muted-foreground underline-offset-4 transition-colors hover:text-foreground hover:underline",
              welcomeBusy && "pointer-events-none opacity-50",
            )}
          >
            Continue with local key
          </button>
        </div>

        {manualUrl && (
          <p className="mt-4 text-xs text-muted-foreground">
            Browser did not open?{" "}
            <a className="underline" href={manualUrl}>
              Continue here
            </a>
            .
          </p>
        )}

        {error && (
          <p className="mt-4 text-xs text-destructive" role="alert">
            {error}
          </p>
        )}
      </div>
    </div>
  );
}
