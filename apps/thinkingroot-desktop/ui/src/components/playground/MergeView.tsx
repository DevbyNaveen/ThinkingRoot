import { useCallback, useState } from "react";
import {
  GitMerge,
  Loader2,
  AlertTriangle,
  CheckCircle2,
  Info,
} from "lucide-react";

import {
  commitMergePlan,
  commitSynthesizeMerge,
  type MergePlan,
  type MergeSynthesis,
} from "@/lib/tauri";
import { cn } from "@/lib/utils";

interface Props {
  workspace: string | null;
}

/**
 * Conflict-resolution view — Phase γ.3 of the Cognition Commits
 * design (`docs/2026-05-15-cognition-commits-design.md`).
 *
 * The user enters two branch names and hits "Compute plan." The
 * deterministic plan renders immediately (no LLM call). A second
 * button ("Synthesize") invokes the LLM-driven γ.2 synthesizer; the
 * resulting reasoning + verified-citation pills + honest "dropped
 * fabricated ids" badge appear below.
 *
 * Honesty rules baked into the UX:
 *   - Trivial plans render a "no synthesis needed" empty state
 *     instead of pretending to synthesize.
 *   - `dropped_citations` are surfaced with an explicit warning chip
 *     so the reviewer can see the LLM's hallucination rate.
 *   - The "Commit synthesis" button (deferred to a follow-up edit
 *     that wires `commitRecord`) is gated on `is_committable` —
 *     trivial + LLM-failed states can't accidentally land empty
 *     commits.
 */
export function MergeView({ workspace }: Props) {
  const [leftBranch, setLeftBranch] = useState("main");
  const [rightBranch, setRightBranch] = useState("");
  const [plan, setPlan] = useState<MergePlan | null>(null);
  const [synthesis, setSynthesis] = useState<MergeSynthesis | null>(null);
  const [loadingPlan, setLoadingPlan] = useState(false);
  const [loadingSynthesis, setLoadingSynthesis] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const compute = useCallback(async () => {
    if (!workspace || !leftBranch.trim() || !rightBranch.trim()) {
      setError("Both branch names are required.");
      return;
    }
    setLoadingPlan(true);
    setError(null);
    setSynthesis(null);
    try {
      const p = await commitMergePlan(leftBranch.trim(), rightBranch.trim());
      setPlan(p);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setPlan(null);
    } finally {
      setLoadingPlan(false);
    }
  }, [workspace, leftBranch, rightBranch]);

  const synthesize = useCallback(async () => {
    if (!workspace || !plan) return;
    setLoadingSynthesis(true);
    setError(null);
    try {
      const s = await commitSynthesizeMerge(plan.left_branch, plan.right_branch);
      setSynthesis(s);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoadingSynthesis(false);
    }
  }, [workspace, plan]);

  if (!workspace) {
    return (
      <div className="flex h-full items-center justify-center text-sm text-muted-foreground">
        Mount a workspace to compute merges.
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col">
      <header className="flex shrink-0 items-center gap-2 border-b border-border bg-surface/60 px-4 py-2">
        <GitMerge className="size-4 text-muted-foreground" />
        <h3 className="truncate text-sm font-semibold">Merge</h3>
        <span className="text-xs text-muted-foreground">
          deterministic plan + LLM-driven synthesis
        </span>
      </header>

      <div className="flex-1 overflow-y-auto px-4 py-3">
        <div className="flex flex-col gap-2 rounded-md border border-border bg-background p-3">
          <div className="flex items-center gap-2">
            <BranchInput
              label="Left (destination)"
              value={leftBranch}
              onChange={setLeftBranch}
            />
            <BranchInput
              label="Right (candidate)"
              value={rightBranch}
              onChange={setRightBranch}
            />
            <button
              type="button"
              onClick={() => void compute()}
              disabled={
                loadingPlan ||
                !leftBranch.trim() ||
                !rightBranch.trim()
              }
              className={cn(
                "mt-5 self-start rounded-md border border-border bg-background px-3 py-1 text-xs font-medium",
                loadingPlan
                  ? "cursor-not-allowed opacity-60"
                  : "hover:bg-muted/40",
              )}
            >
              {loadingPlan ? (
                <span className="flex items-center gap-1">
                  <Loader2 className="size-3 animate-spin" /> Computing
                </span>
              ) : (
                "Compute plan"
              )}
            </button>
          </div>
        </div>

        {error && (
          <div className="mt-3 flex items-center gap-2 rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive">
            <AlertTriangle className="size-4" />
            <span>{error}</span>
          </div>
        )}

        {plan && (
          <section className="mt-4 space-y-3">
            <PlanSummary plan={plan} />
            {!isTrivial(plan) && (
              <button
                type="button"
                onClick={() => void synthesize()}
                disabled={loadingSynthesis}
                className={cn(
                  "rounded-md border border-accent bg-accent/10 px-3 py-1 text-xs font-medium text-accent",
                  loadingSynthesis
                    ? "cursor-not-allowed opacity-60"
                    : "hover:bg-accent/20",
                )}
              >
                {loadingSynthesis ? (
                  <span className="flex items-center gap-1">
                    <Loader2 className="size-3 animate-spin" /> Synthesizing
                  </span>
                ) : (
                  "Synthesize with LLM"
                )}
              </button>
            )}
            {synthesis && <SynthesisCard synthesis={synthesis} />}
          </section>
        )}
      </div>
    </div>
  );
}

function BranchInput({
  label,
  value,
  onChange,
}: {
  label: string;
  value: string;
  onChange: (v: string) => void;
}) {
  return (
    <label className="flex flex-1 flex-col gap-1 text-xs text-muted-foreground">
      <span>{label}</span>
      <input
        type="text"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder="branch name"
        className="rounded-md border border-border bg-background px-2 py-1 font-mono text-sm text-foreground focus:outline-none focus:ring-1 focus:ring-accent"
      />
    </label>
  );
}

function PlanSummary({ plan }: { plan: MergePlan }) {
  const kind = plan.conflict_kind.kind;
  const kindLabel: Record<typeof kind, string> = {
    identical: "Identical — both branches at the same commit",
    left_ahead: "Left ahead — fast-forward applicable",
    right_ahead: "Right ahead — fast-forward applicable",
    diverged: "Diverged — synthesis recommended",
    no_common_history: "No common history — orphan branches",
  };
  return (
    <div className="space-y-3 rounded-md border border-border bg-background p-3">
      <div className="flex items-center gap-2">
        <Info className="size-4 text-muted-foreground" />
        <span className="text-sm font-medium">{kindLabel[kind]}</span>
      </div>
      <div className="grid grid-cols-2 gap-3 text-xs">
        <BranchHeadCard
          label={plan.left_branch}
          head={plan.left_head}
          onlyCount={plan.left_only_commits.length}
        />
        <BranchHeadCard
          label={plan.right_branch}
          head={plan.right_head}
          onlyCount={plan.right_only_commits.length}
        />
      </div>
      <WitnessClassificationGrid plan={plan} />
    </div>
  );
}

function BranchHeadCard({
  label,
  head,
  onlyCount,
}: {
  label: string;
  head: string | null;
  onlyCount: number;
}) {
  return (
    <div className="rounded-md border border-border bg-surface/40 p-2">
      <div className="font-mono text-xs text-muted-foreground">{label}</div>
      <div className="mt-1 truncate font-mono text-xs">
        head: {head ? head.slice(0, 8) : "—"}
      </div>
      <div className="text-xs text-muted-foreground">
        {onlyCount} unique commit{onlyCount === 1 ? "" : "s"} since LCA
      </div>
    </div>
  );
}

function WitnessClassificationGrid({ plan }: { plan: MergePlan }) {
  const w = plan.witnesses;
  const cells: Array<{ label: string; ids: string[]; tone: "agree" | "left" | "right" | "neutral" }> = [
    { label: "Shared citations", ids: w.shared_citations, tone: "agree" },
    { label: "Left-only citations", ids: w.left_only_citations, tone: "left" },
    { label: "Right-only citations", ids: w.right_only_citations, tone: "right" },
    { label: "Shared added", ids: w.shared_added, tone: "agree" },
    { label: "Left-only added", ids: w.left_only_added, tone: "left" },
    { label: "Right-only added", ids: w.right_only_added, tone: "right" },
  ];
  return (
    <div className="grid grid-cols-2 gap-2">
      {cells.map((c) => (
        <div
          key={c.label}
          className="rounded-md border border-border bg-surface/40 p-2"
        >
          <div className="mb-1 text-[10px] uppercase tracking-wide text-muted-foreground">
            {c.label} ({c.ids.length})
          </div>
          <div className="flex flex-wrap gap-1">
            {c.ids.length === 0 ? (
              <span className="text-xs italic text-muted-foreground">—</span>
            ) : (
              c.ids.slice(0, 8).map((id) => (
                <code
                  key={id}
                  title={id}
                  className={cn(
                    "rounded-full px-1.5 py-0.5 font-mono text-[10px]",
                    c.tone === "agree" && "bg-emerald-500/15 text-emerald-700",
                    c.tone === "left" && "bg-blue-500/15 text-blue-700",
                    c.tone === "right" && "bg-orange-500/15 text-orange-700",
                    c.tone === "neutral" && "bg-muted/40 text-foreground/80",
                  )}
                >
                  {id.slice(0, 8)}
                </code>
              ))
            )}
            {c.ids.length > 8 && (
              <span className="text-[10px] text-muted-foreground">
                +{c.ids.length - 8}
              </span>
            )}
          </div>
        </div>
      ))}
    </div>
  );
}

function SynthesisCard({ synthesis }: { synthesis: MergeSynthesis }) {
  const kind = synthesis.outcome.kind;
  if (kind === "trivial") {
    return (
      <div className="rounded-md border border-emerald-500/40 bg-emerald-500/10 p-3 text-sm text-emerald-700">
        <CheckCircle2 className="mr-1 inline size-4" />
        No synthesis needed — the plan is trivial.
      </div>
    );
  }
  if (kind === "llm_unavailable") {
    return (
      <div className="rounded-md border border-amber-500/40 bg-amber-500/10 p-3 text-sm text-amber-700">
        <AlertTriangle className="mr-1 inline size-4" />
        LLM is not configured for this workspace. Wire a provider in Settings
        to enable merge synthesis.
      </div>
    );
  }
  if (kind === "llm_error") {
    return (
      <div className="rounded-md border border-destructive/40 bg-destructive/10 p-3 text-sm text-destructive">
        <AlertTriangle className="mr-1 inline size-4" />
        LLM call failed
        {"message" in synthesis.outcome && synthesis.outcome.message ? (
          <code className="ml-2 block whitespace-pre-wrap rounded bg-muted/40 px-2 py-1 text-xs">
            {synthesis.outcome.message}
          </code>
        ) : null}
      </div>
    );
  }
  return (
    <div className="space-y-2 rounded-md border border-accent/30 bg-background p-3">
      <div className="flex items-center justify-between">
        <div className="text-xs font-semibold uppercase tracking-wide text-muted-foreground">
          Synthesis
        </div>
        <div className="text-xs text-muted-foreground">
          model: <code className="font-mono">{synthesis.model || "—"}</code>
        </div>
      </div>
      <p className="whitespace-pre-wrap text-sm text-foreground/90">
        {synthesis.reasoning}
      </p>
      <div className="flex flex-wrap gap-2 text-xs">
        <CitationGroup
          label="Verified"
          ids={synthesis.verified_citations}
          tone="ok"
        />
        {synthesis.dropped_citations.length > 0 && (
          <CitationGroup
            label="Dropped (fabricated)"
            ids={synthesis.dropped_citations}
            tone="warn"
          />
        )}
      </div>
    </div>
  );
}

function CitationGroup({
  label,
  ids,
  tone,
}: {
  label: string;
  ids: string[];
  tone: "ok" | "warn";
}) {
  return (
    <div className="flex items-center gap-1">
      <span
        className={cn(
          "rounded px-1.5 py-0.5 text-[10px] font-medium uppercase tracking-wide",
          tone === "ok" && "bg-emerald-500/15 text-emerald-700",
          tone === "warn" && "bg-destructive/15 text-destructive",
        )}
      >
        {label} ({ids.length})
      </span>
      {ids.slice(0, 6).map((id) => (
        <code
          key={id}
          title={id}
          className={cn(
            "rounded-full px-1.5 py-0.5 font-mono text-[10px]",
            tone === "ok"
              ? "bg-muted/40 text-foreground/80"
              : "bg-destructive/10 text-destructive",
          )}
        >
          {id.slice(0, 8)}
        </code>
      ))}
      {ids.length > 6 && (
        <span className="text-[10px] text-muted-foreground">
          +{ids.length - 6}
        </span>
      )}
    </div>
  );
}

/**
 * Local replica of Rust's `MergePlan::is_trivial()` — the Rust method
 * lives on the engine type and doesn't reach the wire. Identical /
 * left_ahead / right_ahead all collapse to "no synthesis needed".
 */
function isTrivial(plan: MergePlan): boolean {
  const k = plan.conflict_kind.kind;
  return k === "identical" || k === "left_ahead" || k === "right_ahead";
}
