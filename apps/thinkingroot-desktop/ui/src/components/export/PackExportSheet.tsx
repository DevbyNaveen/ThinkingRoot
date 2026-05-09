import { useEffect, useState } from "react";
import {
  CheckCircle2,
  Download,
  Loader2,
  ShieldCheck,
  ShieldQuestion,
  X,
} from "lucide-react";
import { save } from "@tauri-apps/plugin-dialog";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import {
  packEstimate,
  packExport,
  type PackEstimate,
  type PackExportResult,
} from "@/lib/tauri";
import { toast } from "@/store/toast";

interface Props {
  /** Absolute path to the workspace whose substrate to pack. */
  workspace: string;
  /** Optional non-main branch to pack (T1.4). */
  branch?: string;
  /** Called when the user dismisses or completes the sheet. */
  onClose: () => void;
}

type FormState = {
  name: string;
  version: string;
  license: string;
  description: string;
  signKeyless: boolean;
};

/**
 * Modal sheet that drives `root pack` from the desktop. Mirrors the
 * shape of {@link InstallTrSheet} so the import + export halves of the
 * pack flow share visual language. Three entry points open it: the
 * sidebar workspace context menu, the command palette, and the
 * settings → publish tab.
 *
 * Honesty contract:
 * - `bytes` and `pack_hash` returned by the success state are
 *   recomputed on the desktop side after the subprocess returns; the
 *   sheet never echoes a CLI claim it did not verify.
 * - The estimate is best-effort. When `Pack.toml` is missing the form
 *   asks the user to fill in the required fields explicitly rather
 *   than picking placeholders.
 */
export function PackExportSheet({ workspace, branch, onClose }: Props) {
  const [estimate, setEstimate] = useState<PackEstimate | null>(null);
  const [form, setForm] = useState<FormState>({
    name: "",
    version: "",
    license: "",
    description: "",
    signKeyless: false,
  });
  const [phase, setPhase] = useState<
    | { kind: "form" }
    | { kind: "exporting" }
    | { kind: "done"; result: PackExportResult }
    | { kind: "error"; message: string }
  >({ kind: "form" });

  // Load the lightweight estimate to pre-fill the form.
  useEffect(() => {
    let cancelled = false;
    packEstimate(workspace)
      .then((est) => {
        if (cancelled) return;
        setEstimate(est);
        setForm((prev) => ({
          name: prev.name || est.name,
          version: prev.version || est.version,
          license: prev.license || (est.license ?? ""),
          description: prev.description || (est.description ?? ""),
          signKeyless: prev.signKeyless,
        }));
      })
      .catch((e: unknown) => {
        if (cancelled) return;
        toast("Estimate failed", {
          kind: "error",
          body: e instanceof Error ? e.message : String(e),
        });
      });
    return () => {
      cancelled = true;
    };
  }, [workspace]);

  const compiled = estimate?.compiled ?? false;
  const submittable =
    compiled &&
    form.name.trim().length > 0 &&
    form.version.trim().length > 0 &&
    /^[A-Za-z0-9_-]+\/[A-Za-z0-9_.-]+$/.test(form.name.trim()) &&
    phase.kind === "form";

  const onSubmit = async () => {
    if (!submittable) return;
    const defaultFile = `${form.name.replace("/", "-")}-${form.version}.tr`;
    let outPath: string | null;
    try {
      outPath = await save({
        title: "Export ThinkingRoot pack",
        defaultPath: defaultFile,
        filters: [{ name: "ThinkingRoot pack", extensions: ["tr"] }],
      });
    } catch (e) {
      toast("Save dialog failed", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
      return;
    }
    if (!outPath) return; // user cancelled

    setPhase({ kind: "exporting" });
    try {
      const result = await packExport({
        workspace,
        out_path: outPath,
        name: form.name.trim(),
        version: form.version.trim(),
        license: form.license.trim() || null,
        description: form.description.trim() || null,
        sign_keyless: form.signKeyless,
        branch: branch ?? null,
      });
      setPhase({ kind: "done", result });
      toast("Pack written", {
        kind: "success",
        body: `${result.bytes.toLocaleString()} bytes — ${result.trust_tier}`,
      });
    } catch (e) {
      const message = e instanceof Error ? e.message : String(e);
      setPhase({ kind: "error", message });
      toast("Export failed", { kind: "error", body: message });
    }
  };

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label="Export ThinkingRoot pack"
      className="fixed inset-0 z-[55] flex items-center justify-center bg-background/70 backdrop-blur-sm"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="flex max-h-[90vh] w-full max-w-2xl flex-col overflow-hidden rounded-xl border border-border bg-surface-elevated shadow-elevated">
        <header className="flex items-center justify-between gap-3 border-b border-border px-5 py-3">
          <div className="flex min-w-0 items-center gap-2">
            <Download className="size-4 text-accent" />
            <h2 className="truncate text-sm font-medium tracking-tight">
              Export pack
            </h2>
            <code
              className="truncate font-mono text-[10px] text-muted-foreground"
              title={workspace}
            >
              {workspace}
            </code>
          </div>
          <Button
            variant="ghost"
            size="icon"
            onClick={onClose}
            aria-label="Close export sheet"
            className="h-7 w-7"
          >
            <X className="size-3.5" />
          </Button>
        </header>

        <div className="flex-1 overflow-y-auto px-5 py-4 text-sm">
          {!compiled && (
            <div className="mb-4 rounded-md border border-warn bg-warn/10 px-3 py-2 text-xs text-warn-foreground">
              <strong>Workspace not compiled.</strong> Run <code>Compile</code>{" "}
              from the brain surface (or <code>root compile</code>) before
              exporting.
            </div>
          )}

          <div className="grid grid-cols-1 gap-3">
            <Field
              label="Pack name (owner/slug)"
              value={form.name}
              onChange={(v) => setForm((p) => ({ ...p, name: v }))}
              placeholder="acme/widgets"
              hint="Lowercase letters, digits, '-' and '_'. Required."
            />
            <Field
              label="Version (SemVer)"
              value={form.version}
              onChange={(v) => setForm((p) => ({ ...p, version: v }))}
              placeholder="0.2.1"
              hint="Required. Bumped per published release."
            />
            <Field
              label="License (SPDX)"
              value={form.license}
              onChange={(v) => setForm((p) => ({ ...p, license: v }))}
              placeholder="MIT, Apache-2.0, …"
              hint="Optional. Recommended for distributable packs."
            />
            <Field
              label="Description"
              value={form.description}
              onChange={(v) => setForm((p) => ({ ...p, description: v }))}
              placeholder="One-line description"
              hint="Optional. Short summary visible to consumers."
            />
            <label className="mt-1 flex items-center gap-2 text-xs">
              <input
                type="checkbox"
                checked={form.signKeyless}
                onChange={(e) =>
                  setForm((p) => ({ ...p, signKeyless: e.target.checked }))
                }
                disabled={phase.kind !== "form"}
              />
              Sign with Sigstore keyless DSSE (browser OIDC unless{" "}
              <code>$TR_OIDC_TOKEN</code> is set)
            </label>
          </div>

          {estimate && (
            <div className="mt-4 rounded-md border border-border bg-surface px-3 py-2 text-[11px] text-muted-foreground">
              <div>
                Source store: {estimate.source_files.toLocaleString()} files,{" "}
                {humanBytes(estimate.source_bytes)}
              </div>
              {branch && <div>Packing branch: {branch}</div>}
            </div>
          )}

          {phase.kind === "exporting" && (
            <div className="mt-4 flex items-center gap-2 text-xs">
              <Loader2 className="size-3.5 animate-spin" /> Running root pack…
            </div>
          )}

          {phase.kind === "done" && <DoneCard result={phase.result} />}

          {phase.kind === "error" && (
            <pre className="mt-4 max-h-40 overflow-auto rounded border border-error bg-error/5 px-3 py-2 font-mono text-[11px] text-error-foreground">
              {phase.message}
            </pre>
          )}
        </div>

        <footer className="flex items-center justify-end gap-2 border-t border-border px-5 py-3">
          <Button variant="ghost" onClick={onClose}>
            {phase.kind === "done" ? "Close" : "Cancel"}
          </Button>
          {phase.kind !== "done" && (
            <Button onClick={onSubmit} disabled={!submittable}>
              {phase.kind === "exporting" ? (
                <>
                  <Loader2 className="mr-1.5 size-3.5 animate-spin" /> Working…
                </>
              ) : (
                "Export"
              )}
            </Button>
          )}
        </footer>
      </div>
    </div>
  );
}

function Field(props: {
  label: string;
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  hint?: string;
}) {
  return (
    <label className="flex flex-col gap-1 text-xs">
      <span className="text-muted-foreground">{props.label}</span>
      <input
        className="rounded-md border border-border bg-surface px-2 py-1 text-sm focus:border-accent focus:outline-none"
        value={props.value}
        placeholder={props.placeholder}
        onChange={(e) => props.onChange(e.target.value)}
      />
      {props.hint && (
        <span className="text-[10px] text-muted-foreground/70">
          {props.hint}
        </span>
      )}
    </label>
  );
}

function DoneCard({ result }: { result: PackExportResult }) {
  const trusted = result.trust_tier === "T1";
  return (
    <div className="mt-4 rounded-md border border-border bg-surface px-3 py-3 text-xs">
      <div className="mb-2 flex items-center gap-2 font-medium">
        <CheckCircle2 className="size-3.5 text-success" />
        <span>Pack written</span>
        <span
          className={cn(
            "ml-auto inline-flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px]",
            trusted ? "bg-success/15 text-success" : "bg-muted text-muted-foreground"
          )}
          title={trusted ? "Sigstore-keyless signed" : "Unsigned (T0)"}
        >
          {trusted ? (
            <ShieldCheck className="size-3" />
          ) : (
            <ShieldQuestion className="size-3" />
          )}
          {result.trust_tier}
        </span>
      </div>
      <div className="font-mono text-[11px] text-muted-foreground">
        {result.out_path}
      </div>
      <div className="mt-1 text-[11px]">
        {humanBytes(result.bytes)} ·{" "}
        <span className="font-mono">
          {result.pack_hash ? result.pack_hash.slice(0, 24) + "…" : "(hash unavailable)"}
        </span>
      </div>
      {result.warnings.length > 0 && (
        <ul className="mt-2 list-disc pl-4 text-[11px] text-warn-foreground">
          {result.warnings.map((w, i) => (
            <li key={i}>{w}</li>
          ))}
        </ul>
      )}
    </div>
  );
}

function humanBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ["KiB", "MiB", "GiB", "TiB"];
  let v = n;
  let i = -1;
  do {
    v /= 1024;
    i++;
  } while (v >= 1024 && i < units.length - 1);
  return `${v.toFixed(2)} ${units[i]}`;
}
