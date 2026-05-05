import { useEffect, useState } from "react";
import { Zap, DollarSign, Command, Folder, Loader2, Cloud, CloudOff } from "lucide-react";
import { useApp } from "@/store/app";
import { formatCost, formatTokens } from "@/lib/utils";
import { Button } from "@/components/ui/button";
import { authState, type AuthState } from "@/lib/tauri";

/**
 * Bottom status bar.  Config-driven segments — each segment declares
 * its own visibility predicate so the bar adapts to signed-in / signed-
 * out state without hardcoding mock data.
 *
 * Every segment is grounded in a real engine signal:
 *   • sidecar status — local agent runtime liveness (handled at chat-
 *     command boundary today, surfaced as a static "local sidecar"
 *     label here until B1 ships per-segment ping support)
 *   • active workspace — the user's currently-selected workspace
 *   • compile progress — when a compile is active, shows phase + percent
 *   • cost / tokens — local accumulators tracking the user's BYOC spend
 *   • trust filter — the Brain-view trust filter
 *   • cloud — whether the user has run `tr login`; surfaces handle
 *     when signed in, "Local only" otherwise
 *
 * Two segments from the original §6.5 spec are deliberately NOT
 * included until their backends ship:
 *
 *   ✗ "credits: N / cloud" — there is no cloud chat path live today
 *     (B1 is blocked on a cross-repo wire-shape gap; see plan).
 *     Showing a credits balance would be fake data per CLAUDE.md
 *     honesty rule #1.
 *
 *   ✗ "sync ✓ 2m ago" — `apps/.../commands/auth.rs:67-76` explicitly
 *     declines to fabricate a sync timestamp because the cloud has
 *     no conversations service yet ("storage.cloud = false until the
 *     route ships").  Honesty rule #6 forbids the placeholder.
 *
 * Both will land when their backends do; the segment slots are
 * reserved in the config array below as commented-out entries so the
 * order is stable when they ship.
 */
export function StatusBar() {
  const totalCost = useApp((s) => s.totalCostUsd);
  const totalIn = useApp((s) => s.totalTokensIn);
  const totalOut = useApp((s) => s.totalTokensOut);
  const trust = useApp((s) => s.trust);
  const activeWorkspace = useApp((s) => s.activeWorkspace);
  const compileProgress = useApp((s) => s.compileProgress);
  const openCmd = useApp((s) => s.setCommandPaletteOpen);

  // Cloud sign-in status — fetched once on mount.  Re-fetches on
  // window focus so the bar updates after a `tr login` from another
  // shell without requiring an app restart.
  const [auth, setAuth] = useState<AuthState | null>(null);
  useEffect(() => {
    let alive = true;
    const refresh = () => {
      authState()
        .then((a) => {
          if (alive) setAuth(a);
        })
        .catch(() => {
          // auth_state should never fail (it's a pure read of
          // desktop.toml); if it does, fall through to "no auth state"
          // rather than crash the bar.
          if (alive) setAuth(null);
        });
    };
    refresh();
    window.addEventListener("focus", refresh);
    return () => {
      alive = false;
      window.removeEventListener("focus", refresh);
    };
  }, []);

  // Compile-progress label: empty when no compile is active OR when
  // the pipeline has finished (done/failed/cancelled), so the segment
  // hides itself between runs.  The discriminator is `phase: string`
  // — the union shape is in `lib/tauri.ts::CompileProgress`.
  const compileLabel = (() => {
    if (!compileProgress) return "";
    const p = compileProgress;
    switch (p.phase) {
      case "done":
      case "failed":
      case "cancelled":
        return "";
      case "booting":
        return "compile: waiting for engine…";
      case "started":
        return "compile: starting…";
      case "parse_complete":
        return `compile: parsed ${p.files} files`;
      case "extraction_progress":
        return `compile: extracting (${p.done}/${p.total})`;
      case "extraction_start":
        return `compile: extracting (0/${p.total_chunks})`;
      case "extraction_complete":
        return `compile: extracted ${p.claims} claims`;
      case "grounding_progress":
        return `compile: grounding (${p.done}/${p.total})`;
      case "linking_start":
        return `compile: linking ${p.total_entities} entities`;
      case "linking_progress":
        return `compile: linking (${p.done}/${p.total})`;
      case "vector_progress":
        return `compile: vectoring (${p.done}/${p.total})`;
      default:
        return "compile: running";
    }
  })();
  const compileIsError =
    compileProgress?.phase === "failed";

  return (
    <footer
      role="contentinfo"
      aria-label="status"
      className="flex h-7 shrink-0 items-center justify-between gap-3 border-t border-border bg-surface px-3 text-[11px] text-muted-foreground"
    >
      <div className="flex items-center gap-4">
        {/* 1. Sidecar status — static label until per-tick liveness ping ships */}
        <Segment Icon={Zap} label="local sidecar" />

        {/* 2. Active workspace */}
        {activeWorkspace ? (
          <Segment Icon={Folder} label={activeWorkspace} />
        ) : null}

        {/* 3. Compile progress — only when a compile is active */}
        {compileLabel ? (
          <Segment
            Icon={Loader2}
            label={compileLabel}
            tone={compileIsError ? "warn" : undefined}
          />
        ) : null}

        {/* 4. Cost today */}
        <Segment
          Icon={DollarSign}
          label={`${formatCost(totalCost)} today`}
          tone={totalCost > 5 ? "warn" : undefined}
        />

        {/* 5. Tokens in/out */}
        <Segment label={`${formatTokens(totalIn)} in · ${formatTokens(totalOut)} out`} />

        {/* 6. Trust filter */}
        <Segment label={`trust: ${trust}`} />

        {/* 7. Cloud sign-in state — honest read of desktop.toml + env */}
        <CloudSegment auth={auth} />

        {/* 8. (reserved) credits — pending B1 cloud chat path; intentionally absent */}
        {/* 9. (reserved) sync timestamp — pending cloud conversations API; intentionally absent */}
      </div>
      <div className="flex items-center gap-1">
        <Button
          size="sm"
          variant="ghost"
          className="h-6 gap-1.5 px-2 text-[11px] text-muted-foreground hover:text-foreground"
          onClick={() => openCmd(true)}
        >
          <Command className="size-3" />
          <span className="font-mono">K</span>
          <span className="hidden md:inline">to search</span>
        </Button>
      </div>
    </footer>
  );
}

type Tone = "success" | "warn";

function Segment({
  Icon,
  label,
  tone,
}: {
  Icon?: typeof Zap;
  label: string;
  tone?: Tone;
}) {
  return (
    <span
      className={
        tone === "success"
          ? "flex items-center gap-1 text-success"
          : tone === "warn"
            ? "flex items-center gap-1 text-warn"
            : "flex items-center gap-1"
      }
    >
      {Icon ? <Icon className="size-3" /> : null}
      <span className="whitespace-nowrap">{label}</span>
    </span>
  );
}

/**
 * Cloud segment.  Three states grounded in `auth_state`:
 *
 *   1. `auth === null` (still loading or load failed) — render nothing
 *      so the bar doesn't flash misleading text on first paint.
 *   2. `signed_in === false` — show "Local only" with a CloudOff icon.
 *      This is the honest "you haven't run tr login" indicator.
 *   3. `signed_in === true` — show "@handle" (when handle is set) or
 *      the cloud_base_url host (so the user can audit which cloud they
 *      are pointing at).  Cloud icon, no fake sync state.
 */
function CloudSegment({ auth }: { auth: AuthState | null }) {
  if (!auth) return null;
  if (!auth.signed_in) {
    return <Segment Icon={CloudOff} label="local only" />;
  }
  const handle = auth.handle ? `@${auth.handle}` : null;
  const host = auth.cloud_base_url
    ? safeHost(auth.cloud_base_url) ?? auth.cloud_base_url
    : null;
  const label = handle ?? host ?? "signed in";
  return <Segment Icon={Cloud} label={label} tone="success" />;
}

function safeHost(url: string): string | null {
  try {
    return new URL(url).host;
  } catch {
    return null;
  }
}
