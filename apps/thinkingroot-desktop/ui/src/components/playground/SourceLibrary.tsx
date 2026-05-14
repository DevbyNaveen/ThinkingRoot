import { useCallback, useEffect, useMemo, useState } from "react";
import {
  FileAudio,
  FileImage,
  FileText,
  FolderTree,
  RotateCcw,
} from "lucide-react";

import { playgroundSources, type PlaygroundSource } from "@/lib/tauri";
import { cn } from "@/lib/utils";

/**
 * SourceLibrary — left-rail file list for the active workspace.
 *
 * Lists every source the engine indexed, grouped by file kind so a
 * researcher immediately sees "what's in here": text, images,
 * audio, anything else. Refetches on workspace switch + on
 * compile-progress "done" events the parent passes through
 * `refreshNonce`.
 *
 * Today the panel surfaces only URIs (with display-friendly
 * basenames). Per-source witness counts + click-to-jump land in a
 * follow-up — the substrate exposes them via the existing
 * `/api/v1/ws/{ws}/witnesses` REST endpoint, just not yet wired in
 * the UI.
 */
export function SourceLibrary({
  workspace,
  refreshNonce,
}: {
  workspace: string | null;
  refreshNonce?: number;
}) {
  const [sources, setSources] = useState<PlaygroundSource[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    if (!workspace) {
      setSources(null);
      setError(null);
      return;
    }
    setLoading(true);
    setError(null);
    try {
      const rows = await playgroundSources();
      setSources(rows);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setSources(null);
    } finally {
      setLoading(false);
    }
  }, [workspace]);

  useEffect(() => {
    void load();
  }, [load, refreshNonce]);

  const grouped = useMemo(() => groupByKind(sources ?? []), [sources]);

  return (
    <aside className="flex h-full w-64 shrink-0 flex-col border-r border-border bg-surface/30">
      <header className="flex shrink-0 items-center justify-between gap-2 border-b border-border px-3 py-2">
        <div className="flex min-w-0 items-center gap-2">
          <FolderTree className="size-4 text-muted-foreground" />
          <h3 className="truncate text-xs font-semibold uppercase tracking-wide text-muted-foreground">
            Sources
          </h3>
          {sources && (
            <span className="text-xs text-muted-foreground">{sources.length}</span>
          )}
        </div>
        <button
          type="button"
          onClick={load}
          aria-label="Refresh sources"
          className="rounded-md p-1 text-muted-foreground transition-colors hover:bg-muted/60 hover:text-foreground"
        >
          <RotateCcw className="size-3" />
        </button>
      </header>
      <div className="flex-1 overflow-auto">
        {error ? (
          <p className="px-3 py-3 text-xs text-destructive">{error}</p>
        ) : loading && !sources ? (
          <p className="px-3 py-3 text-xs text-muted-foreground">Loading…</p>
        ) : !workspace ? (
          <p className="px-3 py-3 text-xs text-muted-foreground">
            Pick a workspace to list its sources.
          </p>
        ) : sources && sources.length === 0 ? (
          <p className="px-3 py-3 text-xs text-muted-foreground">
            No sources yet. Drop files onto the window above.
          </p>
        ) : (
          <ul className="flex flex-col py-1">
            {grouped.map(({ kind, label, items }) => (
              <SourceGroup key={kind} kind={kind} label={label} items={items} />
            ))}
          </ul>
        )}
      </div>
    </aside>
  );
}

type SourceKind = "text" | "image" | "audio" | "other";

function SourceGroup({
  kind,
  label,
  items,
}: {
  kind: SourceKind;
  label: string;
  items: PlaygroundSource[];
}) {
  if (items.length === 0) return null;
  return (
    <li className="mt-2 first:mt-0">
      <p className="px-3 pb-1 text-[10px] font-semibold uppercase tracking-wider text-muted-foreground/70">
        {label} ({items.length})
      </p>
      <ul>
        {items.map((s) => (
          <li
            key={s.id}
            className={cn(
              "group flex items-center gap-2 px-3 py-1 text-xs text-foreground/90",
              "hover:bg-muted/40",
            )}
            title={s.uri}
          >
            <KindIcon kind={kind} />
            <span className="truncate">{basenameFromUri(s.uri)}</span>
          </li>
        ))}
      </ul>
    </li>
  );
}

function KindIcon({ kind }: { kind: SourceKind }) {
  const cls = "size-3.5 shrink-0 text-muted-foreground";
  switch (kind) {
    case "image":
      return <FileImage className={cls} />;
    case "audio":
      return <FileAudio className={cls} />;
    case "text":
    case "other":
    default:
      return <FileText className={cls} />;
  }
}

function groupByKind(
  sources: PlaygroundSource[],
): { kind: SourceKind; label: string; items: PlaygroundSource[] }[] {
  const text: PlaygroundSource[] = [];
  const image: PlaygroundSource[] = [];
  const audio: PlaygroundSource[] = [];
  const other: PlaygroundSource[] = [];
  for (const s of sources) {
    const ext = extOf(s.uri);
    if (IMAGE_EXTS.has(ext)) image.push(s);
    else if (AUDIO_EXTS.has(ext)) audio.push(s);
    else if (TEXT_EXTS.has(ext)) text.push(s);
    else other.push(s);
  }
  // Stable lexical sort within each group so the list doesn't
  // shuffle on refresh.
  for (const arr of [text, image, audio, other]) {
    arr.sort((a, b) =>
      basenameFromUri(a.uri).localeCompare(basenameFromUri(b.uri)),
    );
  }
  return [
    { kind: "text", label: "Text", items: text },
    { kind: "image", label: "Image", items: image },
    { kind: "audio", label: "Audio", items: audio },
    { kind: "other", label: "Other", items: other },
  ];
}

function extOf(uri: string): string {
  const last = uri.split(/[\/\\]/).pop() ?? "";
  const dot = last.lastIndexOf(".");
  if (dot < 0) return "";
  return last.slice(dot + 1).toLowerCase();
}

function basenameFromUri(uri: string): string {
  // Trim `file://` prefix if present, then take the trailing path
  // component. Cross-platform safe (slashes / backslashes).
  const trimmed = uri.replace(/^file:\/\//, "");
  return trimmed.split(/[\/\\]/).pop() || trimmed;
}

const IMAGE_EXTS = new Set([
  "jpg",
  "jpeg",
  "png",
  "gif",
  "webp",
  "tiff",
  "tif",
  "bmp",
  "pnm",
  "ppm",
  "pgm",
  "pbm",
]);
const AUDIO_EXTS = new Set([
  "wav",
  "flac",
  "mp3",
  "ogg",
  "opus",
  "m4a",
  "aac",
]);
const TEXT_EXTS = new Set([
  "md",
  "markdown",
  "mdx",
  "rs",
  "py",
  "pyi",
  "js",
  "jsx",
  "mjs",
  "cjs",
  "ts",
  "tsx",
  "go",
  "java",
  "c",
  "h",
  "cpp",
  "cc",
  "cxx",
  "hpp",
  "hxx",
  "cs",
  "rb",
  "kt",
  "kts",
  "swift",
  "php",
  "sh",
  "bash",
  "lua",
  "scala",
  "ex",
  "exs",
  "hs",
  "r",
  "pdf",
  "toml",
  "yaml",
  "yml",
  "json",
  "csv",
  "tsv",
  "txt",
  "cfg",
  "ini",
  "env",
]);
