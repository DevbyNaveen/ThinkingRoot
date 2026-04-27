import { useEffect } from "react";

/**
 * Register a global keyboard shortcut. Honours `data-hotkey-capture`
 * on focused inputs (allows forms to intercept specific chords).
 *
 * `combo` examples: "mod+k", "mod+shift+s", "esc".
 *   `mod` → Cmd on macOS, Ctrl elsewhere.
 */
export function useHotkey(combo: string, handler: (e: KeyboardEvent) => void) {
  useEffect(() => {
    const parts = combo.toLowerCase().split("+").map((p) => p.trim());
    const want = {
      mod: parts.includes("mod"),
      shift: parts.includes("shift"),
      alt: parts.includes("alt"),
      key: parts[parts.length - 1] ?? "",
    };
    const isMac =
      typeof navigator !== "undefined" &&
      /Mac|iPhone|iPad/i.test(navigator.platform);

    const onKey = (e: KeyboardEvent) => {
      const mod = isMac ? e.metaKey : e.ctrlKey;
      if (Boolean(want.mod) !== mod) return;
      if (Boolean(want.shift) !== e.shiftKey) return;
      if (Boolean(want.alt) !== e.altKey) return;
      if (e.key.toLowerCase() !== want.key) return;
      handler(e);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [combo, handler]);
}
