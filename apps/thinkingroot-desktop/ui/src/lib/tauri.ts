/**
 * Typed wrappers around `@tauri-apps/api` `invoke()`. Keeps the
 * command surface discoverable from TypeScript — a single source of
 * truth the components import instead of typing `invoke("chat_send", …)`
 * by hand.
 */
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

// ─── Meta ────────────────────────────────────────────────────────────

export interface Versions {
  app: string;
  runtime: string;
  providers: string;
  trace: string;
  types: string;
}

export async function appVersion(): Promise<Versions> {
  return invoke<Versions>("app_version");
}

export async function appQuit(): Promise<void> {
  return invoke("app_quit");
}

// ─── Memory / Brain ──────────────────────────────────────────────────

export interface ClaimRow {
  id: string;
  tier: "rooted" | "attested" | "unknown";
  confidence: number;
  statement: string;
  source: string;
  claim_type?: string;
}

export async function memoryList(filter?: string): Promise<ClaimRow[]> {
  return invoke<ClaimRow[]>("memory_list", { filter: filter ?? null });
}

export interface BrainEntity {
  name: string;
  entity_type: string;
  claim_count: number;
}

export interface BrainRelation {
  source: string;
  target: string;
  relation_type: string;
  strength: number;
}

export interface BrainSnapshot {
  claims: ClaimRow[];
  entities: BrainEntity[];
  relations: BrainRelation[];
  rooted_ids: string[];
}

export async function brainLoad(): Promise<BrainSnapshot> {
  return invoke<BrainSnapshot>("brain_load");
}

// ─── Settings / config (typed, shared with the CLI) ─────────────────
//
// One config home, structured schemas, no flat env-var soup. See
// `apps/.../src-tauri/src/commands/settings.rs` for the Rust mirror.

/** Where each config file lives on disk. */
export interface ConfigPaths {
  config_path?: string | null;
  credentials_path?: string | null;
  workspaces_path?: string | null;
  desktop_path?: string | null;
}

export async function configPaths(): Promise<ConfigPaths> {
  return invoke<ConfigPaths>("config_paths");
}

export interface AzureProviderView {
  configured: boolean;
  resource_name?: string | null;
  endpoint_base?: string | null;
  deployment?: string | null;
  api_version?: string | null;
  api_key_env?: string | null;
  api_key_env_present: boolean;
}

export interface GenericProviderView {
  configured: boolean;
  api_key_env?: string | null;
  api_key_env_present: boolean;
  base_url?: string | null;
  default_model?: string | null;
}

export interface GlobalLlmConfig {
  default_provider: string;
  extraction_model: string;
  compilation_model: string;
  max_concurrent_requests: number;
  request_timeout_secs: number;
  azure: AzureProviderView;
  /** Keyed by provider name — e.g. "openai", "anthropic". */
  providers: Record<string, GenericProviderView>;
}

export async function globalConfigRead(): Promise<GlobalLlmConfig> {
  return invoke<GlobalLlmConfig>("global_config_read");
}

export interface GlobalLlmWriteArgs {
  default_provider?: string | null;
  extraction_model?: string | null;
  compilation_model?: string | null;
  max_concurrent_requests?: number | null;
  request_timeout_secs?: number | null;
  azure?: {
    resource_name?: string | null;
    endpoint_base?: string | null;
    deployment?: string | null;
    api_version?: string | null;
    api_key_env?: string | null;
  } | null;
}

export async function globalConfigWrite(
  args: GlobalLlmWriteArgs,
): Promise<string> {
  return invoke<string>("global_config_write", { args });
}

/** One credential slot. The value never crosses the IPC boundary —
 *  the UI only learns whether it is set or not. */
export interface CredentialRow {
  env_var: string;
  persisted: boolean;
  in_process_env: boolean;
}

export async function credentialsStatus(): Promise<CredentialRow[]> {
  return invoke<CredentialRow[]>("credentials_status");
}

export async function credentialsSet(
  envVar: string,
  value: string,
): Promise<void> {
  return invoke<void>("credentials_set", {
    args: { env_var: envVar, value },
  });
}

export async function credentialsRemove(envVar: string): Promise<void> {
  return invoke<void>("credentials_remove", { args: { env_var: envVar } });
}

// ─── First-run setup (install manifest `setup_complete_at`) ──────────
//
// Mirrors `get_setup_complete_at` / `mark_setup_complete` in
// `apps/.../src-tauri/src/commands/settings.rs`. The wizard variant of
// `EngineGate` reads the timestamp on mount to decide between the
// friendly first-launch flow and the standard "engine unavailable"
// blocking panel; it calls `markSetupComplete()` once all
// setup-relevant checks turn `ok` (or when the user hits "Skip for
// now").

/** ISO-8601 timestamp when first-run setup completed, or `null` if
 *  the user hasn't yet finished the wizard. */
export async function getSetupCompleteAt(): Promise<string | null> {
  return invoke<string | null>("get_setup_complete_at");
}

/** Stamp `setup_complete_at = now()` on the install manifest.
 *  Idempotent — re-marking is a no-op overwrite. */
export async function markSetupComplete(): Promise<void> {
  return invoke<void>("mark_setup_complete");
}

// ─── Filesystem (file tree on Brain) ─────────────────────────────────

export type FsEntryKind = "directory" | "file" | "symlink";

export interface FsEntry {
  name: string;
  path: string;
  kind: FsEntryKind;
  has_children: boolean;
  size: number | null;
}

export async function fsListDir(path: string): Promise<FsEntry[]> {
  return invoke<FsEntry[]>("fs_list_dir", { args: { path } });
}

export interface FsReadTextBody {
  content: string;
  had_invalid_utf8: boolean;
  size: number;
}

/** Preview a file under a registered workspace (≤ 512 KiB). */
export async function fsReadText(path: string): Promise<FsReadTextBody> {
  return invoke<FsReadTextBody>("fs_read_text", { args: { path } });
}

// ─── Git branches (informational sidebar) ────────────────────────────

export type BranchKind = "local" | "remote";

export interface BranchInfo {
  name: string;
  kind: BranchKind;
  current: boolean;
  remote: string | null;
}

export async function gitBranches(path: string): Promise<BranchInfo[]> {
  return invoke<BranchInfo[]>("git_branches", { args: { path } });
}

// ─── `.tr` install preview ──────────────────────────────────────────

export type Verdict =
  | { kind: "verified"; tier: "T0" | "T1" | "T2" | "T3" | "T4"; author_id: string | null; sigstore_log_index: number | null; revocation_freshness_secs: number }
  | { kind: "unsigned" }
  | { kind: "tampered"; what: "manifest_hash_mismatch" | "archive_corrupt" | "signature_payload_mismatch"; expected?: string; actual?: string }
  | { kind: "revoked"; advisory: { reason?: string; published_at?: string } }
  | { kind: "key_unknown"; key_id: string }
  | { kind: "stale_cache"; age_days: number }
  | { kind: "unsupported"; tier: string; reason: string };

export interface InstallPreview {
  path: string;
  name: string;
  version: string;
  license: string;
  trust_tier: string;
  markdown: string;
  manifest_table: string;
  source_count: number;
  entry_count: number;
  payload_bytes: number;
  verdict: Verdict;
}

export async function installTrFile(path: string): Promise<InstallPreview> {
  return invoke<InstallPreview>("install_tr_file", { path });
}

export function onTrFileOpened(
  handler: (path: string) => void,
): Promise<UnlistenFn> {
  return listen<string>("tr-file-opened", (e) => handler(e.payload));
}

/** Tauri emits `playground-files-dropped` with the array of every
 * non-`.tr` OS-level path in a drag-drop. The Playground DropZone
 * is the sole consumer today; the lib.rs window-event handler
 * filters `.tr` paths off to the install sheet first. */
export function onPlaygroundFilesDropped(
  handler: (paths: string[]) => void,
): Promise<UnlistenFn> {
  return listen<string[]>("playground-files-dropped", (e) => handler(e.payload));
}

// ─── Pack export (Slice 9) ───────────────────────────────────────────

export interface PackEstimate {
  compiled: boolean;
  name: string;
  version: string;
  license: string | null;
  description: string | null;
  source_bytes: number;
  source_files: number;
}

export async function packEstimate(workspace: string): Promise<PackEstimate> {
  return invoke<PackEstimate>("pack_estimate", { workspace });
}

export interface PackExportRequest {
  workspace: string;
  out_path: string;
  name?: string | null;
  version?: string | null;
  license?: string | null;
  description?: string | null;
  sign_keyless?: boolean;
  branch?: string | null;
}

export interface PackExportResult {
  out_path: string;
  bytes: number;
  pack_hash: string;
  trust_tier: string;
  warnings: string[];
  stdout_log: string;
  stderr_log: string;
}

export async function packExport(
  req: PackExportRequest,
): Promise<PackExportResult> {
  return invoke<PackExportResult>("pack_export", { req });
}

// ─── Doctor (Slice 1 desktop wiring) ─────────────────────────────────

export interface DoctorReport {
  verdict: "ok" | "degraded" | "broken";
  raw_json: string;
  stderr_log: string;
  exit_code: number;
}

export async function doctorRun(repair: boolean): Promise<DoctorReport> {
  return invoke<DoctorReport>("doctor_run", { repair });
}

/** Shut down and respawn the bundled `root serve` sidecar (cortex singleton). */
export async function sidecarRestart(): Promise<string> {
  return invoke<string>("sidecar_restart");
}

// ─── Watchdog circuit breaker (Slice F T2/T3) ────────────────────────
//
// Mirrors the Tauri commands added in
// `apps/.../src-tauri/src/agent_runtime_subprocess.rs` (T3). The
// watchdog auto-restarts the daemon with exponential backoff; if it
// trips the breaker (too many consecutive failures), the engine
// surfaces a `daemon.restart.exhausted` doctor row and the UI offers a
// manual reset button. `until_rfc3339` is null when the breaker is not
// active.

export interface CircuitBreakerStatus {
  active: boolean;
  until_rfc3339: string | null;
  recent_failure_count: number;
  recent_crash_signal_count: number;
}

export async function getCircuitBreakerStatus(): Promise<CircuitBreakerStatus> {
  return invoke<CircuitBreakerStatus>("get_circuit_breaker_status");
}

export async function resetCircuitBreaker(): Promise<void> {
  return invoke<void>("reset_circuit_breaker");
}

// ─── Privacy dashboard ───────────────────────────────────────────────

export interface PrivacySource {
  id: string;
  uri: string;
  source_type: string;
}

export interface PrivacySummary {
  workspace: string;
  sources: PrivacySource[];
  source_count: number;
  claim_count: number;
  entity_count: number;
}

export async function privacySummary(): Promise<PrivacySummary> {
  return invoke<PrivacySummary>("privacy_summary");
}

export async function privacyForget(sourceUri: string): Promise<number> {
  return invoke<number>("privacy_forget", { sourceUri });
}

// ─── Local MCP ───────────────────────────────────────────────────────

export interface McpStatus {
  host: string;
  port: number;
  pid: number | null;
  running: boolean;
  well_known_url: string;
  sse_url: string;
}

export async function mcpStatus(): Promise<McpStatus> {
  return invoke<McpStatus>("mcp_status");
}

export type McpToolKey =
  | "claude-desktop"
  | "cursor"
  | "windsurf"
  | "cline"
  | "zed"
  | "vs-code"
  | "claude-code"
  | "gemini-cli"
  | "codex";

export async function mcpGetConfigSnippet(tool: McpToolKey): Promise<string> {
  return invoke<string>("mcp_get_config_snippet", { tool });
}

export interface McpConfigureResult {
  tool: string;
  path: string;
  restart_required: boolean;
}

export async function mcpConfigureTool(
  tool: McpToolKey,
): Promise<McpConfigureResult> {
  return invoke<McpConfigureResult>("mcp_configure_tool", { tool });
}

export interface McpServerRow {
  name: string;
  transport: string;
  status: string;
  description: string | null;
}

export async function mcpListConnected(): Promise<McpServerRow[]> {
  return invoke<McpServerRow[]>("mcp_list_connected");
}

// ─── Workspaces ──────────────────────────────────────────────────────

export interface WorkspaceView {
  name: string;
  path: string;
  port: number;
  compiled: boolean;
  active: boolean;
}

export async function workspaceList(): Promise<WorkspaceView[]> {
  return invoke<WorkspaceView[]>("workspace_list");
}

export interface WorkspaceAddArgs {
  path: string;
  name?: string | null;
  port?: number | null;
}

export async function workspaceAdd(args: WorkspaceAddArgs): Promise<WorkspaceView> {
  return invoke<WorkspaceView>("workspace_add", {
    args: {
      path: args.path,
      name: args.name ?? null,
      port: args.port ?? null,
    },
  });
}

export async function workspaceRemove(name: string): Promise<boolean> {
  return invoke<boolean>("workspace_remove", { args: { name } });
}

export async function workspaceSetActive(name: string): Promise<string> {
  return invoke<string>("workspace_set_active", { args: { name } });
}

export interface WorkspaceCompileArgs {
  target: string;
  branch?: string | null;
}

export async function workspaceCompile(args: WorkspaceCompileArgs): Promise<string> {
  return invoke<string>("workspace_compile", {
    args: {
      target: args.target,
      branch: args.branch ?? null,
    },
  });
}

/**
 * Stop the in-flight compile.  Returns `true` if a compile was running
 * and was signalled to cancel; `false` if no compile was in flight.
 * The Tauri side fires the `CancellationToken` registered in
 * `AppState.active_compile`; the pipeline exits at the next phase
 * boundary with `Error::Cancelled`.
 */
export async function workspaceCompileStop(): Promise<boolean> {
  return invoke<boolean>("workspace_compile_stop");
}

export interface CompileStatus {
  running: boolean;
  workspace: string | null;
}

/** Snapshot whether a compile is currently running (and for which workspace). */
export async function workspaceCompileStatus(): Promise<CompileStatus> {
  return invoke<CompileStatus>("workspace_compile_status");
}

/**
 * Fetch the engine-canonical workspace README markdown — the contents of
 * `<workspace>/.thinkingroot/README.md`, auto-synthesised by Phase 10 of
 * the compile pipeline. Returns an empty string when the workspace has
 * not been compiled yet (the UI renders an empty-state message rather
 * than a fabricated placeholder).
 */
export async function workspaceReadme(): Promise<string> {
  return invoke<string>("workspace_readme");
}

/**
 * Mirror of Rust `thinkingroot_core::IncrementalSummary` — the structured
 * delta surfaced at the end of every successful compile. Always populated
 * (even on the no-edits-since-last-compile early-return path), so React
 * code can render a summary panel without branching on presence.
 *
 * `bytes_re_extracted` is `bigint` because the Rust side is `u64` and
 * a multi-GiB workspace can exceed JavaScript's safe-integer range
 * (2⁵³−1 ≈ 8 PiB, but serde-json emits as a JSON number which loses
 * precision past 2⁵³); upstream serializer keeps it numeric.
 */
export interface IncrementalSummary {
  sources_total: number;
  sources_unchanged: number;
  sources_truly_changed: number;
  sources_deleted: number;
  sources_resolution_dirty: number;
  claims_added: number;
  claims_updated: number;
  claims_deleted: number;
  structural_rows_emitted: number;
  structural_rows_cascaded: number;
  bytes_re_extracted: number;
  llm_calls: number;
  cache_hits: number;
  structural_extractions: number;
  /** Per-phase wall-clock in milliseconds, keyed by canonical phase name. */
  phase_timings: Record<string, number>;
  total_elapsed_ms: number;
}

export type CompileProgress =
  | { phase: "started"; workspace: string }
  // Emitted while the desktop is waiting for the bundled `root`
  // sidecar to finish booting (livez probe).  Without this signal the
  // user clicked Compile and saw no UI activity for up to 60 s; React
  // can now render an explanatory "Waiting for engine…" state.
  | { phase: "booting"; workspace: string }
  // Sidecar is up; we've POSTed `/compile/stream` and are waiting for
  // the server-side pipeline ticker to emit its first event.  Bridges
  // the ~500ms warm / ~2s cold gap between `started` and the first
  // `tick`. The first Tick overwrites this caption within a second
  // on typical workloads.
  | { phase: "connecting"; workspace: string }
  // First attempt failed; auto-retry scheduled.  Carries the retry
  // attempt index (1-based — the user's click was attempt 0), the
  // backoff delay before the retry fires, and the first-attempt error
  // so the UI can render "Retrying after: <error>" honestly.  React
  // also uses this as the trigger to reset its monotonic-max bar
  // tracker so the retry's fresh Reading-5% tick doesn't look like a
  // backward jump.
  | {
      phase: "retrying";
      workspace: string;
      attempt: number;
      after_ms: number;
      first_error: string;
    }
  // Unified compile-progress snapshot — emitted every 250 ms by the
  // daemon ticker while a compile is live. **New UI surfaces should
  // render this as the single source of truth**; the per-phase
  // variants below are kept for back-compat only.
  //   step          — `"reading" | "extracting" | "linking" | "persisting" | "packing"`.
  //                   Step labels can re-appear within a single compile (e.g.
  //                   linking → persisting → linking) — render the current
  //                   step, never gate on "have we passed step N".
  //   step_label    — Human-readable step name (e.g. `"Linking"`).
  //   done / total  — Step-local counter. `total === 0` means
  //                   indeterminate; render a spinner with elapsed only.
  //   eta_ms        — Daemon-computed ETA for the current step.
  //                   `null` when total is 0 or done is 0.
  //   detail        — Short sub-phase caption (e.g.
  //                   "removing changed sources"). Optional; falls
  //                   back to step_label when absent. Renders in the
  //                   indeterminate-spinner caption.
  | {
      phase: "tick";
      step: "reading" | "extracting" | "linking" | "persisting" | "packing";
      step_label: string;
      done: number;
      total: number;
      step_elapsed_ms: number;
      total_elapsed_ms: number;
      eta_ms: number | null;
      detail?: string | null;
    }
  | { phase: "diff_start" }
  | { phase: "diff_complete"; changed: number; unchanged: number; deleted: number }
  | { phase: "parse_complete"; files: number }
  | { phase: "extraction_start"; total_chunks: number; total_batches: number }
  | { phase: "extraction_progress"; done: number; total: number }
  | { phase: "extraction_complete"; claims: number; entities: number }
  | { phase: "extraction_partial"; failed_batches: number; failed_chunk_ranges: Array<[number, number]> }
  | { phase: "grounding_start"; llm_claims: number; structural_claims: number }
  | { phase: "grounding_progress"; done: number; total: number }
  | { phase: "grounding_done"; accepted: number; rejected: number }
  | { phase: "fingerprint_done"; truly_changed: number; cutoffs: number }
  | { phase: "rooting_start"; candidates: number }
  | { phase: "rooting_progress"; done: number; total: number }
  | { phase: "rooting_done"; rooted: number; attested: number; quarantined: number; rejected: number }
  | { phase: "linking_start"; total_entities: number }
  | { phase: "linking_progress"; done: number; total: number }
  | { phase: "vector_progress"; done: number; total: number }
  | { phase: "vector_update_done"; entities_indexed: number; claims_indexed: number }
  | { phase: "compilation_progress"; done: number; total: number }
  | { phase: "compilation_done"; artifacts: number }
  | { phase: "verification_done"; health: number }
  | { phase: "phase_done"; name: string; elapsed_ms: number }
  | {
      phase: "done";
      files_parsed: number;
      claims: number;
      entities: number;
      relations: number;
      contradictions: number;
      artifacts: number;
      health_score: number;
      cache_dirty: boolean;
      // Carried through from PipelineResult so the result panel can
      // render a "compile finished but N batches failed" warning
      // without listening to a separate ExtractionPartial event.
      failed_batches?: number;
      failed_chunk_ranges?: Array<[number, number]>;
      // Full incremental delta (per-phase timings + claim/source/structural
      // counts).  Optional because pre-T8 daemons don't include it; renderer
      // skips the breakdown panel when undefined.
      incremental_summary?: IncrementalSummary;
    }
  | { phase: "failed"; error: string }
  // The Rust `CompileProgress::Cancelled` variant fires when the user
  // hits Stop or the SSE response is dropped mid-compile.  Without
  // this variant the union didn't model it and the App-shell
  // progress handler's `phase === "done" || phase === "failed"`
  // check left the spinner spinning forever after a successful
  // cancel.
  | { phase: "cancelled" };

export function onWorkspaceCompileProgress(
  handler: (payload: CompileProgress) => void,
): Promise<UnlistenFn> {
  return listen<CompileProgress>("workspace_compile_progress", (e) =>
    handler(e.payload),
  );
}

/** Emitted from Rust when `workspaces.toml` or post-compile graph state should reload in the UI. */
export function onWorkspacesChanged(handler: () => void): Promise<UnlistenFn> {
  return listen<boolean>("workspaces-changed", () => handler());
}

// ─── Workspace auto-scan ─────────────────────────────────────────────

export interface ScanResult {
  roots: string[];
  discovered: string[];
  registered: string[];
  total: number;
}

export async function workspaceScan(roots?: string[]): Promise<ScanResult> {
  return invoke<ScanResult>("workspace_scan", {
    args: { roots: roots ?? [] },
  });
}

// ─── Conversations ───────────────────────────────────────────────────

export interface ConversationSummary {
  id: string;
  workspace: string;
  title: string;
  created_at: string;
  updated_at: string;
  message_count: number;
}

export interface ConversationMessage {
  id: string;
  role: string;
  content: string;
  model: string | null;
  created_at: string;
  claims_used: string[];
}

export interface Conversation {
  summary: ConversationSummary;
  messages: ConversationMessage[];
}

export async function conversationsList(workspace?: string): Promise<ConversationSummary[]> {
  return invoke<ConversationSummary[]>("conversations_list", {
    workspace: workspace ?? null,
  });
}

export async function conversationsCreate(
  workspace: string,
  title?: string,
): Promise<ConversationSummary> {
  return invoke<ConversationSummary>("conversations_create", {
    args: { workspace, title: title ?? null },
  });
}

export async function conversationsGet(workspace: string, id: string): Promise<Conversation> {
  return invoke<Conversation>("conversations_get", { workspace, id });
}

export async function conversationsAppendMessage(args: {
  workspace: string;
  conversationId: string;
  role: string;
  content: string;
  model?: string | null;
  claimsUsed?: string[];
}): Promise<ConversationMessage> {
  return invoke<ConversationMessage>("conversations_append_message", {
    args: {
      workspace: args.workspace,
      conversation_id: args.conversationId,
      role: args.role,
      content: args.content,
      model: args.model ?? null,
      claims_used: args.claimsUsed ?? [],
    },
  });
}

export async function conversationsDelete(workspace: string, id: string): Promise<boolean> {
  return invoke<boolean>("conversations_delete", { args: { workspace, id } });
}

export async function conversationsRename(
  workspace: string,
  id: string,
  title: string,
): Promise<ConversationSummary> {
  return invoke<ConversationSummary>("conversations_rename", {
    args: { workspace, id, title },
  });
}

// ─── Chat (sidecar /ask bridge) ──────────────────────────────────────

export interface ChatTurnPayload {
  role: "user" | "assistant";
  content: string;
}

export interface ChatStreamArgs {
  workspace: string;
  question: string;
  conversationId?: string | null;
  sessionScope?: string[];
  /** When true, the engine routes this turn through the multi-turn
   *  tool-using agent (S3) — emits tool_call_* + approval_requested
   *  events the UI must handle. The desktop chat surface flips this
   *  on once claim cards are wired. */
  useAgent?: boolean;
  /** Last 6-8 turns of this conversation, oldest-first. Empty =
   *  single-shot. */
  history?: ChatTurnPayload[];
}

export interface ChatStreamAck {
  turn_id: string;
  host: string;
  port: number;
}

export type ChatEvent =
  | { type: "token"; turn_id: string; text: string }
  | {
      type: "final";
      turn_id: string;
      full_text: string;
      claims_used: number;
      category: string;
      conversation_id: string | null;
    }
  | { type: "error"; turn_id: string; message: string }
  | {
      type: "tool_call_proposed";
      turn_id: string;
      id: string;
      name: string;
      input: unknown;
      is_write: boolean;
    }
  | {
      type: "approval_requested";
      turn_id: string;
      id: string;
      name: string;
      input: unknown;
      /**
       * Phase D Wave 1 (2026-05-17) — populated for the 10
       * system-power tools (file_read / file_write / file_edit /
       * glob / grep / shell_exec / clipboard_* / open_in_default /
       * trash). When present, the UI renders a permission-aware
       * prompt with "Allow always for pattern X" / "Deny always
       * for pattern X" buttons. When
       * `permission_context.default_deny_matched === true`, the UI
       * MUST hide the Allow buttons — the path is protected by
       * ThinkingRoot's hardcoded security policy and no user
       * click can override it.
       */
      permission_context?: PermissionContext;
    }
  | {
      type: "tool_call_executing";
      turn_id: string;
      id: string;
      name: string;
    }
  | {
      type: "tool_call_finished";
      turn_id: string;
      id: string;
      name: string;
      /** Full (untruncated) tool result — render this in the UI. */
      content: string;
      is_error: boolean;
      /** True iff the LLM's history copy was truncated to fit the per-call
       *  token budget; the LLM saw `llm_content_bytes` of the
       *  `original_content_bytes` the UI is showing. Surface honestly in
       *  the tool card so users know the model has a smaller view. */
      llm_truncated: boolean;
      llm_content_bytes: number;
      original_content_bytes: number;
    }
  | {
      type: "tool_call_rejected";
      turn_id: string;
      id: string;
      name: string;
      reason: string;
    }
  | {
      /** Post-stream verifier verdict, one per turn. Engine wire side
       *  lives at intelligence/verifier.rs::Verdict; serialised via
       *  Verdict::to_sse_payload and emitted on `event: trust_receipt`
       *  after `event: final`. */
      type: "trust_receipt";
      turn_id: string;
      kind:
        | "fully_grounded"
        | "partially_grounded"
        | "unverified_citations"
        | "skipped_chitchat"
        | "skipped_rejection"
        | "skipped_bench";
      claims_used: string[];
      auto_cited_count?: number;
      related_count?: number;
      bad_claim_ids?: string[];
    }
  | {
      /** Per-turn engram activation. Emitted by the engine when the
       *  agent calls `materialize_engram` or `probe_engram`. The
       *  `tool` discriminator selects which optional fields are
       *  populated. Engine wire side: rest.rs::parse_engram_activation. */
      type: "engram_activated";
      turn_id: string;
      tool: "materialize_engram" | "probe_engram" | (string & {});
      pointer: string;
      ts_ms: number;
      /** materialize_engram only — best-effort EngramSummary payload. */
      summary?: unknown;
      /** materialize_engram only. */
      source_count?: number;
      /** probe_engram only. */
      answer_count?: number;
    }
  | {
      /** Reflection gaps surfaced when the agent calls the `gaps` MCP
       *  tool. Each entry mirrors thinkingroot_reflect::types::GapReport:
       *  pre-baked `reason` text + entity context + sample size. The
       *  engine pre-filters by min_confidence — UI renders all gaps
       *  in the array. Wire side: rest.rs::parse_gaps_surfacing. */
      type: "gaps_surfaced";
      turn_id: string;
      ts_ms: number;
      gaps: GapEntry[];
    };

/** One reflection gap, mirroring `thinkingroot_reflect::types::GapReport`.
 *  Field names match the engine's serde shape so the wire payload
 *  travels through unchanged. */
export interface GapEntry {
  entity_name: string;
  entity_type: string;
  expected_claim_type: string;
  confidence: number;
  sample_size: number;
  reason: string;
}

export async function chatSendStream(args: ChatStreamArgs): Promise<ChatStreamAck> {
  return invoke<ChatStreamAck>("chat_send_stream", {
    args: {
      workspace: args.workspace,
      question: args.question,
      conversation_id: args.conversationId ?? null,
      session_scope: args.sessionScope ?? [],
      use_agent: args.useAgent ?? false,
      history: args.history ?? [],
    },
  });
}

export function onChatEvent(handler: (e: ChatEvent) => void): Promise<UnlistenFn> {
  return listen<ChatEvent>("chat-event", (ev) => handler(ev.payload));
}

/**
 * Phase D Wave 1 permission rule the UI persists when the user
 * clicks "Allow always" or "Deny always". Sent inside the
 * `chat_approve` call so the rule is saved BEFORE the agent
 * unblocks.
 */
export interface PersistRule {
  kind: "path" | "command";
  /** Glob pattern (path) or shell-command pattern. */
  pattern: string;
  decision: "allow" | "deny";
}

/**
 * Phase D Wave 1 permission context attached to
 * `approval_requested` events for the 10 system-power tools.
 */
export interface PermissionContext {
  /** Which Phase D tool the agent is invoking. */
  tool: string;
  /** Canonical realpath the gate evaluated against rules, when
   *  the input contained a path (file_read/file_write/glob/etc.). */
  canonical_path?: string;
  /** Raw path when canonicalisation failed (e.g. file_write to a
   *  not-yet-existing file). */
  raw_path?: string;
  /** Shell command when the tool is shell_exec. */
  command?: string;
  /** Pattern the UI can offer as "Allow always for X". */
  suggested_pattern?: string;
  /** When true, the path matched DEFAULT_DENY; UI hides Allow buttons. */
  default_deny_matched?: boolean;
}

/** Approve or reject a pending agent write tool call. Resolves the
 *  matching pending oneshot in the engine's `pending_approvals` map
 *  and unblocks the agent's `ToolApprovalRouter::check`.
 *
 *  Phase D Wave 1 — pass `persistRule` to save an
 *  "always" decision to `permissions.toml` BEFORE the agent
 *  unblocks. The backend rejects rules that conflict with
 *  DEFAULT_DENY (e.g. trying to persist `allow ~/.ssh/foo`). */
export async function chatApprove(args: {
  workspace: string;
  toolUseId: string;
  approve: boolean;
  reason?: string;
  persistRule?: PersistRule;
}): Promise<void> {
  return invoke<void>("chat_approve", {
    args: {
      workspace: args.workspace,
      tool_use_id: args.toolUseId,
      approve: args.approve,
      reason: args.reason ?? null,
      persist_rule: args.persistRule ?? null,
    },
  });
}

// ─── LLM health (pre-flight) ─────────────────────────────────────────

export interface LlmHealth {
  /** True when the workspace has a usable LLM client. */
  configured: boolean;
  /** Provider tag e.g. "anthropic", "azure", "openai". */
  provider: string | null;
  /** Display model e.g. "claude-sonnet-4-5". */
  model: string | null;
  /** Number of compiled claims in the workspace. Zero means the engine
   *  will return the "not enough information" fallback regardless of LLM. */
  claim_count: number;
  /** False when the workspace name is not mounted in the engine. */
  mounted: boolean;
}

export async function llmHealth(workspace: string): Promise<LlmHealth> {
  return invoke<LlmHealth>("llm_health", { workspace });
}

/** Per-workspace LLM configuration. Read from
 *  `<workspace>/.thinkingroot/config.toml` so the settings page shows
 *  the values the engine actually uses, not a hardcoded placeholder. */
export interface WorkspaceLlmConfig {
  workspace_path: string | null;
  workspace_name: string | null;
  provider: string | null;
  extraction_model: string | null;
  compilation_model: string | null;
  azure_resource_name: string | null;
  azure_endpoint_base: string | null;
  azure_deployment: string | null;
  azure_api_version: string | null;
  azure_api_key_env: string | null;
  azure_api_key_env_present: boolean;
  config_exists: boolean;
}

export async function workspaceLlmConfig(
  workspacePath: string,
): Promise<WorkspaceLlmConfig> {
  return invoke<WorkspaceLlmConfig>("workspace_llm_config", { workspacePath });
}

export interface WorkspaceLlmWriteArgs {
  workspace_path: string;
  provider?: string | null;
  extraction_model?: string | null;
  compilation_model?: string | null;
  azure_resource_name?: string | null;
  azure_endpoint_base?: string | null;
  azure_deployment?: string | null;
  azure_api_version?: string | null;
  azure_api_key_env?: string | null;
}

export async function workspaceLlmWrite(
  args: WorkspaceLlmWriteArgs,
): Promise<string> {
  return invoke<string>("workspace_llm_write", { args });
}

// ─── Branch slash commands ───────────────────────────────────────────

export interface BranchView {
  name: string;
  parent: string;
  status: string;
  current: boolean;
  description: string | null;
  /** Daemon `BranchKind` JSON (`#[serde(tag = "kind")]`) when present. */
  kind?: unknown;
}

export async function branchList(workspace: string): Promise<BranchView[]> {
  return invoke<BranchView[]>("branch_list", { args: { workspace } });
}

export async function branchCreate(args: {
  workspace: string;
  name: string;
  parent?: string;
  description?: string;
}): Promise<BranchView> {
  return invoke<BranchView>("branch_create", {
    args: {
      workspace: args.workspace,
      name: args.name,
      parent: args.parent ?? null,
      description: args.description ?? null,
    },
  });
}

export async function branchCheckout(workspace: string, name: string): Promise<string> {
  return invoke<string>("branch_checkout", { args: { workspace, name } });
}

export interface MergeOutcome {
  merged: boolean;
  new_claims: number;
  auto_resolved: number;
  conflicts: number;
  blocking_reasons: string[];
}

export async function branchMerge(args: {
  workspace: string;
  name: string;
  force?: boolean;
  propagateDeletions?: boolean;
}): Promise<MergeOutcome> {
  return invoke<MergeOutcome>("branch_merge", {
    args: {
      workspace: args.workspace,
      name: args.name,
      force: args.force ?? false,
      propagate_deletions: args.propagateDeletions ?? false,
    },
  });
}

export async function branchDelete(workspace: string, name: string): Promise<boolean> {
  return invoke<boolean>("branch_delete", { args: { workspace, name } });
}

// ─── Branch extras (T1.2 / T1.3 / T1.6 / T1.7 / T0.5) ────────────────

export interface BranchStats {
  branch: string;
  claim_count: number;
  entity_count: number;
  source_count: number;
  event_count: number;
  status: string;
}

export async function branchStats(branch: string): Promise<BranchStats> {
  return invoke<BranchStats>("branch_stats", { branch });
}

export async function branchEvents(branch: string): Promise<unknown[]> {
  return invoke<unknown[]>("branch_events", { branch });
}

export async function branchLineage(): Promise<unknown> {
  return invoke<unknown>("branch_lineage");
}

export async function branchRebase(branch: string): Promise<void> {
  return invoke<void>("branch_rebase", { branch });
}

export async function branchRollback(branch: string): Promise<void> {
  return invoke<void>("branch_rollback", { branch });
}

// ─── Cross-branch belief diff (T28) ──────────────────────────────────
//
// Wraps the daemon's `GET /api/v1/branches/{branch}/diff`. The
// returned shape mirrors `thinkingroot_core::types::diff::KnowledgeDiff`
// — see crates/thinkingroot-core/src/types/diff.rs for the
// authoritative definition. We mirror the fields BeliefDiffPanel
// renders; less-used fields (e.g. relation diffs, full claim records)
// are kept as `unknown` to avoid coupling the desktop to every shape
// change in the engine.

export type DiffStatus = "Added" | "Modified" | "Removed";

export interface KnowledgeDiff {
  from_branch: string;
  to_branch: string;
  computed_at: string;
  new_claims: DiffClaimEntry[];
  new_entities: DiffEntityEntry[];
  new_relations: DiffRelationEntry[];
  auto_resolved: AutoResolutionEntry[];
  needs_review: ContradictionPairEntry[];
  health_before: unknown;
  health_after: unknown;
  merge_allowed: boolean;
  blocking_reasons: string[];
}

export interface DiffClaimEntry {
  /** Full Claim shape — opaque here. The fields BeliefDiffPanel
   *  reads (id, statement, confidence, sensitivity) all live on it. */
  claim: {
    id: string;
    statement: string;
    confidence: number;
    sensitivity?: string;
  } & Record<string, unknown>;
  entity_context: string[];
  diff_status: DiffStatus;
}

export interface DiffEntityEntry {
  entity: { id: string; canonical_name: string; entity_type: string } & Record<
    string,
    unknown
  >;
  diff_status: DiffStatus;
}

export interface DiffRelationEntry {
  from_name: string;
  to_name: string;
  relation_type: string;
  strength: number;
  diff_status: DiffStatus;
}

export interface AutoResolutionEntry {
  main_claim_id: string;
  branch_claim_id: string;
  winner: string;
  confidence_delta: number;
}

export interface ContradictionPairEntry {
  /** Wire shape varies — main_claim / branch_claim as full Claim
   *  records. Renders the statements + ids; nothing more. */
  main_claim: { id: string; statement: string } & Record<string, unknown>;
  branch_claim: { id: string; statement: string } & Record<string, unknown>;
  reason?: string;
}

export async function branchDiff(branch: string): Promise<KnowledgeDiff> {
  return invoke<KnowledgeDiff>("branch_diff", { branch });
}

// ─── Live aggregate branch-event subscription ───────────────────────
//
// Wire path: daemon `/branch-events/stream` (SSE) →
// Tauri sidecar `branch_event_subscribe` (background task) →
// `branch-event` Tauri channel → `onBranchEvent` listener.
//
// The Rust shape is `BranchEventEnvelope` (see
// `apps/thinkingroot-desktop/src-tauri/src/commands/branch_extras.rs`).
// `kind` is the discriminator; the optional fields are populated per
// variant (`event` for `event`, `head` for `head_changed`, `missed`
// for `lagged`, `reason` for `disconnected`).

export type BranchEventEnvelope =
  | { kind: "event"; branch: string; event: unknown }
  | { kind: "head_changed"; head: string }
  | { kind: "lagged"; missed: number }
  | { kind: "disconnected"; reason: string };

/** Idempotent — calling twice while a subscriber is running is a no-op. */
export async function branchEventSubscribe(): Promise<void> {
  return invoke<void>("branch_event_subscribe");
}

export async function branchEventUnsubscribe(): Promise<void> {
  return invoke<void>("branch_event_unsubscribe");
}

export function onBranchEvent(
  handler: (e: BranchEventEnvelope) => void,
): Promise<UnlistenFn> {
  return listen<BranchEventEnvelope>("branch-event", (ev) => handler(ev.payload));
}

// ─── Tags (T2.5) ─────────────────────────────────────────────────────

export interface TagView {
  name: string;
  target_commit_hash: string;
  message: string | null;
  created_at: string | null;
}

export async function tagList(): Promise<TagView[]> {
  return invoke<TagView[]>("tag_list");
}

export async function tagGet(name: string): Promise<TagView> {
  return invoke<TagView>("tag_get", { name });
}

export async function tagCreate(args: {
  name: string;
  branch: string;
  message?: string;
}): Promise<TagView> {
  return invoke<TagView>("tag_create", {
    args: {
      name: args.name,
      branch: args.branch,
      message: args.message ?? null,
    },
  });
}

// ─── Knowledge Proposals (T0.4) ──────────────────────────────────────

export interface ProposalView {
  id: string;
  source_branch: string;
  target_branch: string;
  status: string;
}

export async function proposalOpen(args: {
  branch: string;
  target?: string;
  description?: string;
  minReviewers?: number;
}): Promise<ProposalView> {
  return invoke<ProposalView>("proposal_open", {
    args: {
      branch: args.branch,
      target: args.target ?? "main",
      description: args.description ?? null,
      min_reviewers: args.minReviewers ?? null,
    },
  });
}

export async function proposalList(branch?: string): Promise<ProposalView[]> {
  return invoke<ProposalView[]>("proposal_list", {
    branch: branch ?? null,
  });
}

export type ProposalDecision = "approve" | "request_changes" | "comment";

export async function proposalReview(args: {
  id: string;
  decision: ProposalDecision;
  note?: string;
}): Promise<void> {
  return invoke<void>("proposal_review", {
    args: {
      id: args.id,
      decision: args.decision,
      note: args.note ?? null,
    },
  });
}

export async function proposalClose(id: string): Promise<void> {
  return invoke<void>("proposal_close", { id });
}

// ─── Brain probes (REST parity) ──────────────────────────────────────

export interface WorkspaceBrief {
  workspace: string;
  entity_count: number;
  claim_count: number;
  source_count: number;
  top_entities: Array<{ name: string; entity_type: string; claim_count: number }>;
  recent_decisions: Array<[string, number]>;
  contradiction_count: number;
}

export async function brainBrief(branch?: string): Promise<WorkspaceBrief> {
  return invoke<WorkspaceBrief>("brain_brief", { branch: branch ?? null });
}

export interface EntityContext {
  id: string;
  name: string;
  entity_type: string;
  description: string;
  aliases: string[];
  outgoing_relations: Array<[string, string, number]>;
  incoming_relations: Array<[string, string, number]>;
  claims: unknown[];
  contradictions: unknown[];
}

export async function brainInvestigate(args: {
  entity: string;
  branch?: string;
}): Promise<EntityContext> {
  return invoke<EntityContext>("brain_investigate", {
    entity: args.entity,
    branch: args.branch ?? null,
  });
}

// ─── Hybrid retrieve ─────────────────────────────────────────────────

export interface HybridHit {
  claim_id: string;
  statement: string;
  fused_score: number;
  admission_tier: string;
  provenance_verified?: boolean;
}

export interface HybridResponse {
  hits: HybridHit[];
  total_candidates: number;
}

export async function retrieveHybrid(args: {
  query: string;
  topK?: number;
  branch?: string;
  profile?: string;
}): Promise<HybridResponse> {
  return invoke<HybridResponse>("retrieve_hybrid", {
    query: args.query,
    topK: args.topK ?? null,
    branch: args.branch ?? null,
    profile: args.profile ?? null,
  });
}

// ─── Claims listing ──────────────────────────────────────────────────

export async function claimsList(args: {
  claimType?: string;
  entity?: string;
  minConfidence?: number;
  limit?: number;
  offset?: number;
}): Promise<unknown> {
  return invoke<unknown>("claims_list", {
    claimType: args.claimType ?? null,
    entity: args.entity ?? null,
    minConfidence: args.minConfidence ?? null,
    limit: args.limit ?? null,
    offset: args.offset ?? null,
  });
}

export async function claimsAsOf(args: {
  asOf: string;
  branch?: string;
}): Promise<unknown> {
  return invoke<unknown>("claims_as_of", {
    asOf: args.asOf,
    branch: args.branch ?? null,
  });
}

export async function claimsRooted(): Promise<unknown> {
  return invoke<unknown>("claims_rooted");
}

// ─── Branch templates (T3.7) ─────────────────────────────────────────

export interface BranchTemplateInfo {
  name: string;
  description: string | null;
  kind?: unknown;
  merge_policy?: unknown;
}

export async function branchTemplateList(): Promise<{ templates: BranchTemplateInfo[] }> {
  return invoke<{ templates: BranchTemplateInfo[] }>("branch_template_list");
}

export async function branchTemplateGet(name: string): Promise<{ template: unknown }> {
  return invoke<{ template: unknown }>("branch_template_get", { name });
}

export async function branchTemplateUpsert(template: unknown): Promise<unknown> {
  return invoke<unknown>("branch_template_upsert", { template });
}

export async function branchTemplateDelete(name: string): Promise<unknown> {
  return invoke<unknown>("branch_template_delete", { name });
}

export async function branchTemplateApply(args: {
  template: string;
  branch: string;
  description?: string;
}): Promise<unknown> {
  return invoke<unknown>("branch_template_apply", {
    template: args.template,
    branch: args.branch,
    description: args.description ?? null,
  });
}

// ─── Connector bulk-contribute + redaction policy ────────────────────

export async function branchContributeBulk(args: {
  branch: string;
  connectorId: string;
  installId: string;
  idempotencyKey: string;
  sessionId?: string;
  backfill?: boolean;
  claims: unknown[];
}): Promise<unknown> {
  return invoke<unknown>("branch_contribute_bulk", {
    branch: args.branch,
    connectorId: args.connectorId,
    installId: args.installId,
    idempotencyKey: args.idempotencyKey,
    sessionId: args.sessionId ?? null,
    backfill: args.backfill ?? null,
    claims: args.claims,
  });
}

export async function branchRedactionSet(args: {
  branch: string;
  policy: unknown | null;
}): Promise<unknown> {
  return invoke<unknown>("branch_redaction_set", {
    branch: args.branch,
    policy: args.policy,
  });
}

// ─── Engrams (Active Engram Protocol) ────────────────────────────────

export async function engramMaterialize(args: {
  sessionId: string;
  topic: string;
  seedEntityIds?: string[];
  scope?: string;
}): Promise<{ pointer: string; summary: unknown }> {
  return invoke<{ pointer: string; summary: unknown }>("engram_materialize", {
    sessionId: args.sessionId,
    topic: args.topic,
    seedEntityIds: args.seedEntityIds ?? null,
    scope: args.scope ?? null,
  });
}

export async function engramList(sessionId: string): Promise<unknown> {
  return invoke<unknown>("engram_list", { sessionId });
}

export async function engramProbe(args: {
  sessionId: string;
  pointer: string;
  question: string;
  clearance?: string[];
  probeKind?: string;
  scoreWithHybrid?: boolean;
}): Promise<unknown> {
  return invoke<unknown>("engram_probe", {
    sessionId: args.sessionId,
    pointer: args.pointer,
    question: args.question,
    clearance: args.clearance ?? null,
    probeKind: args.probeKind ?? null,
    scoreWithHybrid: args.scoreWithHybrid ?? null,
  });
}

export async function engramExpire(args: {
  sessionId: string;
  pointer: string;
}): Promise<{ expired: boolean; pointer: string }> {
  return invoke<{ expired: boolean; pointer: string }>("engram_expire", {
    sessionId: args.sessionId,
    pointer: args.pointer,
  });
}

// ─── Cloud auth (Slice 1) ────────────────────────────────────────────
//
// Mirrors the Rust types in `src-tauri/src/commands/cloud.rs`. The
// `cloud_status_changed` event carries discriminated-union payloads
// that surface state transitions in real time (login flow, credit
// updates, auth-expired). All cloud writes live in `thinkingroot-cloud-auth`;
// these bindings just shuttle the typed shapes across the IPC boundary.

export interface AuthState {
  signed_in: boolean;
  handle?: string | null;
  tier?: string | null;
  credits_remaining?: number | null;
  credits_total?: number | null;
  period_end?: string | null;
  server: string;
  last_refresh_at?: string | null;
  token_redacted?: string | null;
}

export interface CreditsSnapshot {
  remaining: number;
  total: number;
  period_end: string;
}

export type CloudStatusEventPayload =
  | { status: "signed_out" }
  | { status: "logging_in"; manual_url?: string }
  | {
      status: "signed_in";
      handle: string;
      tier: "free" | "pro";
      credits_remaining: number;
      credits_total: number;
      period_end: string;
    }
  | {
      status: "login_failed";
      reason:
        | "timeout"
        | "state_mismatch"
        | "bind_failed"
        | "cancelled"
        | "already_in_flight"
        | "hub_reject";
      detail?: string;
    }
  | { status: "auth_expired" }
  | { status: "credits_updated"; remaining: number; total: number }
  | { status: "tier_changed"; new_tier: "free" | "pro" };

export const CLOUD_STATUS_EVENT = "cloud_status_changed";

export const authState = (): Promise<AuthState> => invoke("auth_state");
export const cloudLoginStart = (): Promise<void> => invoke("cloud_login_start");
export const cloudLoginCancel = (): Promise<void> => invoke("cloud_login_cancel");
export const cloudLogout = (): Promise<void> => invoke("cloud_logout");
export const cloudRefreshMe = (): Promise<AuthState> => invoke("cloud_refresh_me");
export const cloudCreditsPoll = (): Promise<CreditsSnapshot> => invoke("cloud_credits_poll");
export const cloudOpenUpgrade = (): Promise<void> => invoke("cloud_open_upgrade");

// ─── Cloud packs (push / pull) ───────────────────────────────────────
//
// Subprocess wrappers around `root push` and `root pull` exposed as
// Tauri commands. `PackOpResult.error` is `null` on success and
// carries stderr on failure — the UI renders either path honestly.

export interface PackOpResult {
  success: boolean;
  output: string;
  error?: string | null;
}

export const cloudPushWorkspace = (
  workspacePath: string,
  visibility?: "public" | "private",
): Promise<PackOpResult> =>
  invoke("cloud_push_workspace", { workspacePath, visibility });

export const cloudPullPack = (
  packRef: string,
  targetDir?: string,
): Promise<PackOpResult> =>
  invoke("cloud_pull_pack", { packRef, targetDir });

// ─── Embedded terminal (PTY) ─────────────────────────────────────────
//
// Mirror of `apps/.../src-tauri/src/commands/terminal.rs`. Each method
// is a thin invoke wrapper; the raw event subscription used by the
// xterm controller lives in `lib/terminal.ts` because it owns the
// addon lifecycle.

export interface TerminalOpenArgs {
  /** Working directory; falls back to `$HOME` when absent. */
  cwd?: string | null;
  /** Override the shell binary. Falls back to `$SHELL` / pwsh. */
  shell?: string | null;
  cols?: number | null;
  rows?: number | null;
  env?: Record<string, string> | null;
  title?: string | null;
}

export interface TerminalSessionInfo {
  id: string;
  title: string;
  shell: string;
  cwd: string;
  pid: number | null;
  /** ISO-8601 timestamp from chrono::Utc. */
  created_at: string;
  /** Tauri event topic for raw PTY output (base64). */
  data_event: string;
  /** Tauri event topic for shell exit. */
  exit_event: string;
}

export interface TerminalDataEvent {
  /** Base64-encoded raw PTY bytes. */
  data: string;
}

export interface TerminalExitEvent {
  code: number;
  success: boolean;
}

export async function terminalOpen(opts: TerminalOpenArgs = {}): Promise<TerminalSessionInfo> {
  return invoke<TerminalSessionInfo>("terminal_open", {
    opts: {
      cwd: opts.cwd ?? null,
      shell: opts.shell ?? null,
      cols: opts.cols ?? null,
      rows: opts.rows ?? null,
      env: opts.env ?? null,
      title: opts.title ?? null,
    },
  });
}

export async function terminalWrite(id: string, data: string): Promise<void> {
  return invoke("terminal_write", { id, data });
}

export async function terminalResize(id: string, cols: number, rows: number): Promise<void> {
  return invoke("terminal_resize", { id, cols, rows });
}

export async function terminalClose(id: string): Promise<void> {
  return invoke("terminal_close", { id });
}

export async function terminalList(): Promise<TerminalSessionInfo[]> {
  return invoke<TerminalSessionInfo[]>("terminal_list");
}

export async function listenTerminalData(
  topic: string,
  handler: (chunk: TerminalDataEvent) => void,
): Promise<UnlistenFn> {
  return listen<TerminalDataEvent>(topic, (e) => handler(e.payload));
}

export async function listenTerminalExit(
  topic: string,
  handler: (info: TerminalExitEvent) => void,
): Promise<UnlistenFn> {
  return listen<TerminalExitEvent>(topic, (e) => handler(e.payload));
}

// ─── Embedded browser (native child WebView) ─────────────────────────

export interface BrowserBounds {
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface BrowserSessionInfo {
  id: string;
  title: string;
  url: string;
  event: string;
}

export type BrowserEvent =
  | { kind: "loading"; url: string }
  | { kind: "loaded"; url: string }
  | { kind: "title"; title: string }
  | { kind: "navigation"; url: string }
  | { kind: "new_window"; url: string }
  | { kind: "download"; url: string; path?: string | null; success?: boolean | null };

export async function browserOpen(args: {
  url: string;
  bounds: BrowserBounds;
  title?: string | null;
}): Promise<BrowserSessionInfo> {
  return invoke<BrowserSessionInfo>("browser_open", {
    req: {
      url: args.url,
      bounds: args.bounds,
      title: args.title ?? null,
    },
  });
}

export async function browserNavigate(id: string, url: string): Promise<string> {
  return invoke<string>("browser_navigate", { id, url });
}

export async function browserReload(id: string): Promise<void> {
  return invoke("browser_reload", { id });
}

export async function browserBack(id: string): Promise<void> {
  return invoke("browser_back", { id });
}

export async function browserForward(id: string): Promise<void> {
  return invoke("browser_forward", { id });
}

export async function browserSetBounds(id: string, bounds: BrowserBounds): Promise<void> {
  return invoke("browser_set_bounds", { id, bounds });
}

export async function browserShow(id: string): Promise<void> {
  return invoke("browser_show", { id });
}

export async function browserHide(id: string): Promise<void> {
  return invoke("browser_hide", { id });
}

export async function browserFocus(id: string): Promise<void> {
  return invoke("browser_focus", { id });
}

export async function browserClose(id: string): Promise<void> {
  return invoke("browser_close", { id });
}

export async function browserList(): Promise<BrowserSessionInfo[]> {
  return invoke<BrowserSessionInfo[]>("browser_list");
}

export async function listenBrowserEvent(
  topic: string,
  handler: (event: BrowserEvent) => void,
): Promise<UnlistenFn> {
  return listen<BrowserEvent>(topic, (e) => handler(e.payload));
}

export async function browserDevtools(id: string, open: boolean): Promise<boolean> {
  return invoke<boolean>("browser_devtools", { id, open });
}

export async function browserFind(
  id: string,
  query: string,
  options: { caseSensitive?: boolean; backwards?: boolean } = {},
): Promise<void> {
  return invoke("browser_find", {
    id,
    query,
    caseSensitive: options.caseSensitive ?? false,
    backwards: options.backwards ?? false,
  });
}

export async function browserFindClear(id: string): Promise<void> {
  return invoke("browser_find_clear", { id });
}

export async function browserZoom(id: string, factor: number): Promise<number> {
  return invoke<number>("browser_zoom", { id, factor });
}

export async function browserPrint(id: string): Promise<void> {
  return invoke("browser_print", { id });
}

export async function browserScrollTo(id: string, x: number, y: number): Promise<void> {
  return invoke("browser_scroll_to", { id, x, y });
}

// ─── Browser → workspace save ───────────────────────────────────────
//
// `browser_save_page` injects Readability.js + Turndown.js into the
// captive webview, awaits the cleaned-markdown payload via the
// `browser_extract_callback` IPC bridge, writes it under the target
// workspace's `sources/` with frontmatter (`url:`, `content_hash:`),
// stamps any prior file with `superseded_by:` when content changed,
// and kicks off `workspace_compile` so the new bytes flow through
// the Witness Mesh pipeline.

export type BrowserSaveStatus = "saved" | "already_saved" | "updated";

export interface BrowserSavePageResult {
  status: BrowserSaveStatus;
  path: string;
  slug: string;
  title: string;
  url: string;
  workspace: string;
  content_hash: string;
  prior_path?: string;
}

export async function browserSavePage(
  viewId: string,
  workspace: string,
): Promise<BrowserSavePageResult> {
  return invoke<BrowserSavePageResult>("browser_save_page", {
    args: { view_id: viewId, workspace },
  });
}

// ─── Playground ────────────────────────────────────────────────────
//
// The playground workspace is auto-mounted on first launch. This
// command is the manual escape hatch for the rare case where a user
// removed it from the registry and wants it back without restarting
// the app. Idempotent.

export interface PlaygroundView {
  name: string;
  path: string;
  port: number;
  created: boolean;
}

export async function playgroundEnsure(): Promise<PlaygroundView> {
  return invoke<PlaygroundView>("playground_ensure");
}

/** Living Paper payload returned by `paper_get`. `exists == false`
 * honestly signals that the workspace hasn't compiled yet (or
 * Phase 10b synthesis failed; the paper is non-fatal). */
export interface PaperPayload {
  path: string;
  exists: boolean;
  markdown: string;
}

/** Read the Living Paper for a workspace by name. Resolves the
 * workspace via the on-disk WorkspaceRegistry, reads
 * `<root>/.thinkingroot/paper.md` off the main thread. */
export async function paperGet(workspace: string): Promise<PaperPayload> {
  return invoke<PaperPayload>("paper_get", { workspace });
}

/** Honest outcome of a Playground drop. `copied` is the file count
 * that landed in inbox/; `skipped_duplicate` counts same-name
 * collisions (no overwrite — the user can rename + retry);
 * `skipped_unreadable` counts permissions / race-deleted sources. */
export interface DropOutcome {
  copied: number;
  skipped_duplicate: number;
  skipped_unreadable: number;
  destination_paths: string[];
}

/** Copy dropped files into the workspace's inbox/ directory. The
 * Playground DropZone calls this after the Tauri window emits a
 * `playground-files-dropped` event with the OS-level file paths. */
export async function playgroundDrop(
  workspace: string,
  filePaths: string[],
): Promise<DropOutcome> {
  return invoke<DropOutcome>("playground_drop", {
    workspace,
    filePaths,
  });
}

/** One row in the Source Library. Same shape as the engine's
 * `SourceInfo` wire type. */
export interface PlaygroundSource {
  id: string;
  uri: string;
  source_type: string;
  content_hash: string;
}

/** List every source in the active workspace via the sidecar. */
export async function playgroundSources(): Promise<PlaygroundSource[]> {
  return invoke<PlaygroundSource[]>("playground_sources");
}

/** Per-source witness count for the active workspace. Sources with
 * zero witnesses are absent from the response — treat missing as zero. */
export interface WitnessesPerSourceRow {
  source_id: string;
  count: number;
}
export async function playgroundWitnessesBySource(): Promise<WitnessesPerSourceRow[]> {
  return invoke<WitnessesPerSourceRow[]>("playground_witnesses_by_source");
}

/** One row in the Playground source-detail panel. Slim projection
 * of the full Witness wire shape — enough to render the panel but
 * without dragging in the full WitnessInput / WitnessSpan binding. */
export interface PlaygroundWitnessRow {
  id: string;
  witness_type: string;
  rule: string;
  symbol: string | null;
  confidence: number;
  byte_start: number;
  byte_end: number;
}

/** List every witness anchored to a specific source. */
export async function playgroundSourceWitnesses(
  sourceId: string,
): Promise<PlaygroundWitnessRow[]> {
  return invoke<PlaygroundWitnessRow[]>("playground_source_witnesses", {
    sourceId,
  });
}

// ─── Playground v1 — 8 researcher-facing verbs ────────────────

/** Outcome of `playground_save_note` — surfaces the persisted path
 * + size so the UI can offer "Reveal in Finder" and the SourceLibrary
 * refresh picks the note up after the next compile. */
export interface SaveNoteOutcome {
  path: string;
  relative_path: string;
  bytes: number;
  /** `true` when a brand-new file was written; `false` when an existing
   * note at the same path was overwritten. */
  created: boolean;
}

/** Persist the AI's reply (or any markdown body) as a note under the
 * active workspace. Writes atomically — tempfile + rename — and stamps
 * YAML frontmatter so the next compile attributes provenance correctly. */
export async function playgroundSaveNote(
  workspace: string,
  title: string,
  body: string,
): Promise<SaveNoteOutcome> {
  return invoke<SaveNoteOutcome>("playground_save_note", {
    workspace,
    title,
    body,
  });
}

/** Outcome of `playground_open_proposal` — id + branch only. The full
 * proposal record is fetchable via the existing `proposal_*` commands. */
export interface OpenProposalOutcome {
  proposal_id: string;
  branch: string;
}

/** Open a Knowledge Proposal under the given branch (Playground default
 * is `playground`). Delegates to the sidecar's branch-proposals route. */
export async function playgroundOpenProposal(
  workspace: string,
  branch: string,
  title: string,
  body: string,
): Promise<OpenProposalOutcome> {
  return invoke<OpenProposalOutcome>("playground_open_proposal", {
    args: { workspace, branch, title, body },
  });
}

/** Outcome of `playground_branch_conversation`. */
export interface BranchConversationOutcome {
  branch: string;
  parent: string | null;
}

/** Create a knowledge branch off `parent` (or `main` when omitted) so
 * subsequent agent contributions land in an isolated graph. */
export async function playgroundBranchConversation(
  workspace: string,
  name: string,
  parent?: string,
  description?: string,
): Promise<BranchConversationOutcome> {
  return invoke<BranchConversationOutcome>("playground_branch_conversation", {
    args: { workspace, name, parent, description },
  });
}

/** One quiz row. Citations are real witness ids the answer is grounded
 * in; empty `citations` ⇒ the underlying retriever returned no
 * provenance, so the UI surfaces a "low-confidence" tag. */
export interface QuizItem {
  question: string;
  answer: string;
  citations: string[];
}

/** Generate a corpus quiz on `topic`. Pipes a quiz-shaped prompt
 * through the brain.investigate route so cited witness ids are real. */
export async function playgroundQuiz(
  workspace: string,
  topic: string,
  count?: number,
): Promise<QuizItem[]> {
  return invoke<QuizItem[]>("playground_quiz", { workspace, topic, count });
}

/** Outcome of `playground_export_tr` — absolute pack path + byte count. */
export interface ExportTrOutcome {
  path: string;
  bytes: number;
}

/** Export the workspace as a `.tr` pack. Default destination is
 * `~/Downloads/<workspace>.tr`. */
export async function playgroundExportTr(
  workspace: string,
  outPath?: string,
): Promise<ExportTrOutcome> {
  return invoke<ExportTrOutcome>("playground_export_tr", {
    args: { workspace, out_path: outPath },
  });
}

/** Hand-off URL + MCP config snippet for external agents. */
export interface HandoffUrl {
  url: string;
  mcp_config_snippet: string;
}

/** Produce a `tr+mcp://` deep-link plus a paste-ready mcp.json snippet
 * an external editor agent (Claude Code, Cursor, Codex) can use to
 * mount this workspace via the local MCP server. */
export async function playgroundHandoffUrl(
  workspace: string,
): Promise<HandoffUrl> {
  return invoke<HandoffUrl>("playground_handoff_url", { workspace });
}

/** One row of `playground_gaps` — slim projection of the full
 * GapReport shape from `thinkingroot-reflect`. */
export interface GapRow {
  gap_id: string;
  entity: string;
  entity_type: string;
  missing_claim_type: string;
  pattern_confidence: number;
  status: string;
}

/** List known-unknowns for a workspace. Empty list when reflect hasn't
 * run yet — empty state is honest, not an error. */
export async function playgroundGaps(
  workspace: string,
  options?: {
    entity?: string;
    minConfidence?: number;
    branch?: string;
  },
): Promise<GapRow[]> {
  return invoke<GapRow[]>("playground_gaps", {
    workspace,
    entity: options?.entity,
    minConfidence: options?.minConfidence,
    branch: options?.branch,
  });
}

/** Outcome of `paper_regenerate` — the rendered paper bytes, so the
 * UI can refresh the PaperPanel inline without a follow-up `paper_get`. */
export interface PaperRegenerateOutcome {
  byte_length: number;
  sections: number;
  markdown: string;
}

/** Rerun the Living Paper synthesiser against the current Witness
 * Mesh state without driving a full compile. */
export async function paperRegenerate(
  workspace: string,
): Promise<PaperRegenerateOutcome> {
  return invoke<PaperRegenerateOutcome>("paper_regenerate", { workspace });
}

// ─── Cognition Commits — chat history IS the commit DAG ──────────────

/** Author of a cognition commit. `kind === "agent"` carries the model
 *  string and the principal that authorised the call (typically
 *  "thinkingroot" for the in-app Brain chat). `kind === "user"` only
 *  carries the user id. */
export interface CommitAuthor {
  kind: "user" | "agent";
  id?: string;
  model?: string;
  principal?: string;
}

/** One cognition commit row as it lands on the wire. The full shape —
 *  the chat-DAG view renders prompt + reasoning + citation chips
 *  inline so no follow-up fetch is needed. */
export interface CognitionCommit {
  id: string;
  parent: string | null;
  branch: string;
  /** Tolerant projection — the wire emits the typed `CommitAuthor`
   *  union; we expose it through `unknown` so the React renderer can
   *  branch on `author.kind` without a strict binding that breaks on
   *  a future field addition. */
  author: CommitAuthor | Record<string, unknown>;
  prompt: string;
  reasoning: string;
  /** 64-char lower-hex witness ids this commit produced. Auto-commits
   *  ship this empty; explicit `commit_cognition` calls populate it. */
  witnesses_added: string[];
  /** 64-char lower-hex witness ids cited in `reasoning`. Auto-commits
   *  populate this from `[[witness:<id>]]` markers, filtered to
   *  existing witnesses. */
  citations: string[];
  /** Gap ids the agent flagged this turn. */
  gaps_surfaced: string[];
  /** ISO-8601 wall-clock timestamp. */
  created_at: string;
}

/** List cognition commits on a branch newest-first. `branch`
 *  defaults to `main` server-side when omitted. */
export async function commitList(options?: {
  branch?: string;
  limit?: number;
}): Promise<CognitionCommit[]> {
  return invoke<CognitionCommit[]>("commit_list", {
    branch: options?.branch,
    limit: options?.limit,
  });
}

/** Fetch a single commit by id. Throws when unknown. */
export async function commitGet(id: string): Promise<CognitionCommit> {
  return invoke<CognitionCommit>("commit_get", { id });
}

/** Record a commit. Citations + parent are server-verified — fabricated
 *  refs surface as a typed error string. */
export interface CommitRecordArgs {
  branch: string;
  parent_id?: string;
  author_kind: "user" | "agent";
  author_id: string;
  author_model?: string;
  prompt?: string;
  reasoning?: string;
  witnesses_added?: string[];
  citations?: string[];
  gaps_surfaced?: string[];
}

export async function commitRecord(
  args: CommitRecordArgs,
): Promise<CognitionCommit> {
  return invoke<CognitionCommit>("commit_record", { args });
}

// ─── Phase γ.1 — Merge Cognition (deterministic divergence plan) ──────

/** Discriminated union of the five possible divergence outcomes between
 *  two cognition-commit branches. The `kind` tag is set server-side via
 *  `#[serde(tag = "kind", rename_all = "snake_case")]` on the Rust
 *  `CommitDivergence` enum. `diverged` is the only variant that carries
 *  payload (the LCA); the rest are bare. */
export type CommitDivergence =
  | { kind: "identical" }
  | { kind: "left_ahead" }
  | { kind: "right_ahead" }
  | { kind: "diverged"; common_ancestor: string }
  | { kind: "no_common_history" };

/** Partitioned witness + gap sets across a divergence. Each `_only` /
 *  `shared` list is hex-sorted-ascending on the wire so the same plan
 *  serialises byte-stably across runs. */
export interface WitnessClassification {
  left_only_citations: string[];
  right_only_citations: string[];
  shared_citations: string[];
  left_only_added: string[];
  right_only_added: string[];
  shared_added: string[];
  left_only_gaps: string[];
  right_only_gaps: string[];
  shared_gaps: string[];
}

/** Full merge plan as the sidecar emits it. The shape matches the
 *  Rust `thinkingroot_core::types::MergePlan` projection. */
export interface MergePlan {
  left_branch: string;
  right_branch: string;
  left_head: string | null;
  right_head: string | null;
  conflict_kind: CommitDivergence;
  left_only_commits: string[];
  right_only_commits: string[];
  witnesses: WitnessClassification;
  computed_at: string;
}

/** Request a deterministic merge plan between two cognition-commit
 *  branches. The sidecar walks both DAGs, finds the LCA if any, and
 *  partitions every witness id each side cited or added since the
 *  LCA. Pure read — no commit is recorded by this call. */
export async function commitMergePlan(
  leftBranch: string,
  rightBranch: string,
): Promise<MergePlan> {
  return invoke<MergePlan>("commit_merge_plan", {
    leftBranch,
    rightBranch,
  });
}

/** Phase γ.2 — what `synthesize_merge` does (kind tag).
 *  `trivial` / `llm_unavailable` short-circuit without an LLM call;
 *  `llm_error` carries the upstream error; `synthesized` is the
 *  success path. */
export type SynthesisOutcome =
  | { kind: "trivial" }
  | { kind: "llm_unavailable" }
  | { kind: "llm_error"; message?: string }
  | { kind: "synthesized" };

/** Phase γ.2 — Full synthesis result. */
export interface MergeSynthesis {
  outcome: SynthesisOutcome;
  plan: MergePlan;
  reasoning: string;
  verified_citations: string[];
  dropped_citations: string[];
  model: string;
}

/** Request an LLM-driven synthesis of the merge plan between two
 *  branches. Citation honesty: `verified_citations` is filtered against
 *  the plan's surfaced witness ids; any fabricated `[[witness:<id>]]`
 *  the LLM emitted lands in `dropped_citations`. Pure read — no
 *  commit is recorded by this call. The caller decides whether to
 *  hand the synthesis to `commit_record` to land it as a real commit. */
export async function commitSynthesizeMerge(
  leftBranch: string,
  rightBranch: string,
): Promise<MergeSynthesis> {
  return invoke<MergeSynthesis>("commit_synthesize_merge", {
    leftBranch,
    rightBranch,
  });
}

// ─── Phase δ.2 + δ.4 — Substrate Bus (background sub-agents) ──────────

/** A single observation report emitted by a sub-agent tick. Mirrors
 *  the sidecar's `SubAgentReport`. */
export interface SubAgentReport {
  agent: string;
  started_at: string;
  finished_at: string;
  summary: string;
  observations: string[];
  proposals_opened: string[];
  error: string | null;
}

/** Idempotently start the substrate bus for the active workspace.
 *  Returns the registered agent names so the UI can render the
 *  available "Run now" affordances. */
export async function substrateBusStart(): Promise<{
  workspace: string;
  running: boolean;
  agents: string[];
}> {
  return invoke("substrate_bus_start");
}

/** Idempotently shut down the substrate bus for the active workspace. */
export async function substrateBusStop(): Promise<{
  workspace: string;
  running: boolean;
}> {
  return invoke("substrate_bus_stop");
}

/** Snapshot of recent observations across every registered sub-agent.
 *  Newest-first within each agent slice; empty when the bus is not
 *  running for this workspace. */
export async function substrateBusReports(): Promise<SubAgentReport[]> {
  return invoke<SubAgentReport[]>("substrate_bus_reports");
}

/** Manually trigger one tick of an agent (skipping its schedule).
 *  Returns the fresh report so the UI can show the result inline. */
export async function substrateBusRunNow(agent: string): Promise<SubAgentReport> {
  return invoke<SubAgentReport>("substrate_bus_run_now", { agent });
}

// ─── Playground file manager — folders, moves, previews, trash ────────

export interface PlaygroundDirEntry {
  name: string;
  rel_path: string;
  is_dir: boolean;
  size_bytes: number;
  modified: number;
  /** "folder" | "image" | "audio" | "video" | "markdown" | "text"
   *  | "code" | "pdf" | "archive" | "other" */
  kind: string;
}

export interface PlaygroundDirListing {
  workspace: string;
  rel_path: string;
  parent_rel_path: string | null;
  entries: PlaygroundDirEntry[];
}

export interface PlaygroundMoveOutcome {
  moved: number;
  skipped_conflict: number;
  skipped_invalid: number;
  moved_rel_paths: string[];
}

export interface PlaygroundTrashOutcome {
  trashed: number;
  skipped: number;
  trash_rel_paths: string[];
}

export interface PlaygroundPreview {
  workspace: string;
  rel_path: string;
  /** "text" | "markdown" | "code" | "image" | "binary" */
  kind: string;
  mime: string | null;
  size_bytes: number;
  /** Set for text/markdown/code (UTF-8, ≤1 MiB). */
  text: string | null;
  /** `data:image/...;base64,...` for image (≤5 MiB). */
  data_url: string | null;
  absolute_path: string;
  too_large: boolean;
}

export async function playgroundListDirectory(
  workspace: string,
  relPath: string,
): Promise<PlaygroundDirListing> {
  return invoke<PlaygroundDirListing>("playground_list_directory", {
    workspace,
    relPath,
  });
}

export async function playgroundCreateFolder(
  workspace: string,
  parentRelPath: string,
  name: string,
): Promise<string> {
  return invoke<string>("playground_create_folder", {
    workspace,
    parentRelPath,
    name,
  });
}

export async function playgroundRename(
  workspace: string,
  relPath: string,
  newName: string,
): Promise<string> {
  return invoke<string>("playground_rename", {
    workspace,
    relPath,
    newName,
  });
}

export async function playgroundMove(
  workspace: string,
  sourceRelPaths: string[],
  destRelFolder: string,
): Promise<PlaygroundMoveOutcome> {
  return invoke<PlaygroundMoveOutcome>("playground_move", {
    workspace,
    sourceRelPaths,
    destRelFolder,
  });
}

export async function playgroundTrash(
  workspace: string,
  relPaths: string[],
): Promise<PlaygroundTrashOutcome> {
  return invoke<PlaygroundTrashOutcome>("playground_trash", {
    workspace,
    relPaths,
  });
}

export async function playgroundListTrash(
  workspace: string,
): Promise<PlaygroundDirEntry[]> {
  return invoke<PlaygroundDirEntry[]>("playground_list_trash", { workspace });
}

export async function playgroundRestore(
  workspace: string,
  trashRelPaths: string[],
): Promise<number> {
  return invoke<number>("playground_restore", {
    workspace,
    trashRelPaths,
  });
}

export async function playgroundEmptyTrash(
  workspace: string,
): Promise<number> {
  return invoke<number>("playground_empty_trash", { workspace });
}

export async function playgroundPreview(
  workspace: string,
  relPath: string,
): Promise<PlaygroundPreview> {
  return invoke<PlaygroundPreview>("playground_preview", {
    workspace,
    relPath,
  });
}
