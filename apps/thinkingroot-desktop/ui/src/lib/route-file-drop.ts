import { emitComposerFileDrop } from "@/lib/format-dropped-paths";
import { playgroundDrop, workspaceCompile, type DropOutcome } from "@/lib/tauri";
import { useApp } from "@/store/app";
import { useFileDropStore, type FileDropZoneState } from "@/store/file-drop";
import { toast } from "@/store/toast";

function summariseOutcome(o: DropOutcome): string {
  const parts: string[] = [];
  if (o.copied > 0) parts.push(`${o.copied} added`);
  if (o.skipped_duplicate > 0) parts.push(`${o.skipped_duplicate} duplicate skipped`);
  if (o.skipped_unreadable > 0) parts.push(`${o.skipped_unreadable} unreadable skipped`);
  if (parts.length === 0) return "Nothing to add";
  return parts.join(", ");
}

async function runPlaygroundIngest(
  workspace: string,
  paths: string[],
  setZoneState: (s: FileDropZoneState) => void,
) {
  setZoneState({ kind: "ingesting", count: paths.length });
  try {
    const outcome = await playgroundDrop(workspace, paths);
    if (outcome.copied === 0) {
      setZoneState({ kind: "done", outcome, compiledOk: true });
      toast(summariseOutcome(outcome), { kind: "info", durationMs: 5000 });
      return;
    }
    setZoneState({ kind: "compiling", outcome });
    let compiledOk = true;
    try {
      await workspaceCompile({ target: workspace });
    } catch {
      compiledOk = false;
    }
    setZoneState({ kind: "done", outcome, compiledOk });
    toast(
      `${summariseOutcome(outcome)}${compiledOk ? " · compile finished" : " · compile failed (run manually)"}`,
      { kind: compiledOk ? "success" : "warn", durationMs: 6000 },
    );
  } catch (e) {
    const message = e instanceof Error ? e.message : String(e);
    setZoneState({ kind: "error", message });
    toast("Drop failed", { kind: "error", body: message });
  }
}

/** Route a window-level file drop by active surface. */
export async function routeDesktopFileDrop(paths: string[]): Promise<void> {
  if (paths.length === 0) return;

  const surface = useApp.getState().surface;
  const workspace = useApp.getState().activeWorkspace;
  const setZoneState = useFileDropStore.getState().setZoneState;

  if (surface === "chats") {
    emitComposerFileDrop(paths);
    const n = paths.length;
    toast(n === 1 ? "Added file to message" : `Added ${n} files to message`, {
      kind: "success",
      durationMs: 2800,
    });
    return;
  }

  if (!workspace) {
    toast("No active workspace", {
      kind: "warn",
      body: "Pick a workspace before dropping files.",
    });
    return;
  }

  await runPlaygroundIngest(workspace, paths, setZoneState);
}
