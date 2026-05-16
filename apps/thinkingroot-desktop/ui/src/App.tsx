import { useEffect, useState } from "react";
import { TooltipProvider } from "@/components/ui/tooltip";
import { Sidebar } from "@/components/shell/Sidebar";
import { MainPane } from "@/components/shell/MainPane";
import { RightRail } from "@/components/shell/RightRail";
import { CommandPalette } from "@/components/command-palette/CommandPalette";
import { ToastStack } from "@/components/ui/toast-stack";
import { InstallTrSheet } from "@/components/install/InstallTrSheet";
import { PackExportSheet } from "@/components/export/PackExportSheet";
import { EngineGate } from "@/components/engine/EngineGate";
import { onTrFileOpened, onWorkspaceCompileProgress } from "@/lib/tauri";
import { useApp } from "@/store/app";
import { refreshBrainSnapshotCache } from "@/store/brain-cache";

/**
 * Desktop app root. Three horizontal regions inside a vertical
 * column:
 *
 *   +-------------------------------------------------+
 *   | rail | sidebar |     main pane     | right rail |
 *   |      |         |                   |            |
 *   +------+---------+-------------------+------------+
 *
 * Compile progress, tokens, and sidecar status are not shown in a
 * bottom chrome strip — use the Compile right rail and ⌘K when needed.
 * Sidebar and right rail are independently collapsible. The main pane
 * hosts content (chat / settings / …) for the active surface.
 */
export default function App() {
  const theme = useApp((s) => s.theme);
  const setCompileProgress = useApp((s) => s.setCompileProgress);
  const setCompileRootPath = useApp((s) => s.setCompileRootPath);
  const activeWorkspace = useApp((s) => s.activeWorkspace);
  const packExportTarget = useApp((s) => s.packExportTarget);
  const setPackExportTarget = useApp((s) => s.setPackExportTarget);
  const [installTrPath, setInstallTrPath] = useState<string | null>(null);

  // Subscribe to compilation progress events from the background sidecar
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    onWorkspaceCompileProgress((payload) => {
      if (payload.phase === "started" || payload.phase === "booting") {
        setCompileRootPath(payload.workspace);
      }
      setCompileProgress(payload);
      if (
        payload.phase === "done" ||
        payload.phase === "failed" ||
        payload.phase === "cancelled"
      ) {
        if (payload.phase === "done" && activeWorkspace) {
          void refreshBrainSnapshotCache(activeWorkspace).catch(() => {
            // The Brain view can still refresh on demand; compile progress
            // should never fail just because the warm cache pass did.
          });
        }
        setTimeout(() => {
          setCompileProgress(null);
          setCompileRootPath(null);
        }, 3000);
      }
    }).then((un) => {
      unlisten = un;
    });
    return () => {
      unlisten?.();
    };
  }, [activeWorkspace, setCompileProgress, setCompileRootPath]);

  // Subscribe to `tr-file-opened` events emitted by the Rust side
  // when a `.tr` file is dropped on the window or routed via the
  // OS file association.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    onTrFileOpened((path) => setInstallTrPath(path)).then((un) => {
      unlisten = un;
    });
    return () => {
      unlisten?.();
    };
  }, []);

  // Re-apply theme on mount so the <html data-theme> attribute is
  // hydrated from persisted store even on first paint.
  useEffect(() => {
    const resolved =
      theme === "auto"
        ? window.matchMedia("(prefers-color-scheme: light)").matches
          ? "light"
          : "dark"
        : theme;
    document.documentElement.dataset.theme = resolved;
  }, [theme]);

  return (
    <EngineGate>
      <TooltipProvider delayDuration={250} skipDelayDuration={120}>
        <div className="flex h-full w-full flex-col bg-background text-foreground">
          <div className="flex min-h-0 min-w-0 flex-1">
            <Sidebar />
            <MainPane />
            <RightRail />
          </div>
        </div>
        <CommandPalette />
        <InstallTrSheet
          path={installTrPath}
          onClose={() => setInstallTrPath(null)}
        />
        {packExportTarget && (
          <PackExportSheet
            workspace={packExportTarget.workspace}
            branch={packExportTarget.branch}
            onClose={() => setPackExportTarget(null)}
          />
        )}
        <ToastStack />
      </TooltipProvider>
    </EngineGate>
  );
}
