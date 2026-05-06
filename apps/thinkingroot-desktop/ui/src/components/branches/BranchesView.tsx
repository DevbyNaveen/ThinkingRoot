/**
 * Branches workbench — surfaces the engine's branch / tag / proposal /
 * template control plane to end users.
 *
 * Tabs:
 *   - Branches  — list, create, diff/merge/rebase/rollback per row
 *   - Tags      — T2.5 immutable snapshot tags
 *   - Proposals — T0.4 Knowledge Proposal lifecycle
 *   - Templates — T3.7 branch templates (list + apply)
 *
 * Every action routes through Tauri commands → `SidecarClient` → daemon
 * REST. Nothing in this component opens `graph.db`. Errors surface as
 * toasts with the daemon's structured error code visible in the body.
 */
import { useCallback, useEffect, useState } from "react";
import {
  GitBranch,
  GitMerge,
  History,
  Plus,
  RotateCcw,
  Tag as TagIcon,
  Trash2,
  Workflow,
  RefreshCw,
  AlertTriangle,
  CheckCircle2,
  CircleDot,
  Layers,
} from "lucide-react";

import { Button } from "@/components/ui/button";
import { useApp } from "@/store/app";
import { toast } from "@/store/toast";
import { cn } from "@/lib/utils";
import {
  branchList,
  branchCreate,
  branchDelete,
  branchMerge,
  branchRebase,
  branchRollback,
  branchStats,
  type BranchStats,
  type BranchView,
  tagList,
  tagCreate,
  type TagView,
  proposalList,
  proposalOpen,
  proposalReview,
  proposalClose,
  type ProposalView,
  type ProposalDecision,
  branchTemplateList,
  branchTemplateApply,
  type BranchTemplateInfo,
} from "@/lib/tauri";

type Tab = "branches" | "tags" | "proposals" | "templates";

const TABS: Array<{ id: Tab; label: string; icon: typeof GitBranch }> = [
  { id: "branches", label: "Branches", icon: GitBranch },
  { id: "tags", label: "Tags", icon: TagIcon },
  { id: "proposals", label: "Proposals", icon: Workflow },
  { id: "templates", label: "Templates", icon: Layers },
];

export function BranchesView() {
  const activeWorkspace = useApp((s) => s.activeWorkspace);
  const [tab, setTab] = useState<Tab>("branches");

  if (!activeWorkspace) {
    return (
      <div className="flex h-full flex-col items-center justify-center gap-2 px-8 text-center">
        <h2 className="text-base font-medium">No workspace selected</h2>
        <p className="max-w-sm text-sm text-muted-foreground">
          Pick a workspace from the sidebar to manage its branches, tags,
          proposals, and templates.
        </p>
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col">
      <header className="flex h-11 shrink-0 items-center gap-2 border-b border-border px-4">
        <GitBranch className="size-4 text-muted-foreground" />
        <span className="text-sm font-medium">{activeWorkspace}</span>
        <span className="text-muted-foreground">·</span>
        <span className="text-xs text-muted-foreground">Branches</span>
      </header>

      <nav className="flex items-center gap-1 border-b border-border px-2 pt-1.5">
        {TABS.map(({ id, label, icon: Icon }) => (
          <button
            key={id}
            type="button"
            onClick={() => setTab(id)}
            className={cn(
              "flex items-center gap-1.5 rounded-t-md px-3 py-1.5 text-xs transition-colors",
              tab === id
                ? "border border-b-background border-border bg-background text-foreground"
                : "text-muted-foreground hover:text-foreground",
            )}
          >
            <Icon className="size-3.5" />
            {label}
          </button>
        ))}
      </nav>

      <div className="flex-1 overflow-hidden">
        {tab === "branches" && <BranchesPanel workspace={activeWorkspace} />}
        {tab === "tags" && <TagsPanel />}
        {tab === "proposals" && <ProposalsPanel />}
        {tab === "templates" && <TemplatesPanel />}
      </div>
    </div>
  );
}

// ─── Branches panel ──────────────────────────────────────────────────

function BranchesPanel({ workspace }: { workspace: string }) {
  const [branches, setBranches] = useState<BranchView[] | null>(null);
  const [statsByName, setStatsByName] = useState<Record<string, BranchStats>>({});
  const [loading, setLoading] = useState(false);
  const [creating, setCreating] = useState(false);
  const [newName, setNewName] = useState("");
  const [newDescription, setNewDescription] = useState("");
  const [newParent, setNewParent] = useState("main");

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const list = await branchList(workspace);
      setBranches(list);
      // Fetch stats lazily for each branch — cheap probe, runs in parallel.
      const pairs = await Promise.allSettled(list.map((b) => branchStats(b.name)));
      const next: Record<string, BranchStats> = {};
      pairs.forEach((p, i) => {
        const branchEntry = list[i];
        if (p.status === "fulfilled" && branchEntry) {
          next[branchEntry.name] = p.value;
        }
      });
      setStatsByName(next);
    } catch (e) {
      toast("Branch list failed", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
      setBranches([]);
    } finally {
      setLoading(false);
    }
  }, [workspace]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  async function handleCreate() {
    if (!newName.trim()) {
      toast("Branch name required", { kind: "warn" });
      return;
    }
    setCreating(true);
    try {
      await branchCreate({
        workspace,
        name: newName.trim(),
        parent: newParent || undefined,
        description: newDescription.trim() || undefined,
      });
      toast(`Branch ${newName.trim()} created`, { kind: "success" });
      setNewName("");
      setNewDescription("");
      await refresh();
    } catch (e) {
      toast("Branch create failed", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
    } finally {
      setCreating(false);
    }
  }

  async function handleMerge(name: string) {
    if (!confirm(`Merge ${name} into main? Health gate runs first.`)) return;
    try {
      const result = await branchMerge({ workspace, name });
      if (result.merged) {
        toast(`Merged ${name} → main`, {
          kind: "success",
          body: `${result.new_claims} new · ${result.auto_resolved} auto-resolved · ${result.conflicts} conflicts`,
        });
      } else {
        toast(`Merge blocked: ${name}`, {
          kind: "warn",
          body: result.blocking_reasons.join("; ") || "Health gate failed",
        });
      }
      await refresh();
    } catch (e) {
      toast("Merge failed", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
    }
  }

  async function handleRebase(name: string) {
    try {
      await branchRebase(name);
      toast(`Rebased ${name}`, { kind: "success" });
      await refresh();
    } catch (e) {
      toast("Rebase failed", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
    }
  }

  async function handleRollback(name: string) {
    if (
      !confirm(
        `Rollback main to its pre-merge snapshot of ${name}? This reverts the most recent merge of this branch.`,
      )
    ) {
      return;
    }
    try {
      await branchRollback(name);
      toast(`Rollback applied`, {
        kind: "success",
        body: `Main restored to pre-${name} state`,
      });
      await refresh();
    } catch (e) {
      toast("Rollback failed", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
    }
  }

  async function handleDelete(name: string) {
    if (
      !confirm(
        `Abandon branch ${name}? Data is kept on disk; you can re-activate via the registry.`,
      )
    ) {
      return;
    }
    try {
      await branchDelete(workspace, name);
      toast(`Abandoned ${name}`, { kind: "success" });
      await refresh();
    } catch (e) {
      toast("Delete failed", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
    }
  }

  return (
    <div className="flex h-full flex-col gap-4 overflow-y-auto p-4">
      <CreateBranchForm
        name={newName}
        setName={setNewName}
        description={newDescription}
        setDescription={setNewDescription}
        parent={newParent}
        setParent={setNewParent}
        creating={creating}
        onCreate={handleCreate}
      />

      <PanelHeader title={`${branches?.length ?? 0} branches`} onRefresh={refresh} loading={loading} />

      {branches === null ? (
        <Skeleton text="Loading branches…" />
      ) : branches.length === 0 ? (
        <EmptyState
          icon={GitBranch}
          title="No branches yet"
          body="Create one above to start a knowledge sandbox."
        />
      ) : (
        <ul className="flex flex-col gap-2">
          {branches.map((b) => (
            <BranchRow
              key={b.name}
              branch={b}
              stats={statsByName[b.name]}
              onMerge={() => handleMerge(b.name)}
              onRebase={() => handleRebase(b.name)}
              onRollback={() => handleRollback(b.name)}
              onDelete={() => handleDelete(b.name)}
            />
          ))}
        </ul>
      )}
    </div>
  );
}

function CreateBranchForm({
  name,
  setName,
  description,
  setDescription,
  parent,
  setParent,
  creating,
  onCreate,
}: {
  name: string;
  setName: (v: string) => void;
  description: string;
  setDescription: (v: string) => void;
  parent: string;
  setParent: (v: string) => void;
  creating: boolean;
  onCreate: () => void;
}) {
  return (
    <section className="rounded-lg border border-border bg-muted/20 p-3">
      <h3 className="mb-2 text-xs font-medium uppercase tracking-wider text-muted-foreground">
        New branch
      </h3>
      <div className="grid grid-cols-1 gap-2 sm:grid-cols-3">
        <input
          type="text"
          placeholder="branch name"
          value={name}
          onChange={(e) => setName(e.target.value)}
          className="rounded-md border border-border bg-background px-2.5 py-1.5 text-xs"
        />
        <input
          type="text"
          placeholder="parent (default: main)"
          value={parent}
          onChange={(e) => setParent(e.target.value)}
          className="rounded-md border border-border bg-background px-2.5 py-1.5 text-xs"
        />
        <input
          type="text"
          placeholder="description (optional)"
          value={description}
          onChange={(e) => setDescription(e.target.value)}
          className="rounded-md border border-border bg-background px-2.5 py-1.5 text-xs"
        />
      </div>
      <div className="mt-2 flex justify-end">
        <Button onClick={onCreate} disabled={creating || !name.trim()} size="sm">
          {creating ? <RefreshCw className="mr-1 size-3 animate-spin" /> : <Plus className="mr-1 size-3" />}
          Create
        </Button>
      </div>
    </section>
  );
}

function BranchRow({
  branch,
  stats,
  onMerge,
  onRebase,
  onRollback,
  onDelete,
}: {
  branch: BranchView;
  stats?: BranchStats;
  onMerge: () => void;
  onRebase: () => void;
  onRollback: () => void;
  onDelete: () => void;
}) {
  const statusColor =
    branch.status === "active"
      ? "bg-emerald-500"
      : branch.status === "merged"
        ? "bg-blue-500"
        : "bg-zinc-500";

  return (
    <li
      className={cn(
        "rounded-lg border p-3",
        branch.current ? "border-accent bg-accent/5" : "border-border",
      )}
    >
      <div className="flex items-center gap-2">
        <span className={cn("size-2 shrink-0 rounded-full", statusColor)} />
        <span className="font-medium text-sm">{branch.name}</span>
        {branch.current && (
          <span className="rounded-full bg-accent/20 px-2 py-0.5 text-[9px] font-medium uppercase tracking-wider text-accent">
            current
          </span>
        )}
        <span className="ml-auto text-[10px] uppercase tracking-wider text-muted-foreground">
          {branch.status}
        </span>
      </div>
      {branch.description && (
        <p className="mt-1 text-xs text-muted-foreground">{branch.description}</p>
      )}
      <div className="mt-2 flex flex-wrap items-center gap-3 text-[10px] text-muted-foreground">
        <span>parent: {branch.parent || "—"}</span>
        {stats && (
          <>
            <span>·</span>
            <span>{stats.claim_count} claims</span>
            <span>·</span>
            <span>{stats.entity_count} entities</span>
            <span>·</span>
            <span>{stats.source_count} sources</span>
            <span>·</span>
            <span>{stats.event_count} events</span>
          </>
        )}
      </div>
      {branch.status === "active" && (
        <div className="mt-2 flex flex-wrap gap-1.5">
          <Button size="sm" variant="outline" onClick={onMerge} className="h-7 px-2 text-[11px]">
            <GitMerge className="mr-1 size-3" /> Merge into main
          </Button>
          <Button size="sm" variant="outline" onClick={onRebase} className="h-7 px-2 text-[11px]">
            <RotateCcw className="mr-1 size-3" /> Rebase
          </Button>
          <Button size="sm" variant="ghost" onClick={onDelete} className="h-7 px-2 text-[11px]">
            <Trash2 className="mr-1 size-3" /> Abandon
          </Button>
        </div>
      )}
      {branch.status === "merged" && (
        <div className="mt-2 flex gap-1.5">
          <Button size="sm" variant="outline" onClick={onRollback} className="h-7 px-2 text-[11px]">
            <History className="mr-1 size-3" /> Rollback
          </Button>
        </div>
      )}
    </li>
  );
}

// ─── Tags panel ──────────────────────────────────────────────────────

function TagsPanel() {
  const [tags, setTags] = useState<TagView[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [name, setName] = useState("");
  const [branch, setBranch] = useState("main");
  const [message, setMessage] = useState("");
  const [creating, setCreating] = useState(false);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      setTags(await tagList());
    } catch (e) {
      toast("Tag list failed", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
      setTags([]);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  async function handleCreate() {
    if (!name.trim() || !branch.trim()) {
      toast("Tag name + branch required", { kind: "warn" });
      return;
    }
    setCreating(true);
    try {
      await tagCreate({
        name: name.trim(),
        branch: branch.trim(),
        message: message.trim() || undefined,
      });
      toast(`Tag ${name.trim()} created`, { kind: "success" });
      setName("");
      setMessage("");
      await refresh();
    } catch (e) {
      toast("Tag create failed", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
    } finally {
      setCreating(false);
    }
  }

  return (
    <div className="flex h-full flex-col gap-4 overflow-y-auto p-4">
      <section className="rounded-lg border border-border bg-muted/20 p-3">
        <h3 className="mb-2 text-xs font-medium uppercase tracking-wider text-muted-foreground">
          New tag
        </h3>
        <div className="grid grid-cols-1 gap-2 sm:grid-cols-3">
          <input
            type="text"
            placeholder="tag name (immutable)"
            value={name}
            onChange={(e) => setName(e.target.value)}
            className="rounded-md border border-border bg-background px-2.5 py-1.5 text-xs"
          />
          <input
            type="text"
            placeholder="branch"
            value={branch}
            onChange={(e) => setBranch(e.target.value)}
            className="rounded-md border border-border bg-background px-2.5 py-1.5 text-xs"
          />
          <input
            type="text"
            placeholder="message (optional)"
            value={message}
            onChange={(e) => setMessage(e.target.value)}
            className="rounded-md border border-border bg-background px-2.5 py-1.5 text-xs"
          />
        </div>
        <div className="mt-2 flex justify-end">
          <Button onClick={handleCreate} disabled={creating || !name.trim()} size="sm">
            {creating ? <RefreshCw className="mr-1 size-3 animate-spin" /> : <Plus className="mr-1 size-3" />}
            Create tag
          </Button>
        </div>
      </section>

      <PanelHeader title={`${tags?.length ?? 0} tags`} onRefresh={refresh} loading={loading} />

      {tags === null ? (
        <Skeleton text="Loading tags…" />
      ) : tags.length === 0 ? (
        <EmptyState
          icon={TagIcon}
          title="No tags yet"
          body="Tags are immutable snapshots of a branch's state. Create one above to mark a release."
        />
      ) : (
        <ul className="flex flex-col gap-2">
          {tags.map((t) => (
            <li
              key={t.name}
              className="rounded-lg border border-border p-3"
            >
              <div className="flex items-center gap-2">
                <TagIcon className="size-3.5 text-muted-foreground" />
                <span className="font-medium text-sm">{t.name}</span>
                <span className="ml-auto font-mono text-[10px] text-muted-foreground">
                  {t.target_commit_hash.slice(0, 12)}
                </span>
              </div>
              {t.message && (
                <p className="mt-1 text-xs text-muted-foreground">{t.message}</p>
              )}
              {t.created_at && (
                <p className="mt-1 text-[10px] text-muted-foreground/70">
                  {new Date(t.created_at).toLocaleString()}
                </p>
              )}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

// ─── Proposals panel ─────────────────────────────────────────────────

function ProposalsPanel() {
  const [proposals, setProposals] = useState<ProposalView[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [openName, setOpenName] = useState("");
  const [openTarget, setOpenTarget] = useState("main");
  const [openDescription, setOpenDescription] = useState("");
  const [creating, setCreating] = useState(false);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      setProposals(await proposalList());
    } catch (e) {
      toast("Proposal list failed", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
      setProposals([]);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  async function handleOpen() {
    if (!openName.trim()) {
      toast("Source branch required", { kind: "warn" });
      return;
    }
    setCreating(true);
    try {
      const p = await proposalOpen({
        branch: openName.trim(),
        target: openTarget.trim() || "main",
        description: openDescription.trim() || undefined,
      });
      toast(`Proposal ${p.id.slice(0, 8)} opened`, { kind: "success" });
      setOpenName("");
      setOpenDescription("");
      await refresh();
    } catch (e) {
      toast("Open proposal failed", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
    } finally {
      setCreating(false);
    }
  }

  async function handleReview(id: string, decision: ProposalDecision) {
    try {
      await proposalReview({ id, decision });
      toast(`Review recorded`, {
        kind: "success",
        body: `${decision.replace("_", " ")} on ${id.slice(0, 8)}`,
      });
      await refresh();
    } catch (e) {
      toast("Review failed", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
    }
  }

  async function handleClose(id: string) {
    if (!confirm("Close this proposal? No further reviews will be accepted.")) {
      return;
    }
    try {
      await proposalClose(id);
      toast(`Proposal closed`, { kind: "success" });
      await refresh();
    } catch (e) {
      toast("Close failed", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
    }
  }

  return (
    <div className="flex h-full flex-col gap-4 overflow-y-auto p-4">
      <section className="rounded-lg border border-border bg-muted/20 p-3">
        <h3 className="mb-2 text-xs font-medium uppercase tracking-wider text-muted-foreground">
          Open proposal
        </h3>
        <div className="grid grid-cols-1 gap-2 sm:grid-cols-3">
          <input
            type="text"
            placeholder="source branch"
            value={openName}
            onChange={(e) => setOpenName(e.target.value)}
            className="rounded-md border border-border bg-background px-2.5 py-1.5 text-xs"
          />
          <input
            type="text"
            placeholder="target branch (default: main)"
            value={openTarget}
            onChange={(e) => setOpenTarget(e.target.value)}
            className="rounded-md border border-border bg-background px-2.5 py-1.5 text-xs"
          />
          <input
            type="text"
            placeholder="description (optional)"
            value={openDescription}
            onChange={(e) => setOpenDescription(e.target.value)}
            className="rounded-md border border-border bg-background px-2.5 py-1.5 text-xs"
          />
        </div>
        <div className="mt-2 flex justify-end">
          <Button onClick={handleOpen} disabled={creating || !openName.trim()} size="sm">
            {creating ? <RefreshCw className="mr-1 size-3 animate-spin" /> : <Plus className="mr-1 size-3" />}
            Open
          </Button>
        </div>
      </section>

      <PanelHeader
        title={`${proposals?.length ?? 0} proposals`}
        onRefresh={refresh}
        loading={loading}
      />

      {proposals === null ? (
        <Skeleton text="Loading proposals…" />
      ) : proposals.length === 0 ? (
        <EmptyState
          icon={Workflow}
          title="No proposals open"
          body="Knowledge Proposals gate `MergePolicy::RequiresProposal` merges. Open one above on a source branch to start the review flow."
        />
      ) : (
        <ul className="flex flex-col gap-2">
          {proposals.map((p) => (
            <ProposalRow
              key={p.id}
              proposal={p}
              onReview={(d) => handleReview(p.id, d)}
              onClose={() => handleClose(p.id)}
            />
          ))}
        </ul>
      )}
    </div>
  );
}

function ProposalRow({
  proposal,
  onReview,
  onClose,
}: {
  proposal: ProposalView;
  onReview: (decision: ProposalDecision) => void;
  onClose: () => void;
}) {
  const isOpen = proposal.status === "open" || proposal.status === "reviewing";
  return (
    <li className="rounded-lg border border-border p-3">
      <div className="flex items-center gap-2">
        <CircleDot
          className={cn(
            "size-3.5",
            isOpen ? "text-emerald-500" : "text-muted-foreground",
          )}
        />
        <span className="font-mono text-xs">{proposal.id.slice(0, 12)}</span>
        <span className="rounded-full bg-muted px-2 py-0.5 text-[9px] font-medium uppercase tracking-wider text-muted-foreground">
          {proposal.status}
        </span>
        <span className="ml-auto text-[11px] text-muted-foreground">
          {proposal.source_branch} → {proposal.target_branch}
        </span>
      </div>
      {isOpen && (
        <div className="mt-2 flex flex-wrap gap-1.5">
          <Button
            size="sm"
            variant="outline"
            onClick={() => onReview("approve")}
            className="h-7 px-2 text-[11px]"
          >
            <CheckCircle2 className="mr-1 size-3 text-emerald-500" /> Approve
          </Button>
          <Button
            size="sm"
            variant="outline"
            onClick={() => onReview("request_changes")}
            className="h-7 px-2 text-[11px]"
          >
            <AlertTriangle className="mr-1 size-3 text-amber-500" /> Request changes
          </Button>
          <Button
            size="sm"
            variant="outline"
            onClick={() => onReview("comment")}
            className="h-7 px-2 text-[11px]"
          >
            Comment
          </Button>
          <Button
            size="sm"
            variant="ghost"
            onClick={onClose}
            className="h-7 px-2 text-[11px]"
          >
            Close
          </Button>
        </div>
      )}
    </li>
  );
}

// ─── Templates panel ─────────────────────────────────────────────────

function TemplatesPanel() {
  const [templates, setTemplates] = useState<BranchTemplateInfo[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [applyTo, setApplyTo] = useState<Record<string, string>>({});

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const r = await branchTemplateList();
      setTemplates(r.templates);
    } catch (e) {
      toast("Template list failed", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
      setTemplates([]);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  async function handleApply(template: string) {
    const branch = applyTo[template]?.trim();
    if (!branch) {
      toast("Branch name required", { kind: "warn" });
      return;
    }
    try {
      await branchTemplateApply({ template, branch });
      toast(`Branch ${branch} created from template ${template}`, { kind: "success" });
      setApplyTo((prev) => ({ ...prev, [template]: "" }));
    } catch (e) {
      toast("Apply failed", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
    }
  }

  return (
    <div className="flex h-full flex-col gap-4 overflow-y-auto p-4">
      <PanelHeader
        title={`${templates?.length ?? 0} templates`}
        onRefresh={refresh}
        loading={loading}
      />

      {templates === null ? (
        <Skeleton text="Loading templates…" />
      ) : templates.length === 0 ? (
        <EmptyState
          icon={Layers}
          title="No templates registered"
          body="Templates are pre-baked merge-policy / kind / TTL bundles. Edit `<workspace>/.thinkingroot-refs/branch_templates.toml` to add some, or use `root branch-template upsert` from the CLI."
        />
      ) : (
        <ul className="flex flex-col gap-2">
          {templates.map((t) => (
            <li key={t.name} className="rounded-lg border border-border p-3">
              <div className="flex items-center gap-2">
                <Layers className="size-3.5 text-muted-foreground" />
                <span className="font-medium text-sm">{t.name}</span>
              </div>
              {t.description && (
                <p className="mt-1 text-xs text-muted-foreground">{t.description}</p>
              )}
              <div className="mt-2 flex gap-1.5">
                <input
                  type="text"
                  placeholder="new branch name"
                  value={applyTo[t.name] ?? ""}
                  onChange={(e) =>
                    setApplyTo((prev) => ({ ...prev, [t.name]: e.target.value }))
                  }
                  className="flex-1 rounded-md border border-border bg-background px-2.5 py-1 text-xs"
                />
                <Button
                  size="sm"
                  variant="outline"
                  onClick={() => handleApply(t.name)}
                  disabled={!(applyTo[t.name] ?? "").trim()}
                  className="h-7 px-2 text-[11px]"
                >
                  Apply
                </Button>
              </div>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

// ─── Shared helpers ──────────────────────────────────────────────────

function PanelHeader({
  title,
  onRefresh,
  loading,
}: {
  title: string;
  onRefresh: () => void;
  loading: boolean;
}) {
  return (
    <div className="flex items-center justify-between">
      <h2 className="text-sm font-medium">{title}</h2>
      <Button
        variant="ghost"
        size="icon"
        className="h-7 w-7"
        onClick={onRefresh}
        disabled={loading}
        aria-label="Refresh"
      >
        <RefreshCw className={loading ? "size-3.5 animate-spin" : "size-3.5"} />
      </Button>
    </div>
  );
}

function Skeleton({ text }: { text: string }) {
  return (
    <div className="flex h-32 items-center justify-center text-xs text-muted-foreground">
      {text}
    </div>
  );
}

function EmptyState({
  icon: Icon,
  title,
  body,
}: {
  icon: typeof GitBranch;
  title: string;
  body: string;
}) {
  return (
    <div className="flex flex-col items-center justify-center gap-2 rounded-lg border border-dashed border-border bg-muted/20 p-8 text-center">
      <Icon className="size-6 text-muted-foreground/60" />
      <h3 className="text-sm font-medium">{title}</h3>
      <p className="max-w-md text-xs text-muted-foreground">{body}</p>
    </div>
  );
}
