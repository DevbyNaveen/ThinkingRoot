import { useEffect, useState } from "react";
import {
  ChevronRight,
  Folder,
  FolderOpen,
  FileText,
  Loader2,
  AlertTriangle,
  Link as SymlinkIcon,
} from "lucide-react";
import { fsListDir, type FsEntry } from "@/lib/tauri";
import { cn } from "@/lib/utils";

/**
 * VS Code-style collapsible file tree, lazy-loaded one directory at a
 * time. The root is whatever absolute path the caller passes; each
 * folder's children are fetched on first expand and cached for the
 * lifetime of the component.
 *
 * Selection bubbles up via `onSelect` so parents can show a
 * preview/graph for the chosen file. The tree itself doesn't render
 * file content — that's the job of whoever embeds it.
 */
export function FileTree({
  rootPath,
  onSelect,
  selectedPath,
}: {
  rootPath: string;
  onSelect?: (entry: FsEntry) => void;
  selectedPath?: string | null;
}) {
  return (
    <div className="flex h-full flex-col overflow-y-auto py-2 font-mono text-[11px]">
      <FolderNode
        path={rootPath}
        name={prettyRootName(rootPath)}
        depth={0}
        onSelect={onSelect}
        selectedPath={selectedPath}
        defaultOpen
      />
    </div>
  );
}

function prettyRootName(path: string): string {
  const trimmed = path.replace(/\/+$/, "");
  const last = trimmed.split("/").pop();
  return last && last.length > 0 ? last : path;
}

function FolderNode({
  path,
  name,
  depth,
  onSelect,
  selectedPath,
  defaultOpen = false,
}: {
  path: string;
  name: string;
  depth: number;
  onSelect?: (entry: FsEntry) => void;
  selectedPath?: string | null;
  defaultOpen?: boolean;
}) {
  const [open, setOpen] = useState(defaultOpen);
  const [children, setChildren] = useState<FsEntry[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Lazy-load: only fetch the first time the folder opens.
  useEffect(() => {
    if (!open || children !== null) return;
    let cancelled = false;
    setLoading(true);
    setError(null);
    fsListDir(path)
      .then((c) => {
        if (!cancelled) setChildren(c);
      })
      .catch((e) => {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e));
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [open, path, children]);

  return (
    <div>
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        className={cn(
          "flex w-full items-center gap-1 rounded-sm px-1 py-[2px] text-left transition-colors",
          "hover:bg-muted/60",
        )}
        style={{ paddingLeft: `${depth * 12 + 6}px` }}
      >
        <ChevronRight
          className={cn(
            "size-3 shrink-0 text-muted-foreground transition-transform",
            open && "rotate-90",
          )}
        />
        {open ? (
          <FolderOpen className="size-3.5 shrink-0 text-accent" />
        ) : (
          <Folder className="size-3.5 shrink-0 text-muted-foreground" />
        )}
        <span className="min-w-0 truncate text-foreground">{name}</span>
      </button>
      {open && (
        <>
          {loading && (
            <div
              className="flex items-center gap-1 px-1 py-1 text-muted-foreground"
              style={{ paddingLeft: `${(depth + 1) * 12 + 6}px` }}
            >
              <Loader2 className="size-3 animate-spin" />
              loading…
            </div>
          )}
          {error && (
            <div
              className="flex items-start gap-1 px-1 py-1 text-destructive"
              style={{ paddingLeft: `${(depth + 1) * 12 + 6}px` }}
            >
              <AlertTriangle className="mt-px size-3 shrink-0" />
              <span className="break-all">{error}</span>
            </div>
          )}
          {children?.map((entry) =>
            entry.kind === "directory" ? (
              <FolderNode
                key={entry.path}
                path={entry.path}
                name={entry.name}
                depth={depth + 1}
                onSelect={onSelect}
                selectedPath={selectedPath}
              />
            ) : (
              <FileNode
                key={entry.path}
                entry={entry}
                depth={depth + 1}
                onSelect={onSelect}
                selected={selectedPath === entry.path}
              />
            ),
          )}
        </>
      )}
    </div>
  );
}

function FileNode({
  entry,
  depth,
  onSelect,
  selected,
}: {
  entry: FsEntry;
  depth: number;
  onSelect?: (entry: FsEntry) => void;
  selected?: boolean;
}) {
  const Icon = entry.kind === "symlink" ? SymlinkIcon : FileText;
  return (
    <button
      type="button"
      onClick={() => onSelect?.(entry)}
      className={cn(
        "flex w-full items-center gap-1 rounded-sm px-1 py-[2px] text-left transition-colors",
        selected ? "bg-accent/15 text-accent" : "hover:bg-muted/60 text-foreground",
      )}
      style={{ paddingLeft: `${depth * 12 + 6 + 12}px` }}
      title={entry.path}
    >
      <Icon
        className={cn(
          "size-3.5 shrink-0",
          selected ? "text-accent" : "text-muted-foreground",
        )}
      />
      <span className="min-w-0 truncate">{entry.name}</span>
    </button>
  );
}
