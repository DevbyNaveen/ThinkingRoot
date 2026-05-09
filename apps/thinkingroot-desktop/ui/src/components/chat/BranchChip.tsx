// apps/thinkingroot-desktop/ui/src/components/chat/BranchChip.tsx
//
// Header chip that shows the active branch + lets the user switch.
// Lives in the chat header above the messages list.
//
// Wire path:
//   - Initial load: `branchList(workspace)` → find `current === true`
//   - Live updates: subscribe to `branch-event` Tauri channel and
//     re-fetch on Created / Merged / Abandoned / CheckedOut events
//   - Switch: clicking a row in the dropdown calls `branchCheckout`
//
// Behaviour:
//   - Closed state: pill button "🌿 stream/abc" — green when active
//     branch is the workspace default (`main`), blue otherwise
//   - Open state: dropdown lists all branches with status badges,
//     click switches, click outside or ESC closes
//
// Honest scope: this component does not surface ahead/behind counts or
// merge policy badges. Those live in the full Branches view. The chip
// is the always-visible "where am I writing claims" affordance.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { GitBranch, Check, ChevronDown } from "lucide-react";

import { cn } from "@/lib/utils";
import { toast } from "@/store/toast";
import {
  branchList,
  branchCheckout,
  branchEventSubscribe,
  onBranchEvent,
  type BranchView,
  type BranchEventEnvelope,
} from "@/lib/tauri";

interface BranchChipProps {
  workspace: string;
}

const REFRESH_TRIGGERS = new Set([
  "Created",
  "Merged",
  "Abandoned",
  "CheckedOut",
  "Checkout",
  "Deleted",
]);

function isRefreshTrigger(event: unknown): boolean {
  if (event && typeof event === "object") {
    const obj = event as Record<string, unknown>;
    if (typeof obj.kind === "string" && REFRESH_TRIGGERS.has(obj.kind)) return true;
    if (typeof obj.type === "string" && REFRESH_TRIGGERS.has(obj.type)) return true;
    const keys = Object.keys(obj);
    if (keys.length === 1 && REFRESH_TRIGGERS.has(keys[0]!)) return true;
  }
  return false;
}

export function BranchChip({ workspace }: BranchChipProps) {
  const [branches, setBranches] = useState<BranchView[]>([]);
  const [open, setOpen] = useState(false);
  const [switching, setSwitching] = useState<string | null>(null);
  const containerRef = useRef<HTMLDivElement | null>(null);

  const load = useCallback(async () => {
    try {
      const list = await branchList(workspace);
      setBranches(list);
    } catch {
      // Silent — the chip falls back to "no active branch" rather
      // than blocking the chat header on a transient daemon failure.
    }
  }, [workspace]);

  useEffect(() => {
    void load();
  }, [load]);

  // Subscribe to branch events for live updates.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;

    void (async () => {
      try {
        await branchEventSubscribe();
      } catch {
        return;
      }
      if (cancelled) return;
      unlisten = await onBranchEvent((envelope: BranchEventEnvelope) => {
        if (envelope.kind === "lagged" || envelope.kind === "disconnected") {
          void load();
          return;
        }
        if (envelope.kind === "event" && isRefreshTrigger(envelope.event)) {
          void load();
        }
      });
    })();

    return () => {
      cancelled = true;
      if (unlisten) unlisten();
    };
  }, [load]);

  // Click-outside + ESC to close.
  useEffect(() => {
    if (!open) return;
    function onPointerDown(ev: MouseEvent) {
      if (
        containerRef.current &&
        !containerRef.current.contains(ev.target as Node)
      ) {
        setOpen(false);
      }
    }
    function onKey(ev: KeyboardEvent) {
      if (ev.key === "Escape") setOpen(false);
    }
    window.addEventListener("mousedown", onPointerDown);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("mousedown", onPointerDown);
      window.removeEventListener("keydown", onKey);
    };
  }, [open]);

  const active = useMemo(
    () => branches.find((b) => b.current) ?? branches.find((b) => b.name === "main"),
    [branches],
  );
  const activeName = active?.name ?? "main";
  const isMain = activeName === "main";

  const onSwitch = useCallback(
    async (name: string) => {
      if (name === activeName) {
        setOpen(false);
        return;
      }
      setSwitching(name);
      try {
        await branchCheckout(workspace, name);
        toast(`Switched to ${name}`, { kind: "success" });
        setOpen(false);
        // Optimistic local update; SSE will sync the real state.
        setBranches((prev) =>
          prev.map((b) => ({ ...b, current: b.name === name })),
        );
        // Defensive refresh.
        void load();
      } catch (e) {
        toast("Branch switch failed", { body: String(e), kind: "error" });
      } finally {
        setSwitching(null);
      }
    },
    [activeName, workspace, load],
  );

  if (branches.length === 0) {
    // Substrate may not be mounted, or the daemon may not be reachable
    // yet. Render a neutral chip; clicking it does nothing harmful.
    return (
      <div className="inline-flex items-center gap-1.5 rounded-full border border-zinc-200 bg-zinc-50 px-2.5 py-0.5 text-xs text-zinc-500 dark:border-zinc-800 dark:bg-zinc-900/40 dark:text-zinc-500">
        <GitBranch className="h-3 w-3" aria-hidden />
        <span>main</span>
      </div>
    );
  }

  return (
    <div className="relative inline-block" ref={containerRef}>
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        className={cn(
          "inline-flex items-center gap-1.5 rounded-full border px-2.5 py-0.5 text-xs font-medium transition-colors cursor-pointer",
          isMain
            ? "border-emerald-300 bg-emerald-50 text-emerald-700 hover:bg-emerald-100 dark:border-emerald-800 dark:bg-emerald-950/40 dark:text-emerald-300 dark:hover:bg-emerald-950/60"
            : "border-blue-300 bg-blue-50 text-blue-700 hover:bg-blue-100 dark:border-blue-800 dark:bg-blue-950/40 dark:text-blue-300 dark:hover:bg-blue-950/60",
        )}
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-label={`Active branch: ${activeName}. Click to switch.`}
      >
        <GitBranch className="h-3 w-3" aria-hidden />
        <span className="font-mono">{activeName}</span>
        <ChevronDown
          className={cn("h-3 w-3 transition-transform", open && "rotate-180")}
          aria-hidden
        />
      </button>

      {open && (
        <div
          role="listbox"
          aria-label="Branches"
          className="absolute left-0 top-full z-30 mt-1 w-64 overflow-hidden rounded-md border border-border bg-popover shadow-lg"
        >
          <ul className="max-h-72 overflow-y-auto">
            {branches.map((b) => {
              const isActive = b.current;
              return (
                <li key={b.name}>
                  <button
                    type="button"
                    role="option"
                    aria-selected={isActive}
                    disabled={switching != null}
                    onClick={() => void onSwitch(b.name)}
                    className={cn(
                      "flex w-full items-center gap-2 px-3 py-1.5 text-left text-xs hover:bg-muted/60 disabled:opacity-50",
                      isActive && "bg-muted/40",
                    )}
                  >
                    {isActive ? (
                      <Check className="h-3 w-3 flex-shrink-0 text-emerald-600 dark:text-emerald-400" />
                    ) : (
                      <span className="block h-3 w-3 flex-shrink-0" />
                    )}
                    <span className="flex-1 truncate font-mono">{b.name}</span>
                    {b.parent && b.parent !== b.name && (
                      <span className="text-[10px] text-muted-foreground">
                        ← {b.parent}
                      </span>
                    )}
                    <span className="text-[10px] uppercase tracking-wide text-muted-foreground">
                      {b.status}
                    </span>
                  </button>
                </li>
              );
            })}
          </ul>
          <div className="border-t border-border bg-muted/30 px-3 py-1.5 text-[10px] text-muted-foreground">
            Click to switch · ESC to close
          </div>
        </div>
      )}
    </div>
  );
}
