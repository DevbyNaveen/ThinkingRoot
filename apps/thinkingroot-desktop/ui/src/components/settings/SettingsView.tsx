import { useEffect, useMemo, useState } from "react";
import {
  Settings as SettingsIcon,
  KeyRound,
  FolderOpen,
  Paintbrush,
  Bell,
  Plug,
  ShieldCheck,
  Save,
  AlertTriangle,
  FileText,
  Check,
  Copy,
} from "lucide-react";
import { motion } from "framer-motion";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import {
  configRead,
  configWrite,
  mcpGetConfigSnippet,
  mcpStatus,
  type ConfigRead,
  type McpStatus,
  type McpToolKey,
} from "@/lib/tauri";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";
import { useApp } from "@/store/app";
import { toast } from "@/store/toast";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import type { Theme } from "@/types";

type ProviderKey = "azure" | "anthropic" | "openai" | "gemini";

const PROVIDER_META: Record<ProviderKey, {
  label: string;
  keys: Array<{ name: string; label: string; secret?: boolean; placeholder?: string }>;
}> = {
  azure: {
    label: "Azure OpenAI",
    keys: [
      { name: "AZURE_OPENAI_KEY", label: "API key", secret: true, placeholder: "sk-…" },
      { name: "AZURE_OPENAI_ENDPOINT", label: "Endpoint", placeholder: "https://myres.openai.azure.com" },
      { name: "AZURE_OPENAI_DEPLOYMENT", label: "Deployment", placeholder: "gpt-4.1-mini" },
      { name: "AZURE_OPENAI_API_VERSION", label: "API version", placeholder: "2024-10-21" },
    ],
  },
  anthropic: {
    label: "Anthropic",
    keys: [{ name: "ANTHROPIC_API_KEY", label: "API key", secret: true, placeholder: "sk-ant-…" }],
  },
  openai: {
    label: "OpenAI",
    keys: [{ name: "OPENAI_API_KEY", label: "API key", secret: true, placeholder: "sk-…" }],
  },
  gemini: {
    label: "Google Gemini",
    keys: [{ name: "GEMINI_API_KEY", label: "API key", secret: true, placeholder: "AIza…" }],
  },
};

const THEMES: Array<{ id: Theme; label: string; note?: string }> = [
  { id: "dark", label: "Dark", note: "Catppuccin Mocha" },
  { id: "light", label: "Light" },
  { id: "auto", label: "Auto", note: "Follow system" },
  { id: "daltonized-protanopia", label: "CVD · Protanopia", note: "red-deficient palette" },
  { id: "daltonized-deuteranopia", label: "CVD · Deuteranopia", note: "green-deficient palette" },
  { id: "daltonized-tritanopia", label: "CVD · Tritanopia", note: "blue-deficient palette" },
];

/**
 * Settings surface. Single scrollable pane with 5 sections: Provider,
 * Workspace, Appearance, Channels (stub), Covenant. Writes go to the
 * same `~/.config/thinkingroot/config.toml` the CLI uses, atomically,
 * chmod 0600.
 */
export function SettingsView() {
  const theme = useApp((s) => s.theme);
  const setTheme = useApp((s) => s.setTheme);
  const setCovenantOpen = useApp((s) => s.setCovenantOpen);

  const [cfg, setCfg] = useState<ConfigRead | null>(null);
  const [provider, setProvider] = useState<ProviderKey>("azure");
  const [pending, setPending] = useState<Record<string, string>>({});
  const [workspace, setWorkspace] = useState("");
  const [workspaceName, setWorkspaceName] = useState("main");
  const [saving, setSaving] = useState(false);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const c = await configRead();
        if (cancelled) return;
        setCfg(c);

        // Infer which provider is already configured and pre-select it.
        const existing = (Object.keys(PROVIDER_META) as ProviderKey[]).find((p) =>
          PROVIDER_META[p].keys.some((k) => k.secret && c.entries[k.name]),
        );
        if (existing) setProvider(existing);

        // Hydrate workspace fields.
        setWorkspace(c.entries.THINKINGROOT_WORKSPACE ?? "");
        setWorkspaceName(c.entries.THINKINGROOT_WORKSPACE_NAME ?? "main");
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

  const dirty = Object.keys(pending).length > 0;
  const providerMeta = PROVIDER_META[provider];

  function updateField(name: string, value: string) {
    setPending((p) => ({ ...p, [name]: value }));
  }

  async function pickWorkspace() {
    try {
      const picked = await openDialog({
        directory: true,
        multiple: false,
        title: "Choose workspace root",
      });
      if (typeof picked === "string" && picked.length > 0) {
        setWorkspace(picked);
        setPending((p) => ({ ...p, THINKINGROOT_WORKSPACE: picked }));
      }
    } catch (e) {
      toast("Folder picker failed", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
    }
  }

  async function save() {
    setSaving(true);
    try {
      // Also commit provider choice as the default.
      const set: Record<string, string> = { ...pending };
      if (workspace && set.THINKINGROOT_WORKSPACE === undefined) {
        set.THINKINGROOT_WORKSPACE = workspace;
      }
      if (workspaceName) {
        set.THINKINGROOT_WORKSPACE_NAME = workspaceName;
      }
      set.THINKINGROOT_PROVIDER = provider;
      const wrote = await configWrite({ set });
      toast("Settings saved", { kind: "success", body: wrote, durationMs: 3000 });
      setPending({});
      // Re-read so masked keys refresh.
      const reread = await configRead();
      setCfg(reread);
    } catch (e) {
      toast("Save failed", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
    } finally {
      setSaving(false);
    }
  }

  if (loading || !cfg) {
    return (
      <div className="flex h-full items-center justify-center text-sm text-muted-foreground">
        Loading settings…
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col">
      <Header dirty={dirty} saving={saving} onSave={save} path={cfg.path} />
      <div className="flex-1 overflow-y-auto">
        <div className="mx-auto flex w-full max-w-3xl flex-col gap-6 px-6 py-8">
          <Section
            Icon={KeyRound}
            title="Provider"
            body="Pick the LLM backend ThinkingRoot talks to. API keys live only on this machine, chmod 0600, never uploaded."
          >
            <div className="flex flex-wrap gap-2">
              {(Object.keys(PROVIDER_META) as ProviderKey[]).map((p) => {
                const active = provider === p;
                const configured = PROVIDER_META[p].keys.some(
                  (k) => k.secret && cfg.entries[k.name],
                );
                return (
                  <button
                    key={p}
                    type="button"
                    onClick={() => setProvider(p)}
                    className={cn(
                      "flex items-center gap-1.5 rounded-md border px-3 py-1.5 text-xs transition-colors",
                      active
                        ? "border-accent bg-accent/15 text-accent"
                        : "border-border text-muted-foreground hover:text-foreground",
                    )}
                  >
                    {configured && <Check className="size-3 text-success" />}
                    {PROVIDER_META[p].label}
                  </button>
                );
              })}
            </div>
            <div className="mt-4 grid grid-cols-1 gap-3 md:grid-cols-2">
              {providerMeta.keys.map((field) => (
                <Field
                  key={field.name}
                  label={field.label}
                  hint={
                    field.secret && cfg.entries[field.name]
                      ? `stored: ${cfg.entries[field.name]}`
                      : undefined
                  }
                >
                  <input
                    type={field.secret ? "password" : "text"}
                    placeholder={
                      (field.secret && cfg.entries[field.name]) ||
                      field.placeholder ||
                      field.label
                    }
                    value={pending[field.name] ?? ""}
                    onChange={(e) => updateField(field.name, e.target.value)}
                    className="h-8 w-full rounded-md border border-input bg-background px-2 text-xs text-foreground placeholder:text-muted-foreground focus:border-accent focus:outline-none focus:ring-1 focus:ring-accent/40"
                  />
                </Field>
              ))}
            </div>
          </Section>

          <Section
            Icon={FolderOpen}
            title="Workspace"
            body="Your mounted ThinkingRoot workspace — the compiled KG that provenance pills and Brain view read from."
          >
            <div className="grid grid-cols-1 gap-3 md:grid-cols-[1fr_200px]">
              <Field label="Workspace path">
                <div className="flex gap-2">
                  <input
                    type="text"
                    value={workspace}
                    placeholder="/Users/you/Desktop/thinkingroot/.thinkingroot-workspace"
                    onChange={(e) => {
                      setWorkspace(e.target.value);
                      updateField("THINKINGROOT_WORKSPACE", e.target.value);
                    }}
                    className="h-8 flex-1 rounded-md border border-input bg-background px-2 text-xs font-mono text-foreground placeholder:text-muted-foreground focus:border-accent focus:outline-none focus:ring-1 focus:ring-accent/40"
                  />
                  <Button variant="outline" size="sm" onClick={pickWorkspace} className="h-8 gap-1 text-xs">
                    <FolderOpen className="size-3" /> Pick
                  </Button>
                </div>
              </Field>
              <Field label="Workspace name">
                <input
                  type="text"
                  value={workspaceName}
                  onChange={(e) => {
                    setWorkspaceName(e.target.value);
                    updateField("THINKINGROOT_WORKSPACE_NAME", e.target.value);
                  }}
                  className="h-8 w-full rounded-md border border-input bg-background px-2 text-xs text-foreground placeholder:text-muted-foreground focus:border-accent focus:outline-none focus:ring-1 focus:ring-accent/40"
                />
              </Field>
            </div>
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
              Channel adapters arrive in a follow-on phase. Until then, set{" "}
              <code className="rounded bg-muted px-1 font-mono text-[10px]">
                TELEGRAM_BOT_TOKEN
              </code>
              ,{" "}
              <code className="rounded bg-muted px-1 font-mono text-[10px]">
                SLACK_BOT_TOKEN
              </code>
              , and{" "}
              <code className="rounded bg-muted px-1 font-mono text-[10px]">
                DISCORD_BOT_TOKEN
              </code>{" "}
              in config.toml and the CLI picks them up.
            </div>
          </Section>

          <Section
            Icon={ShieldCheck}
            title="Covenant"
            body="The five-commitment contract between you and every ThinkingRoot agent. Ed25519-signed; the fingerprint is embedded in every trace record."
          >
            <div className="flex items-center justify-between rounded-lg border border-border px-3 py-2 text-xs">
              <div className="flex items-center gap-2">
                <FileText className="size-3.5 text-accent" />
                <span className="font-mono text-[11px] text-foreground">
                  covenant-0.1
                </span>
                {cfg.entries.THINKINGROOT_VERIFYING_KEY ? (
                  <span className="text-success">signed</span>
                ) : (
                  <span className="text-warn">not signed yet</span>
                )}
              </div>
              <Button
                variant="outline"
                size="sm"
                onClick={() => setCovenantOpen(true)}
                className="h-7 text-[11px]"
              >
                View covenant
              </Button>
            </div>
          </Section>
        </div>
      </div>
    </div>
  );
}

function Header({
  dirty,
  saving,
  onSave,
  path,
}: {
  dirty: boolean;
  saving: boolean;
  onSave: () => void;
  path?: string | null;
}) {
  return (
    <div className="flex shrink-0 items-center justify-between gap-3 border-b border-border bg-surface px-3 py-2">
      <div className="flex items-center gap-2">
        <SettingsIcon className="size-4 text-accent" />
        <h2 className="text-sm font-medium tracking-tight">Settings</h2>
        {path && (
          <span className="font-mono text-[10px] text-muted-foreground" title={path}>
            {path}
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
  // `useMemo` keeps the label-id stable across renders.
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
