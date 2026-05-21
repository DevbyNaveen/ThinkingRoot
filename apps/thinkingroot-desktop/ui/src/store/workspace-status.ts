/**
 * Slice 0 — unified workspace-status store.
 *
 * Pre-Slice-0 every UI surface (right-rail badge, chat banner,
 * pack-export warning, MCP TOOLS anchor, sidebar dot, command-palette
 * compile state) made its own Tauri call to a different backend probe
 * — and could legitimately disagree on the same screen. This store
 * collapses all of them to one subscription on the daemon's
 * `/api/v1/workspaces/{name}/status/stream` SSE endpoint, mirrored
 * into Zustand by the `workspace_status:{name}` Tauri event.
 *
 * # Rules of use
 *
 * - **One subscription per workspace**, kept alive while it is the
 *   active workspace. {@link useWorkspaceStatusSubscription} mounts
 *   the listener; un-mounting cancels the daemon-side subscriber on
 *   the next workspace switch.
 * - **Read via selectors, never re-probe.** Hooks like
 *   {@link useWorkspaceStatus} and {@link useReadiness} return slices
 *   of the cached snapshot. There is no `refetch()` — refresh is via
 *   the `workspace_status_refresh` Tauri command, which forces a
 *   server-side re-probe and returns the new snapshot through the
 *   same SSE stream.
 * - **Diagnostics power UI text.** Each false readiness flag carries
 *   one or more {@link Diagnostic}s with a stable `code` and
 *   actionable `actions[]`. UIs render the diagnostic message
 *   verbatim — no per-view warning strings, no fragile string
 *   matching.
 * - **Staleness is visible.** {@link useWorkspaceConnection} returns
 *   `{ connected: boolean; lastSeenMs: number | null }` so views can
 *   show "(disconnected — last seen 23s ago)" without inspecting
 *   transport state.
 */

import { useEffect, useMemo } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { create } from "zustand";

// ─── Types — mirror thinkingroot-core::types::workspace_status ──────

export type SubstrateState =
  | { kind: "absent" }
  | { kind: "empty"; graph_db_bytes: number }
  | {
      kind: "populated";
      graph_db_bytes: number;
      claim_count: number;
      entity_count: number;
      source_count_at_last_compile: number;
    }
  | { kind: "orphaned"; workspace_root: string }
  | { kind: "corrupt"; reason: string };

export type SourcesState =
  | { kind: "none" }
  | {
      kind: "some";
      file_count: number;
      total_bytes: number;
      last_changed_at: string | null;
      fingerprint_match: boolean;
    };

export type MountState =
  | { kind: "not_mounted" }
  | { kind: "mounting" }
  | { kind: "mounted"; since: string }
  | { kind: "failed"; reason: string; at: string };

export type LlmState =
  | { kind: "unconfigured" }
  | { kind: "configured"; provider: string; model: string | null }
  | {
      kind: "healthy";
      provider: string;
      model: string | null;
      last_probed_at: string;
    }
  | {
      kind: "unreachable";
      provider: string;
      reason: string;
      last_probed_at: string;
    };

export interface CompileProgress {
  sources_done: number;
  sources_total: number;
  detail: string | null;
}

export type CompileOutcome =
  | {
      kind: "success";
      extracted_claims: number;
      sources_processed: number;
    }
  | {
      kind: "partial";
      extracted_claims: number;
      failed_batches: number;
      summary: string;
    }
  | { kind: "failed"; phase: string; reason: string }
  | { kind: "cancelled"; phase: string };

export type CompileState =
  | {
      kind: "idle";
      last_finished_at: string | null;
      last_duration_ms: number | null;
      last_outcome: CompileOutcome | null;
    }
  | {
      kind: "running";
      phase: string;
      progress: CompileProgress | null;
      started_at: string;
    }
  | { kind: "cancelling"; since: string }
  | { kind: "queued"; since: string; reason: string };

export interface BranchState {
  current: string;
  modified: boolean;
}

export interface Readiness {
  for_compile: boolean;
  for_query: boolean;
  for_chat: boolean;
  for_export: boolean;
  for_publish: boolean;
}

export type DiagnosticSeverity = "info" | "warn" | "error";

/** Stable machine-readable diagnostic codes. Keep in sync with
 * `thinkingroot-core::types::workspace_status::diagnostic_codes`. */
export const DIAGNOSTIC_CODES = {
  NO_SUBSTRATE: "no_substrate",
  EMPTY_SUBSTRATE: "empty_substrate",
  NO_SOURCES: "no_sources",
  ORPHANED: "orphaned",
  CORRUPT: "corrupt",
  NOT_MOUNTED: "not_mounted",
  MOUNT_FAILED: "mount_failed",
  NO_PROVIDER: "no_provider",
  PROVIDER_UNREACHABLE: "provider_unreachable",
  PROVIDER_STALE: "provider_stale",
  COMPILE_RUNNING: "compile_running",
  COMPILE_FAILED: "compile_failed",
  COMPILE_PARTIAL: "compile_partial",
  SOURCES_STALE: "sources_stale",
  BRANCH_DIRTY: "branch_dirty",
  PATH_MISSING: "path_missing",
} as const;

export type DiagnosticCode = (typeof DIAGNOSTIC_CODES)[keyof typeof DIAGNOSTIC_CODES];

export interface DiagnosticAction {
  id: string;
  label: string;
}

export interface Diagnostic {
  code: string;
  severity: DiagnosticSeverity;
  message: string;
  blocks: string[];
  actions: DiagnosticAction[];
}

export interface WorkspaceStatus {
  name: string;
  path: string;
  as_of: string;
  substrate: SubstrateState;
  sources: SourcesState;
  mount: MountState;
  llm: LlmState;
  compile: CompileState;
  branch: BranchState;
  readiness: Readiness;
  diagnostics: Diagnostic[];
}

interface ConnectionState {
  /** True iff the SSE stream is currently attached. */
  connected: boolean;
  /** Wall-clock ms when the last snapshot or heartbeat arrived;
   * `null` until the first event lands. UIs age this against
   * `Date.now()` to surface "disconnected — last seen Xs ago". */
  lastSeenMs: number | null;
  /** Last failure reason from the subscriber, when disconnected. */
  reason: string | null;
}

/** Stable fallback for Zustand selectors — never return a fresh `{}` from
 * `useWorkspaceStatusStore` or `useSyncExternalStore` will see a new
 * snapshot every render and hit "Maximum update depth exceeded". */
const DEFAULT_CONNECTION_STATE: ConnectionState = Object.freeze({
  connected: false,
  lastSeenMs: null,
  reason: null,
});

// ─── Store ───────────────────────────────────────────────────────────

interface WorkspaceStatusStore {
  /** Cached snapshots, keyed by workspace name. */
  byName: Map<string, WorkspaceStatus>;
  /** Connection state per workspace. */
  connections: Map<string, ConnectionState>;
  /** Internal: replace the cached snapshot. */
  _set: (name: string, status: WorkspaceStatus) => void;
  /** Internal: update connection state. */
  _setConnection: (name: string, partial: Partial<ConnectionState>) => void;
  /** Internal: bump last-seen on heartbeat without changing other state. */
  _markSeen: (name: string) => void;
}

const useWorkspaceStatusStore = create<WorkspaceStatusStore>((set) => ({
  byName: new Map(),
  connections: new Map(),
  _set: (name, status) =>
    set((s) => {
      const next = new Map(s.byName);
      next.set(name, status);
      const conns = new Map(s.connections);
      const prev = conns.get(name);
      conns.set(name, {
        connected: prev?.connected ?? true,
        lastSeenMs: Date.now(),
        reason: prev?.reason ?? null,
      });
      return { byName: next, connections: conns };
    }),
  _setConnection: (name, partial) =>
    set((s) => {
      const conns = new Map(s.connections);
      const prev = conns.get(name) ?? {
        connected: false,
        lastSeenMs: null,
        reason: null,
      };
      conns.set(name, { ...prev, ...partial });
      return { connections: conns };
    }),
  _markSeen: (name) =>
    set((s) => {
      const conns = new Map(s.connections);
      const prev = conns.get(name) ?? {
        connected: true,
        lastSeenMs: null,
        reason: null,
      };
      conns.set(name, { ...prev, lastSeenMs: Date.now(), connected: true });
      return { connections: conns };
    }),
}));

// ─── Hooks ───────────────────────────────────────────────────────────

/** Read the current snapshot for a workspace. `null` until the first
 * SSE snapshot lands. */
export function useWorkspaceStatus(name: string | null): WorkspaceStatus | null {
  return useWorkspaceStatusStore((s) =>
    name ? s.byName.get(name) ?? null : null,
  );
}

/** Read the readiness flags for a workspace. Convenience over
 * {@link useWorkspaceStatus} for views that only care about the gate. */
export function useReadiness(name: string | null): Readiness | null {
  const status = useWorkspaceStatus(name);
  return status?.readiness ?? null;
}

/** Read every diagnostic that blocks the named readiness flag. The
 * UI typically renders these as separate banners. */
export function useDiagnosticsFor(
  name: string | null,
  forFlag: keyof Readiness,
): Diagnostic[] {
  const status = useWorkspaceStatus(name);
  return useMemo(() => {
    if (!status) return [];
    return status.diagnostics.filter((d) => d.blocks.includes(forFlag));
  }, [status, forFlag]);
}

/** Look up the first diagnostic with a given code, or `null`. Useful
 * for views that want to render a specific affordance (e.g. the
 * mount banner only cares about `not_mounted`). */
export function useDiagnostic(
  name: string | null,
  code: string,
): Diagnostic | null {
  const status = useWorkspaceStatus(name);
  return useMemo(() => {
    if (!status) return null;
    return status.diagnostics.find((d) => d.code === code) ?? null;
  }, [status, code]);
}

/** Read the connection state for a workspace. Use this to surface
 * "(disconnected)" / "stale" hints across the UI. */
export function useWorkspaceConnection(name: string | null): ConnectionState {
  return useWorkspaceStatusStore((s) => {
    if (!name) return DEFAULT_CONNECTION_STATE;
    return s.connections.get(name) ?? DEFAULT_CONNECTION_STATE;
  });
}

/**
 * Mount this once for the active workspace. Subscribes to the
 * daemon's SSE stream via the Tauri command and registers Tauri
 * event listeners for snapshots, heartbeats, and connection state.
 *
 * The hook is idempotent — switching workspaces calls
 * `subscribe_workspace_status_stream` again, which the desktop side
 * uses as a "swap to this workspace, cancel the previous subscriber"
 * signal.
 *
 * Cleanup unlistens both Tauri events. The desktop-side subscriber
 * keeps running; a subsequent `subscribe_workspace_status_stream`
 * with a different (or same) name takes over cleanly.
 */
export function useWorkspaceStatusSubscription(name: string | null): void {
  useEffect(() => {
    if (!name) return;
    let unsnap: UnlistenFn | null = null;
    let unhb: UnlistenFn | null = null;
    let unconn: UnlistenFn | null = null;
    let cancelled = false;

    void invoke<void>("subscribe_workspace_status_stream", { workspace: name }).catch(
      (e) => {
        useWorkspaceStatusStore.getState()._setConnection(name, {
          connected: false,
          reason: typeof e === "string" ? e : String(e),
        });
      },
    );

    void listen<WorkspaceStatus>(`workspace_status:${name}`, (event) => {
      if (cancelled) return;
      useWorkspaceStatusStore.getState()._set(name, event.payload);
    }).then((u) => {
      if (cancelled) {
        u();
      } else {
        unsnap = u;
      }
    });

    void listen<{ at: string }>(`workspace_status_heartbeat:${name}`, () => {
      if (cancelled) return;
      useWorkspaceStatusStore.getState()._markSeen(name);
    }).then((u) => {
      if (cancelled) {
        u();
      } else {
        unhb = u;
      }
    });

    void listen<{ connected: boolean; reason: string | null }>(
      `workspace_status_connection:${name}`,
      (event) => {
        if (cancelled) return;
        useWorkspaceStatusStore
          .getState()
          ._setConnection(name, {
            connected: event.payload.connected,
            reason: event.payload.reason,
            ...(event.payload.connected ? { lastSeenMs: Date.now() } : {}),
          });
      },
    ).then((u) => {
      if (cancelled) {
        u();
      } else {
        unconn = u;
      }
    });

    // Pull the current cached snapshot (the desktop process may have
    // already received an event before this hook mounted).
    void invoke<WorkspaceStatus | null>("workspace_status_get", {
      workspace: name,
    }).then((snap) => {
      if (cancelled || !snap) return;
      useWorkspaceStatusStore.getState()._set(name, snap);
    });

    return () => {
      cancelled = true;
      unsnap?.();
      unhb?.();
      unconn?.();
    };
  }, [name]);
}

/** Trigger a server-side re-probe of the workspace's on-disk axes.
 * Returns the fresh snapshot — also written to the cache so other
 * subscribers see it. */
export async function refreshWorkspaceStatus(
  name: string,
): Promise<WorkspaceStatus> {
  const next = await invoke<WorkspaceStatus>("workspace_status_refresh", {
    workspace: name,
  });
  useWorkspaceStatusStore.getState()._set(name, next);
  return next;
}

// ─── Pure helpers (testable, no React) ───────────────────────────────

/** Neutral chip surface for substrate / readiness labels (no traffic-light fills). */
export const SUBSTRATE_BADGE_SURFACE_CLASS =
  "rounded-md border border-border/50 bg-muted/[0.06] font-medium text-muted-foreground/90 shadow-none";

/** Human-readable substrate badge label. UIs render this directly.
 *
 * **Compile state takes precedence over substrate state when a compile
 * is in flight.** Pre-fix the badge only consulted `substrate.kind`, so
 * during a long-running recompile (substrate is still `populated` from
 * the *previous* compile until the new run commits) the badge claimed
 * "Up to date" while a fresh compile was actively rewriting the graph
 * — that's exactly the dishonest UX CLAUDE.md §honesty-rule §7 forbids
 * ("The desktop never claims something synced when it didn't"). When
 * `compile.kind === "running" | "cancelling"`, surface that fact in
 * the badge; substrate-derived labels apply only when the engine is
 * idle.
 *
 * **Source-tree freshness also overrides "Up to date".** When the
 * substrate is `populated` but the daemon's source-tree watcher has
 * flipped `sources.fingerprint_match` to `false` (file edited /
 * added / removed since the last successful compile), the substrate
 * is by definition behind the user's files. Returning "Up to date"
 * in that state was the central lie of the pre-2026-05-20 UX. We
 * surface "Behind" with `tone: "warn"` so the user knows a recompile
 * is needed before chat/query results reflect their edits. */
export function substrateBadge(
  status: WorkspaceStatus | null,
): { label: string; tone: "ok" | "info" | "warn" | "error" | "muted" } {
  if (!status) return { label: "Loading", tone: "muted" };
  if (status.compile.kind === "running") {
    return { label: "Compiling", tone: "info" };
  }
  if (status.compile.kind === "queued") {
    return { label: "Syncing soon", tone: "info" };
  }
  if (status.compile.kind === "cancelling") {
    return { label: "Stopping", tone: "warn" };
  }
  switch (status.substrate.kind) {
    case "absent":
      return { label: "Behind", tone: "warn" };
    case "empty":
      return { label: "Behind", tone: "warn" };
    case "populated":
      if (
        status.sources.kind === "some" &&
        status.sources.fingerprint_match === false
      ) {
        return { label: "Behind", tone: "warn" };
      }
      return { label: "Up to date", tone: "ok" };
    case "orphaned":
      return { label: "Behind", tone: "error" };
    case "corrupt":
      return { label: "Corrupt", tone: "error" };
  }
}

/** Should the export dialog enable its primary button? Pure
 * derivation; lets tests pin the contract. */
export function exportButtonEnabled(status: WorkspaceStatus | null): boolean {
  return status?.readiness.for_export ?? false;
}

/** Should the chat compose box accept a send? Pure derivation. */
export function chatComposerEnabled(status: WorkspaceStatus | null): boolean {
  return status?.readiness.for_chat ?? false;
}

/** Pick the primary diagnostic to surface for a given readiness flag.
 * Picks the highest-severity one (error > warn > info) breaking ties
 * by source order. */
export function pickPrimaryDiagnostic(
  status: WorkspaceStatus | null,
  forFlag: keyof Readiness,
): Diagnostic | null {
  if (!status) return null;
  const candidates = status.diagnostics.filter((d) =>
    d.blocks.includes(forFlag),
  );
  if (candidates.length === 0) return null;
  const order: DiagnosticSeverity[] = ["error", "warn", "info"];
  for (const sev of order) {
    const hit = candidates.find((d) => d.severity === sev);
    if (hit) return hit;
  }
  return candidates[0] ?? null;
}

/** Test-only: reset the store. Not exported in the public surface. */
export function _resetWorkspaceStatusStore(): void {
  useWorkspaceStatusStore.setState({
    byName: new Map(),
    connections: new Map(),
  });
}
