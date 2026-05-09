// apps/thinkingroot-desktop/ui/src/components/branches/BeliefDiffPanel.tsx
//
// Substrate Console — cross-branch belief diff. The "compare to
// baseline" surface that wraps the engine's `compute_diff` and
// renders the result as a column of conflict + addition cards.
//
// Wire path:
//   user picks branch → branchDiff(branch) → daemon GET
//     /api/v1/branches/{branch}/diff → engine
//     thinkingroot_branch::diff::compute_diff → wire shape
//     thinkingroot_core::types::diff::KnowledgeDiff
//
// Render contract:
//   - Header: "main vs <branch>" + merge_allowed badge + blocking reasons
//   - New claims: green-tinted cards with statement + confidence
//   - Auto-resolved contradictions: blue cards
//   - Needs-review contradictions: rose cards (load-bearing — these
//     gate merge)
//   - New entities: zinc cards
//
// Honest scope (v1.0):
//   - No "click to apply auto-resolution" — that's a v1.1 affordance
//     once we have a write path to override `winner`.
//   - Health-score before/after rendered as opaque labels; the full
//     HealthScore breakdown lives on the existing Branches view.
//   - Cross-branch relation diffs are listed but not interactive.

import { useCallback, useEffect, useState } from "react";
import {
  AlertCircle,
  CheckCircle2,
  GitCompare,
  Loader2,
  RefreshCw,
  XCircle,
} from "lucide-react";

import { Button } from "@/components/ui/button";
import { useApp } from "@/store/app";
import { toast } from "@/store/toast";
import { cn } from "@/lib/utils";
import {
  branchDiff,
  branchList,
  type BranchView,
  type KnowledgeDiff,
} from "@/lib/tauri";

interface BeliefDiffPanelProps {
  /** Optional starting branch (otherwise the panel picks the first
   *  non-main branch). */
  initialBranch?: string;
}

export function BeliefDiffPanel({ initialBranch }: BeliefDiffPanelProps) {
  const activeWorkspace = useApp((s) => s.activeWorkspace);
  const [branches, setBranches] = useState<BranchView[]>([]);
  const [selected, setSelected] = useState<string | null>(initialBranch ?? null);
  const [diff, setDiff] = useState<KnowledgeDiff | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Load the branch list — always excludes "main" from the picker.
  useEffect(() => {
    if (!activeWorkspace) return;
    let cancelled = false;
    branchList(activeWorkspace)
      .then((list) => {
        if (cancelled) return;
        setBranches(list);
        if (!selected) {
          const firstNonMain = list.find((b) => b.name !== "main");
          if (firstNonMain) setSelected(firstNonMain.name);
        }
      })
      .catch((e) => !cancelled && setError(String(e)));
    return () => {
      cancelled = true;
    };
  }, [activeWorkspace, selected]);

  const computeDiff = useCallback(async () => {
    if (!selected) return;
    setLoading(true);
    setError(null);
    try {
      const result = await branchDiff(selected);
      setDiff(result);
    } catch (e) {
      setError(String(e));
      toast("Diff failed", { kind: "error", body: String(e) });
    } finally {
      setLoading(false);
    }
  }, [selected]);

  // Auto-load when branch selection changes.
  useEffect(() => {
    if (selected) void computeDiff();
  }, [selected, computeDiff]);

  if (!activeWorkspace) {
    return (
      <div className="p-4 text-sm text-muted-foreground">
        No workspace selected.
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col">
      <div className="flex items-center gap-2 border-b border-border/40 px-3 py-2">
        <GitCompare className="h-4 w-4 text-muted-foreground" aria-hidden />
        <span className="text-sm font-semibold">Belief diff</span>
        <span className="text-xs text-muted-foreground">main vs</span>
        <select
          value={selected ?? ""}
          onChange={(e) => setSelected(e.target.value || null)}
          className="rounded border border-border bg-background px-2 py-0.5 text-xs"
          aria-label="Branch to compare against main"
        >
          <option value="">(pick a branch)</option>
          {branches
            .filter((b) => b.name !== "main")
            .map((b) => (
              <option key={b.name} value={b.name}>
                {b.name}
              </option>
            ))}
        </select>
        <div className="flex-1" />
        <Button
          size="sm"
          variant="ghost"
          onClick={() => void computeDiff()}
          disabled={!selected || loading}
          aria-label="Recompute diff"
        >
          {loading ? (
            <Loader2 className="h-3.5 w-3.5 animate-spin" />
          ) : (
            <RefreshCw className="h-3.5 w-3.5" />
          )}
        </Button>
      </div>

      <div className="flex-1 overflow-y-auto p-3">
        {error && (
          <div className="mb-3 rounded border border-rose-300 bg-rose-50 p-2 text-xs text-rose-700 dark:border-rose-800 dark:bg-rose-950/40 dark:text-rose-300">
            {error}
          </div>
        )}

        {!selected && (
          <div className="rounded border border-dashed border-border/60 bg-muted/10 p-6 text-center text-sm text-muted-foreground">
            Pick a branch to compare against main.
          </div>
        )}

        {selected && diff && <DiffSummary diff={diff} />}
      </div>
    </div>
  );
}

function DiffSummary({ diff }: { diff: KnowledgeDiff }) {
  return (
    <div className="space-y-4 text-xs">
      <MergeGate diff={diff} />

      {diff.needs_review.length > 0 && (
        <Section
          title="Needs review"
          count={diff.needs_review.length}
          tone="rose"
        >
          {diff.needs_review.map((pair, i) => (
            <ContradictionCard key={i} pair={pair} />
          ))}
        </Section>
      )}

      {diff.auto_resolved.length > 0 && (
        <Section
          title="Auto-resolved"
          count={diff.auto_resolved.length}
          tone="blue"
        >
          {diff.auto_resolved.map((res, i) => (
            <AutoResolutionCard key={i} resolution={res} />
          ))}
        </Section>
      )}

      {diff.new_claims.length > 0 && (
        <Section
          title="New claims"
          count={diff.new_claims.length}
          tone="emerald"
        >
          {diff.new_claims.slice(0, 50).map((entry, i) => (
            <ClaimCard key={i} entry={entry} />
          ))}
          {diff.new_claims.length > 50 && (
            <p className="text-[10px] text-muted-foreground">
              +{diff.new_claims.length - 50} more (truncated for display)
            </p>
          )}
        </Section>
      )}

      {diff.new_entities.length > 0 && (
        <Section
          title="New entities"
          count={diff.new_entities.length}
          tone="zinc"
        >
          <ul className="grid grid-cols-2 gap-1 text-[11px] font-mono">
            {diff.new_entities.slice(0, 20).map((e, i) => (
              <li
                key={i}
                className="truncate rounded bg-background/40 px-1.5 py-0.5"
                title={e.entity.canonical_name}
              >
                {e.entity.canonical_name}
              </li>
            ))}
          </ul>
          {diff.new_entities.length > 20 && (
            <p className="text-[10px] text-muted-foreground">
              +{diff.new_entities.length - 20} more
            </p>
          )}
        </Section>
      )}

      {diff.new_relations.length > 0 && (
        <Section
          title="New relations"
          count={diff.new_relations.length}
          tone="zinc"
        >
          <ul className="space-y-0.5 text-[11px]">
            {diff.new_relations.slice(0, 20).map((r, i) => (
              <li key={i} className="rounded bg-background/40 px-1.5 py-0.5">
                <span className="font-mono">{r.from_name}</span>{" "}
                <span className="text-muted-foreground">→</span>{" "}
                <span className="font-mono">{r.to_name}</span>{" "}
                <span className="text-[10px] text-muted-foreground">
                  ({r.relation_type})
                </span>
              </li>
            ))}
          </ul>
        </Section>
      )}

      {diff.new_claims.length === 0 &&
        diff.needs_review.length === 0 &&
        diff.auto_resolved.length === 0 &&
        diff.new_entities.length === 0 &&
        diff.new_relations.length === 0 && (
          <div className="rounded border border-dashed border-border/60 bg-muted/10 p-6 text-center text-sm text-muted-foreground">
            No divergence — branch is identical to main.
          </div>
        )}
    </div>
  );
}

function MergeGate({ diff }: { diff: KnowledgeDiff }) {
  if (diff.merge_allowed) {
    return (
      <div className="flex items-center gap-2 rounded border border-emerald-300 bg-emerald-50 p-2 text-xs text-emerald-700 dark:border-emerald-800 dark:bg-emerald-950/40 dark:text-emerald-300">
        <CheckCircle2 className="h-3.5 w-3.5" aria-hidden />
        <span>Merge allowed — no health drop or unresolved contradictions.</span>
      </div>
    );
  }
  return (
    <div className="rounded border border-rose-300 bg-rose-50 p-2 text-xs dark:border-rose-800 dark:bg-rose-950/40">
      <div className="flex items-center gap-2 text-rose-700 dark:text-rose-300">
        <AlertCircle className="h-3.5 w-3.5" aria-hidden />
        <span className="font-semibold">Merge blocked</span>
      </div>
      {diff.blocking_reasons.length > 0 && (
        <ul className="mt-1 list-disc pl-5 text-rose-700 dark:text-rose-300">
          {diff.blocking_reasons.map((r, i) => (
            <li key={i}>{r}</li>
          ))}
        </ul>
      )}
    </div>
  );
}

const TONE_BORDER: Record<string, string> = {
  rose: "border-rose-200 dark:border-rose-900/60",
  blue: "border-blue-200 dark:border-blue-900/60",
  emerald: "border-emerald-200 dark:border-emerald-900/60",
  zinc: "border-zinc-200 dark:border-zinc-800",
};

function Section({
  title,
  count,
  tone,
  children,
}: {
  title: string;
  count: number;
  tone: "rose" | "blue" | "emerald" | "zinc";
  children: React.ReactNode;
}) {
  return (
    <section
      className={cn(
        "rounded border bg-background/40 p-2",
        TONE_BORDER[tone] ?? TONE_BORDER.zinc,
      )}
    >
      <h3 className="mb-1.5 flex items-center gap-1.5 text-[10px] font-semibold uppercase tracking-wide text-muted-foreground">
        {title}
        <span className="text-muted-foreground/70">· {count}</span>
      </h3>
      <div className="space-y-1.5">{children}</div>
    </section>
  );
}

function ClaimCard({ entry }: { entry: import("@/lib/tauri").DiffClaimEntry }) {
  const conf = Math.round(entry.claim.confidence * 100);
  return (
    <div className="rounded bg-emerald-50/30 p-1.5 dark:bg-emerald-950/20">
      <p className="text-foreground">{entry.claim.statement}</p>
      <div className="mt-1 flex items-center gap-2 text-[10px] text-muted-foreground">
        <span className="font-mono">{entry.claim.id.slice(0, 12)}</span>
        <span>conf {conf}%</span>
        {entry.entity_context.length > 0 && (
          <span className="truncate">↳ {entry.entity_context.join(", ")}</span>
        )}
      </div>
    </div>
  );
}

function ContradictionCard({
  pair,
}: {
  pair: import("@/lib/tauri").ContradictionPairEntry;
}) {
  return (
    <div className="rounded bg-rose-50/40 p-1.5 dark:bg-rose-950/20">
      <div className="mb-1 flex items-start gap-2">
        <XCircle
          className="h-3 w-3 flex-shrink-0 text-rose-600 dark:text-rose-400"
          aria-hidden
        />
        <p className="text-foreground">{pair.main_claim.statement}</p>
      </div>
      <div className="ml-4 border-l-2 border-rose-300 pl-2 text-foreground/80 dark:border-rose-800">
        vs {pair.branch_claim.statement}
      </div>
      {pair.reason && (
        <p className="mt-1 text-[10px] italic text-muted-foreground">
          {pair.reason}
        </p>
      )}
    </div>
  );
}

function AutoResolutionCard({
  resolution,
}: {
  resolution: import("@/lib/tauri").AutoResolutionEntry;
}) {
  const delta = Math.abs(resolution.confidence_delta);
  return (
    <div className="rounded bg-blue-50/30 p-1.5 dark:bg-blue-950/20">
      <p className="font-mono text-[11px]">
        {resolution.main_claim_id.slice(0, 10)} ↔{" "}
        {resolution.branch_claim_id.slice(0, 10)}
      </p>
      <p className="text-[10px] text-muted-foreground">
        winner: <span className="font-mono">{resolution.winner.slice(0, 10)}</span>{" "}
        · Δconf {(delta * 100).toFixed(0)}%
      </p>
    </div>
  );
}
