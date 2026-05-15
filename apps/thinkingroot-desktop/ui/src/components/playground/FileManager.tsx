import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  ArrowUp,
  ChevronRight,
  File,
  FileAudio,
  FileCode,
  FileImage,
  FileText,
  FileVideo,
  FolderClosed,
  FolderPlus,
  Home,
  Loader2,
  Pencil,
  RotateCcw,
  Trash2,
  Undo2,
} from "lucide-react";

import {
  playgroundCreateFolder,
  playgroundEmptyTrash,
  playgroundListDirectory,
  playgroundListTrash,
  playgroundMove,
  playgroundRename,
  playgroundRestore,
  playgroundTrash,
  type PlaygroundDirEntry,
  type PlaygroundDirListing,
} from "@/lib/tauri";
import { cn } from "@/lib/utils";
import { FilePreviewPanel } from "@/components/playground/FilePreviewPanel";

interface Props {
  workspace: string | null;
  /** Bumped by the parent on compile completion so the listing
   * refreshes (witness counts, new files from inbox/, etc.). */
  refreshNonce: number;
}

type ViewMode = "browse" | "trash";

/** World-class file manager for the Playground surface.
 *
 * Browse / rename / move / trash / restore / preview, with
 * multi-select, keyboard shortcuts, and honest empty states. Every
 * operation is a thin call into the `playground_*` Tauri commands
 * which canonicalise paths server-side — the UI never assumes a path
 * is safe.
 */
export function FileManager({ workspace, refreshNonce }: Props) {
  if (!workspace) {
    return (
      <div className="flex h-full items-center justify-center text-xs text-muted-foreground">
        Select a workspace to browse files.
      </div>
    );
  }
  return <FileManagerInner workspace={workspace} refreshNonce={refreshNonce} />;
}

interface InnerProps {
  workspace: string;
  refreshNonce: number;
}

function FileManagerInner({ workspace, refreshNonce }: InnerProps) {
  const [view, setView] = useState<ViewMode>("browse");
  const [listing, setListing] = useState<PlaygroundDirListing | null>(null);
  const [trash, setTrash] = useState<PlaygroundDirEntry[]>([]);
  const [relPath, setRelPath] = useState<string>("");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [lastClickedRel, setLastClickedRel] = useState<string | null>(null);
  const [renamingRel, setRenamingRel] = useState<string | null>(null);
  const [renameDraft, setRenameDraft] = useState<string>("");
  const [creatingFolder, setCreatingFolder] = useState(false);
  const [newFolderDraft, setNewFolderDraft] = useState<string>("");
  const [previewRel, setPreviewRel] = useState<string | null>(null);
  const [toast, setToast] = useState<string | null>(null);
  const renameInputRef = useRef<HTMLInputElement | null>(null);
  const newFolderInputRef = useRef<HTMLInputElement | null>(null);

  const showToast = useCallback((msg: string) => {
    setToast(msg);
    window.setTimeout(() => setToast((current) => (current === msg ? null : current)), 2400);
  }, []);

  const reload = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      if (view === "browse") {
        const out = await playgroundListDirectory(workspace, relPath);
        setListing(out);
      } else {
        const t = await playgroundListTrash(workspace);
        setTrash(t);
      }
    } catch (e) {
      setError(typeof e === "string" ? e : (e as Error).message ?? "unknown error");
    } finally {
      setLoading(false);
    }
  }, [workspace, relPath, view]);

  // Reload when workspace / path / view / external refreshNonce changes.
  useEffect(() => {
    setSelected(new Set());
    setRenamingRel(null);
    setCreatingFolder(false);
    setPreviewRel(null);
    void reload();
  }, [reload, refreshNonce]);

  // Focus the rename input when it appears.
  useEffect(() => {
    if (renamingRel && renameInputRef.current) {
      renameInputRef.current.focus();
      renameInputRef.current.select();
    }
  }, [renamingRel]);

  // Focus new-folder input when it appears.
  useEffect(() => {
    if (creatingFolder && newFolderInputRef.current) {
      newFolderInputRef.current.focus();
    }
  }, [creatingFolder]);

  const entries: PlaygroundDirEntry[] = useMemo(() => {
    return view === "browse" ? (listing?.entries ?? []) : trash;
  }, [view, listing, trash]);

  const onRowClick = useCallback(
    (entry: PlaygroundDirEntry, ev: React.MouseEvent) => {
      // Cmd/Ctrl: toggle this row. Shift: range select. Plain click:
      // replace selection with just this row.
      setLastClickedRel(entry.rel_path);
      if (ev.metaKey || ev.ctrlKey) {
        setSelected((prev) => {
          const next = new Set(prev);
          if (next.has(entry.rel_path)) {
            next.delete(entry.rel_path);
          } else {
            next.add(entry.rel_path);
          }
          return next;
        });
        return;
      }
      if (ev.shiftKey && lastClickedRel) {
        const idxA = entries.findIndex((e) => e.rel_path === lastClickedRel);
        const idxB = entries.findIndex((e) => e.rel_path === entry.rel_path);
        if (idxA >= 0 && idxB >= 0) {
          const [lo, hi] = idxA < idxB ? [idxA, idxB] : [idxB, idxA];
          setSelected(new Set(entries.slice(lo, hi + 1).map((e) => e.rel_path)));
          return;
        }
      }
      setSelected(new Set([entry.rel_path]));
      // Single click on a file: open preview pane.
      if (!entry.is_dir && view === "browse") {
        setPreviewRel(entry.rel_path);
      } else {
        setPreviewRel(null);
      }
    },
    [entries, lastClickedRel, view],
  );

  const onRowDoubleClick = useCallback(
    (entry: PlaygroundDirEntry) => {
      if (view === "trash") return; // trash items don't navigate.
      if (entry.is_dir) {
        setRelPath(entry.rel_path);
        setSelected(new Set());
        setPreviewRel(null);
      } else {
        // Double-click a file opens preview (single-click already does
        // it; the double-click is a no-op so existing muscle memory
        // doesn't surprise the user).
        setPreviewRel(entry.rel_path);
      }
    },
    [view],
  );

  const navigateUp = useCallback(() => {
    if (view !== "browse") return;
    const parent = listing?.parent_rel_path ?? null;
    if (parent === null) return;
    setRelPath(parent);
  }, [listing, view]);

  const navigateRoot = useCallback(() => {
    setRelPath("");
    setView("browse");
  }, []);

  const startRename = useCallback(() => {
    if (view !== "browse") return;
    if (selected.size !== 1) return;
    const [rel] = Array.from(selected);
    if (!rel) return;
    const entry = entries.find((e) => e.rel_path === rel);
    if (!entry) return;
    setRenamingRel(rel);
    setRenameDraft(entry.name);
  }, [selected, entries, view]);

  const commitRename = useCallback(async () => {
    if (!renamingRel) return;
    const newName = renameDraft.trim();
    setRenamingRel(null);
    if (!newName) return;
    const entry = entries.find((e) => e.rel_path === renamingRel);
    if (!entry || entry.name === newName) return;
    try {
      await playgroundRename(workspace, renamingRel, newName);
      showToast(`Renamed to "${newName}".`);
      await reload();
    } catch (e) {
      showToast(`Rename failed: ${typeof e === "string" ? e : (e as Error).message}`);
    }
  }, [renamingRel, renameDraft, entries, workspace, reload, showToast]);

  const commitNewFolder = useCallback(async () => {
    const name = newFolderDraft.trim();
    setCreatingFolder(false);
    setNewFolderDraft("");
    if (!name) return;
    try {
      await playgroundCreateFolder(workspace, relPath, name);
      showToast(`Folder "${name}" created.`);
      await reload();
    } catch (e) {
      showToast(`Create failed: ${typeof e === "string" ? e : (e as Error).message}`);
    }
  }, [newFolderDraft, workspace, relPath, reload, showToast]);

  const trashSelected = useCallback(async () => {
    if (view !== "browse" || selected.size === 0) return;
    const paths = Array.from(selected);
    try {
      const out = await playgroundTrash(workspace, paths);
      showToast(
        out.skipped > 0
          ? `${out.trashed} moved to trash, ${out.skipped} skipped.`
          : `${out.trashed} moved to trash.`,
      );
      setSelected(new Set());
      setPreviewRel(null);
      await reload();
    } catch (e) {
      showToast(`Trash failed: ${typeof e === "string" ? e : (e as Error).message}`);
    }
  }, [view, selected, workspace, reload, showToast]);

  const restoreSelected = useCallback(async () => {
    if (view !== "trash" || selected.size === 0) return;
    const paths = Array.from(selected);
    try {
      const count = await playgroundRestore(workspace, paths);
      showToast(`${count} restored.`);
      setSelected(new Set());
      await reload();
    } catch (e) {
      showToast(`Restore failed: ${typeof e === "string" ? e : (e as Error).message}`);
    }
  }, [view, selected, workspace, reload, showToast]);

  const emptyTrash = useCallback(async () => {
    if (view !== "trash" || trash.length === 0) return;
    if (!confirm(`Permanently delete ${trash.length} item(s) from trash?`)) return;
    try {
      const count = await playgroundEmptyTrash(workspace);
      showToast(`${count} permanently deleted.`);
      await reload();
    } catch (e) {
      showToast(`Empty trash failed: ${typeof e === "string" ? e : (e as Error).message}`);
    }
  }, [view, trash.length, workspace, reload, showToast]);

  // Drag-drop reorder: native HTML5 DnD between rows. Source rows
  // set dataTransfer with the selected rel_paths (or just the
  // dragged row if not in selection). Folder rows accept drop and
  // call `playgroundMove`.
  const [dragOverRel, setDragOverRel] = useState<string | null>(null);
  const onDragStart = (entry: PlaygroundDirEntry, ev: React.DragEvent) => {
    if (view !== "browse") {
      ev.preventDefault();
      return;
    }
    const paths = selected.has(entry.rel_path)
      ? Array.from(selected)
      : [entry.rel_path];
    ev.dataTransfer.setData("application/x-playground-rel-paths", JSON.stringify(paths));
    ev.dataTransfer.effectAllowed = "move";
  };
  const onDragOverFolder = (entry: PlaygroundDirEntry, ev: React.DragEvent) => {
    if (!entry.is_dir) return;
    ev.preventDefault();
    ev.dataTransfer.dropEffect = "move";
    setDragOverRel(entry.rel_path);
  };
  const onDragLeaveFolder = () => setDragOverRel(null);
  const onDropFolder = async (entry: PlaygroundDirEntry, ev: React.DragEvent) => {
    if (!entry.is_dir) return;
    ev.preventDefault();
    setDragOverRel(null);
    const raw = ev.dataTransfer.getData("application/x-playground-rel-paths");
    if (!raw) return;
    let paths: string[];
    try {
      paths = JSON.parse(raw) as string[];
    } catch {
      return;
    }
    // Don't move a folder into itself.
    const filtered = paths.filter((p) => p !== entry.rel_path);
    if (filtered.length === 0) return;
    try {
      const out = await playgroundMove(workspace, filtered, entry.rel_path);
      showToast(
        out.skipped_conflict + out.skipped_invalid > 0
          ? `${out.moved} moved, ${out.skipped_conflict + out.skipped_invalid} skipped.`
          : `${out.moved} moved to ${entry.name}.`,
      );
      setSelected(new Set());
      await reload();
    } catch (e) {
      showToast(`Move failed: ${typeof e === "string" ? e : (e as Error).message}`);
    }
  };
  const onDropUp = async (ev: React.DragEvent) => {
    if (view !== "browse" || !listing) return;
    if (listing.parent_rel_path === null) return;
    ev.preventDefault();
    const raw = ev.dataTransfer.getData("application/x-playground-rel-paths");
    if (!raw) return;
    let paths: string[];
    try {
      paths = JSON.parse(raw) as string[];
    } catch {
      return;
    }
    try {
      const out = await playgroundMove(workspace, paths, listing.parent_rel_path);
      showToast(`${out.moved} moved up.`);
      setSelected(new Set());
      await reload();
    } catch (e) {
      showToast(`Move failed: ${typeof e === "string" ? e : (e as Error).message}`);
    }
  };

  // Keyboard shortcuts: scoped via document listener so they fire
  // when the file-manager area has focus. Avoid hijacking shortcuts
  // when the user is typing in an input (rename / new-folder).
  useEffect(() => {
    const handler = (ev: KeyboardEvent) => {
      if (renamingRel || creatingFolder) return;
      const target = ev.target as HTMLElement | null;
      if (target && (target.tagName === "INPUT" || target.tagName === "TEXTAREA")) {
        return;
      }
      if (ev.key === "Delete" || ev.key === "Backspace") {
        if (view === "browse" && selected.size > 0) {
          ev.preventDefault();
          void trashSelected();
        }
        return;
      }
      if (ev.key === "F2") {
        ev.preventDefault();
        startRename();
        return;
      }
      if (ev.key === "Escape") {
        setSelected(new Set());
        setPreviewRel(null);
        return;
      }
      if ((ev.metaKey || ev.ctrlKey) && ev.key.toLowerCase() === "a") {
        ev.preventDefault();
        setSelected(new Set(entries.map((e) => e.rel_path)));
        return;
      }
    };
    document.addEventListener("keydown", handler);
    return () => document.removeEventListener("keydown", handler);
  }, [
    renamingRel,
    creatingFolder,
    view,
    selected.size,
    trashSelected,
    startRename,
    entries,
  ]);

  // Breadcrumb segments derived from relPath.
  const crumbs = useMemo(() => {
    if (view === "trash") return [];
    if (!relPath) return [];
    const segs = relPath.split("/").filter(Boolean);
    return segs.map((seg, idx) => ({
      name: seg,
      rel: segs.slice(0, idx + 1).join("/"),
    }));
  }, [relPath, view]);

  return (
    <div className="flex h-full min-w-0 flex-col">
      {/* Toolbar */}
      <div className="flex shrink-0 items-center gap-1 border-b border-border bg-surface/30 px-2 py-1.5 text-xs">
        <button
          type="button"
          onClick={navigateRoot}
          className="rounded p-1 text-muted-foreground hover:bg-muted/40 hover:text-foreground"
          title="Workspace root"
        >
          <Home className="size-3.5" />
        </button>
        <button
          type="button"
          onClick={navigateUp}
          disabled={view !== "browse" || !listing?.parent_rel_path}
          onDragOver={(ev) => {
            if (view === "browse" && listing?.parent_rel_path !== null) {
              ev.preventDefault();
              ev.dataTransfer.dropEffect = "move";
            }
          }}
          onDrop={(ev) => void onDropUp(ev)}
          className="rounded p-1 text-muted-foreground hover:bg-muted/40 hover:text-foreground disabled:opacity-30 disabled:hover:bg-transparent"
          title="Up one folder (or drop here to move up)"
        >
          <ArrowUp className="size-3.5" />
        </button>
        <div className="mx-1 h-4 w-px bg-border" />
        {/* Breadcrumb */}
        <div className="flex min-w-0 flex-1 items-center gap-0.5 truncate">
          {view === "trash" ? (
            <span className="font-medium text-foreground">Trash</span>
          ) : crumbs.length === 0 ? (
            <span className="text-muted-foreground">/</span>
          ) : (
            crumbs.map((c, idx) => (
              <span key={c.rel} className="flex items-center gap-0.5">
                {idx > 0 && <ChevronRight className="size-3 text-muted-foreground" />}
                <button
                  type="button"
                  onClick={() => setRelPath(c.rel)}
                  className="rounded px-1 py-0.5 text-foreground hover:bg-muted/40"
                >
                  {c.name}
                </button>
              </span>
            ))
          )}
        </div>
        <div className="mx-1 h-4 w-px bg-border" />
        {view === "browse" ? (
          <>
            <button
              type="button"
              onClick={() => {
                setCreatingFolder(true);
                setNewFolderDraft("");
              }}
              className="flex items-center gap-1 rounded px-2 py-1 text-muted-foreground hover:bg-muted/40 hover:text-foreground"
              title="New folder"
            >
              <FolderPlus className="size-3.5" />
              <span>New folder</span>
            </button>
            <button
              type="button"
              onClick={startRename}
              disabled={selected.size !== 1}
              className="flex items-center gap-1 rounded px-2 py-1 text-muted-foreground hover:bg-muted/40 hover:text-foreground disabled:opacity-30 disabled:hover:bg-transparent"
              title="Rename (F2)"
            >
              <Pencil className="size-3.5" />
              <span>Rename</span>
            </button>
            <button
              type="button"
              onClick={() => void trashSelected()}
              disabled={selected.size === 0}
              className="flex items-center gap-1 rounded px-2 py-1 text-rose-400 hover:bg-rose-500/10 disabled:opacity-30 disabled:hover:bg-transparent"
              title="Move to trash (Del)"
            >
              <Trash2 className="size-3.5" />
              <span>Trash</span>
            </button>
            <button
              type="button"
              onClick={() => setView("trash")}
              className="rounded px-2 py-1 text-muted-foreground hover:bg-muted/40 hover:text-foreground"
              title="Open trash"
            >
              View trash
            </button>
          </>
        ) : (
          <>
            <button
              type="button"
              onClick={() => void restoreSelected()}
              disabled={selected.size === 0}
              className="flex items-center gap-1 rounded px-2 py-1 text-muted-foreground hover:bg-muted/40 hover:text-foreground disabled:opacity-30 disabled:hover:bg-transparent"
              title="Restore selected"
            >
              <RotateCcw className="size-3.5" />
              <span>Restore</span>
            </button>
            <button
              type="button"
              onClick={() => void emptyTrash()}
              disabled={trash.length === 0}
              className="flex items-center gap-1 rounded px-2 py-1 text-rose-400 hover:bg-rose-500/10 disabled:opacity-30 disabled:hover:bg-transparent"
              title="Permanently delete every item"
            >
              <Trash2 className="size-3.5" />
              <span>Empty</span>
            </button>
            <button
              type="button"
              onClick={() => {
                setView("browse");
                setSelected(new Set());
              }}
              className="flex items-center gap-1 rounded px-2 py-1 text-muted-foreground hover:bg-muted/40 hover:text-foreground"
              title="Back to files"
            >
              <Undo2 className="size-3.5" />
              <span>Back</span>
            </button>
          </>
        )}
      </div>

      {/* Body: file list + optional preview pane */}
      <div className="flex min-h-0 flex-1">
        <div className="min-w-0 flex-1 overflow-auto">
          {creatingFolder && view === "browse" && (
            <div className="flex items-center gap-2 border-b border-border bg-surface/20 px-3 py-1.5 text-xs">
              <FolderClosed className="size-3.5 text-amber-400" />
              <input
                ref={newFolderInputRef}
                value={newFolderDraft}
                onChange={(e) => setNewFolderDraft(e.target.value)}
                onBlur={() => void commitNewFolder()}
                onKeyDown={(e) => {
                  if (e.key === "Enter") void commitNewFolder();
                  if (e.key === "Escape") {
                    setCreatingFolder(false);
                    setNewFolderDraft("");
                  }
                }}
                placeholder="folder name"
                className="flex-1 rounded border border-border bg-background px-2 py-0.5 text-foreground outline-none focus:border-accent"
              />
            </div>
          )}
          {error && (
            <div className="border-b border-rose-500/30 bg-rose-500/10 px-3 py-2 text-xs text-rose-300">
              {error}
            </div>
          )}
          {loading && entries.length === 0 ? (
            <div className="flex items-center gap-2 px-4 py-6 text-xs text-muted-foreground">
              <Loader2 className="size-3.5 animate-spin" />
              Loading…
            </div>
          ) : entries.length === 0 ? (
            <EmptyState view={view} />
          ) : (
            <ul className="divide-y divide-border/40 text-xs">
              {entries.map((entry) => (
                <FileRow
                  key={entry.rel_path}
                  entry={entry}
                  selected={selected.has(entry.rel_path)}
                  renaming={renamingRel === entry.rel_path}
                  renameDraft={renameDraft}
                  setRenameDraft={setRenameDraft}
                  commitRename={() => void commitRename()}
                  cancelRename={() => setRenamingRel(null)}
                  renameInputRef={renameInputRef}
                  isDragTarget={dragOverRel === entry.rel_path}
                  view={view}
                  onClick={(ev) => onRowClick(entry, ev)}
                  onDoubleClick={() => onRowDoubleClick(entry)}
                  onDragStart={(ev) => onDragStart(entry, ev)}
                  onDragOver={(ev) => onDragOverFolder(entry, ev)}
                  onDragLeave={onDragLeaveFolder}
                  onDrop={(ev) => void onDropFolder(entry, ev)}
                />
              ))}
            </ul>
          )}
        </div>
        {previewRel && view === "browse" && (
          <FilePreviewPanel
            workspace={workspace}
            relPath={previewRel}
            onClose={() => setPreviewRel(null)}
          />
        )}
      </div>

      {/* Footer info bar */}
      <div className="flex shrink-0 items-center justify-between border-t border-border bg-surface/20 px-3 py-1 text-[10px] text-muted-foreground">
        <span>
          {entries.length} item{entries.length === 1 ? "" : "s"}
          {selected.size > 0 && ` · ${selected.size} selected`}
        </span>
        <span className="truncate">
          {view === "browse"
            ? listing
              ? `${listing.workspace} / ${listing.rel_path || "(root)"}`
              : ""
            : "trash"}
        </span>
      </div>

      {/* Toast */}
      {toast && (
        <div className="pointer-events-none absolute bottom-6 left-1/2 -translate-x-1/2 rounded-md border border-border bg-surface px-3 py-1.5 text-xs text-foreground shadow-lg">
          {toast}
        </div>
      )}
    </div>
  );
}

function EmptyState({ view }: { view: ViewMode }) {
  return (
    <div className="flex flex-col items-center gap-2 px-6 py-12 text-center text-xs text-muted-foreground">
      {view === "trash" ? (
        <>
          <Trash2 className="size-6 opacity-40" />
          <p>Trash is empty.</p>
        </>
      ) : (
        <>
          <FolderClosed className="size-6 opacity-40" />
          <p>This folder is empty.</p>
          <p className="opacity-70">
            Drop files anywhere on the Playground, or click "New folder" above.
          </p>
        </>
      )}
    </div>
  );
}

interface FileRowProps {
  entry: PlaygroundDirEntry;
  selected: boolean;
  renaming: boolean;
  renameDraft: string;
  setRenameDraft: (v: string) => void;
  commitRename: () => void;
  cancelRename: () => void;
  renameInputRef: React.MutableRefObject<HTMLInputElement | null>;
  isDragTarget: boolean;
  view: ViewMode;
  onClick: (ev: React.MouseEvent) => void;
  onDoubleClick: () => void;
  onDragStart: (ev: React.DragEvent) => void;
  onDragOver: (ev: React.DragEvent) => void;
  onDragLeave: () => void;
  onDrop: (ev: React.DragEvent) => void;
}

function FileRow({
  entry,
  selected,
  renaming,
  renameDraft,
  setRenameDraft,
  commitRename,
  cancelRename,
  renameInputRef,
  isDragTarget,
  view,
  onClick,
  onDoubleClick,
  onDragStart,
  onDragOver,
  onDragLeave,
  onDrop,
}: FileRowProps) {
  return (
    <li
      onClick={onClick}
      onDoubleClick={onDoubleClick}
      draggable={!renaming && view === "browse"}
      onDragStart={onDragStart}
      onDragOver={onDragOver}
      onDragLeave={onDragLeave}
      onDrop={onDrop}
      className={cn(
        "flex cursor-pointer items-center gap-2 px-3 py-1.5",
        selected ? "bg-accent/15 text-accent-foreground" : "hover:bg-muted/30",
        isDragTarget && "outline outline-1 outline-accent",
      )}
    >
      <span className="shrink-0">
        <FileIcon kind={entry.kind} isDir={entry.is_dir} />
      </span>
      <span className="min-w-0 flex-1 truncate">
        {renaming ? (
          <input
            ref={renameInputRef}
            value={renameDraft}
            onChange={(e) => setRenameDraft(e.target.value)}
            onClick={(e) => e.stopPropagation()}
            onBlur={commitRename}
            onKeyDown={(e) => {
              if (e.key === "Enter") commitRename();
              if (e.key === "Escape") cancelRename();
            }}
            className="w-full rounded border border-border bg-background px-1.5 py-0.5 text-xs outline-none focus:border-accent"
          />
        ) : (
          <span className="truncate">{entry.name}</span>
        )}
      </span>
      <span className="shrink-0 text-[10px] text-muted-foreground">
        {entry.is_dir ? "" : formatBytes(entry.size_bytes)}
      </span>
      <span className="shrink-0 text-[10px] text-muted-foreground">
        {formatRelTime(entry.modified)}
      </span>
    </li>
  );
}

function FileIcon({ kind, isDir }: { kind: string; isDir: boolean }) {
  if (isDir) return <FolderClosed className="size-4 text-amber-400" />;
  switch (kind) {
    case "image":
      return <FileImage className="size-4 text-emerald-400" />;
    case "audio":
      return <FileAudio className="size-4 text-violet-400" />;
    case "video":
      return <FileVideo className="size-4 text-rose-400" />;
    case "markdown":
    case "text":
      return <FileText className="size-4 text-sky-400" />;
    case "code":
      return <FileCode className="size-4 text-orange-400" />;
    default:
      return <File className="size-4 text-muted-foreground" />;
  }
}

function formatBytes(b: number): string {
  if (b < 1024) return `${b} B`;
  if (b < 1024 * 1024) return `${(b / 1024).toFixed(1)} KB`;
  if (b < 1024 * 1024 * 1024) return `${(b / (1024 * 1024)).toFixed(1)} MB`;
  return `${(b / (1024 * 1024 * 1024)).toFixed(2)} GB`;
}

function formatRelTime(unixSecs: number): string {
  if (unixSecs <= 0) return "";
  const now = Date.now() / 1000;
  const diff = Math.max(0, now - unixSecs);
  if (diff < 60) return "just now";
  if (diff < 3600) return `${Math.floor(diff / 60)}m`;
  if (diff < 86400) return `${Math.floor(diff / 3600)}h`;
  if (diff < 86400 * 30) return `${Math.floor(diff / 86400)}d`;
  if (diff < 86400 * 365) return `${Math.floor(diff / (86400 * 30))}mo`;
  return `${Math.floor(diff / (86400 * 365))}y`;
}

