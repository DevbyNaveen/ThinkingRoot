import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Plug, Check, Copy, Loader2, X, Trash2 } from "lucide-react";
import {
  configPaths,
  credentialsRemove,
  credentialsSet,
  credentialsStatus,
  globalConfigRead,
  globalConfigWrite,
  mcpConfigureTool,
  mcpGetConfigSnippet,
  mcpStatus,
  workspaceList,
  workspaceLlmConfig,
  workspaceLlmWrite,
  workspaceRemove,
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
import { PasswordInput } from "@/components/ui/password-input";
import { cn } from "@/lib/utils";
import type { SettingsSectionId, Theme } from "@/types";

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

/** Large title + subtitle for the active settings category (center pane). */
const SETTINGS_PAGE_META: Record<SettingsSectionId, { title: string; subtitle: string }> = {
  provider: {
    title: "Provider",
    subtitle:
      "Pick the LLM backend and credentials. Keys live in credentials.toml on this machine; changes save automatically after you pause typing.",
  },
  workspace: {
    title: "Workspace",
    subtitle:
      "Every answer is grounded in the compiled workspace you select. Switch from here or from the sidebar.",
  },
  appearance: {
    title: "Appearance",
    subtitle: "Theme applies immediately across the app. CVD palettes keep trust colors distinguishable.",
  },
  mcp: {
    title: "MCP",
    subtitle:
      "The local sidecar exposes the same tools as cloud clients, bound to 127.0.0.1. Auto-configure your editor or copy the snippet.",
  },
  channels: {
    title: "Channels",
    subtitle: "Optional Telegram, Slack, or Discord surfaces when the channel bridge ships.",
  },
};

const AUTOSAVE_MS = 750;

/**
 * Settings surface. Reads / writes the same files the CLI does.
 * Provider keys, global LLM config, and workspace LLM overrides persist
 * automatically (debounced) after edits; use **Back to chats** in the
 * left rail to leave. Theme is saved immediately via the app store.
 *
 *   ~/<config_dir>/thinkingroot/credentials.toml   ← provider keys
 *   ~/<config_dir>/thinkingroot/config.toml        ← provider/model defaults
 *   ~/<config_dir>/thinkingroot/workspaces.toml    ← workspace registry + active
 *   <workspace>/.thinkingroot/config.toml          ← per-workspace LLM overrides
 */
export function SettingsView() {
  const theme = useApp((s) => s.theme);
  const setTheme = useApp((s) => s.setTheme);
  const settingsSection = useApp((s) => s.settingsSection);
  const setAppActiveWorkspace = useApp((s) => s.setActiveWorkspace);

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
  const persistInFlight = useRef(false);
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
        const activeName = active?.name ?? null;
        setActiveWorkspaceLocal(activeName);
        useApp.getState().setActiveWorkspace(activeName);
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
    Object.values(keyDraft).some((v) => (v ?? "").trim().length > 0) ||
    Object.keys(wsPending).length > 0 ||
    Object.keys(globalPending).length > 0;

  const persist = useCallback(async () => {
    if (persistInFlight.current) return;
    persistInFlight.current = true;
    setSaving(true);
    try {
      for (const [envVar, value] of Object.entries(keyDraft)) {
        if (value.trim().length > 0) {
          await credentialsSet(envVar, value.trim());
        }
      }

      if (Object.keys(globalPending).length > 0) {
        const patch = { ...globalPending };
        if (patch.default_provider === undefined) {
          patch.default_provider = provider;
        }
        await globalConfigWrite(patch);
      }

      if (wsLlm?.workspace_path && Object.keys(wsPending).length > 0) {
        await workspaceLlmWrite({
          workspace_path: wsLlm.workspace_path,
          provider,
          extraction_model: wsPending.extraction_model,
          compilation_model: wsPending.compilation_model,
        });
      }

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
      persistInFlight.current = false;
    }
  }, [
    keyDraft,
    globalPending,
    wsPending,
    provider,
    wsLlm,
  ]);

  useEffect(() => {
    if (loading || !paths || !globalCfg) return;
    if (!dirty) return;
    const id = window.setTimeout(() => {
      void persist();
    }, AUTOSAVE_MS);
    return () => window.clearTimeout(id);
  }, [dirty, loading, paths, globalCfg, persist]);

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
      setAppActiveWorkspace(name);
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

  async function removeWorkspaceFromRegistry(name: string) {
    const ok = window.confirm(
      `Remove workspace “${name}” from ThinkingRoot?\n\nThis only unregisters the folder — it does not delete your project or .thinkingroot on disk.`,
    );
    if (!ok) return;
    try {
      const removed = await workspaceRemove(name);
      if (!removed) {
        toast("Remove workspace", {
          kind: "warn",
          body: `No registered workspace named “${name}”.`,
        });
        return;
      }
      const wsRes = await workspaceList();
      setWorkspaces(wsRes);
      const next =
        wsRes.find((w) => w.active) ?? (wsRes.length > 0 ? wsRes[0] : undefined);
      if (next) {
        await workspaceSetActive(next.name);
        setActiveWorkspaceLocal(next.name);
        setAppActiveWorkspace(next.name);
        try {
          const llm = await workspaceLlmConfig(next.path);
          setWsLlm(llm);
        } catch {
          setWsLlm(null);
        }
      } else {
        setActiveWorkspaceLocal(null);
        setAppActiveWorkspace(null);
        setWsLlm(null);
      }
      toast("Workspace removed", {
        kind: "success",
        body: `${name} is no longer in the sidebar list.`,
      });
    } catch (e) {
      toast("Remove workspace failed", {
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
  const page = SETTINGS_PAGE_META[settingsSection];

  return (
    <div className="flex h-full flex-col">
      <div className="relative min-h-0 flex-1 overflow-y-auto">
        {saving ? (
          <div
            className="pointer-events-none absolute right-6 top-6 z-10 flex items-center gap-1.5 rounded-lg border border-border/50 bg-background/85 px-2.5 py-1.5 text-[11px] tabular-nums text-muted-foreground shadow-sm backdrop-blur-md"
            aria-live="polite"
          >
            <Loader2 className="size-3.5 animate-spin" aria-hidden />
            Saving…
          </div>
        ) : null}
        <div className="mx-auto w-full max-w-xl px-6 pb-20 pt-10 sm:px-10 sm:pt-12 lg:max-w-2xl lg:px-14">
          <header className="mb-10 space-y-2 sm:mb-12">
            <h1 className="text-2xl font-semibold tracking-tight text-foreground sm:text-[1.7rem] sm:leading-tight">
              {page.title}
            </h1>
            <p className="max-w-lg text-[13px] leading-relaxed text-muted-foreground sm:text-sm">
              {page.subtitle}
            </p>
          </header>

          <div className="flex flex-col gap-10 sm:gap-12">
            {settingsSection === "provider" && (
              <>
                <SettingsGroup label="Backend">
                  <div className="divide-y divide-border/35">
                    <SettingsRow
                      label="Default provider"
                      description="Used for extraction and compilation unless a workspace sets its own models."
                    >
                      <div className="flex flex-wrap justify-end gap-2">
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
                                "flex items-center gap-1.5 rounded-lg border px-3 py-2 text-xs font-medium transition-colors",
                                active
                                  ? "border-accent bg-accent/15 text-accent"
                                  : "border-border/80 bg-background/40 text-muted-foreground hover:border-border hover:text-foreground",
                              )}
                            >
                              {configured && <Check className="size-3 text-success" />}
                              {p.label}
                            </button>
                          );
                        })}
                      </div>
                    </SettingsRow>
                    <SettingsRow
                      label={`${providerMeta.label} API key`}
                      description={
                        credForActive?.persisted
                          ? `Stored in credentials.toml${credForActive.in_process_env ? " · also in process env" : ""}.`
                          : credForActive?.in_process_env
                            ? "Set in process environment (not persisted to file)."
                            : "Not configured yet — paste a key to store it locally."
                      }
                    >
                      <div className="space-y-2">
                        <div className="flex flex-wrap gap-2">
                          <PasswordInput
                            placeholder={providerMeta.placeholder}
                            value={keyDraft[providerMeta.env_var] ?? ""}
                            onChange={(e) =>
                              setKeyDraft((d) => ({
                                ...d,
                                [providerMeta.env_var]: e.target.value,
                              }))
                            }
                            className="h-9 min-w-[12rem] flex-1 rounded-lg border border-input/80 bg-background/80 px-2.5 text-xs text-foreground placeholder:text-muted-foreground focus:outline-none"
                          />
                          {credForActive?.persisted && (
                            <Button
                              type="button"
                              size="sm"
                              variant="outline"
                              onClick={() => void clearKey(providerMeta.env_var)}
                              className="h-9 gap-1 text-[11px]"
                            >
                              <X className="size-3" /> Clear
                            </Button>
                          )}
                        </div>
                        <p className="text-[11px] text-muted-foreground">
                          Engine reads{" "}
                          <code className="rounded bg-muted/80 px-1 font-mono text-[10px]">
                            {providerMeta.env_var}
                          </code>
                        </p>
                      </div>
                    </SettingsRow>
                  </div>
                </SettingsGroup>
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
              </>
            )}

            {settingsSection === "workspace" && (
              <SettingsGroup label="Registered workspaces">
                {workspaces.length === 0 ? (
                  <p className="px-4 py-6 text-sm text-muted-foreground">
                    No workspaces registered yet. Add one from the sidebar.
                  </p>
                ) : (
                  <div className="divide-y divide-border/35">
                    {workspaces.map((w) => {
                      const isActive = activeWorkspace === w.name;
                      return (
                        <div
                          key={w.name}
                          className={cn(
                            "flex items-stretch gap-0 transition-colors",
                            isActive ? "bg-accent/[0.07]" : "hover:bg-muted/25",
                          )}
                        >
                          <button
                            type="button"
                            onClick={() => void changeActiveWorkspace(w.name)}
                            className="flex min-w-0 flex-1 items-center justify-between gap-4 px-4 py-4 text-left"
                          >
                            <div className="min-w-0 space-y-1">
                              <p className="text-sm font-medium text-foreground">
                                {w.name}
                                {w.compiled && (
                                  <span className="ml-2 align-middle rounded-md bg-success/15 px-1.5 py-0.5 text-[10px] font-semibold uppercase tracking-wide text-success">
                                    compiled
                                  </span>
                                )}
                              </p>
                              <p className="break-all font-mono text-[11px] leading-snug text-muted-foreground">
                                {w.path}
                              </p>
                            </div>
                            {isActive && (
                              <Check className="size-4 shrink-0 text-accent" aria-label="Active" />
                            )}
                          </button>
                          <Button
                            type="button"
                            variant="ghost"
                            size="icon"
                            className="my-2 mr-2 h-9 w-9 shrink-0 rounded-lg text-muted-foreground hover:bg-destructive/15 hover:text-destructive"
                            aria-label={`Remove ${w.name} from workspace list`}
                            title="Remove from list (does not delete files)"
                            onClick={() => void removeWorkspaceFromRegistry(w.name)}
                          >
                            <Trash2 className="size-4" />
                          </Button>
                        </div>
                      );
                    })}
                  </div>
                )}
              </SettingsGroup>
            )}

            {settingsSection === "appearance" && (
              <SettingsGroup label="Theme">
                <div className="space-y-3 p-4 sm:p-5">
                  <div className="space-y-1">
                    <p className="text-sm font-medium text-foreground">Color palette</p>
                    <p className="text-[13px] leading-snug text-muted-foreground">
                      Dark is the default. CVD options tune admission-tier colors for protanopia,
                      deuteranopia, and tritanopia.
                    </p>
                  </div>
                  <div className="grid grid-cols-1 gap-2 sm:grid-cols-2 lg:grid-cols-3">
                    {THEMES.map((opt) => {
                      const active = theme === opt.id;
                      return (
                        <button
                          key={opt.id}
                          type="button"
                          onClick={() => setTheme(opt.id)}
                          className={cn(
                            "flex items-start gap-3 rounded-xl border p-3.5 text-left transition-colors",
                            active
                              ? "border-accent bg-accent/10 shadow-sm"
                              : "border-border/60 bg-background/30 hover:border-border hover:bg-muted/30",
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
                            <p className="text-sm font-medium text-foreground">{opt.label}</p>
                            {opt.note && (
                              <p className="mt-0.5 text-[11px] text-muted-foreground">{opt.note}</p>
                            )}
                          </div>
                        </button>
                      );
                    })}
                  </div>
                </div>
              </SettingsGroup>
            )}

            {settingsSection === "mcp" && <McpPane />}

            {settingsSection === "channels" && (
              <SettingsGroup label="Messaging">
                <p className="px-4 py-6 text-[13px] leading-relaxed text-muted-foreground sm:px-5">
                  Channel adapters arrive in a follow-on phase — Telegram, Slack, and Discord will
                  plug into the same MCP bridge the desktop uses.
                </p>
              </SettingsGroup>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

function SettingsGroup({
  label,
  children,
}: {
  label?: string;
  children: React.ReactNode;
}) {
  return (
    <div className="space-y-2.5">
      {label ? (
        <h2 className="px-0.5 text-[11px] font-semibold uppercase tracking-[0.14em] text-muted-foreground/90">
          {label}
        </h2>
      ) : null}
      <div className="overflow-hidden rounded-xl border border-border/45 bg-muted/[0.14] shadow-sm dark:bg-muted/15">
        {children}
      </div>
    </div>
  );
}

function SettingsRow({
  label,
  description,
  children,
}: {
  label: string;
  description?: string;
  children: React.ReactNode;
}) {
  return (
    <div className="flex flex-col gap-3 px-4 py-4 sm:flex-row sm:items-start sm:justify-between sm:gap-8 sm:px-5 sm:py-5">
      <div className="min-w-0 flex-1 space-y-1">
        <p className="text-sm font-medium leading-snug text-foreground">{label}</p>
        {description ? (
          <p className="text-[13px] leading-relaxed text-muted-foreground">{description}</p>
        ) : null}
      </div>
      <div className="w-full shrink-0 sm:w-auto sm:max-w-[min(100%,22rem)] sm:pt-0.5">{children}</div>
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
    <div className="space-y-10">
      <SettingsGroup label="Azure — global">
        <div className="space-y-4 p-4 sm:p-5">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <p className="text-sm font-medium text-foreground">Shared defaults</p>
            <span
              className={cn(
                "rounded-md px-2 py-0.5 text-[10px] font-medium",
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
          <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
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
      </SettingsGroup>

      {wsLlm?.config_exists && (
        <SettingsGroup label="Azure — this workspace">
          <div className="space-y-4 p-4 sm:p-5">
            <p className="text-[13px] leading-relaxed text-muted-foreground">
              <span className="font-medium text-foreground">Override path</span>{" "}
              <span className="break-all font-mono text-[11px]">{wsLlm.workspace_path ?? "—"}</span>
            </p>
            <div className="grid grid-cols-1 gap-4 md:grid-cols-2">

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
        </SettingsGroup>
      )}
    </div>
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
  const [configuring, setConfiguring] = useState(false);

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

  async function configureSelectedTool() {
    setConfiguring(true);
    try {
      const result = await mcpConfigureTool(tool);
      toast(`${result.tool} configured`, {
        kind: "success",
        body: `Wrote ThinkingRoot MCP config to ${result.path}. Restart ${result.tool} to pick it up.`,
        durationMs: 7000,
      });
    } catch (err) {
      toast("Auto-config failed", {
        kind: "error",
        body: err instanceof Error ? err.message : String(err),
        durationMs: 8000,
      });
    } finally {
      setConfiguring(false);
    }
  }

  return (
    <div className="flex flex-col gap-10 sm:gap-12">
      <SettingsGroup label="Sidecar">
        <div className="flex flex-wrap items-center gap-2 px-4 py-4 text-[13px] sm:px-5 sm:py-5">
          <span
            className={cn(
              "inline-block size-2 shrink-0 rounded-full",
              status?.running ? "bg-success" : "bg-warn",
            )}
          />
          {loading ? (
            <span className="text-muted-foreground">Probing sidecar…</span>
          ) : status?.running ? (
            <>
              <span className="text-foreground">
                Running on{" "}
                <code className="font-mono text-[12px]">
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
                className="ml-auto text-sm text-accent hover:underline"
              >
                /.well-known/mcp
              </a>
            </>
          ) : (
            <span className="text-warn">
              Not running — install the OSS <code className="mx-1 font-mono text-[12px]">root</code> binary
              or set <code className="mx-1 font-mono text-[12px]">THINKINGROOT_ROOT_BINARY</code>.
            </span>
          )}
        </div>
      </SettingsGroup>

      <SettingsGroup label="Editor or CLI">
        <div className="flex flex-wrap gap-2 p-4 sm:p-5">
          {MCP_TOOLS.map((opt) => {
            const active = tool === opt.id;
            return (
              <button
                key={opt.id}
                type="button"
                onClick={() => setTool(opt.id)}
                className={cn(
                  "rounded-lg border px-3 py-2 text-xs font-medium transition-colors",
                  active
                    ? "border-accent bg-accent/15 text-accent"
                    : "border-border/80 bg-background/40 text-muted-foreground hover:border-border hover:text-foreground",
                )}
              >
                {opt.label}
              </button>
            );
          })}
        </div>
      </SettingsGroup>

      <SettingsGroup label="MCP config">
        <div className="space-y-4 p-4 sm:p-5">
          <pre className="max-h-[min(50vh,22rem)] overflow-x-auto overflow-y-auto rounded-lg border border-border/40 bg-background/50 p-3 font-mono text-[11px] leading-relaxed text-foreground">
            {snippet || "(loading…)"}
          </pre>
          <div className="flex flex-wrap gap-2">
            <Button
              size="sm"
              variant="default"
              onClick={() => void configureSelectedTool()}
              disabled={configuring}
              className="h-9 gap-1.5 text-[12px]"
            >
              {configuring ? (
                <Loader2 className="size-3.5 animate-spin" />
              ) : (
                <Plug className="size-3.5" />
              )}
              Auto configure
            </Button>
            <Button
              size="sm"
              variant="outline"
              onClick={copySnippet}
              disabled={!snippet}
              className="h-9 gap-1.5 text-[12px]"
            >
              <Copy className="size-3.5" /> Copy snippet
            </Button>
          </div>
          <p className="text-[12px] leading-relaxed text-muted-foreground">
            Auto configure writes the entry below to the tool&apos;s config. Stdio tools spawn{" "}
            <code className="rounded bg-muted/80 px-1 font-mono text-[11px]">root serve --mcp-stdio</code> per
            session — restart the editor after writing.
          </p>
        </div>
      </SettingsGroup>
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
  "h-9 w-full rounded-lg border border-input/70 bg-background/70 px-2.5 text-xs font-mono text-foreground focus:outline-none";
