/**
 * Compile tab — git-style branch graph: main spine, three fixed side
 * rails (Feature+Tag, Sandbox, Stream) from `BranchView.kind`, merge-back
 * when status is merged, compile row on the spine only.
 *
 * River v1.0 additions on top of the per-branch row layout:
 *   - Persistent diamond `mergeGlyph` at the spine join for merged
 *     history rows — the merge is visible after the animation fades.
 *   - Transient 800ms pulse ring on the row when an SSE `merged` event
 *     arrives over `branch-event`. Lets the user see merges land live
 *     even when their cursor is somewhere else on the screen.
 *
 * The REST chat path now auto-creates `stream/{conversation_id}`
 * branches symmetric with the MCP `tools/call` path (see
 * `thinkingroot-serve::mcp::auto_create_session_branch`), so every
 * conversation appears on the stream rail without per-client glue.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { ReactElement } from "react";
import { GitBranchPlus, Loader2, Plus } from "lucide-react";

import {
  branchListNeedsRefetchFromEnvelope,
  branchListShouldRefresh,
} from "@/lib/branchEvents";
import { cn } from "@/lib/utils";
import { RefreshIcon } from "@/components/ui/refresh-icon";
import { useApp } from "@/store/app";
import { Button } from "@/components/ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { toast } from "@/store/toast";
import {
  branchList,
  branchCheckout,
  branchCreate,
  branchEventSubscribe,
  onBranchEvent,
  type BranchView,
  type BranchEventEnvelope,
  type CompileProgress,
} from "@/lib/tauri";

const MAIN_X = 10;
const ROW_H = 26;
const PAD_Y = 8;
const COL_GAP = 22;
/** Max parallel side columns (narrow rail). */
const MAX_SIDE_LANES = 3;
/** Pulse-ring lifetime after an SSE `merged` event lands. */
const MERGE_PULSE_MS = 800;

export type GraphTone =
  | "main"
  | "stream"
  | "feature"
  | "sandbox"
  | "compile"
  | "merged"
  | "abandoned";

export interface GraphNode {
  id: string;
  /** 0 = main spine; 1 = feature/tag, 2 = sandbox, 3 = stream. */
  lane: number;
  label: string;
  branch?: BranchView;
  tone: GraphTone;
  head?: boolean;
  /** Persistent diamond rendered at the spine join — for merged-tone
   *  history rows. The diamond outlives the pulse animation. */
  mergeGlyph?: boolean;
  /** Transient pulse ring driven by SSE `merged` events. The ring is
   *  in the SVG only while the node id is in `recentMerges`; the
   *  enclosing effect clears it after `MERGE_PULSE_MS`. */
  pulsing?: boolean;
}

type BranchKindKey = "main" | "feature" | "stream" | "sandbox" | "tag";

function branchKindKey(b: BranchView): BranchKindKey {
  const raw = b.kind;
  if (raw && typeof raw === "object" && raw !== null && !Array.isArray(raw)) {
    const tag = (raw as { kind?: string }).kind;
    if (tag === "main") return "main";
    if (tag === "feature") return "feature";
    if (tag === "stream") return "stream";
    if (tag === "sandbox") return "sandbox";
    if (tag === "tag") return "tag";
  }
  if (b.name === "main") return "main";
  if (b.name.startsWith("stream/")) return "stream";
  return "feature";
}

/** Side column: 1 feature+tag, 2 sandbox, 3 stream. */
function logicalRailColumn(kind: BranchKindKey): number {
  if (kind === "stream") return 3;
  if (kind === "sandbox") return 2;
  return 1;
}

function effectiveParent(b: BranchView, byName: Map<string, BranchView>): string {
  const p = b.parent?.trim();
  if (!p || !byName.has(p)) return "main";
  return p;
}

function branchToneFromBranch(b: BranchView): GraphTone {
  const s = b.status.toLowerCase();
  if (s === "merged") return "merged";
  if (s === "abandoned" || s === "deleted") return "abandoned";
  const k = branchKindKey(b);
  if (k === "main") return "main";
  if (k === "stream") return "stream";
  if (k === "sandbox") return "sandbox";
  return "feature";
}

function dotFill(tone: GraphTone): string {
  switch (tone) {
    case "main":
      return "#58a6ff";
    case "stream":
      return "#a371f7";
    case "feature":
      return "#d29922";
    case "sandbox":
      return "#3fb950";
    case "compile":
      return "hsl(var(--muted-foreground) / 0.75)";
    case "merged":
      return "hsl(var(--muted-foreground) / 0.35)";
    case "abandoned":
      return "hsl(var(--muted-foreground) / 0.22)";
    default:
      return "hsl(var(--muted-foreground) / 0.5)";
  }
}

function compileLabel(p: CompileProgress | null): { busy: boolean; text: string } {
  if (!p) return { busy: false, text: "compile" };
  switch (p.phase) {
    case "done":
      return { busy: false, text: `compile · ${p.claims}c ${p.entities}e` };
    case "failed":
      return { busy: false, text: `compile · ${truncate(p.error, 28)}` };
    case "cancelled":
      return { busy: false, text: "compile · stopped" };
    case "tick": {
      if (p.total > 0) {
        return {
          busy: true,
          text: `compile · ${p.step_label || p.step} ${p.done}/${p.total}`,
        };
      }
      return {
        busy: true,
        text: `compile · ${p.step_label || p.step} ${(p.step_elapsed_ms / 1000).toFixed(0)}s`,
      };
    }
    case "booting":
      return { busy: true, text: "compile · engine…" };
    case "connecting":
      return { busy: true, text: "compile · connecting…" };
    case "retrying":
      return { busy: true, text: `compile · retry ${p.attempt + 1}/2` };
    case "started":
      return { busy: true, text: "compile · starting" };
    default:
      return { busy: true, text: `compile · ${(p as { phase: string }).phase}` };
  }
}

function truncate(s: string, max: number): string {
  if (s.length <= max) return s;
  return `${s.slice(0, max - 1)}…`;
}

function dotTooltipBody(node: GraphNode): string {
  if (node.id === "compile") {
    return `Local compile / substrate refresh\n${node.label}\n\nSits on the main spine — not a merge from a side branch.`;
  }
  if (node.branch) {
    const b = node.branch;
    const lines = [
      b.name,
      `parent: ${b.parent?.trim() || "(root)"}`,
      `status: ${b.status}`,
    ];
    if (b.description) lines.push(`desc: ${b.description}`);
    if (node.head) lines.push("current HEAD");
    if (node.mergeGlyph) lines.push("merged into main");
    return lines.join("\n");
  }
  if (node.id === "main") {
    return "Workspace trunk (main).";
  }
  return node.label;
}

function laneX(lane: number): number {
  if (lane <= 0) return MAIN_X;
  return MAIN_X + COL_GAP * lane;
}

/**
 * DFS from main: spine row, then children of main on fixed kind rails
 * (1 feature+tag, 2 sandbox, 3 stream); merged/abandoned collapse to
 * the spine; nested branches inherit the parent's side column.
 *
 * `recentMerges` carries branch names that received an SSE `merged`
 * event within the last `MERGE_PULSE_MS` — those nodes get a transient
 * pulse ring drawn around their dot. Merged-tone rows additionally get
 * a persistent diamond glyph so the spine reads as a real merge join.
 */
function buildGraphNodes(
  branches: BranchView[],
  compile: CompileProgress | null,
  recentMerges: ReadonlySet<string>,
): GraphNode[] {
  const byName = new Map(branches.map((b) => [b.name, b]));
  const mainName = branches.find((b) => b.name === "main")?.name ?? "main";

  const children = new Map<string, BranchView[]>();
  for (const b of branches) {
    if (b.name === mainName) continue;
    const par = effectiveParent(b, byName);
    const list = children.get(par) ?? [];
    list.push(b);
    children.set(par, list);
  }

  const graphOrder = (a: BranchView, b: BranchView) => {
    const ta = branchToneFromBranch(a);
    const tb = branchToneFromBranch(b);
    const spineFirst = (t: GraphTone) => (t === "merged" || t === "abandoned" ? 0 : 1);
    if (spineFirst(ta) !== spineFirst(tb)) return spineFirst(ta) - spineFirst(tb);
    if (ta !== "merged" && ta !== "abandoned" && tb !== "merged" && tb !== "abandoned") {
      const ca = logicalRailColumn(branchKindKey(a));
      const cb = logicalRailColumn(branchKindKey(b));
      if (ca !== cb) return ca - cb;
    }
    return a.name.localeCompare(b.name);
  };

  for (const [, list] of children) {
    list.sort(graphOrder);
  }

  const out: GraphNode[] = [];
  const mainB = branches.find((b) => b.name === mainName);
  const head = branches.find((b) => b.current);
  const onMain = head?.name === mainName;

  out.push({
    id: "main",
    lane: 0,
    label: onMain ? `${mainName} · HEAD` : mainName,
    branch: mainB,
    tone: "main",
    head: onMain,
  });

  const mainKids = [...(children.get(mainName) ?? [])].sort(graphOrder);
  const laneByBranch = new Map<string, number>();
  for (const b of mainKids) {
    const tone = branchToneFromBranch(b);
    const lane =
      tone === "merged" || tone === "abandoned" ? 0 : logicalRailColumn(branchKindKey(b));
    laneByBranch.set(b.name, lane);
  }

  function assignLanesDeep(parentName: string) {
    const kids = children.get(parentName) ?? [];
    const base = laneByBranch.get(parentName) ?? 0;
    for (const b of kids) {
      if (!laneByBranch.has(b.name)) {
        const tone = branchToneFromBranch(b);
        if (tone === "merged" || tone === "abandoned") {
          laneByBranch.set(b.name, 0);
        } else if (base > 0) {
          laneByBranch.set(b.name, base);
        } else {
          laneByBranch.set(b.name, logicalRailColumn(branchKindKey(b)));
        }
      }
      assignLanesDeep(b.name);
    }
  }
  for (const b of mainKids) assignLanesDeep(b.name);

  function emitBranch(b: BranchView, depth: number) {
    const tone = branchToneFromBranch(b);
    const rawLane = laneByBranch.get(b.name) ?? Math.min(1 + depth, MAX_SIDE_LANES);
    const lane = tone === "merged" || tone === "abandoned" ? 0 : rawLane;
    const isHead = b.current;

    const label =
      isHead
        ? `${b.name} · HEAD`
        : b.status !== "active"
          ? `${b.name} · ${b.status}`
          : b.name;

    out.push({
      id: b.name,
      lane,
      label,
      branch: b,
      tone,
      head: isHead,
      mergeGlyph: tone === "merged",
      pulsing: recentMerges.has(b.name),
    });

    const sub = [...(children.get(b.name) ?? [])].sort(graphOrder);
    for (const c of sub) emitBranch(c, depth + 1);
  }

  for (const b of mainKids) emitBranch(b, 0);

  const cl = compileLabel(compile);
  out.push({
    id: "compile",
    lane: 0,
    label: cl.text,
    tone: "compile",
    head: false,
  });

  return out;
}

/** Honest empty list: main spine + compile only — no invented branch names. */
function emptyGraphNodes(compile: CompileProgress | null): GraphNode[] {
  const cl = compileLabel(compile);
  return [
    {
      id: "main",
      lane: 0,
      label: "main",
      tone: "main" as const,
      head: false,
    },
    {
      id: "compile",
      lane: 0,
      label: cl.text,
      tone: "compile" as const,
      head: false,
    },
  ];
}

function mergeCompileIntoNodes(nodes: GraphNode[], compile: CompileProgress | null): GraphNode[] {
  const cl = compileLabel(compile);
  return nodes.map((n) =>
    n.id === "compile" ? { ...n, label: cl.text } : n,
  );
}

function BranchGraphSvg({
  nodes,
  growUp = true,
}: {
  nodes: GraphNode[];
  /** true = trunk at bottom, tips toward top (reverse Y). */
  growUp?: boolean;
}) {
  const n = nodes.length;
  const maxLane = Math.min(
    MAX_SIDE_LANES,
    Math.max(0, ...nodes.map((d) => d.lane)),
  );
  const width = laneX(maxLane) + 14;
  const height = PAD_Y * 2 + Math.max(1, n) * ROW_H;

  const cy = (i: number) =>
    growUp
      ? height - PAD_Y - i * ROW_H - ROW_H / 2
      : PAD_Y + i * ROW_H + ROW_H / 2;

  const paths: ReactElement[] = [];
  let pid = 0;

  for (let i = 1; i < n; i++) {
    const a = nodes[i - 1]!;
    const b = nodes[i]!;
    const x0 = laneX(a.lane);
    const y0 = cy(i - 1);
    const x1 = laneX(b.lane);
    const y1 = cy(i);

    if (a.lane === b.lane) {
      paths.push(
        <line
          key={`e-${pid++}`}
          x1={x0}
          y1={y0}
          x2={x1}
          y2={y1}
          stroke="hsl(var(--border))"
          strokeWidth={1.25}
          vectorEffect="non-scaling-stroke"
        />,
      );
    } else if (a.lane === 0 && b.lane > 0) {
      // Fork from main to side — cubic ease-out like git clients
      paths.push(
        <path
          key={`e-${pid++}`}
          d={`M ${x0} ${y0} C ${x0} ${y0 + (y1 - y0) * 0.55}, ${x1} ${y0 + (y1 - y0) * 0.35}, ${x1} ${y1}`}
          fill="none"
          stroke="hsl(var(--border))"
          strokeWidth={1.25}
          vectorEffect="non-scaling-stroke"
        />,
      );
    } else if (a.lane > 0 && b.lane === 0) {
      if (b.id === "compile") {
        // `compile` is a main-spine activity row, not a merge from the side branch above.
        // Drawing a merge curve here wrongly implied feature → main merge.
        continue;
      }
      // Merge back to main (e.g. merged branch tip onto main lane)
      paths.push(
        <path
          key={`e-${pid++}`}
          d={`M ${x0} ${y0} C ${x0} ${y0 + (y1 - y0) * 0.65}, ${x1} ${y0 + (y1 - y0) * 0.45}, ${x1} ${y1}`}
          fill="none"
          stroke="hsl(var(--border))"
          strokeWidth={1.25}
          vectorEffect="non-scaling-stroke"
        />,
      );
    } else {
      // Lane change side→side (nested branch)
      const mx = (x0 + x1) / 2;
      const midY = (y0 + y1) / 2;
      paths.push(
        <path
          key={`e-${pid++}`}
          d={`M ${x0} ${y0} C ${mx} ${midY}, ${mx} ${midY}, ${x1} ${y1}`}
          fill="none"
          stroke="hsl(var(--border))"
          strokeWidth={1.25}
          vectorEffect="non-scaling-stroke"
        />,
      );
    }
  }

  const dots = nodes.map((node, i) => {
    const cx = laneX(node.lane);
    const y = cy(i);
    const r = node.head ? 3.5 : 3;
    const fill = dotFill(node.tone);
    const stroke = node.head ? "hsl(var(--background))" : "none";
    const sw = node.head ? 1.75 : 0;
    const overlays: ReactElement[] = [];

    if (node.mergeGlyph) {
      overlays.push(
        <polygon
          key={`mg-${node.id}`}
          points={`${cx},${y - 4} ${cx + 4},${y} ${cx},${y + 4} ${cx - 4},${y}`}
          fill="hsl(var(--muted-foreground) / 0.55)"
          stroke="hsl(var(--background))"
          strokeWidth={1}
        />,
      );
    }
    if (node.pulsing) {
      overlays.push(
        <circle
          key={`pulse-${node.id}`}
          cx={cx}
          cy={y}
          r={r + 1}
          fill="none"
          stroke={fill}
          strokeWidth={1.5}
          className="branch-merge-pulse"
        />,
      );
    }

    return (
      <g key={node.id}>
        <circle
          cx={cx}
          cy={y}
          r={r}
          fill={fill}
          stroke={stroke}
          strokeWidth={sw}
        />
        {overlays}
      </g>
    );
  });

  return (
    <div
      className="relative shrink-0"
      style={{ width, height }}
      role="img"
      aria-label="Branch graph"
    >
      <svg
        width={width}
        height={height}
        className="pointer-events-none overflow-visible"
        aria-hidden
      >
        {/* Faint main rail — reads like Git even when only side rows sit between main tips */}
        <line
          x1={MAIN_X}
          y1={cy(0)}
          x2={MAIN_X}
          y2={cy(n - 1)}
          stroke="hsl(var(--border) / 0.45)"
          strokeWidth={1}
          vectorEffect="non-scaling-stroke"
        />
        {paths}
        {dots}
      </svg>
      {nodes.map((node, i) => (
        <Tooltip key={`tip-${node.id}`} delayDuration={200}>
          <TooltipTrigger asChild>
            <button
              type="button"
              className="absolute z-[1] size-8 -translate-x-1/2 -translate-y-1/2 rounded-full bg-transparent hover:bg-foreground/[0.06] focus:outline-none focus-visible:ring-1 focus-visible:ring-ring/50"
              style={{ left: laneX(node.lane), top: cy(i) }}
              aria-label={`Graph node: ${node.label || node.id}`}
            />
          </TooltipTrigger>
          <TooltipContent
            side="left"
            align="center"
            className="max-w-[min(260px,calc(100vw-48px))] whitespace-pre-wrap border-border/80 bg-surface-elevated px-2.5 py-2 font-mono text-[10px] leading-snug shadow-elevated"
          >
            {dotTooltipBody(node)}
          </TooltipContent>
        </Tooltip>
      ))}
    </div>
  );
}

const BRANCH_FORM_INPUT =
  "w-full min-w-0 rounded-md border border-border/40 bg-background/40 px-2 py-1.5 text-[11px] text-foreground outline-none transition-[border-color,background-color] placeholder:text-muted-foreground/50 focus:border-ring/40 focus:bg-background/70";

function BranchCreatePanel({
  workspace,
  branches,
  listLoading,
  onCreated,
  onDismiss,
}: {
  workspace: string;
  branches: BranchView[];
  listLoading: boolean;
  onCreated: () => void;
  onDismiss: () => void;
}) {
  const [name, setName] = useState("");
  const [parent, setParent] = useState("main");
  const [description, setDescription] = useState("");
  const [creating, setCreating] = useState(false);
  const parentSeeded = useRef(false);

  useEffect(() => {
    parentSeeded.current = false;
    setName("");
    setDescription("");
    setParent("main");
  }, [workspace]);

  useEffect(() => {
    if (branches.length === 0 || parentSeeded.current) return;
    const cur = branches.find((b) => b.current);
    const next = cur?.name ?? branches.find((b) => b.name === "main")?.name ?? "main";
    setParent(next);
    parentSeeded.current = true;
  }, [branches]);

  async function handleCreate() {
    const n = name.trim();
    if (!n) return;
    setCreating(true);
    try {
      await branchCreate({
        workspace,
        name: n,
        parent: parent.trim() || undefined,
        description: description.trim() || undefined,
      });
      toast(`Branch "${n}" created`, { kind: "success" });
      setName("");
      setDescription("");
      onCreated();
    } catch (e) {
      toast("Create branch failed", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
    } finally {
      setCreating(false);
    }
  }

  return (
    <div className="flex flex-col gap-2 border-b border-border/30 pb-3">
      <p className="text-[11px] font-medium text-foreground">New branch</p>

      <div className="space-y-0.5">
        <label
          htmlFor="compile-branch-name"
          className="text-[9px] font-medium uppercase tracking-wide text-muted-foreground"
        >
          Name
        </label>
        <input
          id="compile-branch-name"
          type="text"
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder="feature/review"
          disabled={listLoading || creating}
          className={BRANCH_FORM_INPUT}
          autoComplete="off"
          autoFocus
        />
      </div>

      <div className="space-y-0.5">
        <label
          htmlFor="compile-branch-parent"
          className="text-[9px] font-medium uppercase tracking-wide text-muted-foreground"
        >
          Parent
        </label>
        <input
          id="compile-branch-parent"
          type="text"
          value={parent}
          onChange={(e) => setParent(e.target.value)}
          placeholder="main"
          disabled={listLoading || creating}
          className={BRANCH_FORM_INPUT}
          autoComplete="off"
        />
      </div>

      <input
        id="compile-branch-desc"
        type="text"
        value={description}
        onChange={(e) => setDescription(e.target.value)}
        placeholder="Description (optional)"
        disabled={listLoading || creating}
        className={BRANCH_FORM_INPUT}
        autoComplete="off"
        aria-label="Branch description (optional)"
      />

      <div className="flex items-center justify-end gap-1 pt-0.5">
        <Button
          type="button"
          variant="ghost"
          size="sm"
          className="h-7 px-2 text-[11px] text-muted-foreground hover:text-foreground"
          disabled={creating}
          onClick={onDismiss}
        >
          Cancel
        </Button>
        <Button
          type="button"
          size="sm"
          disabled={listLoading || creating || !name.trim()}
          onClick={() => void handleCreate()}
          className="h-7 gap-1 border-0 bg-white px-2.5 text-[11px] font-medium text-neutral-950 shadow-none hover:bg-white/90"
        >
          {creating ? (
            <Loader2 className="size-3 animate-spin" aria-hidden />
          ) : (
            <Plus className="size-3" strokeWidth={2} aria-hidden />
          )}
          Create
        </Button>
      </div>
    </div>
  );

}

/** Pulls the merged branch name out of an SSE `branch_event` envelope.
 *  The engine emits `{"kind": "merged", ...}` post-T0.6 thanks to
 *  `#[serde(tag = "kind", rename_all = "snake_case")]`; older daemons
 *  emitted the single-key shape `{"Merged": ...}` which Cozo's serde
 *  occasionally still produces. Both are handled defensively. */
function extractMergedBranchName(envelope: BranchEventEnvelope): string | null {
  if (envelope.kind !== "event") return null;
  if (!branchListShouldRefresh(envelope.event)) return null;
  const ev = envelope.event;
  if (!ev || typeof ev !== "object") return null;
  const obj = ev as Record<string, unknown>;
  const tag =
    typeof obj.kind === "string"
      ? obj.kind.toLowerCase()
      : Object.keys(obj).find((k) => k.toLowerCase() === "merged")?.toLowerCase();
  if (tag !== "merged") return null;
  if (typeof envelope.branch === "string" && envelope.branch.length > 0) {
    return envelope.branch;
  }
  return null;
}

export function BranchResolutionRiver({ workspace }: { workspace: string }) {
  const compileProgress = useApp((s) => s.compileProgress);

  const [branches, setBranches] = useState<BranchView[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [createOpen, setCreateOpen] = useState(false);
  const [recentMerges, setRecentMerges] = useState<ReadonlySet<string>>(
    () => new Set<string>(),
  );

  useEffect(() => {
    setCreateOpen(false);
    setRecentMerges(new Set());
  }, [workspace]);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setBranches(await branchList(workspace));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [workspace]);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    const pulseTimers = new Map<string, ReturnType<typeof setTimeout>>();

    const triggerPulse = (name: string) => {
      setRecentMerges((prev) => {
        if (prev.has(name)) return prev;
        const next = new Set(prev);
        next.add(name);
        return next;
      });
      const existing = pulseTimers.get(name);
      if (existing) clearTimeout(existing);
      const t = setTimeout(() => {
        setRecentMerges((prev) => {
          if (!prev.has(name)) return prev;
          const next = new Set(prev);
          next.delete(name);
          return next;
        });
        pulseTimers.delete(name);
      }, MERGE_PULSE_MS);
      pulseTimers.set(name, t);
    };

    void (async () => {
      try {
        await branchEventSubscribe();
      } catch {
        return;
      }
      if (cancelled) return;
      unlisten = await onBranchEvent((env) => {
        const mergedName = extractMergedBranchName(env);
        if (mergedName) triggerPulse(mergedName);
        if (branchListNeedsRefetchFromEnvelope(env)) void load();
      });
    })();

    return () => {
      cancelled = true;
      unlisten?.();
      for (const t of pulseTimers.values()) clearTimeout(t);
      pulseTimers.clear();
    };
  }, [load]);

  const railSummary = useMemo(() => {
    let feature = 0;
    let sandbox = 0;
    let stream = 0;
    for (const b of branches) {
      const s = b.status.toLowerCase();
      if (s === "merged" || s === "abandoned") continue;
      const k = branchKindKey(b);
      if (k === "stream") stream++;
      else if (k === "sandbox") sandbox++;
      else if (k === "feature" || k === "tag") feature++;
    }
    const parts: string[] = [];
    const label = (n: number, singular: string) =>
      n === 1 ? singular : `${singular} · ${n}`;
    if (feature > 0) parts.push(label(feature, "Feature"));
    if (sandbox > 0) parts.push(label(sandbox, "Sandbox"));
    if (stream > 0) parts.push(label(stream, "Stream"));
    return parts;
  }, [branches]);

  const { nodes, emptyBranchList } = useMemo(() => {
    if (branches.length === 0) {
      return {
        nodes: mergeCompileIntoNodes(emptyGraphNodes(compileProgress), compileProgress),
        emptyBranchList: true as const,
      };
    }
    const built = buildGraphNodes(branches, compileProgress, recentMerges);
    return {
      nodes: mergeCompileIntoNodes(built, compileProgress),
      emptyBranchList: false as const,
    };
  }, [branches, compileProgress, recentMerges]);

  const onBranchClick = useCallback(
    async (name: string) => {
      try {
        await branchCheckout(workspace, name);
        toast(`HEAD → ${name}`, { kind: "success" });
        await load();
      } catch (e) {
        toast("Checkout failed", {
          kind: "error",
          body: e instanceof Error ? e.message : String(e),
        });
      }
    },
    [workspace, load],
  );

  return (
    <section className="flex flex-col gap-2">
      <div className="flex items-center justify-between gap-2">
        <h3 className="text-[10px] font-medium uppercase tracking-wider text-muted-foreground">
          Branch
        </h3>
        <div className="flex shrink-0 items-center gap-0.5">
          <Button
            type="button"
            variant="ghost"
            size="icon"
            className={cn(
              "h-6 w-6 text-muted-foreground hover:text-foreground",
              createOpen && "bg-muted text-foreground",
            )}
            onClick={() => setCreateOpen((o) => !o)}
            aria-label={createOpen ? "Close new branch form" : "New branch"}
            aria-expanded={createOpen}
            title={createOpen ? "Close" : "New branch"}
          >
            <GitBranchPlus className="size-3.5" strokeWidth={2} />
          </Button>
          <Button
            variant="ghost"
            size="icon"
            className="h-6 w-6 shrink-0 text-muted-foreground hover:text-foreground"
            onClick={() => void load()}
            aria-label="Reload branches"
          >
            <RefreshIcon className={cn("size-3", loading && "animate-spin")} />
          </Button>
        </div>
      </div>

      {createOpen && (
        <BranchCreatePanel
          workspace={workspace}
          branches={branches}
          listLoading={loading}
          onCreated={() => {
            void load();
            setCreateOpen(false);
          }}
          onDismiss={() => setCreateOpen(false)}
        />
      )}

      {emptyBranchList && (
        <p className="font-mono text-[10px] leading-snug text-muted-foreground/90">
          Empty branch list from the engine — nothing to invent. Graph grows up from{" "}
          <span className="text-foreground/80">main</span> once forks exist.
        </p>
      )}

      {error && <p className="font-mono text-[10px] text-destructive">{error}</p>}

      <div className="flex min-w-0 gap-0">
        <BranchGraphSvg nodes={nodes} growUp />
        <ul
          className="flex min-w-0 flex-1 flex-col-reverse"
          style={{
            paddingTop: PAD_Y,
            paddingBottom: PAD_Y,
          }}
        >
          {nodes.map((node) => {
            const clickable = Boolean(node.branch && !emptyBranchList);
            const label = (
              <span
                className={cn(
                  "flex min-h-[26px] items-center truncate font-mono text-[11px] leading-snug text-foreground/85",
                  node.tone === "merged" && "text-muted-foreground",
                  node.tone === "abandoned" && "text-muted-foreground/60 line-through",
                  node.pulsing && "text-foreground",
                )}
                title={node.label}
              >
                {node.label}
              </span>
            );
            return (
              <li
                key={node.id}
                className="flex min-h-[26px] items-center"
                style={{ height: ROW_H }}
              >
                {clickable ? (
                  <button
                    type="button"
                    onClick={() => node.branch && onBranchClick(node.branch.name)}
                    className="block w-full min-w-0 text-left hover:text-foreground"
                  >
                    {label}
                  </button>
                ) : (
                  label
                )}
              </li>
            );
          })}
        </ul>
      </div>

      {!emptyBranchList && branches.length > 1 && (
        <details className="text-[10px] text-muted-foreground">
          <summary className="cursor-pointer select-none font-mono hover:text-foreground">
            {railSummary.length > 0
              ? `${railSummary.join(" · ")} · ${branches.length} branches`
              : `${branches.length} branches`}
          </summary>
          <ul className="mt-1 max-h-32 overflow-y-auto border-t border-border/40 pt-1">
            {branches.map((b) => (
              <li key={b.name}>
                <button
                  type="button"
                  onClick={() => void onBranchClick(b.name)}
                  className={cn(
                    "flex w-full items-center gap-2 py-0.5 text-left font-mono text-[10px] hover:text-foreground",
                    b.current ? "text-foreground" : "text-muted-foreground",
                  )}
                >
                  <span className="size-1.5 shrink-0 rounded-full bg-muted-foreground/40" />
                  <span className="min-w-0 flex-1 truncate">{b.name}</span>
                </button>
              </li>
            ))}
          </ul>
        </details>
      )}
    </section>
  );
}
