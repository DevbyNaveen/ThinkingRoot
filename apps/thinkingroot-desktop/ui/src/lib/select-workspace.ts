/**
 * The single canonical "switch to workspace X" verb.
 *
 * Every place the user changes active workspace must funnel through
 * this helper. Three steps, atomic from the caller's perspective:
 *
 *   1. Optimistically update the UI store so the surface re-renders
 *      against the new name immediately (no jarring lag while the
 *      daemon-mount round-trip is in flight).
 *   2. Await `workspace_set_active` on the Rust side. That updates the
 *      on-disk registry's `active` flag AND emits `workspaces-changed`
 *      so peer surfaces (sidebar, settings) re-fetch their lists.
 *   3. If the daemon call fails, roll the UI store back to the prior
 *      value and rethrow so the caller can show a toast.
 *
 * Why this exists: before, every call site re-implemented the
 * (UI-update, daemon-sync, rollback-on-failure) trio by hand. Some
 * forgot the daemon-sync entirely (e.g. `onSelectConv` in Sidebar.tsx),
 * which made right-panel readouts that read from the daemon's notion of
 * "active workspace" (notably `workspace_readme`) silently return data
 * for the *previous* workspace. Funneling through one helper makes
 * that class of bug impossible.
 *
 * Callers should `await` and `try/catch` to surface errors as toasts;
 * passing a successful return value is intentional (Promise<void>).
 */
import { workspaceSetActive } from "@/lib/tauri";
import { useApp } from "@/store/app";

export async function selectWorkspace(name: string): Promise<void> {
  const prior = useApp.getState().activeWorkspace;
  // Step 1 — optimistic UI update so dependent panels re-render now.
  useApp.getState().setActiveWorkspace(name);
  try {
    // Step 2 — converge daemon-side state with the UI's view of
    // "active". Without this, server-side reads (workspace_readme,
    // anything else built on `SidecarClient::ensure_active`) return
    // results for the previously-mounted workspace.
    await workspaceSetActive(name);
  } catch (e) {
    // Step 3 — restore the prior UI state so the surface doesn't show
    // a workspace that isn't actually mounted on the daemon side.
    useApp.getState().setActiveWorkspace(prior);
    throw e;
  }
}
