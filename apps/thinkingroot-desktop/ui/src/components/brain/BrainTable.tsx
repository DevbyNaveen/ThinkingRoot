import { useMemo, useRef, useState } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { Search } from "lucide-react";
import { cn } from "@/lib/utils";
import type { ClaimRow } from "@/lib/tauri";

interface Props {
  claims: ClaimRow[];
}

const TIER_BADGE: Record<ClaimRow["tier"], string> = {
  rooted: "bg-tier-rooted/15 text-tier-rooted border-tier-rooted/30",
  attested: "bg-tier-attested/15 text-tier-attested border-tier-attested/30",
  unknown: "bg-tier-unknown/15 text-tier-unknown border-tier-unknown/30",
};

/**
 * Virtualized table of claims. Built on `@tanstack/react-virtual`
 * so the render cost stays constant with 10k+ rows. Header stays
 * pinned via CSS sticky.
 */
export function BrainTable({ claims }: Props) {
  const [query, setQuery] = useState("");
  const [tierFilter, setTierFilter] = useState<
    ClaimRow["tier"] | "all"
  >("all");
  const parentRef = useRef<HTMLDivElement | null>(null);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    return claims.filter((c) => {
      if (tierFilter !== "all" && c.tier !== tierFilter) return false;
      if (!q) return true;
      return (
        c.id.toLowerCase().includes(q) ||
        c.statement.toLowerCase().includes(q) ||
        c.source.toLowerCase().includes(q) ||
        (c.claim_type ?? "").toLowerCase().includes(q)
      );
    });
  }, [claims, query, tierFilter]);

  const rowVirtualizer = useVirtualizer({
    count: filtered.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => 48,
    overscan: 10,
  });

  return (
    <div className="flex h-full flex-col">
      <header className="flex shrink-0 items-center gap-2 border-b border-border bg-surface px-3 py-2">
        <div className="relative flex-1">
          <Search className="pointer-events-none absolute left-2 top-1/2 size-3.5 -translate-y-1/2 text-muted-foreground" />
          <input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="Filter by id, statement, source, or type"
            className={cn(
              "h-8 w-full rounded-md border border-input bg-background pl-7 pr-2 text-xs",
              "placeholder:text-muted-foreground focus:border-accent focus:outline-none focus:ring-1 focus:ring-accent/40",
            )}
          />
        </div>
        <TierSelect value={tierFilter} onChange={setTierFilter} />
        <span className="text-[11px] text-muted-foreground">
          {filtered.length}/{claims.length} rows
        </span>
      </header>

      <div
        ref={parentRef}
        className="relative flex-1 overflow-auto"
        role="table"
        aria-label="Claims table"
      >
        <TableHeader />
        <div
          style={{
            height: `${rowVirtualizer.getTotalSize()}px`,
            position: "relative",
          }}
        >
          {rowVirtualizer.getVirtualItems().map((virtualRow) => {
            const row = filtered[virtualRow.index];
            if (!row) return null;
            return (
              <div
                key={virtualRow.key}
                role="row"
                className="absolute inset-x-0 grid grid-cols-[110px_90px_1fr_180px_90px] items-center gap-3 border-b border-border/50 px-3 text-xs"
                style={{
                  top: 0,
                  height: virtualRow.size,
                  transform: `translateY(${virtualRow.start}px)`,
                }}
              >
                <span className="truncate font-mono text-[11px] text-foreground">
                  {row.id}
                </span>
                <span
                  className={cn(
                    "inline-flex w-fit items-center gap-1 rounded-full border px-2 py-0.5 text-[10px] capitalize",
                    TIER_BADGE[row.tier],
                  )}
                >
                  {row.tier} · {row.confidence.toFixed(2)}
                </span>
                <span className="truncate text-foreground/80" title={row.statement}>
                  {row.statement}
                </span>
                <span
                  className="truncate font-mono text-[10px] text-muted-foreground"
                  title={row.source}
                >
                  {row.source}
                </span>
                <span className="truncate text-[10px] text-muted-foreground">
                  {row.claim_type}
                </span>
              </div>
            );
          })}
          {filtered.length === 0 && (
            <div className="absolute inset-x-0 top-20 text-center text-xs text-muted-foreground">
              No rows match the current filter.
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

function TableHeader() {
  return (
    <div
      role="row"
      className="sticky top-0 z-10 grid grid-cols-[110px_90px_1fr_180px_90px] items-center gap-3 border-b border-border bg-surface px-3 py-2 text-[10px] font-semibold uppercase tracking-widest text-muted-foreground"
    >
      <span>Claim ID</span>
      <span>Tier</span>
      <span>Statement</span>
      <span>Source</span>
      <span>Type</span>
    </div>
  );
}

function TierSelect({
  value,
  onChange,
}: {
  value: ClaimRow["tier"] | "all";
  onChange: (v: ClaimRow["tier"] | "all") => void;
}) {
  const options: Array<ClaimRow["tier"] | "all"> = [
    "all",
    "rooted",
    "attested",
    "unknown",
  ];
  return (
    <div
      role="radiogroup"
      aria-label="Filter by tier"
      className="flex items-center gap-0.5 rounded-md border border-border bg-surface p-0.5"
    >
      {options.map((opt) => {
        const active = value === opt;
        return (
          <button
            key={opt}
            type="button"
            role="radio"
            aria-checked={active}
            onClick={() => onChange(opt)}
            className={cn(
              "rounded px-2 py-1 text-[10px] capitalize transition-colors",
              active
                ? "bg-accent text-accent-foreground"
                : "text-muted-foreground hover:text-foreground",
            )}
          >
            {opt}
          </button>
        );
      })}
    </div>
  );
}
