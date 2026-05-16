// apps/thinkingroot-desktop/ui/src/components/branches/TopicBranchesPanel.tsx
//
// Phase B.3 (2026-05-17) — surface auto-created `topic/*` Feature
// branches with their B.1-set descriptions (the user's first
// question of the session) and human-driven Merge-to-main /
// Discard actions.
//
// Wire path:
//   - Initial load + every panel-open: `branchList(workspace)` →
//     filter to active topic/* Feature branches.
//   - Live updates: subscribe to `branch-event` Tauri channel and
//     re-fetch on Created / Merged / Abandoned envelopes.
//   - Merge action: `branchMerge(name, parent="main")` then refetch.
//   - Discard action: `branchDelete(name)` (soft abandon — data dir
//     stays on disk) then refetch.
//
// Architecture invariants this surface enforces by code:
//   - Topic branches NEVER auto-promote to main. Promoting is an
//     explicit user click on "Merge to main" — matches the
//     `MergePolicy::Manual` policy `ensure_topic_branch` set on the
//     branch at create time.
//   - "Discard" calls the SOFT-delete path (`branchDelete` →
//     `branchAbandon`), not a hard purge. Agent-contributed work
//     stays on disk in the branch's data dir until an explicit
//     `gc_branches` call.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { GitMerge, Inbox, Loader2, MessageSquareText, Trash2, X } from "lucide-react";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { toast } from "@/store/toast";
import {
  branchDelete,
  branchEventSubscribe,
  branchList,
  branchMerge,
  onBranchEvent,
  type BranchEventEnvelope,
  type BranchView,
} from "@/lib/tauri";
import { branchListNeedsRefetchFromEnvelope } from "@/lib/branchEvents";

interface Props {
  workspace: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

/**
 * True when `kind` is the daemon's `BranchKind::Feature` tagged-JSON
 * shape — `{ kind: "feature" }`. Defensive against older daemons that
 * omit `kind` entirely (in which case `branch.rs::default_branch_kind_json`
 * falls back to `feature` already, so the field is always present at
 * the TS layer).
 */
function isFeatureBranch(kind: unknown): boolean {
  if (typeof kind !== "object" || kind === null) return false;
  const k = (kind as Record<string, unknown>).kind;
  return k === "feature";
}

/**
 * A branch is a "topic branch" iff it is an Active Feature branch
 * whose name starts with `topic/`. Both conditions matter: a
 * user-created Feature branch named "topic/explore-x" should appear
 * here, but the `main` branch (kind = "main") and any `stream/*`
 * branch (kind = "stream", merge_policy = AutoOnSessionEnd) should
 * not — those have different lifecycle semantics.
 */
function isTopicBranch(b: BranchView): boolean {
  return (
    b.status === "active" &&
    b.name.startsWith("topic/") &&
    isFeatureBranch(b.kind)
  );
}

export function TopicBranchesPanel({ workspace, open, onOpenChange }: Props) {
  const [branches, setBranches] = useState<BranchView[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // Per-row pending state so two rows can be acted on in sequence
  // without disabling the whole panel.
  const [merging, setMerging] = useState<string | null>(null);
  const [discarding, setDiscarding] = useState<string | null>(null);
  const closeBtnRef = useRef<HTMLButtonElement | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const list = await branchList(workspace);
      setBranches(list);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [workspace]);

  // Initial load + every panel open. We deliberately re-fetch on
  // open (not just on mount) so the user always sees the freshest
  // list after the cleanup task may have produced new topic
  // branches between opens.
  useEffect(() => {
    if (open) void load();
  }, [open, load]);

  // Live updates while the panel is open.
  useEffect(() => {
    if (!open) return;
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
  }, [open, load]);

  // ESC + click-outside close. Focuses the close button on open so
  // keyboard users can dismiss without mouse.
  useEffect(() => {
    if (!open) return;
    closeBtnRef.current?.focus();
    function onKey(ev: KeyboardEvent) {
      if (ev.key === "Escape" && merging == null && discarding == null) {
        onOpenChange(false);
      }
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onOpenChange, merging, discarding]);

  const topicBranches = useMemo(
    () => branches.filter(isTopicBranch),
    [branches],
  );

  const onMerge = useCallback(
    async (name: string) => {
      setMerging(name);
      try {
        const result = await branchMerge({ workspace, name });
        if (result.merged) {
          toast(`Merged ${name} → main`, {
            kind: "success",
            body: `+${result.new_claims} claims · ${result.conflicts} conflict${
              result.conflicts === 1 ? "" : "s"
            }`,
          });
        } else {
          toast(`Merge blocked: ${name}`, {
            kind: "warn",
            body: result.blocking_reasons.join("\n") || "merge gate refused",
          });
        }
        await load();
      } catch (e) {
        toast(`Merge failed: ${name}`, {
          kind: "error",
          body: e instanceof Error ? e.message : String(e),
        });
      } finally {
        setMerging(null);
      }
    },
    [workspace, load],
  );

  const onDiscard = useCallback(
    async (name: string) => {
      setDiscarding(name);
      try {
        await branchDelete(workspace, name);
        toast(`Discarded ${name}`, {
          kind: "info",
          body: "branch abandoned — data dir kept on disk, run /gc to reclaim",
        });
        await load();
      } catch (e) {
        toast(`Discard failed: ${name}`, {
          kind: "error",
          body: e instanceof Error ? e.message : String(e),
        });
      } finally {
        setDiscarding(null);
      }
    },
    [workspace, load],
  );

  if (!open) return null;

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label="Topic branches"
      className="fixed inset-0 z-[58] flex items-center justify-center bg-background/70 backdrop-blur-sm"
      onClick={(e) => {
        // Click-outside closes unless an action is in flight (would
        // otherwise leave the user wondering whether the merge
        // actually committed).
        if (
          e.target === e.currentTarget &&
          merging == null &&
          discarding == null
        ) {
          onOpenChange(false);
        }
      }}
    >
      <div className="flex max-h-[80vh] w-full max-w-2xl flex-col overflow-hidden rounded-xl border border-border bg-surface-elevated shadow-elevated">
        <header className="flex items-center justify-between border-b border-border/60 px-5 py-3">
          <div className="flex items-center gap-2.5">
            <div className="flex size-7 items-center justify-center rounded-md bg-primary/12 text-primary">
              <MessageSquareText className="size-3.5" aria-hidden />
            </div>
            <div>
              <h2 className="text-sm font-medium tracking-tight">
                Topic branches
              </h2>
              <p className="text-[11px] text-muted-foreground">
                Auto-saved from past chat sessions — merge to main or discard.
              </p>
            </div>
          </div>
          <button
            ref={closeBtnRef}
            type="button"
            className="rounded-md p-1.5 text-muted-foreground transition-colors hover:bg-muted/45 hover:text-foreground focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring/55"
            onClick={() => onOpenChange(false)}
            disabled={merging != null || discarding != null}
            aria-label="Close"
          >
            <X className="size-4" aria-hidden />
          </button>
        </header>

        <div className="flex-1 overflow-y-auto px-5 py-3">
          {loading && topicBranches.length === 0 ? (
            <div className="flex items-center justify-center gap-2 py-12 text-xs text-muted-foreground">
              <Loader2 className="size-3.5 animate-spin" aria-hidden />
              <span>Loading topic branches…</span>
            </div>
          ) : error ? (
            <div className="rounded-md border border-destructive/40 bg-destructive/8 px-3 py-2.5 text-xs text-destructive">
              Failed to list branches: {error}
            </div>
          ) : topicBranches.length === 0 ? (
            <div className="flex flex-col items-center justify-center gap-2 py-14 text-center text-muted-foreground">
              <Inbox className="size-6 opacity-60" aria-hidden />
              <p className="text-xs">No topic branches yet.</p>
              <p className="max-w-sm text-[11px] leading-relaxed">
                When a chat session ends with new claims contributed by the
                agent, ThinkingRoot auto-saves them onto a topic branch.
                Review them here before promoting to <span className="font-mono text-[10px]">main</span>.
              </p>
            </div>
          ) : (
            <ul className="space-y-1.5">
              {topicBranches.map((b) => {
                const isMerging = merging === b.name;
                const isDiscarding = discarding === b.name;
                const anyActing = merging != null || discarding != null;
                return (
                  <li
                    key={b.name}
                    className={cn(
                      "rounded-lg border border-border/55 bg-background/35 px-3 py-2.5 transition-colors",
                      "hover:bg-background/55",
                    )}
                  >
                    <div className="flex items-start justify-between gap-3">
                      <div className="min-w-0 flex-1">
                        <p className="line-clamp-2 text-xs leading-relaxed text-foreground">
                          {b.description ?? "(no title set)"}
                        </p>
                        <p className="mt-1 truncate font-mono text-[10px] text-muted-foreground">
                          {b.name} ← {b.parent}
                        </p>
                      </div>
                      <div className="flex shrink-0 items-center gap-1.5">
                        <Button
                          type="button"
                          size="sm"
                          variant="default"
                          disabled={anyActing}
                          onClick={() => void onMerge(b.name)}
                          aria-label={`Merge ${b.name} into main`}
                        >
                          {isMerging ? (
                            <Loader2 className="size-3 animate-spin" aria-hidden />
                          ) : (
                            <GitMerge className="size-3" aria-hidden />
                          )}
                          <span className="ml-1 text-[11px]">Merge to main</span>
                        </Button>
                        <Button
                          type="button"
                          size="sm"
                          variant="ghost"
                          disabled={anyActing}
                          onClick={() => void onDiscard(b.name)}
                          aria-label={`Discard ${b.name}`}
                        >
                          {isDiscarding ? (
                            <Loader2 className="size-3 animate-spin" aria-hidden />
                          ) : (
                            <Trash2 className="size-3" aria-hidden />
                          )}
                          <span className="ml-1 text-[11px]">Discard</span>
                        </Button>
                      </div>
                    </div>
                  </li>
                );
              })}
            </ul>
          )}
        </div>

        <footer className="border-t border-border/60 bg-background/35 px-5 py-2 text-[10px] text-muted-foreground">
          {topicBranches.length > 0 && !loading && !error ? (
            <span>
              {topicBranches.length} topic branch
              {topicBranches.length === 1 ? "" : "es"} · ESC or click outside to
              close
            </span>
          ) : (
            <span>ESC or click outside to close</span>
          )}
        </footer>
      </div>
    </div>
  );
}
