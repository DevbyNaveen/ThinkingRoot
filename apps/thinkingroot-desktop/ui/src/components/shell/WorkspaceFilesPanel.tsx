/**
 * Right-rail workspace inspector: **Readme** (default) and **Folder**
 * sub-tabs in one panel — tree over the workspace root, optional
 * `.thinkingroot` scope, text preview, and `.tr` export on Folder.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { FolderTree, Package } from "lucide-react";

import { cn } from "@/lib/utils";
import { useApp } from "@/store/app";
import { Button } from "@/components/ui/button";
import { ReadmeView } from "@/components/readme/ReadmeView";
import { FileTree } from "@/components/shell/FileTree";
import { FilePreviewContent } from "@/components/shell/filePreview";
import {
  fsReadText,
  workspaceList,
  type FsEntry,
  type WorkspaceView,
} from "@/lib/tauri";
import { toast } from "@/store/toast";

type Scope = "project" | "thinkingroot";

const PREVIEW_MAX = 512 * 1024;

export function WorkspaceFilesPanel({
  activeWorkspace,
}: {
  activeWorkspace: string | null;
}) {
  const setPackExportTarget = useApp((s) => s.setPackExportTarget);
  const inspectorPage = useApp((s) => s.workspaceInspectorPage);
  const setInspectorPage = useApp((s) => s.setWorkspaceInspectorPage);

  const [w, setW] = useState<WorkspaceView | null>(null);
  const [scope, setScope] = useState<Scope>("project");
  const [selected, setSelected] = useState<FsEntry | null>(null);
  const [preview, setPreview] = useState<string | null>(null);
  const [previewMeta, setPreviewMeta] = useState<{
    path: string;
    binary: boolean;
    tooLarge: boolean;
  } | null>(null);
  const [loadingPreview, setLoadingPreview] = useState(false);

  useEffect(() => {
    let cancelled = false;
    if (!activeWorkspace) {
      setW(null);
      return;
    }
    workspaceList()
      .then((list) => {
        if (cancelled) return;
        setW(list.find((x) => x.name === activeWorkspace) ?? null);
      })
      .catch(() => setW(null));
    return () => {
      cancelled = true;
    };
  }, [activeWorkspace]);

  const treeRoot = useMemo(() => {
    if (!w) return null;
    const base = w.path.replace(/\/+$/, "");
    if (scope === "thinkingroot") {
      return `${base}/.thinkingroot`;
    }
    return base;
  }, [w, scope]);

  useEffect(() => {
    setSelected(null);
    setPreview(null);
    setPreviewMeta(null);
  }, [scope, treeRoot]);

  const loadPreview = useCallback(async (entry: FsEntry) => {
    if (entry.kind === "directory") {
      setSelected(entry);
      setPreview(null);
      setPreviewMeta(null);
      return;
    }
    const sz = entry.size ?? 0;
    if (sz > PREVIEW_MAX) {
      setSelected(entry);
      setPreview(null);
      setPreviewMeta({
        path: entry.path,
        binary: false,
        tooLarge: true,
      });
      return;
    }
    setSelected(entry);
    setLoadingPreview(true);
    setPreviewMeta(null);
    try {
      const body = await fsReadText(entry.path);
      setPreview(body.content);
      setPreviewMeta({
        path: entry.path,
        binary: body.had_invalid_utf8,
        tooLarge: false,
      });
    } catch (e) {
      setPreview(null);
      setPreviewMeta({
        path: entry.path,
        binary: false,
        tooLarge: false,
      });
      toast("Could not read file", {
        kind: "error",
        body: e instanceof Error ? e.message : String(e),
      });
    } finally {
      setLoadingPreview(false);
    }
  }, []);

  if (!activeWorkspace) {
    return (
      <p className="px-4 py-4 text-[11px] text-muted-foreground">
        Select a workspace in the sidebar to browse files.
      </p>
    );
  }

  const folderReady = Boolean(w && treeRoot);

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex shrink-0 border-b border-border/40 px-2 py-1.5">
        <div className="flex w-full rounded-lg bg-muted/30 p-0.5">
          <button
            type="button"
            className={cn(
              "flex-1 rounded-md px-2 py-1.5 text-[10px] font-medium transition-colors",
              inspectorPage === "readme"
                ? "bg-background text-foreground shadow-sm"
                : "text-muted-foreground hover:text-foreground",
            )}
            onClick={() => setInspectorPage("readme")}
          >
            Readme
          </button>
          <button
            type="button"
            className={cn(
              "flex-1 rounded-md px-2 py-1.5 text-[10px] font-medium transition-colors",
              inspectorPage === "folder"
                ? "bg-background text-foreground shadow-sm"
                : "text-muted-foreground hover:text-foreground",
            )}
            onClick={() => setInspectorPage("folder")}
          >
            Folder
          </button>
        </div>
      </div>

      {inspectorPage === "readme" && (
        <div className="flex min-h-0 flex-1 flex-col overflow-hidden">
          <ReadmeView panelMode />
        </div>
      )}

      {inspectorPage === "folder" && !folderReady && (
        <p className="px-4 py-4 text-[11px] text-muted-foreground">
          Loading workspace path…
        </p>
      )}

      {inspectorPage === "folder" && folderReady && (
        <>
          <div className="flex shrink-0 flex-col gap-2 border-b border-border/40 px-3 py-2.5">
            <div className="flex rounded-lg bg-muted/30 p-0.5">
              <button
                type="button"
                className={cn(
                  "flex-1 rounded-md px-2 py-1.5 text-[10px] font-medium transition-colors",
                  scope === "project"
                    ? "bg-background text-foreground shadow-sm"
                    : "text-muted-foreground hover:text-foreground",
                )}
                onClick={() => setScope("project")}
              >
                Project
              </button>
              <button
                type="button"
                className={cn(
                  "flex-1 rounded-md px-2 py-1.5 text-[10px] font-medium transition-colors",
                  scope === "thinkingroot"
                    ? "bg-background text-foreground shadow-sm"
                    : "text-muted-foreground hover:text-foreground",
                )}
                onClick={() => setScope("thinkingroot")}
              >
                .thinkingroot
              </button>
            </div>
            <p
              className="truncate font-mono text-[9px] text-muted-foreground/90"
              title={treeRoot ?? undefined}
            >
              {(treeRoot as string).replace(/^\/Users\/[^/]+|^\/home\/[^/]+/, "~")}
            </p>
            <Button
              variant="outline"
              size="sm"
              className="h-8 w-full justify-center gap-1.5 rounded-xl border-border/70 text-xs"
              onClick={() =>
                setPackExportTarget({ workspace: activeWorkspace as string })
              }
            >
              <Package className="size-3.5" />
              Export .tr pack
            </Button>
          </div>

          <div className="flex min-h-0 flex-1 flex-col gap-0 lg:flex-row">
            <div
              className={cn(
                "flex min-h-0 w-full shrink-0 flex-col border-b border-border/40",
                "lg:h-full lg:max-w-[11rem] lg:min-w-[8.75rem] lg:w-[min(28%,11rem)] lg:flex-none lg:border-b-0 lg:border-r",
              )}
            >
              <div className="flex items-center gap-1.5 border-b border-border/30 px-2 py-1.5 text-[10px] font-medium uppercase tracking-wider text-muted-foreground/70">
                <FolderTree className="size-3" />
                Files
              </div>
              <div className="min-h-[140px] flex-1 overflow-y-auto overflow-x-hidden lg:max-h-none">
                <FileTree
                  key={treeRoot as string}
                  rootPath={treeRoot as string}
                  selectedPath={selected?.path}
                  onSelect={(e) => void loadPreview(e)}
                />
              </div>
            </div>

            <div className="flex min-h-[120px] min-w-0 flex-1 flex-col bg-[#1e1e1e]">
              <div className="border-b border-border/30 bg-muted/10 px-2 py-1.5 text-[10px] font-medium uppercase tracking-wider text-muted-foreground/70">
                Preview
              </div>
              <div className="min-h-0 min-w-0 flex-1 overflow-auto">
                {!selected && !loadingPreview && (
                  <p className="px-3 py-3 text-[11px] text-muted-foreground">
                    Select a file to preview (text, ≤ 512 KiB).
                  </p>
                )}
                {selected?.kind === "directory" && (
                  <p className="px-3 py-3 text-[11px] text-muted-foreground">
                    Folder — expand the tree or pick a file.
                  </p>
                )}
                {loadingPreview && (
                  <p className="px-3 py-3 text-[11px] text-muted-foreground">Loading…</p>
                )}
                {previewMeta?.tooLarge && (
                  <p className="px-3 py-3 text-[11px] text-amber-600/90">
                    File too large for preview (&gt; 512 KiB). Open it in an
                    external editor.
                  </p>
                )}
                {previewMeta?.binary && preview !== null && (
                  <p className="mb-0 px-3 pt-3 text-[10px] text-amber-600/90">
                    Non-UTF8 bytes — shown as lossy decode.
                  </p>
                )}
                {preview !== null &&
                  selected &&
                  selected.kind !== "directory" && (
                    <FilePreviewContent path={selected.path} text={preview} />
                  )}
              </div>
            </div>
          </div>
        </>
      )}
    </div>
  );
}
