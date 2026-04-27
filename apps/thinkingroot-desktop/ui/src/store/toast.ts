import { create } from "zustand";

/** Single toast sent by any component. */
export interface Toast {
  id: number;
  kind: "info" | "success" | "warn" | "error";
  title: string;
  body?: string;
  /** Milliseconds before auto-dismiss. 0 = sticky. */
  durationMs: number;
}

interface ToastStore {
  items: Toast[];
  push: (t: Omit<Toast, "id">) => void;
  dismiss: (id: number) => void;
}

let nextId = 1;

export const useToasts = create<ToastStore>((set, get) => ({
  items: [],
  push: (t) => {
    const id = nextId++;
    const toast: Toast = { ...t, id };
    set((s) => ({ items: [...s.items, toast] }));
    if (toast.durationMs > 0) {
      setTimeout(() => get().dismiss(id), toast.durationMs);
    }
  },
  dismiss: (id) => set((s) => ({ items: s.items.filter((t) => t.id !== id) })),
}));

/**
 * Convenience entry point — bypass React hooks so components and
 * command handlers can call it from any context.
 */
export function toast(
  title: string,
  opts: Partial<Omit<Toast, "id" | "title">> = {},
) {
  useToasts.getState().push({
    kind: opts.kind ?? "info",
    title,
    body: opts.body,
    durationMs: opts.durationMs ?? 3500,
  });
}
