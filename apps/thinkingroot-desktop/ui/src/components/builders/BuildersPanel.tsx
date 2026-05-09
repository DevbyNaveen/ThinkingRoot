import { useCallback, useEffect, useState } from "react";
import {
  AlertCircle,
  CheckCircle2,
  Database,
  Folder,
  Loader2,
  Package,
  RefreshCw,
  Search,
  Share2,
} from "lucide-react";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import {
  brainLoad,
  branchList,
  privacySummary,
  retrieveHybrid,
  workspaceCompile,
  type BrainEntity,
  type BrainRelation,
  type BrainSnapshot,
  type BranchView,
  type ClaimRow,
  type HybridResponse,
  type PrivacySource,
} from "@/lib/tauri";
import { useApp } from "@/store/app";
import { toast } from "@/store/toast";
import {
  pickPrimaryDiagnostic,
  substrateBadge,
  useWorkspaceConnection,
  useWorkspaceStatus,
  useWorkspaceStatusSubscription,
} from "@/store/workspace-status";

type DataTab = "sources" | "claims" | "entities" | "relations" | "branches" | "query";

const DATA_TABS: Array<{ id: DataTab; label: string }> = [
  { id: "sources", label: "Sources" },
  { id: "claims", label: "Claims" },
  { id: "entities", label: "Entities" },
  { id: "relations", label: "Relations" },
  { id: "branches", label: "Branches" },
  { id: "query", label: "Query" },
];

export function BuildersPanel({
  activeWorkspace,
}: {
  activeWorkspace: string | null;
}) {
  const setPackExportTarget = useApp((s) => s.setPackExportTarget);
  const setSurface = useApp((s) => s.setSurface);
  const [tab, setTab] = useState<DataTab>("claims");
  const [loading, setLoading] = useState(true);
  const [compiling, setCompiling] = useState(false);
  const [brain, setBrain] = useState<BrainSnapshot | null>(null);
  const [sources, setSources] = useState<PrivacySource[]>([]);
  const [branches, setBranches] = useState<BranchView[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [queryResult, setQueryResult] = useState<HybridResponse | null>(null);
  const [querying, setQuerying] = useState(false);

  useWorkspaceStatusSubscription(activeWorkspace);
  const workspaceStatus = useWorkspaceStatus(activeWorkspace);
  const connection = useWorkspaceConnection(activeWorkspace);

  const load = useCallback(async () => {
    if (!activeWorkspace) {
      setBrain(null);
      setSources([]);
      setBranches([]);
      setLoading(false);
      return;
    }

    setLoading(true);
    setError(null);
    try {
      const [snapshot, privacy, branchRows] = await Promise.all([
        brainLoad(),
        privacySummary(),
        branchList(activeWorkspace),
      ]);
      setBrain(snapshot);
      setSources(privacy.sources);
      setBranches(branchRows);
    } catch (err) {
      setBrain(null);
      setSources([]);
      setBranches([]);
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setLoading(false);
    }
  }, [activeWorkspace]);

  useEffect(() => {
    void load();
  }, [load]);

  if (!activeWorkspace) {
    return (
      <div className="flex flex-col gap-3 px-4 py-5 text-[11px] text-muted-foreground">
        <p>Select a workspace in the sidebar to inspect its backend data.</p>
      </div>
    );
  }

  const badge = substrateBadge(workspaceStatus);
  const queryDiag = pickPrimaryDiagnostic(workspaceStatus, "for_query");
  const exportDiag = pickPrimaryDiagnostic(workspaceStatus, "for_export");
  const canQuery = workspaceStatus?.readiness.for_query ?? false;
  const canExport = workspaceStatus?.readiness.for_export ?? false;
  const canCompile = workspaceStatus?.readiness.for_compile ?? true;

  const graphBytes =
    workspaceStatus?.substrate.kind === "populated" ||
    workspaceStatus?.substrate.kind === "empty"
      ? workspaceStatus.substrate.graph_db_bytes
      : null;
  const claimCount =
    workspaceStatus?.substrate.kind === "populated"
      ? workspaceStatus.substrate.claim_count
      : 0;
  const entityCount =
    workspaceStatus?.substrate.kind === "populated"
      ? workspaceStatus.substrate.entity_count
      : 0;
  const sourceCount =
    workspaceStatus?.sources.kind === "some"
      ? workspaceStatus.sources.file_count
      : 0;

  const filteredClaims = filterClaims(brain?.claims ?? [], query);
  const filteredEntities = filterEntities(brain?.entities ?? [], query);
  const filteredSources = filterSources(sources, query);
  const filteredRelations = filterRelations(brain?.relations ?? [], query);

  async function runQuery() {
    const q = query.trim();
    if (!q) return;
    setQuerying(true);
    setQueryResult(null);
    try {
      setQueryResult(await retrieveHybrid({ query: q, topK: 10 }));
    } catch (err) {
      toast("Query failed", {
        kind: "error",
        body: err instanceof Error ? err.message : String(err),
      });
    } finally {
      setQuerying(false);
    }
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col overflow-y-auto px-4 py-4">
      <section className="rounded-2xl border border-border/70 bg-background/35 p-3.5 shadow-sm">
        <div className="flex items-start gap-2">
          <div className="flex size-8 shrink-0 items-center justify-center rounded-xl bg-accent/12 text-accent">
            <Database className="size-4" />
          </div>
          <div className="min-w-0 flex-1">
            <div className="text-xs font-semibold text-foreground">
              Brain Data Explorer
            </div>
            <p className="mt-1 text-[11px] leading-snug text-muted-foreground">
              Inspect the real sources, claims, entities, relations, branches,
              and query results your app receives from this workspace.
            </p>
          </div>
          <Button
            type="button"
            variant="ghost"
            size="icon"
            className="h-7 w-7 text-muted-foreground"
            onClick={() => void load()}
            aria-label="Refresh builder data"
          >
            <RefreshCw className={cn("size-3.5", loading && "animate-spin")} />
          </Button>
        </div>

        <div className="mt-3 grid grid-cols-2 gap-2">
          <Metric label="Claims" value={String(claimCount)} />
          <Metric label="Entities" value={String(entityCount)} />
          <Metric label="Sources" value={String(sourceCount)} />
          <Metric label="Graph" value={graphBytes === null ? "n/a" : formatBytes(graphBytes)} />
        </div>

        <div className="mt-3 flex flex-wrap gap-1.5">
          <StatusPill ok={Boolean(brain)} label={brain ? "Data loaded" : "Data not loaded"} />
          <StatusPill ok={canQuery} label={canQuery ? "Queryable" : "Not queryable"} />
          <StatusPill ok={canExport} label={canExport ? "Export ready" : "Export blocked"} />
          <span
            className={cn(
              "rounded-full px-2 py-1 text-[10px] font-medium",
              badge.tone === "ok"
                ? "bg-emerald-500/15 text-emerald-400"
                : badge.tone === "warn"
                  ? "bg-amber-500/15 text-amber-400"
                  : badge.tone === "error"
                    ? "bg-rose-500/15 text-rose-400"
                    : "bg-muted/45 text-muted-foreground",
            )}
          >
            {badge.label}
          </span>
        </div>

        {!connection.connected && connection.lastSeenMs && (
          <p className="mt-2 text-[10px] text-muted-foreground">
            Status stream disconnected, last seen{" "}
            {Math.round((Date.now() - connection.lastSeenMs) / 1000)}s ago.
          </p>
        )}
      </section>

      {(queryDiag || exportDiag) && (
        <section className="mt-3 rounded-xl border border-border/70 bg-muted/20 px-3 py-2.5">
          <div className="flex items-start gap-2">
            <AlertCircle
              className={cn(
                "mt-0.5 size-3.5 shrink-0",
                (queryDiag ?? exportDiag)?.severity === "error"
                  ? "text-rose-400"
                  : "text-amber-400",
              )}
            />
            <p className="text-[11px] leading-snug text-muted-foreground">
              {(queryDiag ?? exportDiag)?.message}
            </p>
          </div>
        </section>
      )}

      <section className="mt-4 flex flex-col gap-2">
        <div className="flex flex-wrap gap-1.5">
          {DATA_TABS.map((item) => (
            <button
              key={item.id}
              type="button"
              onClick={() => setTab(item.id)}
              className={cn(
                "rounded-lg border px-2.5 py-1.5 text-[10px] transition-colors",
                tab === item.id
                  ? "border-accent bg-accent/15 text-accent"
                  : "border-border/70 text-muted-foreground hover:text-foreground",
              )}
            >
              {item.label}
            </button>
          ))}
        </div>
        <div className="relative">
          <Search className="pointer-events-none absolute left-2.5 top-2.5 size-3.5 text-muted-foreground/70" />
          <input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && tab === "query") void runQuery();
            }}
            placeholder={
              tab === "query"
                ? "Ask the backend with hybrid retrieval..."
                : `Filter ${DATA_TABS.find((t) => t.id === tab)?.label.toLowerCase()}...`
            }
            className="h-9 w-full rounded-xl border border-border/70 bg-background/60 pl-8 pr-3 text-xs outline-none transition-colors placeholder:text-muted-foreground/50 focus:border-accent/70"
          />
        </div>
      </section>

      <section className="mt-3 min-h-[18rem] rounded-2xl border border-border/70 bg-background/25">
        <div className="flex items-center justify-between border-b border-border/50 px-3 py-2">
          <div className="text-[10px] font-semibold uppercase tracking-widest text-muted-foreground/70">
            {DATA_TABS.find((t) => t.id === tab)?.label}
          </div>
          <span className="font-mono text-[10px] text-muted-foreground">
            {countForTab(tab, {
              sources: filteredSources.length,
              claims: filteredClaims.length,
              entities: filteredEntities.length,
              relations: filteredRelations.length,
              branches: branches.length,
              query: queryResult?.hits.length ?? 0,
            })}
          </span>
        </div>
        <div className="max-h-[28rem] overflow-y-auto">
          {loading && <StateLine icon="loading" text="Loading backend data..." />}
          {!loading && error && <StateLine icon="error" text={error} />}
          {!loading && !error && tab === "sources" && <SourcesTable rows={filteredSources} />}
          {!loading && !error && tab === "claims" && <ClaimsTable rows={filteredClaims} />}
          {!loading && !error && tab === "entities" && <EntitiesTable rows={filteredEntities} />}
          {!loading && !error && tab === "relations" && <RelationsTable rows={filteredRelations} />}
          {!loading && !error && tab === "branches" && <BranchesTable rows={branches} />}
          {!loading && !error && tab === "query" && (
            <QueryPanel
              canQuery={canQuery}
              query={query}
              querying={querying}
              result={queryResult}
              onRun={() => void runQuery()}
            />
          )}
        </div>
      </section>

      <section className="mt-4 flex flex-col gap-2">
        <Button
          type="button"
          variant="default"
          size="sm"
          className="h-8 justify-center gap-1.5 rounded-xl text-xs"
          disabled={compiling || !canCompile}
          onClick={async () => {
            setCompiling(true);
            try {
              await workspaceCompile({ target: activeWorkspace });
              toast("Compile queued", {
                kind: "info",
                body: "The data explorer updates after compile completes.",
              });
            } catch (err) {
              toast("Compile failed", {
                kind: "error",
                body: err instanceof Error ? err.message : String(err),
              });
            } finally {
              setCompiling(false);
            }
          }}
        >
          {compiling ? (
            <Loader2 className="size-3.5 animate-spin" />
          ) : (
            <RefreshCw className="size-3.5" />
          )}
          Compile backend data
        </Button>
        <div className="grid grid-cols-2 gap-2">
          <Button
            type="button"
            variant="outline"
            size="sm"
            className="h-8 justify-center gap-1.5 rounded-xl text-xs"
            disabled={!canExport}
            onClick={() => setPackExportTarget({ workspace: activeWorkspace })}
          >
            <Package className="size-3.5" />
            Export .tr
          </Button>
          <Button
            type="button"
            variant="outline"
            size="sm"
            className="h-8 justify-center gap-1.5 rounded-xl text-xs"
            onClick={() => setSurface("docs")}
          >
            <Share2 className="size-3.5" />
            Connect docs
          </Button>
        </div>
      </section>
    </div>
  );
}

function Metric({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded-xl bg-muted/30 px-2.5 py-2">
      <div className="text-[9px] uppercase tracking-wider text-muted-foreground/70">
        {label}
      </div>
      <div className="mt-0.5 truncate font-mono text-[12px] text-foreground">
        {value}
      </div>
    </div>
  );
}

function StatusPill({ ok, label }: { ok: boolean; label: string }) {
  return (
    <span
      className={cn(
        "inline-flex items-center gap-1 rounded-full px-2 py-1 text-[10px] font-medium",
        ok ? "bg-emerald-500/15 text-emerald-400" : "bg-amber-500/15 text-amber-400",
      )}
    >
      {ok ? <CheckCircle2 className="size-3" /> : <AlertCircle className="size-3" />}
      {label}
    </span>
  );
}

function SourcesTable({ rows }: { rows: PrivacySource[] }) {
  if (rows.length === 0) return <StateLine icon="empty" text="No sources in this workspace." />;
  return (
    <div className="divide-y divide-border/40">
      {rows.slice(0, 120).map((s) => (
        <Row key={s.id} title={s.uri} meta={`${s.source_type} · ${shortId(s.id)}`} />
      ))}
    </div>
  );
}

function ClaimsTable({ rows }: { rows: ClaimRow[] }) {
  if (rows.length === 0) return <StateLine icon="empty" text="No claims match this view." />;
  return (
    <div className="divide-y divide-border/40">
      {rows.slice(0, 120).map((claim) => (
        <Row
          key={claim.id}
          title={claim.statement}
          meta={`${claim.tier} · ${(claim.confidence * 100).toFixed(0)}% · ${claim.source}`}
        />
      ))}
    </div>
  );
}

function EntitiesTable({ rows }: { rows: BrainEntity[] }) {
  if (rows.length === 0) return <StateLine icon="empty" text="No entities match this view." />;
  return (
    <div className="divide-y divide-border/40">
      {rows.slice(0, 120).map((entity) => (
        <Row
          key={`${entity.entity_type}:${entity.name}`}
          title={entity.name}
          meta={`${entity.entity_type} · ${entity.claim_count} claim${entity.claim_count === 1 ? "" : "s"}`}
        />
      ))}
    </div>
  );
}

function RelationsTable({ rows }: { rows: BrainRelation[] }) {
  if (rows.length === 0) return <StateLine icon="empty" text="No relations match this view." />;
  return (
    <div className="divide-y divide-border/40">
      {rows.slice(0, 120).map((edge, idx) => (
        <Row
          key={`${edge.source}:${edge.relation_type}:${edge.target}:${idx}`}
          title={`${edge.source} → ${edge.target}`}
          meta={`${edge.relation_type} · strength ${edge.strength.toFixed(2)}`}
        />
      ))}
    </div>
  );
}

function BranchesTable({ rows }: { rows: BranchView[] }) {
  if (rows.length === 0) return <StateLine icon="empty" text="No branches in this workspace." />;
  return (
    <div className="divide-y divide-border/40">
      {rows.map((branch) => (
        <Row
          key={branch.name}
          title={branch.name}
          meta={`${branch.current ? "current · " : ""}${branch.status}${branch.description ? ` · ${branch.description}` : ""}`}
        />
      ))}
    </div>
  );
}

function QueryPanel({
  canQuery,
  query,
  querying,
  result,
  onRun,
}: {
  canQuery: boolean;
  query: string;
  querying: boolean;
  result: HybridResponse | null;
  onRun: () => void;
}) {
  return (
    <div className="p-3">
      <Button
        type="button"
        size="sm"
        className="h-8 w-full rounded-xl text-xs"
        disabled={!canQuery || !query.trim() || querying}
        onClick={onRun}
      >
        {querying ? <Loader2 className="size-3.5 animate-spin" /> : <Search className="size-3.5" />}
        Run hybrid query
      </Button>
      {!canQuery && (
        <p className="mt-2 text-[11px] text-amber-300/90">
          This workspace must be loaded and compiled before it can answer backend queries.
        </p>
      )}
      {result && (
        <div className="mt-3 divide-y divide-border/40 rounded-xl border border-border/50">
          {result.hits.length === 0 ? (
            <StateLine icon="empty" text="No hits returned for this query." />
          ) : (
            result.hits.map((hit) => (
              <Row
                key={hit.claim_id}
                title={hit.statement}
                meta={`${hit.admission_tier} · score ${hit.fused_score.toFixed(3)} · ${shortId(hit.claim_id)}`}
              />
            ))
          )}
        </div>
      )}
    </div>
  );
}

function Row({ title, meta }: { title: string; meta: string }) {
  return (
    <div className="px-3 py-2.5">
      <div className="line-clamp-2 text-[11px] leading-snug text-foreground" title={title}>
        {title}
      </div>
      <div className="mt-1 truncate font-mono text-[9.5px] text-muted-foreground" title={meta}>
        {meta}
      </div>
    </div>
  );
}

function StateLine({
  icon,
  text,
}: {
  icon: "loading" | "error" | "empty";
  text: string;
}) {
  return (
    <div className="flex items-center gap-2 px-3 py-4 text-[11px] text-muted-foreground">
      {icon === "loading" && <Loader2 className="size-3.5 animate-spin" />}
      {icon === "error" && <AlertCircle className="size-3.5 text-rose-400" />}
      {icon === "empty" && <Folder className="size-3.5" />}
      <span>{text}</span>
    </div>
  );
}

function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "n/a";
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KiB", "MiB", "GiB", "TiB"];
  let value = bytes / 1024;
  let unit = units[0]!;
  for (let i = 1; i < units.length && value >= 1024; i += 1) {
    value /= 1024;
    unit = units[i]!;
  }
  return `${value >= 10 ? value.toFixed(0) : value.toFixed(1)} ${unit}`;
}

function filterClaims(rows: ClaimRow[], filter: string): ClaimRow[] {
  const q = filter.trim().toLowerCase();
  if (!q) return rows;
  return rows.filter((r) =>
    `${r.statement} ${r.source} ${r.tier} ${r.claim_type ?? ""}`.toLowerCase().includes(q),
  );
}

function filterEntities(rows: BrainEntity[], filter: string): BrainEntity[] {
  const q = filter.trim().toLowerCase();
  if (!q) return rows;
  return rows.filter((r) => `${r.name} ${r.entity_type}`.toLowerCase().includes(q));
}

function filterSources(rows: PrivacySource[], filter: string): PrivacySource[] {
  const q = filter.trim().toLowerCase();
  if (!q) return rows;
  return rows.filter((r) => `${r.uri} ${r.source_type} ${r.id}`.toLowerCase().includes(q));
}

function filterRelations(rows: BrainRelation[], filter: string): BrainRelation[] {
  const q = filter.trim().toLowerCase();
  if (!q) return rows;
  return rows.filter((r) =>
    `${r.source} ${r.target} ${r.relation_type}`.toLowerCase().includes(q),
  );
}

function countForTab(tab: DataTab, counts: Record<DataTab, number>): string {
  const count = counts[tab];
  return `${count} ${count === 1 ? "row" : "rows"}`;
}

function shortId(id: string): string {
  return id.length <= 12 ? id : `${id.slice(0, 6)}…${id.slice(-4)}`;
}
