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
import {
  exportButtonEnabled,
  pickPrimaryDiagnostic,
  useDiagnosticsFor,
  useWorkspaceStatus,
  useWorkspaceStatusSubscription,
} from "@/store/workspace-status";

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

  // Load the lightweight estimate to pre-fill the form. Bugfix
  // 2026-05-10 — the form now starts populated with sensible defaults
  // derived from the workspace name (`local/<slug>` + `0.1.0`) so the
  // user never lands on an empty form they have to figure out. They
  // can still overwrite anything, and the on-submit sanitiser fixes
  // partial input rather than blocking it.
  useEffect(() => {
    let cancelled = false;
    packEstimate(workspace)
      .then((est) => {
        if (cancelled) return;
        setEstimate(est);
        setForm((prev) => ({
          name: prev.name || est.name || defaultPackName(workspace),
          version: prev.version || est.version || "0.1.0",
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

  // Slice 0 — unified workspace status. Pre-Slice-0 the export sheet
  // read its own `pack_estimate` Tauri command and decided "compiled
  // = bool" from a substrate-only check, which conflicted with the
  // right-rail badge and the chat banner. Now we read the same
  // snapshot every other view reads, including the diagnostic that
  // explains *why* export is blocked (no claims, no sources, mid-
  // compile, …).
  useWorkspaceStatusSubscription(workspace);
  const status = useWorkspaceStatus(workspace);
  const exportDiagnostics = useDiagnosticsFor(workspace, "for_export");
  const blocker = pickPrimaryDiagnostic(status, "for_export");

  // Bugfix 2026-05-10 — the form auto-sanitises name + version on
  // submit. Pre-fix any non-conforming input (e.g. `Cipher` instead
  // of `acme/cipher`, or `1.01` instead of `1.0.1`) just disabled the
  // button silently. Now we always show what the export will use
  // (computed below) and the only block is the workspace-readiness
  // axis. The CLI contract (`owner/slug` + SemVer) is preserved by
  // the sanitiser, never by gating the button.
  const sanitizedName = sanitizePackName(form.name, workspace);
  const sanitizedVersion = sanitizeSemver(form.version);
  const willRewriteName = sanitizedName !== form.name.trim();
  const willRewriteVersion = sanitizedVersion !== form.version.trim();

  const workspaceBlocker =
    !exportButtonEnabled(status)
      ? blocker?.message ?? "Workspace not ready to export."
      : null;

  const submittable = workspaceBlocker === null && phase.kind === "form";

  const onSubmit = async () => {
    if (!submittable) return;
    const defaultFile = `${sanitizedName.replace("/", "-")}-${sanitizedVersion}.tr`;
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
        name: sanitizedName,
        version: sanitizedVersion,
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
          {exportDiagnostics.length > 0 && (
            <div className="mb-4 flex flex-col gap-1 rounded-md border border-warn bg-warn/10 px-3 py-2 text-xs text-warn-foreground">
              {exportDiagnostics.map((d) => (
                <div key={d.code} className="flex flex-col gap-0.5">
                  <span>
                    <strong>
                      {d.severity === "error" ? "Cannot export." : "Heads up."}
                    </strong>{" "}
                    {d.message}
                  </span>
                  {d.actions.length > 0 && (
                    <span className="text-[10px] opacity-80">
                      Suggested: {d.actions.map((a) => a.label).join(" · ")}
                    </span>
                  )}
                </div>
              ))}
            </div>
          )}
          {blocker === null &&
            exportButtonEnabled(status) === false &&
            status === null && (
              <div className="mb-4 rounded-md border border-border bg-muted/40 px-3 py-2 text-xs text-muted-foreground">
                Loading workspace status…
              </div>
            )}

          <div className="grid grid-cols-1 gap-3">
            <Field
              label="Pack name (owner/slug)"
              value={form.name}
              onChange={(v) => setForm((p) => ({ ...p, name: v }))}
              placeholder="acme/widgets"
              hint="Format: `owner/slug` (e.g. `acme/cipher`). Each part accepts letters, digits, '-' and '_'."
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

          {(willRewriteName || willRewriteVersion) && phase.kind === "form" && (
            <div className="mt-3 rounded-md border border-border bg-surface px-3 py-2 text-[11px] text-muted-foreground">
              Will export as{" "}
              <code className="font-mono text-foreground/90">
                {sanitizedName}@{sanitizedVersion}
              </code>{" "}
              (auto-fixed to match the `owner/slug` + SemVer contract).
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
        className="rounded-md border border-border bg-surface px-2 py-1 text-sm focus:outline-none"
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

/**
 * Compute a sane default `owner/slug` from the workspace name. Used
 * to pre-fill the pack name field so the user lands on a working
 * default rather than an empty field. Owner defaults to `local`
 * because nothing on the desktop has been associated with a remote
 * registry account yet — the user can overwrite it freely.
 */
function defaultPackName(workspace: string): string {
  const slug = sanitizeSlug(workspace) || "pack";
  return `local/${slug}`;
}

/**
 * Lowercase, replace runs of invalid chars with `-`, trim leading/
 * trailing `-`. Mirrors what `tr-format`'s manifest validator
 * accepts. Empty input yields empty output (the caller decides what
 * to default to).
 */
function sanitizeSlug(s: string): string {
  return s
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9_-]+/g, "-")
    .replace(/^-+|-+$/g, "");
}

/**
 * Normalise whatever the user typed into a valid `owner/slug`.
 * - Has a slash: sanitise each half.
 * - No slash: treat the whole string as the slug, prefix with the
 *   workspace-derived default owner.
 * - Empty: fall back to the workspace default.
 */
function sanitizePackName(input: string, workspace: string): string {
  const raw = input.trim();
  if (raw.length === 0) return defaultPackName(workspace);
  if (raw.includes("/")) {
    const [ownerRaw, ...rest] = raw.split("/");
    const slugRaw = rest.join("-"); // collapse multi-slash typos
    const owner = sanitizeSlug(ownerRaw ?? "") || "local";
    const slug = sanitizeSlug(slugRaw) || sanitizeSlug(workspace) || "pack";
    return `${owner}/${slug}`;
  }
  const slug = sanitizeSlug(raw) || sanitizeSlug(workspace) || "pack";
  return `local/${slug}`;
}

/**
 * Normalise user-typed version strings into valid SemVer.
 * - `1.0.1` → `1.0.1` (passthrough)
 * - `1.01` → `1.0.1` (pads short forms to 3 components)
 * - `1` → `1.0.0`
 * - `v1.2.3` → `1.2.3` (drops common prefix)
 * - garbage → `0.1.0`
 */
function sanitizeSemver(input: string): string {
  const raw = input.trim().replace(/^v/, "");
  if (raw.length === 0) return "0.1.0";
  // Already valid SemVer?
  if (/^\d+\.\d+\.\d+(?:[-+][A-Za-z0-9.-]+)?$/.test(raw)) return raw;
  // Pull leading numeric components.
  const match = raw.match(/^(\d+)(?:\.(\d+))?(?:\.(\d+))?/);
  if (!match) return "0.1.0";
  const major = match[1] ?? "0";
  const minor = match[2] ?? "0";
  const patch = match[3] ?? "0";
  return `${major}.${minor}.${patch}`;
}
