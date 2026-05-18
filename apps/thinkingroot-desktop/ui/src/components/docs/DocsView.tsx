import { useCallback, useEffect, useMemo, useState } from "react";
import { Copy, Loader2, Network, Plug } from "lucide-react";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";

import { SettingsGroup, SettingsRow } from "@/components/settings/SettingsView";
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
import type { DocSectionId } from "@/types";
import { toast } from "@/store/toast";

const DOCS_PAGE_META: Record<DocSectionId, { title: string; subtitle: string }> = {
  overview: {
    title: "Overview",
    subtitle: "The shortest mental model for mounting, compiling, and connecting a workspace.",
  },
  cursor: {
    title: "Cursor / MCP",
    subtitle: "Paste this MCP config into Cursor so agents can use the active workspace.",
  },
  node: {
    title: "Node",
    subtitle: "Use the TypeScript Brain SDK from a worker, server route, or agent.",
  },
  python: {
    title: "Python",
    subtitle: "Use the Python Brain facade from scripts, notebooks, and services.",
  },
  curl: {
    title: "curl",
    subtitle: "Call the REST API directly from any language.",
  },
  lovable: {
    title: "Lovable",
    subtitle: "Put this behind a server route; never expose API keys in browser code.",
  },
  export: {
    title: ".tr Export",
    subtitle: "Package and move backend knowledge as a portable .tr pack.",
  },
};

export function DocsView() {
  const activeWorkspace = useApp((s) => s.activeWorkspace);
  const docsSection = useApp((s) => s.docsSection);
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
  const mcpUrl = status?.running ? status.sse_url : "Sidecar not running";

  const snippet = useMemo(() => buildSnippet(docsSection, {
    baseUrl,
    mcpSnippet,
    restBase,
    workspaceName,
    workspacePath: workspace?.path,
  }), [baseUrl, docsSection, mcpSnippet, restBase, workspace?.path, workspaceName]);

  const page = DOCS_PAGE_META[docsSection];

  async function copySnippet() {
    try {
      await writeText(snippet);
      toast("Snippet copied", { kind: "success", durationMs: 2200 });
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
      useApp.getState().setDocsSection("cursor");
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
    <div className="flex h-full flex-col">
      <div className="relative min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto w-full max-w-xl px-6 pb-20 pt-10 sm:px-10 sm:pt-12 lg:max-w-2xl lg:px-14">
          <header className="mb-10 flex items-start justify-between gap-4 sm:mb-12">
            <div className="min-w-0 space-y-2">
              <h1 className="text-2xl font-semibold tracking-tight text-foreground sm:text-[1.7rem] sm:leading-tight">
                {page.title}
              </h1>
              <p className="max-w-lg text-[13px] leading-relaxed text-muted-foreground sm:text-sm">
                {page.subtitle}
              </p>
            </div>
            <Button
              type="button"
              variant="ghost"
              size="icon"
              className="h-8 w-8 shrink-0 text-muted-foreground"
              onClick={() => void load()}
              aria-label="Refresh connection data"
            >
              <Network className={cn("size-4", loading && "animate-pulse")} />
            </Button>
          </header>

          <div className="flex flex-col gap-10 sm:gap-12">
            <SettingsGroup label="Connection">
              <SettingsRow label="Workspace" description="Active workspace for examples below.">
                <p className="font-mono text-xs text-foreground">{workspaceName}</p>
              </SettingsRow>
              <SettingsRow label="MCP" description="Local sidecar SSE endpoint for editor tools.">
                <p
                  className={cn(
                    "break-all font-mono text-xs",
                    status?.running ? "text-foreground" : "text-amber-600 dark:text-amber-300",
                  )}
                >
                  {loading ? "Loading…" : mcpUrl}
                </p>
              </SettingsRow>
              <SettingsRow label="REST" description="Workspace-scoped HTTP API base URL.">
                <p className="break-all font-mono text-xs text-foreground">
                  {loading ? "Loading…" : restBase}
                </p>
              </SettingsRow>
            </SettingsGroup>

            <SettingsGroup label={docsSection === "overview" ? "Flow" : "Snippet"}>
              {docsSection === "cursor" ? (
                <div className="flex flex-wrap justify-end gap-2 border-b border-border/35 px-4 py-3 sm:px-5">
                  <Button
                    type="button"
                    variant="default"
                    size="sm"
                    className="h-8 gap-1.5 text-xs"
                    onClick={() => void configureCursor()}
                    disabled={configuringCursor || loading}
                  >
                    {configuringCursor ? (
                      <Loader2 className="size-3.5 animate-spin" />
                    ) : (
                      <Plug className="size-3.5" />
                    )}
                    Auto configure Cursor
                  </Button>
                  <Button
                    type="button"
                    variant="outline"
                    size="sm"
                    className="h-8 gap-1.5 text-xs"
                    onClick={() => void copySnippet()}
                    disabled={loading}
                  >
                    <Copy className="size-3.5" />
                    Copy
                  </Button>
                </div>
              ) : (
                <div className="flex justify-end border-b border-border/35 px-4 py-3 sm:px-5">
                  <Button
                    type="button"
                    variant="outline"
                    size="sm"
                    className="h-8 gap-1.5 text-xs"
                    onClick={() => void copySnippet()}
                    disabled={loading}
                  >
                    <Copy className="size-3.5" />
                    Copy
                  </Button>
                </div>
              )}
              <pre className="max-h-[28rem] overflow-auto bg-muted/20 p-4 font-mono text-xs leading-relaxed text-foreground sm:px-5 sm:py-5">
                {loading ? "Loading connection details…" : snippet}
              </pre>
            </SettingsGroup>
          </div>
        </div>
      </div>
    </div>
  );
}

function buildSnippet(
  tab: DocSectionId,
  ctx: {
    baseUrl: string;
    mcpSnippet: string;
    restBase: string;
    workspaceName: string;
    workspacePath?: string;
  },
): string {
  const { baseUrl, mcpSnippet, restBase, workspaceName, workspacePath } = ctx;
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
        '  sessionId: "app-dev",',
        "  apiKey: process.env.THINKINGROOT_API_KEY ?? null,",
        "});",
        "",
        'const result = await brain.hybridSearch("Your question", { top_k: 10 });',
        "console.log(result.hits);",
      ].join("\n");
    case "python":
      return [
        "from thinkingroot import Brain",
        "",
        `brain = Brain.remote("${baseUrl}", workspace="${escapeForSnippet(workspaceName)}")`,
        'result = brain.hybrid_search("Your question", top_k=10)',
        'print(result["hits"])',
      ].join("\n");
    case "curl":
      return [
        `curl -sS "${restBase}/ask" \\`,
        '  -H "Content-Type: application/json" \\',
        '  -H "X-TR-Session-Id: app-dev" \\',
        '  -d \'{"question":"How should the agent reply?"}\'',
      ].join("\n");
    case "lovable":
      return [
        "// Server route only. Do not expose THINKINGROOT_API_KEY in browser JS.",
        "export async function POST(req: Request) {",
        "  const { question, sessionId } = await req.json();",
        `  const resp = await fetch("${restBase}/ask", {`,
        '    method: "POST",',
        "    headers: {",
        '      "Content-Type": "application/json",',
        '      "X-TR-Session-Id": sessionId ?? "lovable-session",',
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
        `root pack "${workspacePath ?? "./workspace"}" --name "${workspaceName}" --version 1.0.0`,
        `root install "${workspaceName}-1.0.0.tr"`,
        `root mount "${workspaceName}-1.0.0.tr"`,
        `root query "what knowledge does this backend contain?"`,
      ].join("\n");
  }
}

function escapeForSnippet(value: string): string {
  return value.replace(/\\/g, "\\\\").replace(/"/g, '\\"');
}
