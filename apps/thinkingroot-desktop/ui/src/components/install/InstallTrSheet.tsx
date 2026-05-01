import { useEffect, useState } from "react";
import {
  AlertTriangle,
  FileWarning,
  Loader2,
  ShieldCheck,
  ShieldQuestion,
  X,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { installTrFile, type InstallPreview, type Verdict } from "@/lib/tauri";
import { toast } from "@/store/toast";

interface Props {
  /** Absolute path to the `.tr` file the OS / drag-drop handed us. */
  path: string | null;
  /** Called when the user dismisses the sheet (Cancel, Confirm, or X). */
  onClose: () => void;
}

/**
 * Modal sheet that opens whenever a `.tr` file is double-clicked or
 * dragged onto the window. Loads an `InstallPreview` from the Rust
 * side and renders the manifest essentials, capabilities, archive
 * stats, and trust verdict in a single scrollable view.
 *
 * Confirm is a deliberate no-op for v0.1 — the actual install path
 * lives in the OSS `root install` CLI; integrating that flow with
 * extraction progress + capsule undo is the next phase. The sheet's
 * job today is to decode the pack honestly and let the user decide
 * whether to proceed.
 */
export function InstallTrSheet({ path, onClose }: Props) {
  const [state, setState] = useState<
    | { kind: "loading" }
    | { kind: "ready"; preview: InstallPreview }
    | { kind: "error"; message: string }
  >({ kind: "loading" });

  useEffect(() => {
    if (!path) return;
    let cancelled = false;
    setState({ kind: "loading" });
    installTrFile(path)
      .then((preview) => {
        if (!cancelled) setState({ kind: "ready", preview });
      })
      .catch((err: unknown) => {
        if (cancelled) return;
        setState({
          kind: "error",
          message: err instanceof Error ? err.message : String(err),
        });
      });
    return () => {
      cancelled = true;
    };
  }, [path]);

  if (!path) return null;

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label="Install ThinkingRoot capsule"
      className="fixed inset-0 z-[55] flex items-center justify-center bg-background/70 backdrop-blur-sm"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="flex max-h-[90vh] w-full max-w-2xl flex-col overflow-hidden rounded-xl border border-border bg-surface-elevated shadow-elevated">
        <header className="flex items-center justify-between gap-3 border-b border-border px-5 py-3">
          <div className="flex min-w-0 items-center gap-2">
            <FileWarning className="size-4 text-accent" />
            <h2 className="truncate text-sm font-medium tracking-tight">
              Install capsule
            </h2>
            <code
              className="truncate font-mono text-[10px] text-muted-foreground"
              title={path}
            >
              {path}
            </code>
          </div>
          <Button
            variant="ghost"
            size="icon"
            onClick={onClose}
            aria-label="Close install sheet"
            className="h-7 w-7"
          >
            <X className="size-3.5" />
          </Button>
        </header>

        <div className="flex-1 overflow-y-auto px-5 py-4">
          {state.kind === "loading" && <Loading />}
          {state.kind === "error" && (
            <ErrorCard message={state.message} />
          )}
          {state.kind === "ready" && (
            <PreviewBody preview={state.preview} />
          )}
        </div>

        <footer className="flex items-center justify-between gap-2 border-t border-border px-5 py-3">
          <p className="text-[11px] text-muted-foreground">
            Confirming runs the OSS `root install` flow against the configured
            workspace.
          </p>
          <div className="flex items-center gap-2">
            <Button variant="outline" size="sm" onClick={onClose} className="h-8 text-xs">
              Cancel
            </Button>
            <Button
              size="sm"
              onClick={() => {
                if (state.kind === "ready") {
                  toast("Install integration ships in the next phase", {
                    kind: "info",
                    body: "Today the sheet is preview-only — run `root install` from the CLI to extract.",
                    durationMs: 5000,
                  });
                }
                onClose();
              }}
              disabled={state.kind !== "ready"}
              className="h-8 text-xs"
            >
              Confirm install
            </Button>
          </div>
        </footer>
      </div>
    </div>
  );
}

function Loading() {
  return (
    <div className="flex h-40 items-center justify-center gap-2 text-sm text-muted-foreground">
      <Loader2 className="size-4 animate-spin" />
      <span>Reading capsule…</span>
    </div>
  );
}

function ErrorCard({ message }: { message: string }) {
  return (
    <div className="flex items-start gap-2 rounded-md border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">
      <AlertTriangle className="mt-0.5 size-4 shrink-0" />
      <div className="min-w-0">
        <p className="font-medium">Could not read capsule</p>
        <p className="mt-1 break-words font-mono text-[11px]">{message}</p>
      </div>
    </div>
  );
}

function PreviewBody({ preview }: { preview: InstallPreview }) {
  return (
    <div className="flex flex-col gap-4">
      <header className="flex flex-col gap-1">
        <h3 className="text-base font-medium tracking-tight">
          {preview.name} <span className="text-muted-foreground">{preview.version}</span>
        </h3>
        <p className="text-[11px] text-muted-foreground">
          {preview.license ? `License ${preview.license} · ` : ""}
          {preview.claim_count} claims · {preview.source_count} sources · {formatBytes(preview.source_archive_bytes)}
        </p>
      </header>

      <VerdictBadge verdict={preview.verdict} />

      <section>
        <h4 className="mb-1 text-[10px] font-semibold uppercase tracking-widest text-muted-foreground">
          Manifest
        </h4>
        <pre className="overflow-x-auto rounded-md border border-border bg-background p-3 font-mono text-[11px] leading-relaxed text-foreground">
          {preview.manifest_table}
        </pre>
      </section>

      <section>
        <h4 className="mb-1 text-[10px] font-semibold uppercase tracking-widest text-muted-foreground">
          Summary
        </h4>
        <div className="rounded-md border border-border bg-background p-3 text-xs leading-relaxed text-foreground">
          <pre className="whitespace-pre-wrap font-sans">{preview.markdown}</pre>
        </div>
      </section>
    </div>
  );
}

function VerdictBadge({ verdict }: { verdict: Verdict }) {
  const tone = verdictTone(verdict);
  const Icon = tone.Icon;
  return (
    <div
      className={cn(
        "flex items-start gap-2 rounded-md border p-3",
        tone.classes,
      )}
    >
      <Icon className="mt-0.5 size-4 shrink-0" />
      <div className="min-w-0">
        <p className="text-xs font-medium">{tone.title}</p>
        <p className="mt-0.5 text-[11px] leading-relaxed text-muted-foreground">
          {tone.body}
        </p>
      </div>
    </div>
  );
}

function verdictTone(verdict: Verdict): {
  title: string;
  body: string;
  Icon: typeof ShieldCheck;
  classes: string;
} {
  switch (verdict.kind) {
    case "verified":
      return {
        title: `Verified`,
        body: verdict.identity
          ? `Signed by identity ${verdict.identity}.`
          : "Manifest hash + revocation cache passed.",
        Icon: ShieldCheck,
        classes: "border-success/40 bg-success/10 text-success",
      };
    case "unsigned":
      return {
        title: "Unsigned",
        body: "No signature attached. Local installs accept this; remote installs require --allow-unsigned.",
        Icon: ShieldQuestion,
        classes: "border-warn/40 bg-warn/10 text-warn",
      };
    case "tampered":
      return {
        title: "Tampered",
        body:
          verdict.what === "pack_hash_mismatch"
            ? `Pack hash mismatch (expected ${verdict.declared}, computed ${verdict.recomputed}).`
            : `Signature check failed: ${verdict.reason}`,
        Icon: AlertTriangle,
        classes: "border-destructive/40 bg-destructive/10 text-destructive",
      };
    case "revoked":
      return {
        title: "Revoked",
        body: verdict.advisory.reason ?? "Pack appears on the registry deny-list.",
        Icon: AlertTriangle,
        classes: "border-destructive/40 bg-destructive/10 text-destructive",
      };
  }
}

function formatBytes(n: number): string {
  const KIB = 1024;
  const MIB = 1024 * KIB;
  const GIB = 1024 * MIB;
  if (n >= GIB) return `${(n / GIB).toFixed(2)} GiB`;
  if (n >= MIB) return `${(n / MIB).toFixed(2)} MiB`;
  if (n >= KIB) return `${(n / KIB).toFixed(2)} KiB`;
  return `${n} B`;
}
