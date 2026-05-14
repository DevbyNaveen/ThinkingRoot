// apps/thinkingroot-desktop/ui/src/components/branches/BranchTree.tsx
//
// Live branch tree (Substrate Console).
//
// Subscribes to the daemon's aggregate `/branch-events/stream` SSE
// endpoint via the `branch-event` Tauri channel; renders parent/child
// hierarchy + active marker + per-row last-event timestamp.
//
// Wire path:
//   daemon /branch-events/stream
//     → sidecar `branch_event_subscribe` (commands/branch_extras.rs)
//     → `branch-event` Tauri channel
//     → `onBranchEvent` listener here
//     → setState updates per-branch `lastEventAt` + triggers refresh
//
// Render contract:
//   * Parent branches (where `parent === ""` or unknown) render at root.
//   * Children render indented under their parent.
//   * The active (current) branch shows a filled green dot; others a
//     hollow zinc dot.
//   * Status badges: active (default), merged, abandoned, divergent.
//   * Last-event indicator: dim time-since label that updates when a
//     `branch-event` arrives for that branch.
//   * On `lagged` or `disconnected` envelope, the tree refreshes from
//     `branchList(workspace)` — those signals mean local state may be
//     out of sync.
//
// Honest limitation: the engine's `BranchEvent` doesn't currently
// carry a stable timestamp on the wire shape we forward (`event` is
// `unknown`). We use the moment we received the envelope as the
// timestamp — fine for "show recent activity", not as an audit
// source-of-truth.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { GitBranch, GitMerge, History, RefreshCw, CircleDot } from "lucide-react";

import { Button } from "@/components/ui/button";
import { useApp } from "@/store/app";
import { toast } from "@/store/toast";
import { cn } from "@/lib/utils";
import {
  branchList,
  branchCheckout,
  branchEventSubscribe,
  onBranchEvent,
  type BranchView,
  type BranchEventEnvelope,
} from "@/lib/tauri";
import { branchListShouldRefresh } from "@/lib/branchEvents";

interface BranchNode extends BranchView {
  children: BranchNode[];
}

interface PerBranchActivity {
  /** Local Date.now() of the last `branch-event` for this branch. */
  lastEventAt?: number;
  /** Most recent event tag for hover-detail (e.g. "Created", "Merged"). */
  lastEventLabel?: string;
}

function buildTree(branches: BranchView[]): BranchNode[] {
  const byName = new Map<string, BranchNode>();
  for (const b of branches) {
    byName.set(b.name, { ...b, children: [] });
  }
  const roots: BranchNode[] = [];
  for (const b of branches) {
    const node = byName.get(b.name);
    if (!node) continue;
    const parent = b.parent && byName.get(b.parent);
    if (parent) {
      parent.children.push(node);
    } else {
      roots.push(node);
    }
  }
  // Stable alphabetical sort within each level.
  const sortLevel = (nodes: BranchNode[]) => {
    nodes.sort((a, b) => a.name.localeCompare(b.name));
    for (const n of nodes) sortLevel(n.children);
  };
  sortLevel(roots);
  return roots;
}

function describeEvent(event: unknown): string | undefined {
  // The wire shape from the daemon's `BranchEvent` is intentionally
  // open here — we don't want to drift the UI every time the engine
  // adds a new variant. Pull the most useful tag we can find.
  if (event && typeof event === "object") {
    const obj = event as Record<string, unknown>;
    if (typeof obj.kind === "string") return obj.kind;
    // `serde(tag = "type")` shape from older variants.
    if (typeof obj.type === "string") return obj.type;
    // Single-key object (untagged enum) — use the key name.
    const keys = Object.keys(obj);
    if (keys.length === 1) return keys[0];
  }
  return undefined;
}

function timeSince(at: number, now: number): string {
  const seconds = Math.max(0, Math.floor((now - at) / 1000));
  if (seconds < 5) return "just now";
  if (seconds < 60) return `${seconds}s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return `${days}d ago`;
}

function statusBadgeClass(status: string): string {
  const s = status.toLowerCase();
  if (s === "active" || s === "current") {
    return "bg-emerald-100 text-emerald-700 dark:bg-emerald-950/40 dark:text-emerald-300";
  }
  if (s === "merged") {
    return "bg-blue-100 text-blue-700 dark:bg-blue-950/40 dark:text-blue-300";
  }
  if (s === "abandoned" || s === "deleted") {
    return "bg-zinc-100 text-zinc-500 dark:bg-zinc-900 dark:text-zinc-500";
  }
  return "bg-amber-100 text-amber-700 dark:bg-amber-950/40 dark:text-amber-300";
}

export function BranchTree({
  panelMode = false,
}: {
  panelMode?: boolean;
}) {
  const activeWorkspace = useApp((s) => s.activeWorkspace);
  const [branches, setBranches] = useState<BranchView[]>([]);
  const [activity, setActivity] = useState<Map<string, PerBranchActivity>>(
    () => new Map(),
  );
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // Tick every 30s so "just now" / "5s ago" labels keep refreshing
  // without forcing a re-fetch.
  const [, setTickNonce] = useState(0);
  const lastEnvelopeAt = useRef<number>(0);

  const load = useCallback(async () => {
    if (!activeWorkspace) return;
    setLoading(true);
    setError(null);
    try {
      const list = await branchList(activeWorkspace);
      setBranches(list);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, [activeWorkspace]);

  useEffect(() => {
    void load();
  }, [load]);

  // Subscribe to live events. Subscribe is idempotent on the Rust
  // side, so re-subscribing across re-renders / hot reloads is safe.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;

    void (async () => {
      try {
        await branchEventSubscribe();
      } catch (e) {
        // Sidecar might not be ready yet — silently fall through; the
        // initial branchList() still works and the user can hit
        // refresh.
        console.warn("branch-event subscribe failed", e);
        return;
      }
      if (cancelled) return;
      unlisten = await onBranchEvent((envelope) => {
        handleEnvelope(envelope);
      });
    })();

    function handleEnvelope(envelope: BranchEventEnvelope) {
      const now = Date.now();
      lastEnvelopeAt.current = now;
      if (envelope.kind === "event") {
        const label = describeEvent(envelope.event);
        setActivity((prev) => {
          const next = new Map(prev);
          next.set(envelope.branch, { lastEventAt: now, lastEventLabel: label });
          return next;
        });
        // The branch-list cache (parent / status / current) changes on
        // branch lifecycle events, permission/redaction updates, bulk
        // contribute, HEAD checkout (`head_changed` envelope), or when
        // the relay signals staleness.
        if (branchListShouldRefresh(envelope.event)) {
          void load();
        }
      } else if (
        envelope.kind === "head_changed" ||
        envelope.kind === "lagged" ||
        envelope.kind === "disconnected"
      ) {
        void load();
      }
    }

    return () => {
      cancelled = true;
      if (unlisten) unlisten();
      // We deliberately do NOT call branchEventUnsubscribe on
      // unmount — other Substrate Console components (branch chip,
      // BeliefDiffPanel) want the same stream. Subscriber cleanup
      // happens at app shutdown only.
    };
  }, [load]);

  // 30s tick for relative-time labels.
  useEffect(() => {
    const interval = window.setInterval(() => setTickNonce((n) => n + 1), 30_000);
    return () => window.clearInterval(interval);
  }, []);

  const tree = useMemo(() => buildTree(branches), [branches]);
  const now = Date.now();

  const onCheckout = useCallback(
    async (name: string) => {
      if (!activeWorkspace) return;
      try {
        await branchCheckout(activeWorkspace, name);
        toast(`Switched to ${name}`, { kind: "success" });
        void load();
      } catch (e) {
        toast(`Checkout failed`, { body: String(e), kind: "error" });
      }
    },
    [activeWorkspace, load],
  );

  if (!activeWorkspace) {
    return (
      <div className="flex h-full items-center justify-center p-6 text-sm text-muted-foreground">
        No workspace selected.
      </div>
    );
  }

  return (
    <div
      className={cn(
        "flex h-full flex-col",
        panelMode ? "" : "p-4",
      )}
    >
      <div className="mb-2 flex items-center justify-between">
        <div className="flex items-center gap-2">
          <GitBranch className="h-4 w-4" />
          <h3 className="text-sm font-semibold">Branches</h3>
          <span className="text-xs text-muted-foreground">
            {branches.length} total
          </span>
        </div>
        <Button
          size="sm"
          variant="ghost"
          onClick={() => void load()}
          disabled={loading}
          aria-label="Refresh branch list"
        >
          <RefreshCw className={cn("h-3.5 w-3.5", loading && "animate-spin")} />
        </Button>
      </div>

      {error && (
        <div className="mb-2 rounded border border-rose-300 bg-rose-50 px-2 py-1 text-xs text-rose-700 dark:border-rose-800 dark:bg-rose-950/40 dark:text-rose-300">
          {error}
        </div>
      )}

      <div className="flex-1 overflow-y-auto">
        {tree.length === 0 ? (
          <div className="p-4 text-sm text-muted-foreground">
            No branches yet. Create one from the Branches view.
          </div>
        ) : (
          <ul className="space-y-0.5 text-sm">
            {tree.map((node) => (
              <BranchTreeNode
                key={node.name}
                node={node}
                depth={0}
                activity={activity}
                now={now}
                onCheckout={onCheckout}
              />
            ))}
          </ul>
        )}
      </div>
    </div>
  );
}

function BranchTreeNode({
  node,
  depth,
  activity,
  now,
  onCheckout,
}: {
  node: BranchNode;
  depth: number;
  activity: Map<string, PerBranchActivity>;
  now: number;
  onCheckout: (name: string) => void;
}) {
  const act = activity.get(node.name);
  const lastEventAtLabel = act?.lastEventAt ? timeSince(act.lastEventAt, now) : null;
  const indent = depth * 14;
  const isActive = node.current;

  return (
    <>
      <li
        style={{ paddingLeft: indent }}
        className={cn(
          "group flex items-center gap-2 rounded px-1.5 py-1 hover:bg-muted/60 cursor-pointer",
          isActive && "bg-emerald-50/40 dark:bg-emerald-950/20",
        )}
        onClick={() => !isActive && onCheckout(node.name)}
        aria-label={
          isActive
            ? `Active branch: ${node.name}`
            : `Switch to branch: ${node.name}`
        }
      >
        {isActive ? (
          <CircleDot
            className="h-3 w-3 flex-shrink-0 text-emerald-600 dark:text-emerald-400"
            aria-hidden
          />
        ) : (
          <span
            className="block h-2 w-2 flex-shrink-0 rounded-full border border-zinc-400 dark:border-zinc-600"
            aria-hidden
          />
        )}
        <span className="flex-1 truncate font-mono text-xs">{node.name}</span>
        <span
          className={cn(
            "rounded px-1.5 py-0.5 text-[10px] font-medium uppercase tracking-wide",
            statusBadgeClass(node.status),
          )}
          title={`Status: ${node.status}`}
        >
          {node.status}
        </span>
        {lastEventAtLabel && (
          <span
            className="flex items-center gap-0.5 text-[10px] text-muted-foreground"
            title={
              act?.lastEventLabel
                ? `Last event: ${act.lastEventLabel}`
                : "Last event recorded by aggregate stream"
            }
          >
            <History className="h-3 w-3" aria-hidden />
            {lastEventAtLabel}
          </span>
        )}
        {node.parent && depth === 0 && (
          <span className="text-[10px] text-muted-foreground" title="Parent">
            <GitMerge className="inline h-3 w-3" aria-hidden /> {node.parent}
          </span>
        )}
      </li>
      {node.children.map((child) => (
        <BranchTreeNode
          key={child.name}
          node={child}
          depth={depth + 1}
          activity={activity}
          now={now}
          onCheckout={onCheckout}
        />
      ))}
    </>
  );
}
