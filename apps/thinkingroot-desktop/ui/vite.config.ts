import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { fileURLToPath, URL } from "node:url";

// Tauri expects a fixed port; kill previous process rather than
// bouncing to an alternate port silently.
const HOST = process.env.TAURI_DEV_HOST;

// https://vite.dev/config/
export default defineConfig(async () => ({
  plugins: [react()],
  resolve: {
    alias: {
      "@": fileURLToPath(new URL("./src", import.meta.url)),
    },
  },
  // Pre-bundle Tauri plugins up front. Without this, adding a new
  // `@tauri-apps/plugin-*` import (e.g. `plugin-opener` for the
  // Browser panel) can leave the dev client holding a stale
  // `?v=…` URL to `node_modules/.vite/deps/*`, which Vite answers
  // with **504 Outdated Optimize Dep** and a black screen until a
  // hard refresh. `include` forces these entries into the first
  // optimize pass so the dependency graph stays stable across HMR.
  optimizeDeps: {
    include: [
      "@tauri-apps/api",
      "@tauri-apps/plugin-clipboard-manager",
      "@tauri-apps/plugin-dialog",
      "@tauri-apps/plugin-notification",
      "@tauri-apps/plugin-opener",
      "@tauri-apps/plugin-os",
      "@tauri-apps/plugin-window-state",
    ],
  },
  // Prevent vite from obscuring rust errors
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: HOST || false,
    hmr: HOST
      ? { protocol: "ws", host: HOST, port: 1421 }
      : undefined,
    watch: {
      ignored: ["**/src-tauri/**"],
    },
  },
}));
