import { useCallback, useEffect, useMemo, useState } from "react";
import { FileSearch, X } from "lucide-react";

import {
  playgroundSourceWitnesses,
  type PlaygroundWitnessRow,
} from "@/lib/tauri";
import { cn } from "@/lib/utils";

/**
 * SourceDetailPanel — right-side slide-over showing every witness
 * anchored to a single source row. Opens when a user clicks a row in
 * the SourceLibrary; closes on the explicit `×` button or by
 * clicking another row.
 *
 * Surfaces:
 * - Rule name (the catalog entry that fired)
 * - Witness type (the produced category, e.g. `image::phash`)
 * - Symbol (function/type name for code; whole-file feature payload
 *   for image/audio rules — exact same `Witness.symbol` the engine
 *   wrote)
 * - Confidence (rule catalog default at write time)
 * - Byte range `[start..end)` of the span
 *
 * Witnesses are grouped by `witness_type` so a research user sees
 * "this PDF produced 5 tree-sitter::function-decl witnesses + 12
 * markdown::heading witnesses" at a glance.
 */
export function SourceDetailPanel({
  sourceId,
  sourceUri,
  onClose,
}: {
  sourceId: string | null;
  sourceUri: string | null;
  onClose: () => void;
}) {
  const [rows, setRows] = useState<PlaygroundWitnessRow[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    if (!sourceId) {
      setRows(null);
      setError(null);
      return;
    }
    setLoading(true);
    setError(null);
    try {
      const fresh = await playgroundSourceWitnesses(sourceId);
      setRows(fresh);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setRows(null);
    } finally {
      setLoading(false);
    }
  }, [sourceId]);

  useEffect(() => {
    void load();
  }, [load]);

  const grouped = useMemo(() => groupByType(rows ?? []), [rows]);

  if (!sourceId) return null;

  return (
    <aside className="flex h-full w-80 shrink-0 flex-col border-l border-border bg-surface/30">
      <header className="flex shrink-0 items-center justify-between gap-2 border-b border-border px-3 py-2">
        <div className="flex min-w-0 items-center gap-2">
          <FileSearch className="size-4 text-muted-foreground" />
          <h3 className="truncate text-xs font-semibold uppercase tracking-wide text-muted-foreground">
            Source detail
          </h3>
        </div>
        <button
          type="button"
          onClick={onClose}
          aria-label="Close detail panel"
          className="rounded-md p-1 text-muted-foreground transition-colors hover:bg-muted/60 hover:text-foreground"
        >
          <X className="size-3.5" />
        </button>
      </header>
      <div className="border-b border-border px-3 py-2 text-xs">
        <p className="truncate font-medium" title={sourceUri ?? ""}>
          {basename(sourceUri ?? "")}
        </p>
        <p className="mt-0.5 truncate text-muted-foreground" title={sourceUri ?? ""}>
          {sourceUri}
        </p>
      </div>
      <div className="flex-1 overflow-auto">
        {error ? (
          <p className="px-3 py-3 text-xs text-destructive">{error}</p>
        ) : loading && !rows ? (
          <p className="px-3 py-3 text-xs text-muted-foreground">Loading…</p>
        ) : rows && rows.length === 0 ? (
          <p className="px-3 py-3 text-xs text-muted-foreground">
            No witnesses for this source. The extractor produced
            nothing — either the format isn't covered by a rule, or
            the file failed to decode (check `audio::skipped@v1` /
            `image::skipped@v1` in the all-witnesses listing).
          </p>
        ) : (
          <ul className="flex flex-col py-1">
            {grouped.map((group) => (
              <li key={group.witness_type} className="mt-2 first:mt-0">
                <p className="px-3 pb-1 text-[10px] font-semibold uppercase tracking-wider text-muted-foreground/70">
                  {group.witness_type} ({group.rows.length})
                </p>
                <ul>
                  {group.rows.map((w) => (
                    <WitnessRow key={w.id} witness={w} />
                  ))}
                </ul>
              </li>
            ))}
          </ul>
        )}
      </div>
    </aside>
  );
}

function WitnessRow({ witness: w }: { witness: PlaygroundWitnessRow }) {
  return (
    <li className="group flex flex-col gap-0.5 px-3 py-1.5 text-xs hover:bg-muted/40">
      <div className="flex items-center gap-2">
        <span
          className={cn(
            "shrink-0 rounded px-1 py-px font-mono text-[9px] uppercase",
            "bg-muted/50 text-muted-foreground",
          )}
          title="Rule that fired"
        >
          {familyOf(w.rule)}
        </span>
        <span className="min-w-0 flex-1 truncate font-medium">
          {w.symbol && w.symbol.trim() !== "" ? w.symbol : "(unnamed)"}
        </span>
        <span
          className="shrink-0 font-mono text-[10px] text-muted-foreground"
          title="Catalog confidence"
        >
          {(w.confidence * 100).toFixed(0)}%
        </span>
      </div>
      <div className="flex items-center justify-between gap-2 text-[10px] text-muted-foreground">
        <span className="truncate font-mono" title={w.rule}>
          {w.rule}
        </span>
        <span className="shrink-0 font-mono">
          {w.byte_start}–{w.byte_end}
        </span>
      </div>
    </li>
  );
}

function groupByType(
  rows: PlaygroundWitnessRow[],
): { witness_type: string; rows: PlaygroundWitnessRow[] }[] {
  const m = new Map<string, PlaygroundWitnessRow[]>();
  for (const r of rows) {
    const key = r.witness_type || "(unknown)";
    const arr = m.get(key) ?? [];
    arr.push(r);
    m.set(key, arr);
  }
  // Stable: sort groups by type name; within each, by byte_start.
  const out: { witness_type: string; rows: PlaygroundWitnessRow[] }[] = [];
  for (const [witness_type, arr] of [...m.entries()].sort((a, b) =>
    a[0].localeCompare(b[0]),
  )) {
    arr.sort((a, b) => a.byte_start - b.byte_start);
    out.push({ witness_type, rows: arr });
  }
  return out;
}

function familyOf(rule: string): string {
  const idx = rule.indexOf("::");
  if (idx < 0) return rule.slice(0, 8);
  return rule.slice(0, idx);
}

function basename(uri: string): string {
  const trimmed = uri.replace(/^file:\/\//, "");
  return trimmed.split(/[\/\\]/).pop() || trimmed;
}
