import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type CSSProperties,
} from "react";
import { createPortal } from "react-dom";
import { ChevronDown, Loader2 } from "lucide-react";

import { cn } from "@/lib/utils";
import { toast } from "@/store/toast";
import {
  authState,
  globalConfigRead,
  providerFetchModelsStored,
  providerSetActiveModel,
  type AuthState,
  type LlmHealth,
} from "@/lib/tauri";

function formatProviderLabel(provider: string | null | undefined): string {
  if (!provider) return "Provider";
  const k = provider.toLowerCase();
  const map: Record<string, string> = {
    anthropic: "Anthropic",
    openai: "OpenAI",
    azure: "Azure",
    google: "Google",
    gemini: "Google",
    groq: "Groq",
    ollama: "Ollama",
    mistral: "Mistral",
    deepseek: "DeepSeek",
    openrouter: "OpenRouter",
    together: "Together",
    perplexity: "Perplexity",
    "thinkingroot-cloud": "ThinkingRoot Cloud",
  };
  return map[k] ?? provider.charAt(0).toUpperCase() + provider.slice(1);
}

function modelShortName(model: string): string {
  const tail = model.split("/").pop() ?? model;
  return tail.length > 22 ? `${tail.slice(0, 20)}…` : tail;
}

function compactCount(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return n.toString();
}

function usageCaption(auth: AuthState | null, providerId: string | null): string {
  if (auth?.signed_in && providerId === "thinkingroot-cloud") {
    if (auth.credits_remaining != null && auth.credits_total != null) {
      return `${compactCount(auth.credits_remaining)} / ${compactCount(auth.credits_total)} credits`;
    }
    return "Cloud account";
  }
  if (providerId && providerId !== "thinkingroot-cloud") {
    return `BYOK · ${formatProviderLabel(providerId)}`;
  }
  return "";
}

type MenuPlacement = "below" | "above";

function useAnchoredMenu(
  open: boolean,
  triggerRef: React.RefObject<HTMLButtonElement | null>,
  menuRef: React.RefObject<HTMLDivElement | null>,
  preferred: MenuPlacement,
  deps: unknown[],
) {
  const [style, setStyle] = useState<CSSProperties>({ visibility: "hidden" });

  useLayoutEffect(() => {
    if (!open) return;

    const update = () => {
      const trigger = triggerRef.current;
      if (!trigger) return;

      const rect = trigger.getBoundingClientRect();
      const menuWidth = menuRef.current?.offsetWidth ?? 256;
      const menuHeight = menuRef.current?.offsetHeight ?? 160;
      const gap = 6;
      const pad = 8;

      const spaceBelow = window.innerHeight - rect.bottom - pad;
      const spaceAbove = rect.top - pad;

      let placement: MenuPlacement = preferred;
      if (preferred === "below" && spaceBelow < menuHeight && spaceAbove > spaceBelow) {
        placement = "above";
      } else if (preferred === "above" && spaceAbove < menuHeight && spaceBelow > spaceAbove) {
        placement = "below";
      }

      let left = rect.left;
      left = Math.max(pad, Math.min(left, window.innerWidth - menuWidth - pad));

      const next: CSSProperties = {
        position: "fixed",
        left,
        zIndex: 9999,
        width: "max-content",
        minWidth: "14rem",
        maxWidth: `min(22rem, calc(100vw - ${pad * 2}px))`,
        visibility: "visible",
      };

      if (placement === "below") {
        next.top = rect.bottom + gap;
        next.maxHeight = Math.max(120, spaceBelow - gap);
      } else {
        next.bottom = window.innerHeight - rect.top + gap;
        next.maxHeight = Math.max(120, spaceAbove - gap);
      }

      setStyle(next);
    };

    update();
    const raf = requestAnimationFrame(update);
    window.addEventListener("resize", update);
    window.addEventListener("scroll", update, true);
    return () => {
      cancelAnimationFrame(raf);
      window.removeEventListener("resize", update);
      window.removeEventListener("scroll", update, true);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps -- reposition when menu content changes
  }, [open, preferred, ...deps]);

  return style;
}

export function ComposerModelPicker({
  health,
  variant = "session",
  onModelChanged,
}: {
  health: LlmHealth | null;
  variant?: "idle" | "session";
  onModelChanged?: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [models, setModels] = useState<string[]>([]);
  const [loading, setLoading] = useState(false);
  const [saving, setSaving] = useState(false);
  const [providerId, setProviderId] = useState<string | null>(null);
  const [activeModel, setActiveModel] = useState<string | null>(null);
  const [auth, setAuth] = useState<AuthState | null>(null);
  const rootRef = useRef<HTMLDivElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);

  const preferredPlacement: MenuPlacement = "above";
  const menuStyle = useAnchoredMenu(open, triggerRef, menuRef, preferredPlacement, [
    models.length,
    loading,
    providerId,
  ]);

  const refreshCatalog = useCallback(async () => {
    setLoading(true);
    try {
      const [cfg, authSnap] = await Promise.all([
        globalConfigRead(),
        authState().catch(() => null),
      ]);
      setAuth(authSnap);
      const pid =
        cfg.default_provider ||
        health?.provider ||
        (authSnap?.signed_in ? "thinkingroot-cloud" : null);
      if (!pid) {
        setProviderId(null);
        setModels([]);
        setActiveModel(health?.model ?? cfg.extraction_model ?? null);
        return;
      }
      setProviderId(pid);
      setActiveModel(health?.model ?? cfg.extraction_model ?? null);
      const list = await providerFetchModelsStored(pid);
      setModels(list);
    } catch (e) {
      setModels([]);
      toast("Could not load models", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
    } finally {
      setLoading(false);
    }
  }, [health?.model, health?.provider]);

  useEffect(() => {
    void refreshCatalog();
  }, [refreshCatalog]);

  useEffect(() => {
    if (!open) return;
    const onDoc = (e: MouseEvent) => {
      const target = e.target as Node;
      if (rootRef.current?.contains(target)) return;
      if (menuRef.current?.contains(target)) return;
      setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("mousedown", onDoc, true);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDoc, true);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  const pickModel = async (model: string) => {
    if (saving || model === activeModel) {
      setOpen(false);
      return;
    }
    setSaving(true);
    try {
      await providerSetActiveModel(model);
      setActiveModel(model);
      setOpen(false);
      onModelChanged?.();
    } catch (e) {
      toast("Model switch failed", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
    } finally {
      setSaving(false);
    }
  };

  const triggerLabel =
    activeModel != null && activeModel.length > 0
      ? modelShortName(activeModel)
      : health?.configured
        ? formatProviderLabel(health.provider)
        : "Auto";

  const usage = usageCaption(auth, providerId);

  const menu =
    open &&
    createPortal(
      <div
        ref={menuRef}
        role="menu"
        aria-label="Model picker"
        style={menuStyle}
        className="flex flex-col overflow-hidden rounded-lg border border-border/80 bg-surface py-1 shadow-lg"
      >
        <div className="shrink-0 border-b border-border/50 px-3 py-2">
          <p className="text-[11px] font-medium text-foreground">
            {formatProviderLabel(providerId)}
          </p>
          {usage && (
            <p className="mt-0.5 text-[10px] text-muted-foreground">{usage}</p>
          )}
        </div>
        <div className="min-h-0 flex-1 overflow-y-auto py-0.5">
          {loading && models.length === 0 && (
            <div className="flex items-center gap-2 px-3 py-2 text-[11px] text-muted-foreground">
              <Loader2 className="size-3 animate-spin" />
              Loading models…
            </div>
          )}
          {!loading && models.length === 0 && (
            <p className="px-3 py-2 text-[11px] text-muted-foreground">
              {providerId === "azure"
                ? "No deployments — set Azure deployment in Settings."
                : "No models — check provider keys in Settings."}
            </p>
          )}
          {models.map((m) => {
            const selected = m === activeModel;
            return (
              <button
                key={m}
                type="button"
                role="menuitem"
                onClick={() => void pickModel(m)}
                className={cn(
                  "flex w-full px-3 py-1.5 text-left text-[11px]",
                  selected
                    ? "bg-accent/10 font-medium text-foreground"
                    : "text-foreground/90 hover:bg-muted/60",
                )}
              >
                <span className="truncate">{m}</span>
              </button>
            );
          })}
        </div>
      </div>,
      document.body,
    );

  return (
    <div ref={rootRef} className="relative min-w-0 shrink-0">
      <button
        ref={triggerRef}
        type="button"
        onClick={() => {
          setOpen((o) => !o);
          if (!open && models.length === 0) void refreshCatalog();
        }}
        disabled={loading || saving}
        title={
          activeModel
            ? `${formatProviderLabel(providerId)} · ${activeModel}`
            : "Choose model"
        }
        className={cn(
          "inline-flex max-w-full items-center gap-0.5 rounded-md transition-colors",
          "text-muted-foreground/75 hover:text-foreground/90",
          "focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring/45",
          variant === "session"
            ? "h-8 max-w-[9rem] shrink-0 px-1 text-[13px]"
            : "h-7 max-w-[14rem] shrink-0 px-1 text-[10.5px]",
        )}
      >
        {loading || saving ? (
          <Loader2 className="size-3 shrink-0 animate-spin" />
        ) : null}
        <span className="truncate">{triggerLabel}</span>
        <ChevronDown className="size-3 shrink-0 opacity-45" strokeWidth={2} aria-hidden />
      </button>
      {menu}
    </div>
  );
}
