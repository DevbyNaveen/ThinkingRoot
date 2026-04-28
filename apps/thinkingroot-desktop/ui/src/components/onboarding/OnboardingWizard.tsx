import { useEffect, useState } from "react";
import { motion, AnimatePresence } from "framer-motion";
import {
  Sparkles,
  KeyRound,
  FolderOpen,
  ShieldCheck,
  Bell,
  Check,
  ArrowRight,
  ArrowLeft,
  ExternalLink,
} from "lucide-react";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import {
  credentialsSet,
  globalConfigWrite,
  workspaceAdd,
  workspaceSetActive,
} from "@/lib/tauri";
import { toast } from "@/store/toast";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

interface Props {
  open: boolean;
  onComplete: () => void;
  onSkip: () => void;
}

type ProviderChoice = "azure" | "anthropic" | "openai";

const STEPS: Array<{
  id: number;
  title: string;
  Icon: typeof Sparkles;
  description: string;
}> = [
  {
    id: 1,
    Icon: Sparkles,
    title: "Welcome",
    description:
      "The local-first personal AI that remembers, proves, and never acts without your covenant.",
  },
  {
    id: 2,
    Icon: KeyRound,
    title: "Choose a provider",
    description:
      "Pick the LLM backend. Your API key stays on this machine, chmod 0600, never uploaded.",
  },
  {
    id: 3,
    Icon: FolderOpen,
    title: "Pick a workspace",
    description:
      "Your ThinkingRoot workspace — the compiled knowledge graph the engine reads from and writes to.",
  },
  {
    id: 4,
    Icon: Bell,
    title: "Channels (optional)",
    description: "Telegram / Slack / Discord. You can skip this and add them later from Settings.",
  },
  {
    id: 5,
    Icon: ShieldCheck,
    title: "Covenant",
    description:
      "The five-commitment contract. You can review it now or later; nothing is signed until D-11.",
  },
];

export function OnboardingWizard({ open, onComplete, onSkip }: Props) {
  const [step, setStep] = useState(1);
  const [provider, setProvider] = useState<ProviderChoice>("azure");
  const [azureKey, setAzureKey] = useState("");
  const [azureResource, setAzureResource] = useState("");
  const [azureDeployment, setAzureDeployment] = useState("gpt-4.1-mini");
  const [azureApiVersion, setAzureApiVersion] = useState("2024-12-01-preview");
  const [anthropicKey, setAnthropicKey] = useState("");
  const [openaiKey, setOpenaiKey] = useState("");
  const [workspace, setWorkspace] = useState("");
  const [workspaceName, setWorkspaceName] = useState("main");
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    if (open) setStep(1);
  }, [open]);

  async function pickWorkspace() {
    try {
      const picked = await openDialog({
        directory: true,
        multiple: false,
        title: "Choose workspace root",
      });
      if (typeof picked === "string") setWorkspace(picked);
    } catch (e) {
      toast("Folder picker failed", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
    }
  }

  const canAdvance = (() => {
    if (step === 2) {
      if (provider === "azure")
        return (
          azureKey.trim().length > 0 &&
          azureResource.trim().length > 0 &&
          azureDeployment.trim().length > 0
        );
      if (provider === "anthropic") return anthropicKey.trim().length > 0;
      if (provider === "openai") return openaiKey.trim().length > 0;
    }
    return true;
  })();

  async function finish() {
    setSaving(true);
    try {
      // 1. Credential goes to credentials.toml under its canonical env-var
      //    name — same name the engine reads. No more AZURE_OPENAI_KEY vs
      //    AZURE_OPENAI_API_KEY split.
      if (provider === "azure" && azureKey.trim()) {
        await credentialsSet("AZURE_OPENAI_API_KEY", azureKey.trim());
      } else if (provider === "anthropic" && anthropicKey.trim()) {
        await credentialsSet("ANTHROPIC_API_KEY", anthropicKey.trim());
      } else if (provider === "openai" && openaiKey.trim()) {
        await credentialsSet("OPENAI_API_KEY", openaiKey.trim());
      }

      // 2. Provider/model defaults go to the global config.toml.
      const globalPatch =
        provider === "azure"
          ? {
              default_provider: "azure",
              extraction_model: azureDeployment.trim(),
              compilation_model: azureDeployment.trim(),
              azure: {
                resource_name: azureResource.trim() || null,
                deployment: azureDeployment.trim() || null,
                api_version: azureApiVersion.trim() || null,
                api_key_env: "AZURE_OPENAI_API_KEY",
              },
            }
          : provider === "anthropic"
            ? {
                default_provider: "anthropic",
                extraction_model: "claude-sonnet-4-5",
                compilation_model: "claude-sonnet-4-5",
              }
            : {
                default_provider: "openai",
                extraction_model: "gpt-4.1-mini",
                compilation_model: "gpt-4.1-mini",
              };
      await globalConfigWrite(globalPatch);

      // 3. Workspace goes to the registry + active pointer.
      if (workspace) {
        const view = await workspaceAdd({
          path: workspace,
          name: workspaceName.trim() || undefined,
        });
        await workspaceSetActive(view.name);
      }

      toast("ThinkingRoot is ready", {
        kind: "success",
        body: "Restart the app once if the sidecar started before your key was saved.",
        durationMs: 6000,
      });
      onComplete();
    } catch (e) {
      toast("Setup failed", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
    } finally {
      setSaving(false);
    }
  }

  if (!open) return null;
  const current = STEPS[step - 1];
  if (!current) return null;

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label="ThinkingRoot onboarding"
      className="fixed inset-0 z-[60] flex items-center justify-center bg-background"
    >
      <div className="flex h-full w-full max-w-3xl flex-col">
        <Progress step={step} total={STEPS.length} />
        <div className="flex-1 overflow-y-auto px-10 py-10">
          <header className="flex items-start gap-4">
            <div className="flex size-12 shrink-0 items-center justify-center rounded-2xl bg-accent/15 text-accent">
              <current.Icon className="size-6" />
            </div>
            <div className="min-w-0">
              <p className="text-[10px] font-semibold uppercase tracking-[0.25em] text-muted-foreground">
                Step {step} / {STEPS.length}
              </p>
              <h1 className="mt-0.5 text-2xl font-medium tracking-tight">
                {current.title}
              </h1>
              <p className="mt-1 max-w-2xl text-sm leading-relaxed text-muted-foreground">
                {current.description}
              </p>
            </div>
          </header>

          <div className="mt-8">
            <AnimatePresence mode="wait">
              <motion.div
                key={step}
                initial={{ opacity: 0, y: 8 }}
                animate={{ opacity: 1, y: 0 }}
                exit={{ opacity: 0, y: -8 }}
                transition={{ duration: 0.18 }}
              >
                {step === 1 && <WelcomeStep />}
                {step === 2 && (
                  <ProviderStep
                    provider={provider}
                    setProvider={setProvider}
                    azureKey={azureKey}
                    setAzureKey={setAzureKey}
                    azureResource={azureResource}
                    setAzureResource={setAzureResource}
                    azureDeployment={azureDeployment}
                    setAzureDeployment={setAzureDeployment}
                    azureApiVersion={azureApiVersion}
                    setAzureApiVersion={setAzureApiVersion}
                    anthropicKey={anthropicKey}
                    setAnthropicKey={setAnthropicKey}
                    openaiKey={openaiKey}
                    setOpenaiKey={setOpenaiKey}
                  />
                )}
                {step === 3 && (
                  <WorkspaceStep
                    workspace={workspace}
                    setWorkspace={setWorkspace}
                    workspaceName={workspaceName}
                    setWorkspaceName={setWorkspaceName}
                    onPick={pickWorkspace}
                  />
                )}
                {step === 4 && <ChannelsStep />}
                {step === 5 && <CovenantStep />}
              </motion.div>
            </AnimatePresence>
          </div>
        </div>

        <footer className="flex items-center justify-between gap-3 border-t border-border px-10 py-4">
          <div className="flex items-center gap-2">
            <Button
              variant="ghost"
              size="sm"
              onClick={onSkip}
              className="h-8 text-xs text-muted-foreground"
            >
              Skip setup
            </Button>
          </div>
          <div className="flex items-center gap-2">
            <Button
              variant="outline"
              size="sm"
              onClick={() => setStep((s) => Math.max(1, s - 1))}
              disabled={step === 1}
              className="h-8 gap-1 text-xs"
            >
              <ArrowLeft className="size-3" /> Back
            </Button>
            {step < STEPS.length ? (
              <Button
                size="sm"
                onClick={() => setStep((s) => Math.min(STEPS.length, s + 1))}
                disabled={!canAdvance}
                className="h-8 gap-1 text-xs"
              >
                Next <ArrowRight className="size-3" />
              </Button>
            ) : (
              <Button
                size="sm"
                onClick={finish}
                disabled={saving}
                className="h-8 gap-1 text-xs"
              >
                {saving ? "Saving…" : "Finish setup"}
                <Check className="size-3" />
              </Button>
            )}
          </div>
        </footer>
      </div>
    </div>
  );
}

function Progress({ step, total }: { step: number; total: number }) {
  return (
    <div className="flex shrink-0 items-center gap-2 border-b border-border px-10 py-4">
      {Array.from({ length: total }).map((_, i) => {
        const done = i + 1 < step;
        const active = i + 1 === step;
        return (
          <span
            key={i}
            className={cn(
              "h-1 flex-1 rounded-full transition-colors",
              done ? "bg-accent" : active ? "bg-accent/60" : "bg-border",
            )}
          />
        );
      })}
    </div>
  );
}

function WelcomeStep() {
  return (
    <div className="space-y-4 text-sm leading-relaxed text-muted-foreground">
      <p>
        ThinkingRoot is a local-first personal AI. Your memory, your traces,
        your covenant — all on this machine. Nothing uploads unless you
        explicitly wire a cloud channel.
      </p>
      <ul className="grid grid-cols-1 gap-2 md:grid-cols-2">
        {[
          "Compiled knowledge graph with provenance",
          "Signed, replayable session traces",
          "Grace-period undo for every destructive action",
          "Five-commitment Agent Covenant",
        ].map((line) => (
          <li
            key={line}
            className="flex items-start gap-2 rounded-lg border border-border p-3 text-foreground"
          >
            <Check className="mt-0.5 size-3.5 shrink-0 text-success" />
            <span className="text-xs">{line}</span>
          </li>
        ))}
      </ul>
    </div>
  );
}

function ProviderStep(props: {
  provider: ProviderChoice;
  setProvider: (p: ProviderChoice) => void;
  azureKey: string;
  setAzureKey: (s: string) => void;
  azureResource: string;
  setAzureResource: (s: string) => void;
  azureDeployment: string;
  setAzureDeployment: (s: string) => void;
  azureApiVersion: string;
  setAzureApiVersion: (s: string) => void;
  anthropicKey: string;
  setAnthropicKey: (s: string) => void;
  openaiKey: string;
  setOpenaiKey: (s: string) => void;
}) {
  const options: Array<{ id: ProviderChoice; label: string; sub: string }> = [
    {
      id: "azure",
      label: "Azure OpenAI",
      sub: "Enterprise-grade, deployment-scoped keys",
    },
    { id: "anthropic", label: "Anthropic", sub: "Claude family" },
    { id: "openai", label: "OpenAI", sub: "GPT family" },
  ];

  return (
    <div className="space-y-6">
      <div className="grid grid-cols-3 gap-3">
        {options.map((opt) => {
          const active = props.provider === opt.id;
          return (
            <button
              key={opt.id}
              type="button"
              onClick={() => props.setProvider(opt.id)}
              className={cn(
                "flex flex-col items-start gap-1 rounded-lg border p-3 text-left transition-colors",
                active
                  ? "border-accent bg-accent/10"
                  : "border-border hover:border-accent/60 hover:bg-muted/40",
              )}
            >
              <span className="text-xs font-medium text-foreground">{opt.label}</span>
              <span className="text-[10px] text-muted-foreground">{opt.sub}</span>
            </button>
          );
        })}
      </div>

      <div className="space-y-3">
        {props.provider === "azure" && (
          <>
            <Field label="Azure OpenAI API key">
              <input
                type="password"
                value={props.azureKey}
                onChange={(e) => props.setAzureKey(e.target.value)}
                placeholder="paste your Azure subscription key"
                className={inputClass}
                autoComplete="off"
              />
            </Field>
            <Field
              label="Resource name"
              hint="Used to build https://{resource}.openai.azure.com — leave alone if unsure"
            >
              <input
                type="text"
                value={props.azureResource}
                onChange={(e) => props.setAzureResource(e.target.value)}
                placeholder="my-company-openai"
                className={cn(inputClass, "font-mono")}
              />
            </Field>
            <Field label="Deployment">
              <input
                type="text"
                value={props.azureDeployment}
                onChange={(e) => props.setAzureDeployment(e.target.value)}
                placeholder="gpt-4.1-mini"
                className={cn(inputClass, "font-mono")}
              />
            </Field>
            <Field label="API version">
              <input
                type="text"
                value={props.azureApiVersion}
                onChange={(e) => props.setAzureApiVersion(e.target.value)}
                placeholder="2024-12-01-preview"
                className={cn(inputClass, "font-mono")}
              />
            </Field>
          </>
        )}
        {props.provider === "anthropic" && (
          <Field label="Anthropic API key">
            <input
              type="password"
              value={props.anthropicKey}
              onChange={(e) => props.setAnthropicKey(e.target.value)}
              placeholder="sk-ant-…"
              className={inputClass}
              autoComplete="off"
            />
          </Field>
        )}
        {props.provider === "openai" && (
          <Field label="OpenAI API key">
            <input
              type="password"
              value={props.openaiKey}
              onChange={(e) => props.setOpenaiKey(e.target.value)}
              placeholder="sk-…"
              className={inputClass}
              autoComplete="off"
            />
          </Field>
        )}
      </div>

      <p className="text-[11px] text-muted-foreground">
        Keys are written to{" "}
        <code className="rounded bg-muted px-1 font-mono text-[10px]">
          credentials.toml
        </code>{" "}
        with chmod 0600 — same file the CLI's{" "}
        <code className="rounded bg-muted px-1 font-mono text-[10px]">
          root setup
        </code>{" "}
        uses. Provider/model defaults go to{" "}
        <code className="rounded bg-muted px-1 font-mono text-[10px]">
          config.toml
        </code>
        . One source of truth, shared with the CLI.
      </p>
    </div>
  );
}

function WorkspaceStep({
  workspace,
  setWorkspace,
  workspaceName,
  setWorkspaceName,
  onPick,
}: {
  workspace: string;
  setWorkspace: (s: string) => void;
  workspaceName: string;
  setWorkspaceName: (s: string) => void;
  onPick: () => void;
}) {
  return (
    <div className="space-y-4">
      <Field label="Workspace path">
        <div className="flex gap-2">
          <input
            type="text"
            value={workspace}
            onChange={(e) => setWorkspace(e.target.value)}
            placeholder="/Users/you/Desktop/my-project"
            className={cn(inputClass, "font-mono")}
          />
          <Button
            type="button"
            variant="outline"
            size="sm"
            onClick={onPick}
            className="h-9 gap-1 text-xs"
          >
            <FolderOpen className="size-3.5" />
            Pick
          </Button>
        </div>
      </Field>
      <Field label="Workspace name">
        <input
          type="text"
          value={workspaceName}
          onChange={(e) => setWorkspaceName(e.target.value)}
          placeholder="main"
          className={inputClass}
        />
      </Field>
      <div className="rounded-lg border border-dashed border-border/70 p-3 text-[11px] text-muted-foreground">
        You can skip this step and add a workspace later from the sidebar.
        Without one, the chat surface still loads but provenance pills and
        the Brain view stay empty.
      </div>
    </div>
  );
}

function ChannelsStep() {
  return (
    <div className="space-y-3">
      <div className="rounded-lg border border-border p-4">
        <h3 className="text-sm font-medium tracking-tight">Mobile channels</h3>
        <p className="mt-1 text-xs leading-relaxed text-muted-foreground">
          Reach ThinkingRoot from your phone via Telegram, Slack, or Discord —
          same brain, no native mobile app required. Channel wiring lands
          in a follow-on phase.
        </p>
      </div>
      <p className="text-[11px] text-muted-foreground">
        Skip this step for now — you can always add channels from Settings.
      </p>
    </div>
  );
}

function CovenantStep() {
  return (
    <div className="space-y-3">
      <div className="rounded-lg border border-border p-4">
        <h3 className="flex items-center gap-2 text-sm font-medium tracking-tight">
          <ShieldCheck className="size-4 text-accent" />
          The five commitments
        </h3>
        <ul className="mt-2 space-y-1.5 text-xs text-muted-foreground">
          <li>1. Local-first memory</li>
          <li>2. Reversible destructive actions via Action Capsules</li>
          <li>3. Provenance-native answers — no hallucinated citations</li>
          <li>4. Cryptographic audit via Ed25519-signed traces</li>
          <li>5. No impersonation — ThinkingRoot never claims to be you</li>
        </ul>
        <a
          href="https://github.com/DevbyNaveen/ThinkingRoot"
          target="_blank"
          rel="noreferrer"
          className="mt-3 inline-flex items-center gap-1 text-[11px] text-accent hover:underline"
        >
          <ExternalLink className="size-3" /> Read the full covenant on GitHub
        </a>
      </div>
      <p className="text-[11px] text-muted-foreground">
        Covenant signing arrives in D-11 alongside the keypair lifecycle. For
        now ThinkingRoot runs under an implicit acknowledgement.
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
  return (
    <label className="flex flex-col gap-1">
      <span className="text-[10px] font-semibold uppercase tracking-widest text-muted-foreground">
        {label}
      </span>
      {children}
      {hint && (
        <span className="text-[10px] text-muted-foreground">{hint}</span>
      )}
    </label>
  );
}

const inputClass =
  "h-9 w-full rounded-md border border-input bg-background px-3 text-sm text-foreground placeholder:text-muted-foreground focus:border-accent focus:outline-none focus:ring-1 focus:ring-accent/40";
