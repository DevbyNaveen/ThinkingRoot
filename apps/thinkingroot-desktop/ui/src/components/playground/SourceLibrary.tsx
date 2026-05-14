import { useCallback, useEffect, useMemo, useState } from "react";
import {
  FileAudio,
  FileImage,
  FileText,
  FolderTree,
  RotateCcw,
} from "lucide-react";

import {
  playgroundSources,
  playgroundWitnessesBySource,
  type PlaygroundSource,
} from "@/lib/tauri";
import { cn } from "@/lib/utils";

/**
 * SourceLibrary — left-rail file list for the active workspace.
 *
 * Lists every source the engine indexed, grouped by file kind so a
 * researcher immediately sees "what's in here": text, images,
 * audio, anything else. Refetches on workspace switch + on
 * compile-progress "done" events the parent passes through
 * `refreshNonce`. Clicking a row signals the parent via
 * `onSelect` (the parent renders SourceDetailPanel).
 */
export function SourceLibrary({
  workspace,
  refreshNonce,
  selectedSourceId,
  onSelect,
}: {
  workspace: string | null;
  refreshNonce?: number;
  selectedSourceId?: string | null;
  onSelect?: (source: PlaygroundSource | null) => void;
}) {
  const [sources, setSources] = useState<PlaygroundSource[] | null>(null);
  const [witnessCounts, setWitnessCounts] = useState<Map<string, number>>(
    new Map(),
  );
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    if (!workspace) {
      setSources(null);
      setWitnessCounts(new Map());
      setError(null);
      return;
    }
    setLoading(true);
    setError(null);
    try {
      // Fetch sources + witness counts in parallel — both are cheap
      // sidecar GETs over loopback; serialising them would add an
      // unnecessary round-trip to the panel-open time.
      const [rows, counts] = await Promise.all([
        playgroundSources(),
        playgroundWitnessesBySource().catch(() => []),
      ]);
      setSources(rows);
      const m = new Map<string, number>();
      for (const r of counts) m.set(r.source_id, r.count);
      setWitnessCounts(m);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setSources(null);
      setWitnessCounts(new Map());
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
              <SourceGroup
                key={kind}
                kind={kind}
                label={label}
                items={items}
                witnessCounts={witnessCounts}
                selectedSourceId={selectedSourceId}
                onSelect={onSelect}
              />
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
  witnessCounts,
  selectedSourceId,
  onSelect,
}: {
  kind: SourceKind;
  label: string;
  items: PlaygroundSource[];
  witnessCounts: Map<string, number>;
  selectedSourceId?: string | null;
  onSelect?: (source: PlaygroundSource | null) => void;
}) {
  if (items.length === 0) return null;
  return (
    <li className="mt-2 first:mt-0">
      <p className="px-3 pb-1 text-[10px] font-semibold uppercase tracking-wider text-muted-foreground/70">
        {label} ({items.length})
      </p>
      <ul>
        {items.map((s) => {
          const count = witnessCounts.get(s.id) ?? 0;
          const selected = selectedSourceId === s.id;
          return (
            <li key={s.id}>
              <button
                type="button"
                onClick={() => onSelect?.(selected ? null : s)}
                aria-pressed={selected}
                className={cn(
                  "group flex w-full items-center gap-2 px-3 py-1 text-left text-xs transition-colors",
                  selected
                    ? "bg-accent/10 text-accent"
                    : "text-foreground/90 hover:bg-muted/40",
                )}
                title={s.uri}
              >
                <KindIcon kind={kind} />
                <span className="min-w-0 flex-1 truncate">
                  {basenameFromUri(s.uri)}
                </span>
                <span
                  className={cn(
                    "shrink-0 rounded px-1.5 py-px font-mono text-[10px]",
                    selected
                      ? "bg-accent text-accent-foreground"
                      : count > 0
                        ? "bg-accent/15 text-accent"
                        : "bg-muted/40 text-muted-foreground",
                  )}
                  title={`${count} witness${count === 1 ? "" : "es"}`}
                >
                  {count}
                </span>
              </button>
            </li>
          );
        })}
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
