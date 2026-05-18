// apps/thinkingroot-desktop/ui/src/components/chat/BranchChip.tsx
//
// Branch chip — active branch + switcher. Shown in the composer footer
// (session chat); was previously in the chat context header.
//
// Wire path:
//   - Initial load: `branchList(workspace)` → find `current === true`
//   - Live updates: subscribe to `branch-event` Tauri channel and
//     re-fetch on Created / Merged / Abandoned / CheckedOut events
//   - Switch: clicking a row in the dropdown calls `branchCheckout`
//
// Behaviour:
//   - Closed state: branch name + chevron only (no pill/box); hover
//     darkens text; non-main branch reads slightly stronger
//   - Open state: dropdown lists all branches with status badges,
//     click switches, click outside or ESC closes
//
// Honest scope: this component does not surface ahead/behind counts or
// merge policy badges. Those live in the full Branches view. The chip
// is the always-visible "where am I writing claims" affordance.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Check, ChevronDown } from "lucide-react";

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
import { branchListNeedsRefetchFromEnvelope } from "@/lib/branchEvents";

interface BranchChipProps {
  workspace: string;
  /** Smaller label for the chat context header. */
  compact?: boolean;
  /** Open the branch menu upward (composer footer). */
  dropUp?: boolean;
}

export function BranchChip({
  workspace,
  compact = false,
  dropUp = false,
}: BranchChipProps) {
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
        if (branchListNeedsRefetchFromEnvelope(envelope)) void load();
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
    // yet. Show branch label only (no interactive affordance).
    return (
      <span
        className={cn(
          "font-medium text-muted-foreground",
          compact ? "text-[10px]" : "text-xs",
        )}
      >
        main
      </span>
    );
  }

  return (
    <div className="relative inline-block leading-none" ref={containerRef}>
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        className={cn(
          "inline-flex max-w-[min(220px,100%)] cursor-pointer items-center gap-1 rounded-sm py-0.5 text-left font-medium transition-colors",
          compact ? "text-[10px]" : "text-xs",
          "hover:text-foreground",
          "focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring/55 focus-visible:ring-offset-2 focus-visible:ring-offset-background",
          open ? "text-foreground" : isMain ? "text-muted-foreground" : "text-foreground/85",
        )}
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-label={`Active branch: ${activeName}. Click to switch.`}
      >
        <span className="min-w-0 truncate">{activeName}</span>
        <ChevronDown
          className={cn(
            "shrink-0 opacity-70 transition-transform",
            compact ? "h-2.5 w-2.5" : "h-3 w-3",
            open && "rotate-180",
          )}
          aria-hidden
        />
      </button>

      {open && (
        <div
          role="listbox"
          aria-label="Branches"
          className={cn(
            "absolute left-0 z-30 w-72 overflow-hidden rounded-xl border border-border/70 bg-surface-elevated shadow-elevated",
            dropUp ? "bottom-full mb-1.5" : "top-full mt-1.5",
          )}
        >
          <ul className="max-h-72 overflow-y-auto p-1">
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
                      "flex w-full items-center gap-2 rounded-lg px-2.5 py-1.5 text-left text-xs transition-colors hover:bg-muted/45 disabled:opacity-50",
                      isActive && "bg-muted/55 text-foreground",
                    )}
                  >
                    {isActive ? (
                      <Check className="h-3 w-3 flex-shrink-0 text-success" />
                    ) : (
                      <span className="block h-3 w-3 flex-shrink-0" />
                    )}
                    <span className="min-w-0 flex-1 truncate text-xs font-medium">{b.name}</span>
                    {b.parent && b.parent !== b.name && (
                      <span className="max-w-20 truncate text-[10px] text-muted-foreground">
                        ← {b.parent}
                      </span>
                    )}
                    <span className="rounded-md border border-border/50 bg-background/35 px-1.5 py-0.5 text-[9px] uppercase tracking-wide text-muted-foreground">
                      {b.status}
                    </span>
                  </button>
                </li>
              );
            })}
          </ul>
          <div className="border-t border-border/60 bg-background/35 px-3 py-1.5 text-[10px] text-muted-foreground">
            Click to switch · ESC to close
          </div>
        </div>
      )}
    </div>
  );
}
