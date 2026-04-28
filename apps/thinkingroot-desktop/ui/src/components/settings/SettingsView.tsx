import { useEffect, useMemo, useState } from "react";
import {
  Settings as SettingsIcon,
  KeyRound,
  FolderOpen,
  Paintbrush,
  Bell,
  Plug,
  Save,
  AlertTriangle,
  Check,
  Copy,
  X,
} from "lucide-react";
import { motion } from "framer-motion";
import {
  configPaths,
  credentialsRemove,
  credentialsSet,
  credentialsStatus,
  globalConfigRead,
  globalConfigWrite,
  mcpGetConfigSnippet,
  mcpStatus,
  workspaceList,
  workspaceLlmConfig,
  workspaceLlmWrite,
  workspaceSetActive,
  type ConfigPaths,
  type CredentialRow,
  type GlobalLlmConfig,
  type McpStatus,
  type McpToolKey,
  type WorkspaceLlmConfig,
  type WorkspaceView,
} from "@/lib/tauri";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";
import { useApp } from "@/store/app";
import { toast } from "@/store/toast";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import type { Theme } from "@/types";

/** Provider keys the Settings UI surfaces. The `env_var` matches the
 *  canonical env-var name the engine looks up — single source of truth
 *  with `crates/thinkingroot-extract/src/llm.rs::resolve_key`. */
const PROVIDER_META: Array<{
  id: "azure" | "anthropic" | "openai" | "openrouter" | "groq" | "deepseek";
  label: string;
  env_var: string;
  placeholder: string;
}> = [
  { id: "azure", label: "Azure OpenAI", env_var: "AZURE_OPENAI_API_KEY", placeholder: "Azure subscription key" },
  { id: "anthropic", label: "Anthropic", env_var: "ANTHROPIC_API_KEY", placeholder: "sk-ant-…" },
  { id: "openai", label: "OpenAI", env_var: "OPENAI_API_KEY", placeholder: "sk-…" },
  { id: "openrouter", label: "OpenRouter", env_var: "OPENROUTER_API_KEY", placeholder: "sk-or-…" },
  { id: "groq", label: "Groq", env_var: "GROQ_API_KEY", placeholder: "gsk_…" },
  { id: "deepseek", label: "DeepSeek", env_var: "DEEPSEEK_API_KEY", placeholder: "sk-…" },
];

const THEMES: Array<{ id: Theme; label: string; note?: string }> = [
  { id: "dark", label: "Dark", note: "Catppuccin Mocha" },
  { id: "light", label: "Light" },
  { id: "auto", label: "Auto", note: "Follow system" },
  { id: "daltonized-protanopia", label: "CVD · Protanopia", note: "red-deficient palette" },
  { id: "daltonized-deuteranopia", label: "CVD · Deuteranopia", note: "green-deficient palette" },
  { id: "daltonized-tritanopia", label: "CVD · Tritanopia", note: "blue-deficient palette" },
];

/**
 * Settings surface. Reads / writes the same files the CLI does:
 *
 *   ~/<config_dir>/thinkingroot/credentials.toml   ← provider keys
 *   ~/<config_dir>/thinkingroot/config.toml        ← provider/model defaults
 *   ~/<config_dir>/thinkingroot/workspaces.toml    ← workspace registry + active
 *   <workspace>/.thinkingroot/config.toml          ← per-workspace LLM overrides
 */
export function SettingsView() {
  const theme = useApp((s) => s.theme);
  const setTheme = useApp((s) => s.setTheme);

  const [paths, setPaths] = useState<ConfigPaths | null>(null);
  const [globalCfg, setGlobalCfg] = useState<GlobalLlmConfig | null>(null);
  const [credentials, setCredentials] = useState<CredentialRow[]>([]);
  const [provider, setProvider] = useState<typeof PROVIDER_META[number]["id"]>("azure");
  const [keyDraft, setKeyDraft] = useState<Record<string, string>>({});
  const [workspaces, setWorkspaces] = useState<WorkspaceView[]>([]);
  const [activeWorkspace, setActiveWorkspaceLocal] = useState<string | null>(null);
  const [wsLlm, setWsLlm] = useState<WorkspaceLlmConfig | null>(null);
  const [wsPending, setWsPending] = useState<Record<string, string>>({});
  const [globalPending, setGlobalPending] = useState<{
    default_provider?: string;
    extraction_model?: string;
    compilation_model?: string;
    azure?: {
      resource_name?: string;
      endpoint_base?: string;
      deployment?: string;
      api_version?: string;
    };
  }>({});
  const [saving, setSaving] = useState(false);
  const [loading, setLoading] = useState(true);

  // Initial load — every config source in parallel.
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const [pathsRes, cfgRes, credsRes, wsRes] = await Promise.all([
          configPaths(),
          globalConfigRead(),
          credentialsStatus(),
          workspaceList(),
        ]);
        if (cancelled) return;
        setPaths(pathsRes);
        setGlobalCfg(cfgRes);
        setCredentials(credsRes);
        setWorkspaces(wsRes);
        const active: WorkspaceView | undefined =
          wsRes.find((w) => w.active) ?? wsRes[0];
        setActiveWorkspaceLocal(active?.name ?? null);
        if (cfgRes.default_provider) {
          const known = PROVIDER_META.find((p) => p.id === cfgRes.default_provider);
          if (known) setProvider(known.id);
        }
        if (active) {
          try {
            const llm = await workspaceLlmConfig(active.path);
            if (!cancelled) setWsLlm(llm);
          } catch {
            /* leave wsLlm null — section just hides */
          }
        }
      } catch (e) {
        toast("Could not load settings", {
          kind: "error",
          body: e instanceof Error ? e.message : String(e),
        });
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  const dirty =
    Object.values(keyDraft).some((v) => (v ?? "").length > 0) ||
    Object.keys(wsPending).length > 0 ||
    Object.keys(globalPending).length > 0;

  async function save() {
    setSaving(true);
    try {
      // 1. Credentials — write each non-empty draft, clear nothing
      //    automatically (use the explicit "Remove" button for that).
      for (const [envVar, value] of Object.entries(keyDraft)) {
        if (value.trim().length > 0) {
          await credentialsSet(envVar, value.trim());
        }
      }

      // 2. Global LLM config patch.
      if (Object.keys(globalPending).length > 0) {
        const patch = { ...globalPending };
        // Always tag the active provider when the user edited any
        // global field — so picking a card updates `default_provider`.
        if (patch.default_provider === undefined) {
          patch.default_provider = provider;
        }
        await globalConfigWrite(patch);
      }

      // 3. Per-workspace LLM patch (deployment, endpoint, api_version,
      //    extraction/compilation model). Only if we actually have a
      //    workspace and the user touched a workspace-scoped field.
      if (
        wsLlm?.workspace_path &&
        Object.keys(wsPending).length > 0
      ) {
        await workspaceLlmWrite({
          workspace_path: wsLlm.workspace_path,
          provider,
          extraction_model: wsPending.extraction_model,
          compilation_model: wsPending.compilation_model,
          azure_resource_name: wsPending.azure_resource_name,
          azure_endpoint_base: wsPending.azure_endpoint_base,
          azure_deployment: wsPending.azure_deployment,
          azure_api_version: wsPending.azure_api_version,
          azure_api_key_env: wsPending.azure_api_key_env,
        });
      }

      toast("Settings saved", {
        kind: "success",
        body:
          "Restart the app to push new credentials into the running sidecar.",
        durationMs: 5000,
      });
      // Re-read everything so indicators reflect the new state.
      const [cfgRes, credsRes] = await Promise.all([
        globalConfigRead(),
        credentialsStatus(),
      ]);
      setGlobalCfg(cfgRes);
      setCredentials(credsRes);
      setKeyDraft({});
      setGlobalPending({});
      setWsPending({});
      if (wsLlm?.workspace_path) {
        const fresh = await workspaceLlmConfig(wsLlm.workspace_path);
        setWsLlm(fresh);
      }
    } catch (e) {
      toast("Save failed", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
    } finally {
      setSaving(false);
    }
  }

  async function clearKey(envVar: string) {
    try {
      await credentialsRemove(envVar);
      const credsRes = await credentialsStatus();
      setCredentials(credsRes);
      toast(`Removed ${envVar}`, { kind: "success" });
    } catch (e) {
      toast("Remove failed", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
    }
  }

  async function changeActiveWorkspace(name: string) {
    try {
      await workspaceSetActive(name);
      setActiveWorkspaceLocal(name);
      const wsRes = await workspaceList();
      setWorkspaces(wsRes);
      const ws = wsRes.find((w) => w.name === name);
      if (ws) {
        const llm = await workspaceLlmConfig(ws.path);
        setWsLlm(llm);
      }
    } catch (e) {
      toast("Activate workspace failed", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
    }
  }

  if (loading || !paths || !globalCfg) {
    return (
      <div className="flex h-full items-center justify-center text-sm text-muted-foreground">
        Loading settings…
      </div>
    );
  }

  const providerMeta = PROVIDER_META.find((p) => p.id === provider)!;
  const credForActive = credentials.find((c) => c.env_var === providerMeta.env_var);

  return (
    <div className="flex h-full flex-col">
      <Header
        dirty={dirty}
        saving={saving}
        onSave={save}
        configPath={paths.config_path ?? undefined}
        credentialsPath={paths.credentials_path ?? undefined}
      />
      <div className="flex-1 overflow-y-auto">
        <div className="mx-auto flex w-full max-w-3xl flex-col gap-6 px-6 py-8">
          <Section
            Icon={KeyRound}
            title="Provider"
            body="Pick the LLM backend ThinkingRoot talks to. API keys live in credentials.toml on this machine, chmod 0600, never uploaded. The CLI reads the same file."
          >
            <div className="flex flex-wrap gap-2">
              {PROVIDER_META.map((p) => {
                const active = provider === p.id;
                const cred = credentials.find((c) => c.env_var === p.env_var);
                const configured =
                  cred?.persisted === true || cred?.in_process_env === true;
                return (
                  <button
                    key={p.id}
                    type="button"
                    onClick={() => {
                      setProvider(p.id);
                      setGlobalPending((g) => ({ ...g, default_provider: p.id }));
                    }}
                    className={cn(
                      "flex items-center gap-1.5 rounded-md border px-3 py-1.5 text-xs transition-colors",
                      active
                        ? "border-accent bg-accent/15 text-accent"
                        : "border-border text-muted-foreground hover:text-foreground",
                    )}
                  >
                    {configured && <Check className="size-3 text-success" />}
                    {p.label}
                  </button>
                );
              })}
            </div>

            <div className="mt-4 grid grid-cols-1 gap-3">
              <Field
                label={`${providerMeta.label} API key`}
                hint={
                  credForActive?.persisted
                    ? `Stored in credentials.toml${credForActive.in_process_env ? " · also in process env" : ""}`
                    : credForActive?.in_process_env
                      ? "Set in process env (not persisted to file)"
                      : "Not configured"
                }
              >
                <div className="flex gap-2">
                  <input
                    type="password"
                    placeholder={providerMeta.placeholder}
                    value={keyDraft[providerMeta.env_var] ?? ""}
                    onChange={(e) =>
                      setKeyDraft((d) => ({
                        ...d,
                        [providerMeta.env_var]: e.target.value,
                      }))
                    }
                    autoComplete="off"
                    className="h-8 flex-1 rounded-md border border-input bg-background px-2 text-xs text-foreground placeholder:text-muted-foreground focus:border-accent focus:outline-none focus:ring-1 focus:ring-accent/40"
                  />
                  {credForActive?.persisted && (
                    <Button
                      type="button"
                      size="sm"
                      variant="outline"
                      onClick={() => void clearKey(providerMeta.env_var)}
                      className="h-8 gap-1 text-[11px]"
                    >
                      <X className="size-3" /> Clear
                    </Button>
                  )}
                </div>
              </Field>
              <p className="text-[10px] text-muted-foreground">
                Engine env-var name:{" "}
                <code className="rounded bg-muted px-1 font-mono">
                  {providerMeta.env_var}
                </code>
              </p>
            </div>

            {provider === "azure" && (
              <AzureWorkspaceCard
                wsLlm={wsLlm}
                wsPending={wsPending}
                setWsPending={setWsPending}
                globalAzure={globalCfg.azure}
                globalPending={globalPending}
                setGlobalPending={setGlobalPending}
              />
            )}
          </Section>

          <Section
            Icon={FolderOpen}
            title="Workspace"
            body="The mounted ThinkingRoot workspace — every chat answer is grounded in claims compiled here. Switch via the sidebar, or pick a different one below."
          >
            {workspaces.length === 0 ? (
              <p className="text-xs text-muted-foreground">
                No workspaces registered yet. Add one from the sidebar.
              </p>
            ) : (
              <div className="space-y-2">
                {workspaces.map((w) => {
                  const isActive = activeWorkspace === w.name;
                  return (
                    <button
                      key={w.name}
                      type="button"
                      onClick={() => void changeActiveWorkspace(w.name)}
                      className={cn(
                        "flex w-full items-center justify-between gap-3 rounded-md border px-3 py-2 text-left text-xs transition-colors",
                        isActive
                          ? "border-accent bg-accent/10"
                          : "border-border hover:border-accent/60 hover:bg-muted/40",
                      )}
                    >
                      <div className="flex min-w-0 flex-col">
                        <span className="font-medium text-foreground">
                          {w.name}
                          {w.compiled && (
                            <span className="ml-2 rounded bg-success/15 px-1.5 py-0.5 text-[9px] uppercase tracking-wider text-success">
                              compiled
                            </span>
                          )}
                        </span>
                        <span className="font-mono text-[10px] text-muted-foreground">
                          {w.path}
                        </span>
                      </div>
                      {isActive && <Check className="size-3.5 text-accent" />}
                    </button>
                  );
                })}
              </div>
            )}
          </Section>

          <Section
            Icon={Paintbrush}
            title="Appearance"
            body="Five palettes — dark (default), light, and three Color Vision Deficiency variants that keep admission-tier colors distinguishable."
          >
            <div className="grid grid-cols-2 gap-2 md:grid-cols-3">
              {THEMES.map((opt) => {
                const active = theme === opt.id;
                return (
                  <button
                    key={opt.id}
                    type="button"
                    onClick={() => setTheme(opt.id)}
                    className={cn(
                      "flex items-start gap-2 rounded-lg border p-3 text-left transition-colors",
                      active
                        ? "border-accent bg-accent/10"
                        : "border-border hover:border-accent/60 hover:bg-muted/40",
                    )}
                  >
                    <span
                      className={cn(
                        "mt-0.5 flex size-4 shrink-0 items-center justify-center rounded-full border",
                        active ? "border-accent bg-accent" : "border-border",
                      )}
                    >
                      {active && <Check className="size-2.5 text-accent-foreground" />}
                    </span>
                    <div className="min-w-0">
                      <p className="text-xs font-medium text-foreground">{opt.label}</p>
                      {opt.note && (
                        <p className="mt-0.5 text-[10px] text-muted-foreground">
                          {opt.note}
                        </p>
                      )}
                    </div>
                  </button>
                );
              })}
            </div>
          </Section>

          <Section
            Icon={Plug}
            title="MCP"
            body="The local MCP sidecar exposes the same tools (`ask`, `query_claims`, …) every cloud client uses, bound to 127.0.0.1 only. Paste a snippet into Claude Desktop, Cursor, Zed, etc. to connect."
          >
            <McpPane />
          </Section>

          <Section
            Icon={Bell}
            title="Channels"
            body="Optional mobile surfaces — reach ThinkingRoot from Telegram, Slack, or Discord. Adapter wiring lands when the channel messaging crate ships the MCP bridge."
          >
            <div className="rounded-lg border border-dashed border-border/70 p-3 text-[11px] text-muted-foreground">
              Channel adapters arrive in a follow-on phase.
            </div>
          </Section>
        </div>
      </div>
    </div>
  );
}

function AzureWorkspaceCard({
  wsLlm,
  wsPending,
  setWsPending,
  globalAzure,
  globalPending,
  setGlobalPending,
}: {
  wsLlm: WorkspaceLlmConfig | null;
  wsPending: Record<string, string>;
  setWsPending: (
    f: (prev: Record<string, string>) => Record<string, string>,
  ) => void;
  globalAzure: GlobalLlmConfig["azure"];
  globalPending: {
    azure?: {
      resource_name?: string;
      endpoint_base?: string;
      deployment?: string;
      api_version?: string;
    };
  };
  setGlobalPending: (
    f: (
      prev: {
        azure?: {
          resource_name?: string;
          endpoint_base?: string;
          deployment?: string;
          api_version?: string;
        };
      },
    ) => {
      azure?: {
        resource_name?: string;
        endpoint_base?: string;
        deployment?: string;
        api_version?: string;
      };
    },
  ) => void;
}) {
  // The workspace block edits THIS workspace's `.thinkingroot/config.toml`.
  // The global block edits the shared config the engine falls back to when
  // a workspace doesn't define its own LLM section.
  const wsField = (k: keyof typeof wsPending) => wsPending[k];

  function patchGlobalAzure(field: "resource_name" | "endpoint_base" | "deployment" | "api_version", value: string) {
    setGlobalPending((g) => ({
      ...g,
      azure: { ...(g.azure ?? {}), [field]: value },
    }));
  }

  return (
    <div className="mt-4 space-y-3">
      <div className="rounded-md border border-border/60 bg-muted/20 p-3">
        <div className="mb-3 flex items-center justify-between">
          <div className="text-xs font-medium text-foreground">Global Azure config</div>
          <span
            className={cn(
              "rounded px-1.5 py-0.5 text-[10px]",
              globalAzure.api_key_env_present
                ? "bg-success/15 text-success"
                : "bg-yellow-500/15 text-yellow-300",
            )}
          >
            {globalAzure.api_key_env_present
              ? `${globalAzure.api_key_env ?? "AZURE_OPENAI_API_KEY"} live`
              : `${globalAzure.api_key_env ?? "AZURE_OPENAI_API_KEY"} not in env`}
          </span>
        </div>
        <div className="grid grid-cols-1 gap-3 md:grid-cols-2">
          <Field label="Resource name">
            <input
              type="text"
              value={
                globalPending.azure?.resource_name ??
                globalAzure.resource_name ??
                ""
              }
              onChange={(e) => patchGlobalAzure("resource_name", e.target.value)}
              className={fieldClass}
              placeholder="my-company-openai"
            />
          </Field>
          <Field label="Endpoint base (override)">
            <input
              type="text"
              value={
                globalPending.azure?.endpoint_base ??
                globalAzure.endpoint_base ??
                ""
              }
              onChange={(e) => patchGlobalAzure("endpoint_base", e.target.value)}
              className={fieldClass}
              placeholder="https://*.cognitiveservices.azure.com"
            />
          </Field>
          <Field label="Deployment">
            <input
              type="text"
              value={
                globalPending.azure?.deployment ??
                globalAzure.deployment ??
                ""
              }
              onChange={(e) => patchGlobalAzure("deployment", e.target.value)}
              className={fieldClass}
              placeholder="gpt-4.1-mini"
            />
          </Field>
          <Field label="API version">
            <input
              type="text"
              value={
                globalPending.azure?.api_version ??
                globalAzure.api_version ??
                ""
              }
              onChange={(e) => patchGlobalAzure("api_version", e.target.value)}
              className={fieldClass}
              placeholder="2024-12-01-preview"
            />
          </Field>
        </div>
      </div>

      {wsLlm?.config_exists && (
        <div className="rounded-md border border-border/60 bg-muted/20 p-3">
          <div className="mb-3 flex items-center justify-between">
            <div className="text-xs font-medium text-foreground">
              Workspace override
              <span className="ml-2 font-mono text-[10px] text-muted-foreground">
                {wsLlm.workspace_path ?? "—"}
              </span>
            </div>
            <span
              className={cn(
                "rounded px-1.5 py-0.5 text-[10px]",
                wsLlm.azure_api_key_env_present
                  ? "bg-success/15 text-success"
                  : "bg-yellow-500/15 text-yellow-300",
              )}
            >
              {wsLlm.azure_api_key_env_present
                ? `${wsLlm.azure_api_key_env ?? "AZURE_OPENAI_API_KEY"} live`
                : `${wsLlm.azure_api_key_env ?? "AZURE_OPENAI_API_KEY"} not in env`}
            </span>
          </div>
          <div className="grid grid-cols-1 gap-3 md:grid-cols-2">
            <Field label="Resource name">
              <input
                type="text"
                value={wsField("azure_resource_name") ?? wsLlm.azure_resource_name ?? ""}
                onChange={(e) =>
                  setWsPending((p) => ({ ...p, azure_resource_name: e.target.value }))
                }
                className={fieldClass}
              />
            </Field>
            <Field label="Endpoint base (override)">
              <input
                type="text"
                value={wsField("azure_endpoint_base") ?? wsLlm.azure_endpoint_base ?? ""}
                onChange={(e) =>
                  setWsPending((p) => ({ ...p, azure_endpoint_base: e.target.value }))
                }
                className={fieldClass}
              />
            </Field>
            <Field label="Deployment">
              <input
                type="text"
                value={wsField("azure_deployment") ?? wsLlm.azure_deployment ?? ""}
                onChange={(e) =>
                  setWsPending((p) => ({ ...p, azure_deployment: e.target.value }))
                }
                className={fieldClass}
              />
            </Field>
            <Field label="API version">
              <input
                type="text"
                value={wsField("azure_api_version") ?? wsLlm.azure_api_version ?? ""}
                onChange={(e) =>
                  setWsPending((p) => ({ ...p, azure_api_version: e.target.value }))
                }
                className={fieldClass}
              />
            </Field>
            <Field label="Extraction model">
              <input
                type="text"
                value={wsField("extraction_model") ?? wsLlm.extraction_model ?? ""}
                onChange={(e) =>
                  setWsPending((p) => ({ ...p, extraction_model: e.target.value }))
                }
                className={fieldClass}
              />
            </Field>
            <Field label="Compilation model">
              <input
                type="text"
                value={wsField("compilation_model") ?? wsLlm.compilation_model ?? ""}
                onChange={(e) =>
                  setWsPending((p) => ({ ...p, compilation_model: e.target.value }))
                }
                className={fieldClass}
              />
            </Field>
          </div>
        </div>
      )}
    </div>
  );
}

function Header({
  dirty,
  saving,
  onSave,
  configPath,
  credentialsPath,
}: {
  dirty: boolean;
  saving: boolean;
  onSave: () => void;
  configPath?: string;
  credentialsPath?: string;
}) {
  return (
    <div className="flex shrink-0 items-center justify-between gap-3 border-b border-border bg-surface px-3 py-2">
      <div className="flex items-center gap-2">
        <SettingsIcon className="size-4 text-accent" />
        <h2 className="text-sm font-medium tracking-tight">Settings</h2>
        {configPath && (
          <span
            className="font-mono text-[10px] text-muted-foreground"
            title={`config: ${configPath}\ncredentials: ${credentialsPath ?? "—"}`}
          >
            {configPath}
          </span>
        )}
      </div>
      <div className="flex items-center gap-2">
        {dirty && !saving && (
          <motion.span
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            className="flex items-center gap-1 text-[11px] text-warn"
          >
            <AlertTriangle className="size-3" />
            unsaved changes
          </motion.span>
        )}
        <Button
          onClick={onSave}
          disabled={!dirty || saving}
          size="sm"
          className="h-7 gap-1 text-xs"
        >
          <Save className="size-3" />
          {saving ? "Saving…" : "Save"}
        </Button>
      </div>
    </div>
  );
}

function Section({
  Icon,
  title,
  body,
  children,
}: {
  Icon: typeof SettingsIcon;
  title: string;
  body?: string;
  children: React.ReactNode;
}) {
  return (
    <section className="rounded-xl border border-border bg-surface p-5">
      <header className="flex items-start gap-3">
        <div className="flex size-8 shrink-0 items-center justify-center rounded-md bg-accent/10 text-accent">
          <Icon className="size-4" />
        </div>
        <div className="min-w-0">
          <h3 className="text-sm font-medium tracking-tight text-foreground">{title}</h3>
          {body && <p className="mt-0.5 text-xs leading-relaxed text-muted-foreground">{body}</p>}
        </div>
      </header>
      <div className="mt-4">{children}</div>
    </section>
  );
}

const MCP_TOOLS: Array<{ id: McpToolKey; label: string }> = [
  { id: "claude-desktop", label: "Claude Desktop" },
  { id: "claude-code", label: "Claude Code" },
  { id: "cursor", label: "Cursor" },
  { id: "windsurf", label: "Windsurf" },
  { id: "vs-code", label: "VS Code" },
  { id: "zed", label: "Zed" },
  { id: "cline", label: "Cline" },
  { id: "gemini-cli", label: "Gemini CLI" },
  { id: "codex", label: "Codex" },
];

function McpPane() {
  const [status, setStatus] = useState<McpStatus | null>(null);
  const [tool, setTool] = useState<McpToolKey>("claude-desktop");
  const [snippet, setSnippet] = useState<string>("");
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const [s, snip] = await Promise.all([
          mcpStatus(),
          mcpGetConfigSnippet(tool),
        ]);
        if (cancelled) return;
        setStatus(s);
        setSnippet(snip);
      } catch (err) {
        if (!cancelled) {
          toast("MCP status failed", {
            kind: "error",
            body: err instanceof Error ? err.message : String(err),
          });
        }
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [tool]);

  async function copySnippet() {
    try {
      await writeText(snippet);
      toast("Snippet copied", {
        kind: "success",
        body: "Paste into the AI tool's MCP config and restart it.",
        durationMs: 4000,
      });
    } catch (err) {
      toast("Copy failed", {
        kind: "error",
        body: err instanceof Error ? err.message : String(err),
      });
    }
  }

  return (
    <div className="flex flex-col gap-3">
      <div className="flex items-center gap-2 rounded-md border border-border bg-background px-3 py-2 text-[11px]">
        <span
          className={cn(
            "inline-block size-2 rounded-full",
            status?.running ? "bg-success" : "bg-warn",
          )}
        />
        {loading ? (
          <span className="text-muted-foreground">Probing sidecar…</span>
        ) : status?.running ? (
          <>
            <span className="text-foreground">
              Sidecar running on{" "}
              <code className="font-mono">
                {status.host}:{status.port}
              </code>
            </span>
            {status.pid !== null && (
              <span className="text-muted-foreground">pid {status.pid}</span>
            )}
            <a
              href={status.well_known_url}
              target="_blank"
              rel="noreferrer"
              className="ml-auto text-accent hover:underline"
            >
              /.well-known/mcp
            </a>
          </>
        ) : (
          <span className="text-warn">
            Sidecar is not running — install the OSS `root` binary or set
            <code className="mx-1 font-mono">THINKINGROOT_ROOT_BINARY</code>.
          </span>
        )}
      </div>

      <div className="flex flex-wrap gap-2">
        {MCP_TOOLS.map((opt) => {
          const active = tool === opt.id;
          return (
            <button
              key={opt.id}
              type="button"
              onClick={() => setTool(opt.id)}
              className={cn(
                "rounded-md border px-3 py-1.5 text-xs transition-colors",
                active
                  ? "border-accent bg-accent/15 text-accent"
                  : "border-border text-muted-foreground hover:text-foreground",
              )}
            >
              {opt.label}
            </button>
          );
        })}
      </div>

      <div className="relative">
        <pre className="overflow-x-auto rounded-md border border-border bg-background p-3 font-mono text-[11px] leading-relaxed text-foreground">
          {snippet || "(loading…)"}
        </pre>
        <Button
          size="sm"
          variant="outline"
          onClick={copySnippet}
          disabled={!snippet}
          className="absolute right-2 top-2 h-7 gap-1 text-[11px]"
        >
          <Copy className="size-3" /> Copy
        </Button>
      </div>

      <p className="text-[10px] text-muted-foreground">
        Stdio entries spawn `root serve --mcp-stdio` per session, so the AI
        tool talks to the local engine directly. Restart the AI tool after
        pasting the snippet.
      </p>
    </div>
  );
}

function Field({
  label,
  hint,
  children,
}: {
  label: string;
  hint?: string;
  children: React.ReactNode;
}) {
  const id = useMemo(() => `f-${label.replace(/\s+/g, "-").toLowerCase()}`, [label]);
  return (
    <label htmlFor={id} className="flex flex-col gap-1">
      <span className="text-[10px] font-semibold uppercase tracking-widest text-muted-foreground">
        {label}
      </span>
      <div id={id}>{children}</div>
      {hint && <span className="text-[10px] text-muted-foreground">{hint}</span>}
    </label>
  );
}

const fieldClass =
  "h-8 w-full rounded-md border border-input bg-background px-2 text-xs font-mono text-foreground focus:border-accent focus:outline-none focus:ring-1 focus:ring-accent/40";
