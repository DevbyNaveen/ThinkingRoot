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

export interface OnboardingStatusRead {
  paths: ConfigPaths;
  has_any_provider_key: boolean;
  workspace_count: number;
  active_workspace?: string | null;
  missing: string[];
}

export async function onboardingStatus(): Promise<OnboardingStatusRead> {
  return invoke<OnboardingStatusRead>("onboarding_status");
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
 * Stop the in-progress compile, if any.  Resolves to `true` when a
 * compile was active and its cancellation token was tripped; `false`
 * otherwise.  The actual abort + `CompileProgress::Cancelled` event
 * arrive through the existing `workspace_compile_progress` channel
 * after the pipeline reaches its next phase boundary (typically <1s).
 */
export async function workspaceCompileStop(): Promise<boolean> {
  return invoke<boolean>("workspace_compile_stop");
}

export interface CompileStatus {
  active: boolean;
  workspace: string | null;
}

/**
 * Poll for the current compile status.  Used by the Compile modal so
 * the Stop button can stay disabled when nothing is running, without
 * depending on event ordering.
 */
export async function workspaceCompileStatus(): Promise<CompileStatus> {
  return invoke<CompileStatus>("workspace_compile_status");
}

export type CompileProgress =
  | { phase: "started"; workspace: string }
  | { phase: "parse_complete"; files: number }
  | { phase: "extraction_start"; total_chunks: number; total_batches: number }
  | { phase: "extraction_progress"; done: number; total: number }
  | { phase: "extraction_complete"; claims: number; entities: number }
  | {
      phase: "extraction_partial";
      failed_batches: number;
      failed_chunk_ranges: [number, number][];
    }
  | { phase: "grounding_progress"; done: number; total: number }
  | { phase: "linking_start"; total_entities: number }
  | { phase: "linking_progress"; done: number; total: number }
  | { phase: "vector_progress"; done: number; total: number }
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
      failed_batches: number;
      failed_chunk_ranges: [number, number][];
    }
  | { phase: "cancelled" }
  | { phase: "failed"; error: string };

export function onWorkspaceCompileProgress(
  handler: (payload: CompileProgress) => void,
): Promise<UnlistenFn> {
  return listen<CompileProgress>("workspace_compile_progress", (e) =>
    handler(e.payload),
  );
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
      content: string;
      is_error: boolean;
    }
  | {
      type: "tool_call_rejected";
      turn_id: string;
      id: string;
      name: string;
      reason: string;
    };

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

/** Approve or reject a pending agent write tool call. Resolves the
 *  matching pending oneshot in the engine's `pending_approvals` map
 *  and unblocks the agent's `ToolApprovalRouter::check`. */
export async function chatApprove(args: {
  workspace: string;
  toolUseId: string;
  approve: boolean;
  reason?: string;
}): Promise<void> {
  return invoke<void>("chat_approve", {
    args: {
      workspace: args.workspace,
      tool_use_id: args.toolUseId,
      approve: args.approve,
      reason: args.reason ?? null,
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

// ─── Auth state (honest local/cloud read) ────────────────────────────

export interface AuthState {
  signed_in: boolean;
  cloud_base_url: string | null;
  handle: string | null;
  storage: { local: boolean; cloud: boolean };
}

export async function authState(): Promise<AuthState> {
  return invoke<AuthState>("auth_state");
}
