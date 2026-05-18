import { create } from "zustand";
import type { DropOutcome } from "@/lib/tauri";

export type FileDropZoneState =
  | { kind: "idle" }
  | { kind: "ingesting"; count: number }
  | { kind: "compiling"; outcome: DropOutcome }
  | { kind: "done"; outcome: DropOutcome; compiledOk: boolean }
  | { kind: "error"; message: string };

interface FileDropStore {
  dragOverlay: boolean;
  setDragOverlay: (active: boolean) => void;
  zoneState: FileDropZoneState;
  setZoneState: (state: FileDropZoneState) => void;
}

export const useFileDropStore = create<FileDropStore>((set) => ({
  dragOverlay: false,
  setDragOverlay: (dragOverlay) => set({ dragOverlay }),
  zoneState: { kind: "idle" },
  setZoneState: (zoneState) => set({ zoneState }),
}));
