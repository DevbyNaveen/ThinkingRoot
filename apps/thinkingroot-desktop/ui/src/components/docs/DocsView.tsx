import { useCallback, useEffect, useMemo, useState } from "react";
import { BookOpen, Code2, Copy, Loader2, Network, Plug, Server } from "lucide-react";
import type { LucideIcon } from "lucide-react";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import {
  mcpConfigureTool,
  mcpGetConfigSnippet,
  mcpStatus,
  workspaceList,
  type McpStatus,
  type WorkspaceView,
} from "@/lib/tauri";
import { useApp } from "@/store/app";
import { toast } from "@/store/toast";

type DocTab = "overview" | "cursor" | "node" | "python" | "curl" | "lovable" | "export";

const DOC_TABS: Array<{ id: DocTab; label: string }> = [
  { id: "overview", label: "Overview" },
  { id: "cursor", label: "Cursor / MCP" },
  { id: "node", label: "Node" },
  { id: "python", label: "Python" },
  { id: "curl", label: "curl" },
  { id: "lovable", label: "Lovable" },
  { id: "export", label: ".tr Export" },
];

export function DocsView() {
  const activeWorkspace = useApp((s) => s.activeWorkspace);
  const [tab, setTab] = useState<DocTab>("overview");
  const [status, setStatus] = useState<McpStatus | null>(null);
  const [workspace, setWorkspace] = useState<WorkspaceView | null>(null);
  const [mcpSnippet, setMcpSnippet] = useState("");
  const [loading, setLoading] = useState(true);
  const [configuringCursor, setConfiguringCursor] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const [sidecar, workspaces, cursorSnippet] = await Promise.all([
        mcpStatus(),
        workspaceList(),
        mcpGetConfigSnippet("cursor"),
      ]);
      setStatus(sidecar);
      setWorkspace(
        activeWorkspace
          ? workspaces.find((w) => w.name === activeWorkspace) ?? null
          : workspaces.find((w) => w.active) ?? workspaces[0] ?? null,
      );
      setMcpSnippet(cursorSnippet);
    } catch (err) {
      toast("Docs connection data failed to load", {
        kind: "error",
        body: err instanceof Error ? err.message : String(err),
      });
      setStatus(null);
      setMcpSnippet("");
    } finally {
      setLoading(false);
    }
  }, [activeWorkspace]);

  useEffect(() => {
    void load();
  }, [load]);

  const workspaceName = workspace?.name ?? activeWorkspace ?? "workspace";
  const encodedWorkspace = encodeURIComponent(workspaceName);
  const baseUrl = status ? `http://${status.host}:${status.port}` : "http://127.0.0.1:31760";
  const restBase = `${baseUrl}/api/v1/ws/${encodedWorkspace}`;

  const snippet = useMemo(() => {
    switch (tab) {
      case "overview":
        return [
          "ThinkingRoot backend flow:",
          "1. Add or mount a folder as a workspace.",
          "2. Compile it into sources, claims, entities, and relations.",
          "3. Inspect the live backend in the Builders tab.",
          "4. Connect apps through MCP, REST, or the Brain SDK.",
          "5. Export the knowledge backend as a .tr pack when you need portability.",
        ].join("\n");
      case "cursor":
        return mcpSnippet || "Cursor MCP config is loading.";
      case "node":
        return [
          'import { Brain } from "thinkingroot";',
          "",
          `const brain = await Brain.remote("${baseUrl}", {`,
          `  workspace: "${escapeForSnippet(workspaceName)}",`,
          "  sessionId: \"app-dev\",",
          "  apiKey: process.env.THINKINGROOT_API_KEY ?? null,",
          "});",
          "",
          "const result = await brain.hybridSearch(",
          "  \"Find the policy and examples for refund follow-up emails\",",
          "  { top_k: 10 }",
          ");",
          "console.log(result.hits);",
        ].join("\n");
      case "python":
        return [
          "from thinkingroot import Brain",
          "",
          `brain = Brain.remote("${baseUrl}", workspace="${escapeForSnippet(workspaceName)}")`,
          "result = brain.hybrid_search(",
          "    \"Find the policy and examples for refund follow-up emails\",",
          "    top_k=10,",
          ")",
          "print(result[\"hits\"])",
        ].join("\n");
      case "curl":
        return [
          `curl -sS "${restBase}/ask" \\`,
          "  -H \"Content-Type: application/json\" \\",
          "  -H \"X-TR-Session-Id: app-dev\" \\",
          "  -d '{\"question\":\"How should the agent reply to refund requests?\"}'",
        ].join("\n");
      case "lovable":
        return [
          "// Server route only. Do not expose THINKINGROOT_API_KEY in browser JS.",
          "export async function POST(req: Request) {",
          "  const { question, sessionId } = await req.json();",
          `  const resp = await fetch("${restBase}/ask", {`,
          "    method: \"POST\",",
          "    headers: {",
          "      \"Content-Type\": \"application/json\",",
          "      \"X-TR-Session-Id\": sessionId ?? \"lovable-session\",",
          "      ...(process.env.THINKINGROOT_API_KEY",
          "        ? { Authorization: `Bearer ${process.env.THINKINGROOT_API_KEY}` }",
          "        : {}),",
          "    },",
          "    body: JSON.stringify({ question }),",
          "  });",
          "  return Response.json(await resp.json(), { status: resp.status });",
          "}",
        ].join("\n");
      case "export":
        return [
          `root pack "${workspace?.path ?? "./workspace"}" --name "${workspaceName}" --version 1.0.0`,
          `root install "${workspaceName}-1.0.0.tr"`,
          `root mount "${workspaceName}-1.0.0.tr"`,
          `root query "what knowledge does this backend contain?"`,
        ].join("\n");
    }
  }, [baseUrl, mcpSnippet, restBase, tab, workspace?.path, workspaceName]);

  async function copySnippet() {
    try {
      await writeText(snippet);
      toast("Docs snippet copied", { kind: "success", durationMs: 2200 });
    } catch (err) {
      toast("Copy failed", {
        kind: "error",
        body: err instanceof Error ? err.message : String(err),
      });
    }
  }

  async function configureCursor() {
    setConfiguringCursor(true);
    try {
      const result = await mcpConfigureTool("cursor");
      toast("Cursor MCP configured", {
        kind: "success",
        body: `Wrote ThinkingRoot MCP config to ${result.path}. Restart Cursor to pick it up.`,
        durationMs: 7000,
      });
      setTab("cursor");
      await load();
    } catch (err) {
      toast("Cursor auto-config failed", {
        kind: "error",
        body: err instanceof Error ? err.message : String(err),
        durationMs: 8000,
      });
    } finally {
      setConfiguringCursor(false);
    }
  }

  return (
    <div className="h-full min-h-0 overflow-y-auto">
      <div className="mx-auto flex w-full max-w-6xl flex-col px-6 py-6">
        <header className="flex items-start gap-3">
          <div className="flex size-10 shrink-0 items-center justify-center rounded-2xl bg-accent/12 text-accent">
            <BookOpen className="size-5" />
          </div>
          <div className="min-w-0 flex-1">
            <h1 className="text-xl font-semibold tracking-tight text-foreground">
              Builder Docs
            </h1>
            <p className="mt-1 max-w-2xl text-sm leading-6 text-muted-foreground">
              Connect the active workspace to apps, agents, Cursor, Lovable,
              REST, SDKs, and portable .tr packs.
            </p>
          </div>
          <Button
            type="button"
            variant="ghost"
            size="icon"
            className="h-8 w-8 shrink-0 text-muted-foreground"
            onClick={() => void load()}
            aria-label="Refresh docs connection data"
          >
            <Network className={cn("size-4", loading && "animate-pulse")} />
          </Button>
        </header>

        <section className="mt-5 grid gap-3 md:grid-cols-3">
          <StatusCard
            Icon={Server}
            label="Workspace"
            value={workspaceName}
            tone={workspace ? "ok" : "warn"}
          />
          <StatusCard
            Icon={Plug}
            label="MCP"
            value={status?.running ? status.sse_url : "Sidecar not running"}
            tone={status?.running ? "ok" : "warn"}
          />
          <StatusCard
            Icon={Network}
            label="REST"
            value={restBase}
            tone="ok"
          />
        </section>

        <div className="mt-5 grid min-h-0 gap-5 lg:grid-cols-[13rem_minmax(0,1fr)]">
          <aside className="rounded-2xl border border-border/70 bg-surface/70 p-2">
            <div className="px-2 pb-2 pt-1 text-[10px] font-semibold uppercase tracking-widest text-muted-foreground/70">
              Guides
            </div>
            <nav className="flex flex-col gap-1">
            {DOC_TABS.map((item) => (
              <button
                key={item.id}
                type="button"
                onClick={() => setTab(item.id)}
                className={cn(
                  "rounded-xl px-3 py-2 text-left text-xs transition-colors",
                  tab === item.id
                    ? "bg-accent/12 text-accent"
                    : "text-muted-foreground hover:bg-muted/45 hover:text-foreground",
                )}
              >
                {item.label}
              </button>
            ))}
            </nav>
          </aside>

          <main className="min-w-0 overflow-hidden rounded-2xl border border-border/70 bg-surface shadow-sm">
            <div className="flex items-center gap-3 border-b border-border/60 px-4 py-3">
              <div className="flex size-8 shrink-0 items-center justify-center rounded-xl bg-muted/40 text-accent">
                <Code2 className="size-4" />
              </div>
              <div className="min-w-0 flex-1">
                <div className="text-sm font-medium text-foreground">
                  {DOC_TABS.find((item) => item.id === tab)?.label}
                </div>
                <div className="truncate text-[11px] text-muted-foreground">
                  {copyForTab(tab)}
                </div>
              </div>
              {tab === "cursor" && (
                <Button
                  type="button"
                  variant="default"
                  size="sm"
                  className="h-8 shrink-0 gap-1.5 text-xs"
                  onClick={() => void configureCursor()}
                  disabled={configuringCursor}
                >
                  {configuringCursor ? (
                    <Loader2 className="size-3.5 animate-spin" />
                  ) : (
                    <Plug className="size-3.5" />
                  )}
                  Auto configure Cursor
                </Button>
              )}
              <Button
                type="button"
                variant="outline"
                size="sm"
                className="h-8 shrink-0 gap-1.5 text-xs"
                onClick={() => void copySnippet()}
                disabled={loading}
              >
                <Copy className="size-3.5" />
                Copy
              </Button>
            </div>
            <div className="bg-[#151515]">
              <pre className="max-h-[32rem] min-h-[22rem] overflow-auto p-4 font-mono text-xs leading-relaxed text-foreground">
                {loading ? "Loading connection details..." : snippet}
              </pre>
            </div>
          </main>
        </div>
      </div>
    </div>
  );
}

function StatusCard({
  Icon,
  label,
  value,
  tone,
}: {
  Icon: LucideIcon;
  label: string;
  value: string;
  tone: "ok" | "warn";
}) {
  return (
    <div className="min-w-0 rounded-2xl border border-border/70 bg-surface/70 px-3 py-3">
      <div className="flex items-center gap-2 text-[10px] font-semibold uppercase tracking-widest text-muted-foreground/70">
        <Icon className={cn("size-3.5", tone === "ok" ? "text-emerald-400" : "text-amber-400")} />
        {label}
      </div>
      <div className="mt-2 truncate font-mono text-[11px] text-foreground" title={value}>
        {value}
      </div>
    </div>
  );
}

function copyForTab(tab: DocTab): string {
  switch (tab) {
    case "overview":
      return "The shortest mental model for the backend flow.";
    case "cursor":
      return "Paste this MCP config into Cursor to let agents use the workspace.";
    case "node":
      return "Use the TypeScript Brain SDK from a worker, server route, or agent.";
    case "python":
      return "Use the Python Brain facade from scripts, notebooks, and services.";
    case "curl":
      return "Call the REST API directly from any language.";
    case "lovable":
      return "Put this behind a server route; never expose API keys in browser code.";
    case "export":
      return "Package and move the backend knowledge as a portable .tr pack.";
  }
}

function escapeForSnippet(value: string): string {
  return value.replace(/\\/g, "\\\\").replace(/"/g, '\\"');
}
