/**
 * Wire-format types for the ThinkingRoot REST API.
 *
 * Field names mirror the Rust serde-serialized shapes verbatim — the
 * SDK does not translate. Optional fields use `?` so partial server
 * responses (e.g. AEP probe answers with empty caveats arrays) round-
 * trip cleanly through `JSON.parse`.
 */

/** Standard envelope: `{ ok: true, data }` or `{ ok: false, error }`. */
export interface ApiEnvelope<T> {
  ok: boolean;
  data?: T;
  error?: { code: string; message: string };
}

// ─── Core entities ────────────────────────────────────────────

export interface WorkspaceInfo {
  name: string;
  path: string;
  entity_count: number;
  claim_count: number;
  source_count: number;
}

export interface Entity {
  id: string;
  canonical_name: string;
  entity_type: string;
  aliases: string[];
  attributes: string[];
  first_seen: string;
  last_updated: string;
  description?: string;
}

export interface Claim {
  id: string;
  statement: string;
  claim_type: string;
  source: string;
  confidence: number;
  admission_tier?: string;
  byte_start?: number;
  byte_end?: number;
  source_path?: string;
}

export interface SearchResult {
  query: string;
  entities: Array<{ id: string; canonical_name: string; score: number }>;
  claims: Array<{ id: string; statement: string; score: number }>;
}

// ─── Hybrid Retrieval ─────────────────────────────────────────

export interface RetrievalRequest {
  query_text: string;
  typed_predicates?: unknown[];
  session_id: string;
  clearance?: Array<"public" | "internal" | "confidential" | "restricted">;
  top_k?: number;
  time_window?: [number, number] | null;
  scoring_profile?: "default" | "compliance";
  require_certificate?: boolean;
  include_test_origin?: boolean;
  include_quarantined?: boolean;
  require_provenance_verified?: boolean;
  now?: string | null;
  scoped_claim_ids?: string[] | null;
}

export interface ScoreBreakdown {
  vector: number;
  admission: number;
  trial: number;
  source_authority: number;
  recency: number;
  complexity: number;
  marker: number;
  gap_proximity: number;
  contradiction: number;
  test_origin: number;
}

export interface RetrievalHit {
  claim_id: string;
  score: number;
  score_breakdown: ScoreBreakdown;
  byte_span?: { source_id: string; byte_start: number; byte_end: number };
  content_blake3?: string;
  provenance_verified?: boolean;
}

export interface HybridResponse {
  hits: RetrievalHit[];
  routing: {
    shape: string;
    total_candidates: number;
    vector_candidates: number;
    datalog_candidates: number;
  };
  stage_timings_ms?: Record<string, number>;
}

// ─── RARP / Active Engram Protocol ────────────────────────────

export interface EngramScope {
  depth_hops?: number;
  event_window_days?: number;
  clearance?: string[];
  seed_claim_ids?: string[];
  score_with_hybrid?: boolean;
}

export interface EngramRef {
  pointer: string;
  topic: string;
  workspace: string;
  created_at: number;
  entity_count: number;
  claim_count: number;
}

export interface MaterializeResponse {
  pointer: string;
  summary: EngramSummary;
}

export interface EngramSummary {
  pointer: string;
  topic: string;
  created_at: number;
  entity_cluster: Array<{ id: string; canonical_name: string }>;
  claim_count_by_tier: Record<string, number>;
  source_authority: unknown[];
  source_references: unknown[];
  temporal_window: [number | null, number | null];
  supersession_terminals: unknown[];
  events_window: unknown[];
  doc_tags_summary: unknown;
  headings_outline: unknown[];
  call_graph_edges: unknown[];
  test_origins: unknown[];
  code_markers: unknown[];
  code_metrics: unknown[];
  quantitative_signals: unknown[];
  structural_pattern_hits: unknown[];
  gaps: unknown[];
  unresolved_contradictions: unknown[];
  derivation_roots_by_claim: Record<string, string[]>;
  git_commits_summary: unknown;
  git_blame_summary: unknown;
  stale_rows: unknown[];
  applied_clearance: string[];
  redacted_count: number;
}

export type AnswerRow =
  | { kind: "factual"; statement: string }
  | {
      kind: "quantitative";
      metric_name: string;
      value: number;
      unit: string;
      qualifier: string;
      is_live: boolean;
    }
  | { kind: string; [key: string]: unknown };

export interface ProbeAnswer {
  answer: AnswerRow[];
  claim_ids: string[];
  source_byte_spans: Array<{
    source_id: string;
    byte_start: number;
    byte_end: number;
  }>;
  source_authority: string[];
  source_blake3s: string[];
  admission_tier: string;
  trial_scores?: unknown;
  certificate_hash?: string;
  grounding_score?: number;
  grounding_method?: string;
  valid_window: [number | null, number | null];
  superseded_by_chain: string[];
  derivation_parents: string[];
  derivation_root?: string;
  sensitivity: string;
  turn_provenance?: unknown;
  git_blame: unknown[];
  test_origin?: unknown;
  related_quantities: unknown[];
  related_doc_tags: unknown[];
  related_calls: unknown[];
  related_markers: unknown[];
  caveats: unknown[];
}

// ─── Mount summary ────────────────────────────────────────────

export interface MountSummary {
  name: string;
  workspace: string;
  version: string;
  root_path: string;
  source_files: number;
  claims: number;
  entities: number;
  rest_url: string;
  mcp_url: string;
  daemon_pid: number;
  daemon_port: number;
  signed: boolean;
  recompiled: boolean;
}
