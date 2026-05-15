import { useEffect, useState } from "react";
import { ExternalLink, Loader2, X } from "lucide-react";
import { openUrl } from "@tauri-apps/plugin-opener";

import { playgroundPreview, type PlaygroundPreview } from "@/lib/tauri";

interface Props {
  workspace: string;
  relPath: string;
  onClose: () => void;
}

/** Right-side preview pane for the FileManager.
 *
 * Dispatches on the server-reported `kind`:
 *   - text / markdown / code → renders the UTF-8 content (up to 1 MiB)
 *   - image                  → renders the base64 data URL (up to 5 MiB)
 *   - binary                 → metadata + "Open externally" button
 *
 * "Open externally" uses the Tauri opener plugin to hand the file to
 * the OS default app (Preview / Quicktime / VLC / Acrobat / …). This
 * is the honest fallback for media kinds we don't inline, and the
 * graceful escape hatch for files that exceed the inline budget.
 */
export function FilePreviewPanel({ workspace, relPath, onClose }: Props) {
  const [preview, setPreview] = useState<PlaygroundPreview | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setError(null);
    setPreview(null);
    playgroundPreview(workspace, relPath)
      .then((p) => {
        if (!cancelled) setPreview(p);
      })
      .catch((e) => {
        if (!cancelled) {
          setError(typeof e === "string" ? e : (e as Error).message ?? "preview failed");
        }
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [workspace, relPath]);

  return (
    <aside className="flex w-96 shrink-0 flex-col border-l border-border bg-surface/30">
      <header className="flex shrink-0 items-center justify-between border-b border-border px-3 py-2 text-xs">
        <span className="truncate font-medium text-foreground" title={relPath}>
          {relPath.split("/").pop() ?? relPath}
        </span>
        <button
          type="button"
          onClick={onClose}
          className="rounded p-1 text-muted-foreground hover:bg-muted/40 hover:text-foreground"
          aria-label="Close preview"
        >
          <X className="size-3.5" />
        </button>
      </header>
      <div className="flex min-h-0 flex-1 flex-col overflow-hidden">
        {loading ? (
          <div className="flex flex-1 items-center justify-center text-xs text-muted-foreground">
            <Loader2 className="mr-2 size-3.5 animate-spin" />
            Loading preview…
          </div>
        ) : error ? (
          <div className="m-3 rounded border border-rose-500/30 bg-rose-500/10 p-3 text-xs text-rose-300">
            {error}
          </div>
        ) : preview ? (
          <PreviewBody preview={preview} />
        ) : null}
      </div>
      {preview && (
        <footer className="flex shrink-0 items-center justify-between border-t border-border px-3 py-1.5 text-[10px] text-muted-foreground">
          <span>{formatBytes(preview.size_bytes)}</span>
          <button
            type="button"
            onClick={() => void openUrl(`file://${preview.absolute_path}`)}
            className="flex items-center gap-1 rounded px-1.5 py-0.5 hover:bg-muted/40 hover:text-foreground"
            title="Open in default app"
          >
            <ExternalLink className="size-3" />
            Open externally
          </button>
        </footer>
      )}
    </aside>
  );
}

function PreviewBody({ preview }: { preview: PlaygroundPreview }) {
  if (preview.too_large) {
    return (
      <div className="flex flex-1 flex-col items-center justify-center gap-2 px-4 text-center text-xs text-muted-foreground">
        <p>This file is too large to preview inline ({formatBytes(preview.size_bytes)}).</p>
        <p className="opacity-70">Open it externally to view.</p>
      </div>
    );
  }
  switch (preview.kind) {
    case "image":
      return preview.data_url ? (
        <div className="flex flex-1 items-center justify-center overflow-auto bg-black/20 p-3">
          <img
            src={preview.data_url}
            alt={preview.rel_path}
            className="max-h-full max-w-full object-contain"
          />
        </div>
      ) : null;
    case "text":
    case "markdown":
    case "code":
      return (
        <pre className="flex-1 overflow-auto whitespace-pre-wrap break-words p-3 text-[11px] leading-relaxed text-foreground">
          {preview.text ?? ""}
        </pre>
      );
    default:
      return (
        <div className="flex flex-1 flex-col items-center justify-center gap-2 px-4 text-center text-xs text-muted-foreground">
          <p>No inline preview for this file kind.</p>
          <p className="opacity-70">Use "Open externally" below to view it.</p>
        </div>
      );
  }
}

function formatBytes(b: number): string {
  if (b < 1024) return `${b} B`;
  if (b < 1024 * 1024) return `${(b / 1024).toFixed(1)} KB`;
  if (b < 1024 * 1024 * 1024) return `${(b / (1024 * 1024)).toFixed(1)} MB`;
  return `${(b / (1024 * 1024 * 1024)).toFixed(2)} GB`;
}
