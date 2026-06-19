use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};

use crate::graph_cache::{CachedClaim, KnowledgeGraph, RawGraphData};
pub use crate::intelligence::hybrid_types::{
    ByteSpan, ByteSpanBundle, CodeSignatureRef, HybridResponse, RetrievalCaveat, RetrievalHit,
    RetrievalRequest, RoutingShape, ScoreBreakdown, ScoringProfile, TypedPredicate,
};
pub use crate::pipeline::PipelineResult;
use thinkingroot_core::{Config, Error, Result};
use thinkingroot_graph::StorageEngine;
use thinkingroot_health::Verifier;
pub use thinkingroot_health::verifier::VerificationResult;

// ---------------------------------------------------------------------------
// Public response types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceInfo {
    pub name: String,
    pub path: String,
    pub entity_count: usize,
    pub claim_count: usize,
    pub source_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct EntityInfo {
    pub id: String,
    pub name: String,
    pub entity_type: String,
    pub claim_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct EntityDetail {
    pub id: String,
    pub name: String,
    pub entity_type: String,
    pub aliases: Vec<String>,
    pub claims: Vec<ClaimInfo>,
    pub relations: Vec<RelationInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClaimInfo {
    pub id: String,
    pub statement: String,
    pub claim_type: String,
    pub confidence: f64,
    pub source_uri: String,
    /// Unix epoch of the actual event date, or None if not extracted.
    pub event_date: Option<f64>,
}

/// Summary payload returned by the `rooting_report` MCP tool and
/// `QueryEngine::rooting_report`.
#[derive(Debug, Clone, Serialize)]
pub struct RootingReport {
    pub workspace: String,
    pub rooted: usize,
    pub attested: usize,
    pub quarantined: usize,
    pub rejected: usize,
    pub total: usize,
}

/// An SVO event with entity names resolved from the in-memory KG cache.
/// This is what ReAct uses — entity IDs (ULIDs) would be useless to an LLM.
#[derive(Debug, Clone, Serialize)]
pub struct EventHit {
    pub id: String,
    pub subject_name: String,
    pub verb: String,
    pub object_name: String,
    pub normalized_date: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RelationInfo {
    pub target: String,
    pub relation_type: String,
    pub strength: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct GalaxyNode {
    pub id: String,
    pub name: String,
    pub entity_type: String,
    pub claim_count: usize,
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct GalaxyLink {
    pub source: String,
    pub target: String,
    pub relation_type: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GalaxyMap {
    pub nodes: Vec<GalaxyNode>,
    pub links: Vec<GalaxyLink>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArtifactInfo {
    pub artifact_type: String,
    pub available: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArtifactContent {
    pub artifact_type: String,
    pub content: String,
}

/// Result of [`QueryEngine::read_source`]. Returns the exact source bytes a
/// claim or Witness cites — the round-trip the `read_source` MCP tool
/// exposes. Backed by the CCC I-2 byte-anchored invariant (see
/// `.claude/rules/compile-completeness.md`). `text` is the UTF-8 decoding
/// of `bytes` when valid; consumers needing the raw bytes can inspect
/// `bytes` directly.
#[derive(Debug, Clone, Serialize)]
pub struct ReadSourceResult {
    /// URI of the source the claim was extracted from (e.g. `src/lib.rs`).
    pub file: String,
    /// Inclusive byte offset inside `file`.
    pub byte_start: u64,
    /// Exclusive byte offset inside `file`.
    pub byte_end: u64,
    /// UTF-8 decoding of the cited bytes when valid; empty when the byte
    /// range is unknown ((0, 0) sentinel) or the source bytes can't be
    /// decoded as UTF-8.
    pub text: String,
    /// Raw bytes the claim cites. Same byte range as `(byte_start, byte_end)`.
    #[serde(skip)]
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceInfo {
    pub id: String,
    pub uri: String,
    pub source_type: String,
    /// BLAKE3 content hash of the source bytes when known. Empty for
    /// agent-contributed claims that have no underlying file. Stream B
    /// added this field so `root status` can compare against the
    /// daemon's `GET /api/v1/ws/{ws}/sources` without needing a
    /// dedicated status endpoint.
    #[serde(default)]
    pub content_hash: String,
    /// On-disk size of the backing source file in bytes, when it still
    /// exists on the workspace volume. `None` for agent-contributed
    /// sources (no file) or files since removed. Read live from the
    /// filesystem at list time — never fabricated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_size: Option<u64>,
    /// Unix epoch SECONDS when the backing source file was imported —
    /// the file's creation time (falling back to last-modified where the
    /// platform doesn't expose birth time). `None` when there is no file.
    /// This is the real filesystem time, not a stored/guessed value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imported_at: Option<i64>,
}

/// Resolve a source's `uri` to its file on the workspace volume and read its
/// size + import time STRAIGHT FROM THE FILESYSTEM. The compiler records the
/// absolute on-disk path as the uri, so we stat that directly; we also try the
/// uri joined under `root` as a fallback. Returns `(None, None)` for sources
/// with no backing file (agent-contributed) or files since removed — never a
/// fabricated value. Import time = the file's creation (birth) time where the
/// platform exposes it, else last-modified, as Unix epoch seconds.
fn source_file_meta(root: &std::path::Path, uri: &str) -> (Option<u64>, Option<i64>) {
    let candidates = [
        std::path::PathBuf::from(uri),
        root.join(uri.trim_start_matches('/')),
    ];
    for path in candidates {
        let Ok(md) = std::fs::metadata(&path) else {
            continue;
        };
        if !md.is_file() {
            continue;
        }
        let ts = md
            .created()
            .or_else(|_| md.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);
        return (Some(md.len()), ts);
    }
    (None, None)
}

#[derive(Debug, Clone, Serialize)]
pub struct ContradictionInfo {
    pub id: String,
    pub claim_a: String,
    pub claim_b: String,
    pub explanation: String,
    pub status: String,
}

/// Result of a `sleep` (consolidation) pass — the being "rests and wakes wiser":
/// unresolved contradictions are resolved by superseding the older/less-confident
/// claim, so recall returns the surviving truth.
#[derive(Debug, Clone, Serialize)]
pub struct SleepReport {
    /// Contradictions resolved this pass (loser superseded, contradiction cleared).
    pub contradictions_resolved: usize,
    /// Claims superseded by contradiction resolution.
    pub claims_superseded: usize,
    /// Old, low-confidence claims expired (dropped from active recall) — only when
    /// a stale cutoff was requested; 0 otherwise.
    pub stale_expired: usize,
}

/// §11 #26 — outcome of a Night Shift dream pass (generative abstraction in a
/// quarantined branch, verify-before-merge).
#[derive(Debug, Clone, Serialize)]
pub struct DreamReport {
    /// Insight/playbook claims synthesized this pass.
    pub insights: usize,
    /// The quarantined dream branch they were written to (empty if none).
    pub branch: String,
    /// Whether the dream was merged into main (else kept on the branch for review).
    pub merged: bool,
    /// Honest note (why nothing dreamed, what was kept/discarded).
    pub note: String,
}

/// §1 — outcome of a grounded `predict` ("what happens next"). Verified-or-
/// silent: `refused` when there's no basis or no grounded citation.
#[derive(Debug, Clone, Serialize)]
pub struct PredictReport {
    /// The grounded prediction text (with inline `[claim:id]` citations), or
    /// empty when refused.
    pub prediction: String,
    /// Model-stated confidence 0..=1 (0 when refused).
    pub confidence: f64,
    /// Recalled claim ids the prediction is grounded in (the falsifier set).
    pub citations: Vec<String>,
    /// True when the prediction was withheld (no evidence / no grounded cite).
    pub refused: bool,
    /// Honest note.
    pub note: String,
}

/// P2 — the being's HONEST developmental age. Every field is a real measured
/// signal; `developmental_age` is a function of verified capability + knowledge +
/// reconciliations, NOT wall-clock uptime (the research keystone: "age = verified
/// capability + consolidated wisdom − senescence").
#[derive(Debug, Clone, Serialize)]
pub struct AgeReport {
    /// Distinct capabilities (functions) with ≥1 successful invocation.
    pub verified_capabilities: usize,
    /// All deployed capabilities (functions), verified or not.
    pub total_capabilities: usize,
    /// Σ Wilson lower-bound success score across all learned (class, function)
    /// experience — the "verified capability mass".
    pub capability_score: f64,
    /// Claims the brain currently holds.
    pub claims: usize,
    /// Claims superseded by resolution (corrections made — wisdom from sleep).
    pub superseded_claims: usize,
    /// Honest composite = capability_score + ln(1+claims) + 0.1·superseded.
    pub developmental_age: f64,
    /// Coarse life stage derived from developmental_age + verified capabilities.
    pub stage: String,
}

/// P3 — the being's DRIVES: its behavioral posture, derived from measured maturity.
/// Curiosity decays as the being accumulates verified capability + knowledge (the
/// research curiosity-decay arc: young explores everywhere, mature focuses on
/// frontiers) — grounded in real state, NOT a wall-clock timer.
#[derive(Debug, Clone, Serialize)]
pub struct DrivesReport {
    pub stage: String,
    /// [0,1] — appetite to explore/learn. High when immature, low when mature.
    pub curiosity: f64,
    /// [0,1] — how readily the being should explore/forge new capabilities.
    pub exploration_rate: f64,
    /// [0,1] — focus on frontier/novel gaps (= 1 − curiosity).
    pub frontier_focus: f64,
    /// Human-readable behavioral guidance for this stage.
    pub recommendation: String,
}

/// P4 — one verified capability in a [`LegacyBundle`] (the genome unit).
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct LegacyCapability {
    pub name: String,
    pub body: String,
    pub language: String,
}

/// P4 — one high-confidence knowledge item in a [`LegacyBundle`].
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct LegacyClaim {
    pub statement: String,
    pub confidence: f64,
}

/// P4 — a being's VERIFIED inheritance (the world-first): the genome it passes to a
/// successor on death/handoff. Only VERIFIED capabilities + HIGH-CONFIDENCE
/// knowledge — never the raw, error-carrying memory stream. A successor that
/// inherits this provably starts from confirmed skills, not a memory dump.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct LegacyBundle {
    pub capabilities: Vec<LegacyCapability>,
    pub knowledge: Vec<LegacyClaim>,
    pub forebear_stage: String,
    pub forebear_age: f64,
}

/// P4 — result of inheriting a [`LegacyBundle`] into a successor workspace.
#[derive(Debug, Clone, Serialize)]
pub struct InheritReport {
    pub capabilities_inherited: usize,
    pub knowledge_inherited: usize,
    pub forebear_stage: String,
}

/// Workspaces that auto-mount on first reference and fall back to the primary
/// (shared) brain for unscoped reads: per-user `u_*` brains and per-agent
/// `agent_*` brains. Both get their OWN isolated graph (functions, prompts,
/// branches, memory) yet inherit the shared project brain when they don't carry
/// their own — so one agent owns everything but still uses the shared pool.
pub(crate) fn is_auto_scoped_ws(ws: &str) -> bool {
    ws.starts_with("u_") || ws.starts_with("agent_")
}

/// Compiled-prompt name that holds an agent's persona. Namespaced so it never
/// collides with the workspace `assistant` voice prompt. The persona flows
/// through the single prompt pipeline under this name.
pub(crate) fn agent_persona_prompt_name(agent_name: &str) -> String {
    format!("agent::{agent_name}::persona")
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub entities: Vec<EntitySearchHit>,
    pub claims: Vec<ClaimSearchHit>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EntitySearchHit {
    pub id: String,
    pub name: String,
    pub entity_type: String,
    pub claim_count: usize,
    pub relevance: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClaimSearchHit {
    pub id: String,
    pub statement: String,
    pub claim_type: String,
    pub confidence: f64,
    pub source_uri: String,
    pub relevance: f32,
    /// Claim-level ingestion timestamp (Unix epoch seconds), from
    /// `Claim::valid_from`. Used by the synthesizer's recency split to order
    /// conflicting facts newest-first even when they share a session date.
    /// `0` when unknown (e.g. keyword-fallback hits) — callers fall back to
    /// `session_dates` in that case.
    pub valid_from: i64,
}

// ---------------------------------------------------------------------------
// RARP / Active Engram Protocol v2 wire types.
// Spec: docs/active-engram-protocol.md §4.1 (EngramSummary) + §5.1 (ProbeAnswer).
// ID fields are wire-format `String` (the typed `thinkingroot_core::ClaimId`
// etc. live on the internal `Engram` struct in `intelligence/engram.rs`).
// ---------------------------------------------------------------------------

/// Pointer issued by `materialize_engram`. Format: `0x` + 4 hex digits
/// (16-bit pointer space, HMAC-derived per `EngramManager::next_pointer`).
pub type EngramPointer = String;

/// Returned by `materialize_engram`. Server-side rows live on the
/// `EngramManager`; the LLM holds only this summary (~30 tokens after
/// JSON serialisation) plus the pointer.
#[derive(Debug, Clone, Serialize)]
pub struct EngramSummary {
    // Identity
    pub pointer: EngramPointer,
    pub topic: String,
    /// Unix epoch seconds.
    pub created_at: f64,

    // Cluster shape
    pub entity_cluster: Vec<EntityRef>,
    pub claim_count_by_tier: TierHistogram,

    // Source authority overlay (spec §4 step 5)
    pub source_authority: Vec<SourceAuthority>,
    pub source_references: Vec<SourceReferenceEdge>,

    // Temporal (spec §4 step 6 + §4 step 8)
    pub temporal_window: (Option<f64>, Option<f64>),
    pub supersession_terminals: Vec<ClaimRef>,
    pub events_window: Vec<EventTriple>,

    // Structure (spec §4 steps 11–17)
    pub doc_tags_summary: DocTagHistogram,
    pub headings_outline: Vec<HeadingRef>,
    pub call_graph_edges: Vec<CallEdge>,
    pub test_origins: Vec<TestAnnotationRef>,
    pub code_markers: Vec<CodeMarkerRef>,
    pub code_metrics: Vec<CodeMetricRef>,
    pub quantitative_signals: Vec<QuantityRef>,

    // Truth & gaps (spec §4 steps 7, 9, 10, 19)
    pub structural_pattern_hits: Vec<PatternMatch>,
    pub gaps: Vec<KnownUnknown>,
    pub unresolved_contradictions: Vec<ContradictionRef>,
    pub derivation_roots_by_claim: HashMap<String, Vec<String>>,

    // Authorship (spec §4 step 15)
    pub git_commits_summary: GitCommitsSummary,
    pub git_blame_summary: GitBlameSummary,

    // Provenance integrity (I-4 — spec §4 step 20)
    pub stale_rows: Vec<RowRef>,

    // Privacy (spec §4 step 18)
    pub applied_clearance: Vec<thinkingroot_core::types::Sensitivity>,
    pub redacted_count: u32,
}

/// Returned by `probe_engram`. The central read-path contract — every field
/// is sourced from one of the 33 substrate tables, never invented.
#[derive(Debug, Clone, Serialize)]
pub struct ProbeAnswer {
    /// One row per concrete answer the probe produced.
    pub answer: Vec<AnswerRow>,

    // Provenance per answer row (parallel arrays — index i across all four
    // refers to the same answer row).
    pub claim_ids: Vec<String>,
    pub source_byte_spans: Vec<SourceByteSpan>,
    pub source_authority: Vec<thinkingroot_core::types::TrustLevel>,
    pub source_blake3s: Vec<String>,

    // Trust diagnostics
    pub admission_tier: thinkingroot_core::types::AdmissionTier,
    pub trial_scores: Option<TrialScores>,
    pub certificate_hash: Option<String>,
    pub grounding_score: Option<f64>,
    pub grounding_method: Option<thinkingroot_core::types::GroundingMethod>,

    // Temporal
    pub valid_window: (Option<f64>, Option<f64>),
    /// Empty when the answer claim is itself terminal in the supersession chain.
    pub superseded_by_chain: Vec<String>,

    // Lineage
    pub derivation_parents: Vec<String>,
    pub derivation_root: Option<String>,

    // Privacy
    pub sensitivity: thinkingroot_core::types::Sensitivity,

    // Origin
    pub turn_provenance: Option<TurnRef>,
    pub git_blame: Vec<GitBlameRef>,
    pub test_origin: Option<TestAnnotationRef>,

    // Cluster-aware context (drawn from the Engram, not re-queried)
    pub related_quantities: Vec<QuantityRef>,
    pub related_doc_tags: Vec<DocTagRef>,
    pub related_calls: Vec<CallEdge>,
    pub related_markers: Vec<CodeMarkerRef>,

    // Caveats surfaced by the protocol (never errors — see ProbeCaveat docs)
    pub caveats: Vec<ProbeCaveat>,
}

/// One concrete answer row. The shape varies by probe kind; for Factual
/// it's a statement, for Quantitative a numeric tuple, etc.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AnswerRow {
    Factual {
        statement: String,
    },
    Quantitative {
        metric_name: String,
        value: f64,
        unit: String,
        qualifier: String,
        is_live: bool,
    },
    Temporal {
        subject: String,
        verb: String,
        object: String,
        timestamp: f64,
        normalized_date: String,
    },
    Authorship {
        author: String,
        commit_sha: String,
        blamed_at: f64,
    },
    Structural {
        parameters_json: String,
        return_type: String,
        visibility: String,
        trait_name: String,
        parent_scope: String,
        field_types_json: String,
    },
    Relation {
        peer_claim_id: String,
        edge_kind: String,
        fragment: String,
    },
    Existential {
        present: bool,
        witness_claim_id: Option<String>,
    },
    Comparative {
        a_statement: String,
        b_statement: String,
        delta_summary: String,
    },
    Counterfactual {
        descendant_claim_id: String,
        descendant_statement: String,
        descendant_admission_tier: thinkingroot_core::types::AdmissionTier,
    },
}

/// 5 trial-verdict probe scores (spec §4 step 4 + §5.1).
/// All scores in [0.0, 1.0]; higher is better.
#[derive(Debug, Clone, Serialize)]
pub struct TrialScores {
    pub provenance_score: f64,
    pub contradiction_score: f64,
    pub predicate_score: f64,
    pub topology_score: f64,
    pub temporal_score: f64,
}

/// Caveats surfaced *inside* `ProbeAnswer` — never as typed errors.
/// Clearance violations and BLAKE3 mismatches are *always* caveats so the
/// LLM gets a typed-result-with-caveats response, not an HTTP-500-shaped
/// failure (Plan §3.3).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProbeCaveat {
    UnresolvedContradiction {
        with_claim_id: String,
        explanation: String,
    },
    StaleRow {
        content_blake3_mismatch: bool,
        /// `"verify_failed"` for hash mismatch; `"bytes_unavailable"` when
        /// the source bytes were never written to the byte-store (e.g.
        /// pre-Compile-Completeness-Contract workspaces).
        reason: String,
    },
    LowConfidence {
        measured: f64,
        threshold: f64,
    },
    DerivedFromTest {
        framework: String,
    },
    SupersededByNewerClaim {
        successor_id: String,
    },
    GapAdjacent {
        gap_id: String,
        expected_claim_type: String,
    },
    SensitivityRedaction {
        hidden_field: String,
        required_clearance: thinkingroot_core::types::Sensitivity,
    },
}

// ---------------------------------------------------------------------------
// Supporting `*Ref` shapes for EngramSummary + ProbeAnswer.
// Each is a thin row projection from a substrate table — no behaviour, no
// heap-allocated cycles. All `Serialize` for MCP wire format.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct EntityRef {
    pub id: String,
    pub canonical_name: String,
    pub entity_type: String,
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct TierHistogram {
    pub rooted: u32,
    pub attested: u32,
    pub quarantined: u32,
    pub rejected: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceAuthority {
    pub source_id: String,
    pub uri: String,
    pub trust_level: thinkingroot_core::types::TrustLevel,
    pub claim_count: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceReferenceEdge {
    pub from_source_id: String,
    pub to_source_id: String,
    pub reference_kind: String,
    pub fragment: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClaimRef {
    pub id: String,
    pub statement: String,
    pub admission_tier: thinkingroot_core::types::AdmissionTier,
}

#[derive(Debug, Clone, Serialize)]
pub struct EventTriple {
    pub subject_entity_id: String,
    pub verb: String,
    pub object_entity_id: String,
    pub timestamp: f64,
    pub normalized_date: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct DocTagHistogram {
    pub param: u32,
    pub returns: u32,
    pub throws: u32,
    pub deprecated: u32,
    pub see: u32,
    pub other: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct HeadingRef {
    pub id: String,
    pub source_id: String,
    pub level: u8,
    pub text: String,
    pub parent_heading_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CallEdge {
    pub caller_claim_id: String,
    pub callee_name: String,
    pub callee_claim_id: String,
    pub source_id: String,
    pub byte_start: u64,
    pub byte_end: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TestAnnotationRef {
    pub id: String,
    pub claim_id: String,
    pub framework: String,
    pub annotation_kind: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CodeMarkerRef {
    pub id: String,
    pub source_id: String,
    pub kind: String,
    pub text: String,
    pub in_claim_id: String,
    pub byte_start: u64,
    pub byte_end: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CodeMetricRef {
    pub source_id: String,
    pub scope: String,
    pub scope_claim_id: String,
    pub loc: u32,
    pub cyclomatic: u32,
    pub fan_in: u32,
    pub fan_out: u32,
    pub complexity_method: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct QuantityRef {
    pub claim_id: String,
    pub metric_name: String,
    pub value: f64,
    pub unit: String,
    pub qualifier: String,
    pub is_live: bool,
    pub captured_at: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocTagRef {
    pub claim_id: String,
    pub kind: String,
    pub target: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PatternMatch {
    pub pattern_id: String,
    pub entity_type: String,
    pub condition_claim_type: String,
    pub expected_claim_type: String,
    pub frequency: f64,
    pub sample_size: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct KnownUnknown {
    pub gap_id: String,
    pub entity_id: String,
    pub expected_claim_type: String,
    pub confidence: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContradictionRef {
    pub id: String,
    pub claim_a: String,
    pub claim_b: String,
    pub explanation: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct GitCommitsSummary {
    pub total_commits: u32,
    pub authors: Vec<String>,
    pub earliest_commit: Option<f64>,
    pub latest_commit: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct GitBlameSummary {
    pub authors: Vec<String>,
    pub line_count: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct GitBlameRef {
    pub source_id: String,
    pub line_start: u32,
    pub line_end: u32,
    pub commit_sha: String,
    pub author: String,
    pub blamed_at: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RowRef {
    pub table: String,
    pub source_id: String,
    pub byte_start: u64,
    pub byte_end: u64,
    pub expected_blake3: String,
    pub computed_blake3: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceByteSpan {
    pub source_id: String,
    pub byte_start: u64,
    pub byte_end: u64,
}

/// `turn_provenance` reference — populated when the answer claim was first
/// introduced in a turn within the most-recent-200 turn window of the
/// session (Plan §3.8). Outside that window we emit `Unknown`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnRef {
    Found {
        session_id: String,
        turn_number: u64,
        timestamp: f64,
    },
    Unknown {
        reason: String,
    },
}

#[derive(Debug, Clone, Default)]
pub struct ClaimFilter {
    pub claim_type: Option<String>,
    pub entity_name: Option<String>,
    pub min_confidence: Option<f64>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

/// Identity of an actor invoking a branch operation.
///
/// Replaces the historical `BranchActor` enum (T0.6,
/// `docs/branch-system-improvements.md` §T0.6) by adding two new
/// principal classes that the existing two-variant enum couldn't
/// represent:
///
/// - `Connector { connector_id, install_id }` — every connector
///   ingest (GitHub webhook, Slack archive, Notion sync) was being
///   attributed as `Agent("…")`, which conflated "claude wrote this"
///   with "alice's GitHub webhook wrote this." The connector_id +
///   install_id pair pins a specific installation.
/// - `MountConsumer { pack_hash }` — someone who `tr-mount`'d a
///   read-only `.tr` pack and is now writing into their own session
///   branch. Distinguishing them lets the maintenance task safely
///   purge their stream branches without confusing them with
///   genuine `User`/`Agent` work.
///
/// `User`/`Agent`/`System`/`Anonymous` are preserved for source
/// compatibility — every legacy call site keeps compiling with the
/// same variant names. `BranchActor` remains as a type alias so any
/// out-of-tree consumer that imported it doesn't break.
#[derive(Debug, Clone)]
pub enum Principal {
    /// No identity attached (public read paths, unauthenticated
    /// background reflect runs). Permission checks short-circuit to
    /// "allow" for an anonymous principal — owner-gating only kicks
    /// in once an identity exists.
    Anonymous,
    /// A human user. The string is the user identifier (the same
    /// value matched against `BranchPermissions::{readers, writers,
    /// mergers}` and `BranchRef::owner`).
    User(String),
    /// An AI agent. The string is the agent identifier.
    Agent(String),
    /// A connector installation: GitHub / Slack / Notion / Linear /
    /// Drive / custom HMAC webhook. `install_id` disambiguates
    /// distinct installations of the same connector. Both fields
    /// participate in the permission identity (`identity()` returns
    /// `connector_id:install_id`).
    Connector {
        connector_id: String,
        install_id: String,
    },
    /// Someone who `tr-mount`'d a `.tr` pack. `pack_hash` is the
    /// content hash of the pack (matches `Manifest.content_hash`)
    /// so the same pack mounted twice resolves to the same
    /// principal across processes.
    MountConsumer { pack_hash: String },
    /// Internal system actor — maintenance, gc, scheduled reflect.
    /// Permission checks short-circuit to "allow" so background
    /// jobs aren't blocked by branch permissions; visible in audit
    /// logs as `system`.
    System,
}

/// Backwards-compatible alias for the historical `BranchActor` name.
/// Out-of-tree code that imported `BranchActor` (e.g. an embedder)
/// continues to compile against `Principal`.
pub type BranchActor = Principal;

impl Principal {
    fn label(&self) -> String {
        match self {
            Self::Anonymous => "anonymous".to_string(),
            Self::User(user) => format!("user:{user}"),
            Self::Agent(agent) => format!("agent:{agent}"),
            Self::Connector {
                connector_id,
                install_id,
            } => format!("connector:{connector_id}:{install_id}"),
            Self::MountConsumer { pack_hash } => format!("mount:{pack_hash}"),
            Self::System => "system".to_string(),
        }
    }

    fn identity(&self) -> Option<String> {
        match self {
            Self::User(user) => Some(user.clone()),
            Self::Agent(agent) => Some(agent.clone()),
            Self::Connector {
                connector_id,
                install_id,
            } => Some(format!("{connector_id}:{install_id}")),
            Self::MountConsumer { pack_hash } => Some(format!("mount:{pack_hash}")),
            Self::Anonymous | Self::System => None,
        }
    }

    /// True when this principal is a connector — `contribute_bulk`
    /// uses this to decide whether to apply the idempotency cache.
    /// Idempotency keys from non-connector principals are ignored
    /// (no `connector_id`/`install_id` to scope them to).
    pub fn as_connector(&self) -> Option<(&str, &str)> {
        match self {
            Self::Connector {
                connector_id,
                install_id,
            } => Some((connector_id.as_str(), install_id.as_str())),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Internal workspace handle
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct WorkspaceHandle {
    name: String,
    root_path: PathBuf,
    /// Write operations (pipeline, agent contribute) go through CozoDB.
    storage: Arc<Mutex<StorageEngine>>,
    /// All read operations are served from this in-memory cache.
    /// Multiple concurrent requests read simultaneously; compile/contribute
    /// take an exclusive write lock to reload after mutating CozoDB.
    cache: Arc<RwLock<KnowledgeGraph>>,
    config: Config,
    /// LLM client for ReAct synthesis — None if provider is not configured.
    llm: Option<Arc<thinkingroot_llm::llm::LlmClient>>,
}

// ---------------------------------------------------------------------------
// Root Function capabilities (co-located compute over the cognition graph)
// ---------------------------------------------------------------------------

/// Which co-located capabilities a Root Function run may exercise. Capture
/// of the cognition graph happens through a *cloned* [`WorkspaceHandle`], so
/// every op is confined to the function's own (per-user `u_*`) workspace —
/// there is no path to another user's namespace or another project. The
/// `mcp`/`fetch`-style egress capabilities are gated separately (default
/// deny) because they cross the trust boundary; the graph capabilities are
/// default-allow because they only touch the caller's own brain.
/// Serde contract (A1): a STORED grant set is restrictive — any capability
/// missing from the stored JSON deserialises to `false` (deny), so a partial
/// grant document can only narrow, never widen. An ABSENT row falls back to
/// [`CapSet::default_own_workspace`] at the invoke site (unrestricted
/// functions keep today's behaviour).
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CapSet {
    pub can_recall: bool,
    pub can_remember: bool,
    pub can_prompt: bool,
    pub can_branch: bool,
    pub can_mcp: bool,
    /// `ctx.acquire` — deploy a new function version into the run's own
    /// workspace (self-extension). A graph write on the caller's own brain,
    /// so default-on like remember.
    pub can_acquire: bool,
    /// P2c — max concurrent runs of this function (0 = unlimited). Enforced via
    /// an in-process fair semaphore. `u32` (not a String key) so CapSet stays
    /// `Copy`. Stored in the same caps grant; default 0 keeps today's behaviour.
    pub concurrency_limit: u32,
    /// P2c — when true the limit is PER SCOPE (per-user fairness: at most
    /// `limit` runs per `u_*`); else global to the function across all scopes.
    pub concurrency_per_scope: bool,
}

impl CapSet {
    /// Default for a function running in its own workspace: graph ops on,
    /// outbound MCP off (egress is opt-in, like `fetch`). M4 wires this to
    /// per-function metadata.
    pub fn default_own_workspace() -> Self {
        Self {
            can_recall: true,
            can_remember: true,
            can_prompt: true,
            can_branch: true,
            // The external-MCP registry is the real gate (only project-owner
            // configured servers are reachable), so this can default on.
            can_mcp: true,
            // Self-extension is a graph write on the run's own workspace.
            can_acquire: true,
            // Unlimited concurrency by default (opt-in fairness).
            concurrency_limit: 0,
            concurrency_per_scope: false,
        }
    }

    /// Parse a stored grant document. `None` on malformed JSON — the caller
    /// must then FAIL CLOSED for that invoke (a corrupt grant must never
    /// silently restore all-caps).
    pub fn from_json(s: &str) -> Option<Self> {
        serde_json::from_str(s).ok()
    }

    /// All-deny — the fail-closed grant used when a stored caps document
    /// exists but cannot be parsed.
    pub fn deny_all() -> Self {
        Self::default()
    }
}

/// P2c — in-process fair semaphores for Root Function concurrency limits, keyed
/// by `"{fn}:{scope|_global}"`. A permit is held for a run's duration (RAII), so
/// `limit` bounds concurrent runs. Per-scope keys give per-user fairness. The
/// semaphore for a key is created once at its first limited run; changing a
/// function's limit takes effect on the next process start (acceptable v1).
static FN_CONCURRENCY: std::sync::OnceLock<
    tokio::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Semaphore>>>,
> = std::sync::OnceLock::new();

async fn acquire_concurrency_permit(key: String, limit: usize) -> tokio::sync::OwnedSemaphorePermit {
    let map =
        FN_CONCURRENCY.get_or_init(|| tokio::sync::Mutex::new(std::collections::HashMap::new()));
    let sem = {
        let mut guard = map.lock().await;
        guard
            .entry(key)
            .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(limit)))
            .clone()
    };
    // The semaphore is never closed, so acquire_owned cannot fail.
    sem.acquire_owned()
        .await
        .expect("fn concurrency semaphore unexpectedly closed")
}

/// One recalled claim, JSON-serialisable for the isolate boundary.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RecallHit {
    pub id: String,
    pub statement: String,
    pub score: f32,
    pub source_uri: String,
}

/// The narrow, capability-gated facade threaded into a Root Function's
/// isolate (as `Arc<FnCapabilities>` in `FnHostState`). It holds a *cloned*
/// `WorkspaceHandle` (cheap — Arcs inside) so ops recall/remember/assemble
/// against the cognition graph **without re-locking the engine**, plus the
/// resolved workspace name + this run's id (for deterministic, idempotent
/// `remember` ids). Branch/MCP fields are added in later milestones.
pub struct FnCapabilities {
    handle: WorkspaceHandle,
    /// Workspace root on disk — backs branch ops (M2).
    pub(crate) root_path: PathBuf,
    /// Resolved workspace (the per-user `u_*` namespace) this run is bound to.
    pub(crate) ws: String,
    /// This run's id — the deterministic-idempotency anchor for `remember`.
    run_id: String,
    /// External MCP registry — backs `ctx.mcp.call` (M3). The registry itself
    /// is the real gate: a function can only reach servers the PROJECT OWNER
    /// configured (`mcp-servers.toml`).
    mcp: Arc<crate::mcp::external_registry::ExternalMcpRegistry>,
    caps: CapSet,
    /// A2 — branch-scoped invoke. When set, `memory.remember` writes its
    /// claim into THIS branch's graph (a copy-on-write clone of main at
    /// fork) instead of main, so a function's cognitive side effects are
    /// quarantined for verify-before-keep (forge) and dreaming. `None` =
    /// write to main (the default; fully backward compatible). Interior-mutable
    /// (RwLock) so `ctx.transaction` can switch the write target MID-RUN — the
    /// memory-saga: writes route to a fork, then commit-merges or rolls back.
    target_branch: std::sync::RwLock<Option<String>>,
    /// Root Function SOTA P1 — the PRIMARY brain's handle, where durable
    /// timers (`ctx.scheduleSelf`/`ctx.schedule`) are persisted so the engine
    /// ticker fires them even when this (per-user) scope is unmounted. `None`
    /// → timers fall back to this run's own handle (e.g. running in main).
    primary_handle: Option<WorkspaceHandle>,
}

/// A2 — options for a branch-scoped Root Function invocation.
/// `default()` (no branch, no dry-run) reproduces the original invoke
/// behavior exactly.
#[derive(Debug, Clone, Default)]
pub struct InvokeBranchOpts {
    /// Route this run's `memory.remember` writes to this branch (forked
    /// from main if absent). The caller later merges or abandons it.
    pub target_branch: Option<String>,
    /// Run on a fresh ephemeral branch that is abandoned after the run —
    /// a true dry run (side effects happen in isolation, then vanish).
    pub dry_run: bool,
    /// P1b-ii — retry attempt number (0/1 = first run; bumped by the durable
    /// retry timer). Surfaced as `ctx.attempt`. 0 is treated as 1.
    pub attempt: u32,
    /// P2b — cross-call exactly-once: a fresh prior result stored under this key
    /// short-circuits the run (duplicate webhook / client retry).
    pub idempotency_key: Option<String>,
}

impl FnCapabilities {
    pub fn new(
        handle: WorkspaceHandle,
        ws: String,
        run_id: String,
        mcp: Arc<crate::mcp::external_registry::ExternalMcpRegistry>,
        caps: CapSet,
    ) -> Self {
        let root_path = handle.root_path.clone();
        Self {
            handle,
            root_path,
            ws,
            run_id,
            mcp,
            caps,
            target_branch: std::sync::RwLock::new(None),
            primary_handle: None,
        }
    }

    /// A2 — route `memory.remember` writes to `branch`'s graph instead of
    /// main. Builder so the (long) `new` call sites stay unchanged.
    pub fn with_target_branch(mut self, branch: Option<String>) -> Self {
        self.target_branch = std::sync::RwLock::new(branch);
        self
    }

    /// P1 — the primary brain's handle, where durable timers are persisted.
    pub fn with_primary_handle(mut self, h: Option<WorkspaceHandle>) -> Self {
        self.primary_handle = h;
        self
    }

    /// `ctx.scheduleSelf(when, input)` / `ctx.schedule(fn, when, input)` —
    /// register a durable future invocation. Persisted in the PRIMARY brain
    /// keyed by THIS run's scope, so the engine ticker fires a fresh run of
    /// `fn_name` in this scope at `fire_at` even if the scope is unmounted. A
    /// non-empty `dedupe_key` replaces any prior pending timer with the same
    /// (scope, fn, key) — the per-user proactive re-arm pattern. Returns the
    /// timer id.
    pub async fn schedule_timer(
        &self,
        fn_name: &str,
        fire_at: f64,
        input_json: &str,
        dedupe_key: &str,
    ) -> Result<String> {
        if !self.caps.can_remember {
            return Err(Error::Config(
                "capability 'schedule' is not granted to this function".to_string(),
            ));
        }
        let handle = self.primary_handle.as_ref().unwrap_or(&self.handle);
        let id = ulid::Ulid::new().to_string();
        let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
        let timer = thinkingroot_graph::root_function::FnTimer {
            id: id.clone(),
            scope: self.ws.clone(),
            fn_name: fn_name.to_string(),
            kind: "schedule".to_string(),
            run_id: String::new(),
            fire_at,
            input_json: input_json.to_string(),
            dedupe_key: dedupe_key.to_string(),
            status: "pending".to_string(),
            created_at: now,
        };
        let storage = handle.storage.lock().await;
        storage.graph.cancel_timer_dedupe(&self.ws, fn_name, dedupe_key)?;
        storage.graph.put_timer(&timer)?;
        Ok(id)
    }

    /// `ctx.emit(name, payload)` — deliver an event to waiters in THIS scope, or
    /// buffer it (1h TTL) if none are waiting yet. Matching waiters are marked
    /// `ready` (the engine ticker resumes them — never re-enters synchronously).
    /// Returns the number of waiters delivered to (0 → buffered).
    pub async fn emit_event(&self, event_name: &str, payload_json: &str) -> Result<u32> {
        if !self.caps.can_remember {
            return Err(Error::Config(
                "capability 'emit' is not granted to this function".to_string(),
            ));
        }
        let handle = self.primary_handle.as_ref().unwrap_or(&self.handle);
        let storage = handle.storage.lock().await;
        let waiters = storage.graph.find_pending_waiters(&self.ws, event_name)?;
        if waiters.is_empty() {
            let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
            let id = ulid::Ulid::new().to_string();
            storage
                .graph
                .put_event_buffer(&id, &self.ws, event_name, payload_json, now + 3600.0)?;
            return Ok(0);
        }
        let mut delivered = 0u32;
        for mut w in waiters {
            w.status = "ready".to_string();
            w.payload_json = payload_json.to_string();
            storage.graph.put_waiter(&w)?;
            delivered += 1;
        }
        Ok(delivered)
    }

    /// `ctx.memory.recall(query, k)` — semantic recall scoped to this run's
    /// own workspace (per-user isolation is the workspace boundary itself).
    pub async fn recall(&self, query: &str, k: usize) -> Result<Vec<RecallHit>> {
        if !self.caps.can_recall {
            return Err(Error::Config(
                "capability 'memory.recall' is not granted to this function".to_string(),
            ));
        }
        let k = k.clamp(1, 100);
        let empty = std::collections::HashSet::new();
        let res = QueryEngine::search_scoped_on(&self.handle, query, k, &empty).await?;
        Ok(res
            .claims
            .into_iter()
            .map(|c| RecallHit {
                id: c.id,
                statement: c.statement,
                score: c.relevance,
                source_uri: c.source_uri,
            })
            .collect())
    }

    // ── ctx.workspace — durable compute over the COMPILED workspace ──
    // Read-only code-graph queries against the run's own captured workspace
    // handle (no network hop). Gated on `can_recall` (same read-only graph
    // surface as memory.recall). The graph is fixed for the run, so these are
    // deterministic on replay — no journaling needed (mirrors recall).

    /// `ctx.workspace.search(keyword)` — code entities by symbol.
    pub async fn ws_search_entity(
        &self,
        keyword: &str,
    ) -> Result<Vec<thinkingroot_graph::codegraph::EntityHit>> {
        if !self.caps.can_recall {
            return Err(Error::Config(
                "capability 'workspace' is not granted to this function".to_string(),
            ));
        }
        let storage = self.handle.storage.lock().await;
        storage.graph.search_entity(keyword)
    }

    /// `ctx.workspace.traverse(...)` — walk the code graph from a symbol (or
    /// claim id). Unknown symbol → empty (honest).
    pub async fn ws_traverse(
        &self,
        start: &str,
        by_claim_id: bool,
        dir: thinkingroot_graph::codegraph::TraversalDirection,
        hops: u32,
        edges: &[thinkingroot_graph::codegraph::EdgeKind],
    ) -> Result<Vec<thinkingroot_graph::codegraph::TraversedNode>> {
        if !self.caps.can_recall {
            return Err(Error::Config(
                "capability 'workspace' is not granted to this function".to_string(),
            ));
        }
        let storage = self.handle.storage.lock().await;
        let start_id = if by_claim_id {
            start.to_string()
        } else {
            match storage.graph.search_entity(start)?.into_iter().next() {
                Some(h) => h.claim_id,
                None => return Ok(Vec::new()),
            }
        };
        storage.graph.traverse_graph(&start_id, dir, hops.min(16), edges)
    }

    /// `ctx.workspace.repoMap(budget, query)` — PageRank repo-map.
    pub async fn ws_repo_map(
        &self,
        budget_tokens: usize,
        query: Option<&str>,
    ) -> Result<crate::intelligence::repo_map::RepoMap> {
        if !self.caps.can_recall {
            return Err(Error::Config(
                "capability 'workspace' is not granted to this function".to_string(),
            ));
        }
        let storage = self.handle.storage.lock().await;
        crate::intelligence::repo_map::build_repo_map(&storage.graph, budget_tokens, query)
    }

    /// `ctx.memory.remember(fact, opts)` — persist a claim into this run's
    /// workspace graph + vector index so it is recallable. The claim id is
    /// **deterministic** in `(run_id, seq, statement)`, so a crash *after*
    /// the write but *before* the step journal persists replays to the SAME
    /// id — `insert_claim`/vector `upsert` are `:put`/upsert, so the replay
    /// is a no-op rather than a duplicate (exactly-once for the effect).
    pub async fn remember(
        &self,
        statement: &str,
        claim_type: &str,
        confidence: f64,
        seq: u64,
    ) -> Result<String> {
        use thinkingroot_core::types::{ContentHash, SourceType, TrustLevel};

        if !self.caps.can_remember {
            return Err(Error::Config(
                "capability 'memory.remember' is not granted to this function".to_string(),
            ));
        }
        if statement.trim().is_empty() {
            return Err(Error::Config("memory.remember: empty fact".to_string()));
        }

        // Deterministic claim id: blake3(run_id|seq|statement) → u128 → Ulid.
        let key = format!("{}|{}|{}", self.run_id, seq, statement);
        let digest = blake3::hash(key.as_bytes());
        let mut b = [0u8; 16];
        b.copy_from_slice(&digest.as_bytes()[..16]);
        let det = ulid::Ulid::from(u128::from_le_bytes(b));
        let claim_id = thinkingroot_core::types::ClaimId::from_ulid(det);

        let source_uri = format!("rootfn://{}/{}", self.handle.name, self.run_id);
        let source = thinkingroot_core::Source::new(source_uri.clone(), SourceType::ChatMessage)
            .with_trust(TrustLevel::Untrusted)
            .with_hash(ContentHash(format!("rootfn-{}-{}", self.run_id, seq)));

        let mut claim = thinkingroot_core::Claim::new(
            statement.to_string(),
            parse_claim_type_str(claim_type),
            source.id,
            thinkingroot_core::types::WorkspaceId::new(),
        )
        .with_confidence(confidence.clamp(0.0, 1.0))
        .with_extraction_tier(thinkingroot_core::types::ExtractionTier::AgentInferred);
        claim.id = claim_id; // force determinism (idempotent on replay)

        let ctype = claim_type.to_string();
        let conf = confidence.clamp(0.0, 1.0);

        // ── A2: branch-scoped write ──────────────────────────────────────
        // When this run is branch-scoped, the claim lands on the branch's
        // own `graph.db` (a CoW clone of main at fork) — NOT the mounted
        // main storage, and NOT the main read cache. This is the engine's
        // own branch-write path (identical to `contribute_bulk(branch=…)`):
        // open the branch graph directly via `resolve_data_dir`. Vector
        // indexing is deferred to compile/merge (same as contribute-bulk),
        // so semantic recall of these claims is keyword/graph-only until
        // the branch is compiled or merged — the graph write itself is
        // durable and immediately recallable by traversal. We do NOT touch
        // the main cache, so a concurrent main read never sees branch state.
        let active_branch = self.target_branch.read().unwrap().clone();
        if let Some(branch) = active_branch.as_deref() {
            let branch_graph_dir =
                thinkingroot_branch::snapshot::resolve_data_dir(&self.root_path, Some(branch))
                    .join("graph");
            let bg = thinkingroot_graph::graph::GraphStore::init(&branch_graph_dir)
                .map_err(|e| Error::GraphStorage(format!("remember(branch={branch}): {e}")))?;
            bg.insert_source(&source)?;
            bg.insert_claim(&claim)?;
            bg.link_claim_to_source(&claim.id.to_string(), &source.id.to_string())?;
            let _ = ctype; // (parity with main path; branch write is graph-only)
            let _ = conf;
            return Ok(claim.id.to_string());
        }

        {
            let mut storage = self.handle.storage.lock().await;
            storage.graph.insert_source(&source)?;
            storage.graph.insert_claim(&claim)?;
            storage
                .graph
                .link_claim_to_source(&claim.id.to_string(), &source.id.to_string())?;
            // Vector-index the claim so semantic recall finds it without a
            // recompile (mirrors the contribute_claims write path). ONNX
            // embedding is sync; on the isolate's current-thread runtime
            // `run_blocking` runs it inline (no block_in_place panic).
            let meta = format!("claim|{}|{}|{}|{}", claim.id, ctype, conf, source_uri);
            let vkey = format!("claim:{}", claim.id);
            let stmt = statement.to_string();
            // Non-fatal (honesty contract, mirrors contribute_claims): the graph
            // write is already durable. A missing/uninitialised embedder degrades
            // *semantic* recall to keyword recall — it must not fail the write.
            if let Err(e) = run_blocking(|| storage.vector.upsert(&vkey, &stmt, &meta)) {
                tracing::warn!(
                    "remember: vector upsert failed (claim durable; semantic recall degraded \
                     to keyword until reindex): {e}"
                );
            }
            // Reload the read cache while holding storage so no write slips in
            // between the CozoDB write and the cache refresh — otherwise a
            // subsequent recall in the same run would miss the just-written
            // claim (the honesty contract).
            let new_cache = KnowledgeGraph::load_from_graph(&storage.graph)
                .map_err(|e| Error::GraphStorage(format!("remember: cache reload failed: {e}")))?;
            *self.handle.cache.write().await = new_cache;
        }
        Ok(claim.id.to_string())
    }

    /// `ctx.prompt(name, vars)` — assemble a compiled prompt template (M2).
    pub async fn assemble_prompt(
        &self,
        name: &str,
        vars: &std::collections::BTreeMap<String, String>,
    ) -> Result<String> {
        if !self.caps.can_prompt {
            return Err(Error::Config(
                "capability 'prompt' is not granted to this function".to_string(),
            ));
        }
        let storage = self.handle.storage.lock().await;
        storage.graph.assemble_prompt(name, vars)
    }

    /// `ctx.branch.fork(name, parent?)` — create an isolated branch of this
    /// workspace's cognition graph (for safe experiments / A-B). Branch ops
    /// are free functions over `root_path` — no engine lock involved.
    /// Idempotent-ish: the branch name is the natural key.
    pub async fn branch_fork(&self, name: &str, parent: Option<&str>) -> Result<BranchForkResult> {
        if !self.caps.can_branch {
            return Err(Error::Config(
                "capability 'branch' is not granted to this function".to_string(),
            ));
        }
        let parent = parent.unwrap_or("main");
        // If the branch already exists (replay/resume), return it instead of
        // erroring — keeps `ctx.branch.fork` idempotent across re-execution.
        if let Ok(branches) = thinkingroot_branch::list_branches(&self.root_path)
            && let Some(b) = branches.iter().find(|b| b.name == name)
        {
            return Ok(BranchForkResult {
                name: b.name.clone(),
                slug: b.slug.clone(),
                parent: b.parent.clone(),
            });
        }
        let b = thinkingroot_branch::create_branch(
            &self.root_path,
            name,
            parent,
            Some(format!("forked by root function (run {})", self.run_id)),
        )
        .await
        .map_err(|e| Error::Config(format!("branch.fork failed: {e}")))?;
        Ok(BranchForkResult { name: b.name, slug: b.slug, parent: b.parent })
    }

    /// `ctx.branch.merge(source, target?)` — merge a branch into the target
    /// (default `main`). Returns a summary of what merged + what needs review.
    pub async fn branch_merge(
        &self,
        source: &str,
        target: Option<&str>,
    ) -> Result<BranchMergeResult> {
        if !self.caps.can_branch {
            return Err(Error::Config(
                "capability 'branch' is not granted to this function".to_string(),
            ));
        }
        let target = target.unwrap_or("main");
        let merged_by = thinkingroot_core::types::MergedBy::Agent {
            agent_id: format!("rootfn:{}", self.run_id),
        };
        let diff = thinkingroot_branch::merge_into(
            &self.root_path,
            source,
            target,
            merged_by,
            false, // force
            false, // propagate_deletions
        )
        .await
        .map_err(|e| Error::Config(format!("branch.merge failed: {e}")))?;
        Ok(BranchMergeResult {
            from_branch: diff.from_branch,
            to_branch: diff.to_branch,
            new_claims: diff.new_claims.len() as u64,
            new_entities: diff.new_entities.len() as u64,
            auto_resolved: diff.auto_resolved.len() as u64,
            needs_review: diff.needs_review.len() as u64,
        })
    }

    // ── Memory-saga (ctx.transaction) — durable transactional memory ────────
    // begin → (writes route to a forked branch) → commit (merge into main) OR
    // rollback (abandon). Built on the existing fork/merge; the only new bit is
    // switching the run's write target mid-run (the RwLock target_branch).

    /// `ctx.transaction` begin — fork `branch` (idempotent) and route this run's
    /// subsequent `memory.remember` writes to it. Requires `can_branch`.
    pub async fn tx_begin(&self, branch: &str) -> Result<()> {
        if !self.caps.can_branch {
            return Err(Error::Config(
                "capability 'branch' is not granted to this function (ctx.transaction)".to_string(),
            ));
        }
        self.branch_fork(branch, None).await?;
        *self.target_branch.write().unwrap() = Some(branch.to_string());
        Ok(())
    }

    /// `ctx.transaction` commit — merge the active tx branch into main, then
    /// route writes back to main. No-op if no tx is active.
    pub async fn tx_commit(&self) -> Result<()> {
        let active = self.target_branch.read().unwrap().clone();
        if let Some(branch) = active {
            self.branch_merge(&branch, None).await?;
            *self.target_branch.write().unwrap() = None;
        }
        Ok(())
    }

    /// `ctx.transaction` rollback — abandon the active tx branch (its writes are
    /// discarded; the branch graph is left unmerged) and route writes back to
    /// main. Sync (just clears the target).
    pub fn tx_rollback(&self) {
        *self.target_branch.write().unwrap() = None;
    }

    /// `ctx.mcp.call(tool, args)` — invoke a tool on a project-configured
    /// external MCP server (e.g. `"sendgrid::send"`). The registry is the
    /// gate: only servers the project owner installed are reachable; an
    /// unknown tool returns an honest "not found" error.
    pub async fn mcp_call(
        &self,
        tool: &str,
        args: serde_json::Value,
    ) -> Result<crate::mcp::client::McpToolResult> {
        if !self.caps.can_mcp {
            return Err(Error::Config(
                "capability 'mcp.call' is not granted to this function".to_string(),
            ));
        }
        // Derive per-user identity from the bound workspace (`u_<id>` →
        // `id`).  OAuth connectors require this; non-OAuth connectors
        // ignore it.  A Root Function running in a per-user scope ALWAYS
        // has a `u_*` workspace — a function running in `main` has `None`.
        let user_id: Option<String> = self.ws.strip_prefix("u_").map(|rest| {
            rest.split("__").next().unwrap_or(rest).to_string()
        });
        match self.mcp.dispatch(tool, args, user_id.as_deref()).await {
            Some(Ok(r)) => Ok(r),
            Some(Err(e)) => Err(Error::Config(format!("mcp.call '{tool}' failed: {e}"))),
            None => Err(Error::Config(format!(
                "mcp.call: tool '{tool}' not found — no configured external MCP server \
                 provides it (install one via the Console / mcp_server_install first)"
            ))),
        }
    }

    /// `ctx.acquire(spec)` host side — deploy a new (versioned) function into
    /// THIS run's own workspace. Co-located self-extension: a function grows
    /// the brain a new capability at runtime, then can invoke it via
    /// `ctx.mcp.call("function::<name>", input)` (the two-way MCP path). The
    /// new version never clobbers prior ones (`{name}@{version}`), so this is
    /// safe to roll back. Confined to the caller's own workspace by the handle.
    pub async fn acquire(&self, name: &str, body: &str, language: &str) -> Result<String> {
        if !self.caps.can_acquire {
            return Err(Error::Config(
                "capability 'acquire' is not granted to this function".to_string(),
            ));
        }
        if name.trim().is_empty() {
            return Err(Error::Config("ctx.acquire: name is required".to_string()));
        }
        let lang = if language.trim().is_empty() { "js" } else { language };
        let storage = self.handle.storage.lock().await;
        let f = storage.graph.put_function(name, body, lang)?;
        Ok(f.id)
    }

    /// `ctx.predict(question, top_k?)` — grounded, falsifier-gated "what happens
    /// next" over THIS run's own workspace memory. Recall → the run's workspace
    /// LLM (passed in) → verified-or-silent (refuse if no memory, the model
    /// declines, or the prediction cites no recalled claim). Mirrors
    /// `QueryEngine::predict` using the SAME `intelligence::` helpers, but over
    /// the captured handle — so it never re-locks the engine mid-run.
    pub async fn predict(
        &self,
        llm: &thinkingroot_llm::llm::LlmClient,
        question: &str,
        top_k: usize,
    ) -> Result<PredictReport> {
        if !self.caps.can_recall {
            return Err(Error::Config(
                "capability 'predict' (recall) is not granted to this function".to_string(),
            ));
        }
        let empty = std::collections::HashSet::new();
        let res = QueryEngine::search_scoped_on(&self.handle, question, top_k.clamp(1, 50), &empty)
            .await?;
        let claims: Vec<(String, String)> =
            res.claims.iter().map(|c| (c.id.clone(), c.statement.clone())).collect();
        let refused = |note: &str| PredictReport {
            prediction: String::new(),
            confidence: 0.0,
            citations: vec![],
            refused: true,
            note: note.to_string(),
        };
        if claims.is_empty() {
            return Ok(refused("no relevant memory to predict from"));
        }
        let prompt = crate::intelligence::predict::build_predict_prompt(question, &claims);
        let out = llm.chat(crate::intelligence::predict::PREDICT_SYSTEM, &prompt).await?;
        if crate::intelligence::predict::is_refusal(&out) {
            return Ok(refused("insufficient evidence to predict"));
        }
        let recalled: std::collections::HashSet<&str> =
            claims.iter().map(|(id, _)| id.as_str()).collect();
        let grounded: Vec<String> = crate::intelligence::citations::parse_all_markers(&out)
            .into_iter()
            .filter(|id| recalled.contains(id.as_str()))
            .collect();
        if grounded.is_empty() {
            return Ok(refused("prediction had no grounded citation — withheld"));
        }
        let confidence = crate::intelligence::predict::parse_confidence(&out).unwrap_or(0.5);
        Ok(PredictReport {
            prediction: out.trim().to_string(),
            confidence,
            citations: grounded,
            refused: false,
            note: "grounded prediction".to_string(),
        })
    }

    /// `ctx.dream(opts?)` — the Night-Shift verb: sample this workspace's memory,
    /// abstract insights via the run's workspace LLM, write them to a QUARANTINED
    /// `dream/<ulid>` branch (honest provenance), and merge into main only when
    /// `auto_merge` (else left for review). Built on the run's own fork/remember/
    /// merge primitives — the verify-before-merge quarantine holds, no engine lock.
    pub async fn dream(
        &self,
        llm: &thinkingroot_llm::llm::LlmClient,
        max_claims: usize,
        max_insights: usize,
        auto_merge: bool,
    ) -> Result<DreamReport> {
        if !self.caps.can_recall || !self.caps.can_remember || !self.caps.can_branch {
            return Err(Error::Config(
                "ctx.dream needs recall + remember + branch capabilities".to_string(),
            ));
        }
        let claims: Vec<String> = {
            let cache = self.handle.cache.read().await;
            cache
                .all_claims()
                .filter(|c| !c.statement.trim().is_empty())
                .take(max_claims.clamp(1, 200))
                .map(|c| c.statement.clone())
                .collect()
        };
        if claims.len() < 3 {
            return Ok(DreamReport {
                insights: 0,
                branch: String::new(),
                merged: false,
                note: "not enough claims to dream over (need ≥3)".to_string(),
            });
        }
        let prompt = crate::intelligence::dream::build_dream_prompt(&claims);
        let out = llm.chat(crate::intelligence::dream::DREAM_SYSTEM, &prompt).await?;
        let insights =
            crate::intelligence::dream::parse_dream_insights(&out, max_insights.clamp(1, 20));
        if insights.is_empty() {
            return Ok(DreamReport {
                insights: 0,
                branch: String::new(),
                merged: false,
                note: "no insights synthesized this pass".to_string(),
            });
        }
        // Quarantined dream branch. Route insight writes there, then restore the
        // prior write target (so a dream inside a ctx.transaction doesn't clobber it).
        let branch = format!("dream/{}", ulid::Ulid::new());
        self.branch_fork(&branch, Some("main")).await?;
        let prior = self.target_branch.read().unwrap().clone();
        *self.target_branch.write().unwrap() = Some(branch.clone());
        // Dream insights live in a separate deterministic seq space so their ids
        // never collide with the function's own remember() calls.
        let base_seq = 1_000_000u64;
        for (i, s) in insights.iter().enumerate() {
            let _ = self.remember(s, "insight", 0.6, base_seq + i as u64).await;
        }
        *self.target_branch.write().unwrap() = prior;
        let merged = if auto_merge {
            self.branch_merge(&branch, Some("main")).await.is_ok()
        } else {
            false
        };
        Ok(DreamReport {
            insights: insights.len(),
            branch,
            merged,
            note: if merged {
                "insights merged into main".to_string()
            } else {
                "insights kept on the dream branch (review before merge)".to_string()
            },
        })
    }
}

/// Result of `ctx.branch.fork`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BranchForkResult {
    pub name: String,
    pub slug: String,
    pub parent: String,
}

/// Result of `ctx.branch.merge` — a token-cheap summary for the function.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BranchMergeResult {
    pub from_branch: String,
    pub to_branch: String,
    pub new_claims: u64,
    pub new_entities: u64,
    pub auto_resolved: u64,
    pub needs_review: u64,
}

// ---------------------------------------------------------------------------
// Artifact type <-> filename mapping
// ---------------------------------------------------------------------------

/// Run a blocking closure from within an async context.
///
/// On the multi-threaded tokio runtime (production server) this defers to
/// `block_in_place`, telling the scheduler that this worker is about to
/// block so it can repark other tasks onto other workers — preventing a
/// reactor stall under concurrent load (the 10K-VU scenario).
///
/// On the single-threaded runtime (the default for `#[tokio::test]`) there
/// are no other workers to repark onto, so the closure is just invoked
/// directly. `block_in_place` cannot be used there — it panics.
#[inline]
fn run_blocking<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    match tokio::runtime::Handle::current().runtime_flavor() {
        // The original commit (e115307) had `run_blocking(f)` here —
        // a recursive self-call instead of `block_in_place(f)`. That
        // tail-recurses without bound on the multi-thread runtime
        // (production `root serve`), blowing the tokio worker stack
        // on the very first `search` / `search_scoped` call. Tests
        // are unaffected because `#[tokio::test]` defaults to the
        // single-thread flavor and falls through to the `_ => f()`
        // arm. Reproduces with `curl /api/v1/ws/{ws}/ask` against
        // any workspace, including empty ones.
        tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(f),
        _ => f(),
    }
}

fn artifact_filename(artifact_type: &str) -> Option<&'static str> {
    match artifact_type {
        "architecture-map" => Some("architecture-map.md"),
        "contradiction-report" => Some("contradiction-report.md"),
        "decision-log" => Some("decision-log.md"),
        "task-pack" => Some("task-pack.md"),
        "agent-brief" => Some("agent-brief.md"),
        "runbook" => Some("runbook.md"),
        "health-report" => Some("health-report.md"),
        "entity-pages" => Some("entities"),
        _ => None,
    }
}

/// All known artifact type keys.
const ARTIFACT_TYPES: &[&str] = &[
    "architecture-map",
    "contradiction-report",
    "decision-log",
    "task-pack",
    "agent-brief",
    "runbook",
    "health-report",
    "entity-pages",
    "gap-report",
];

/// Artifacts rendered on-demand from live graph state rather than read
/// from a pre-written file in `.thinkingroot/artifacts/`. These are
/// always "available" — there is no disk file to check.
fn is_dynamic_artifact(artifact_type: &str) -> bool {
    matches!(artifact_type, "gap-report")
}

// ---------------------------------------------------------------------------
// Pagination helper
// ---------------------------------------------------------------------------

fn apply_pagination<T>(vec: &mut Vec<T>, offset: Option<usize>, limit: Option<usize>) {
    if let Some(off) = offset {
        if off >= vec.len() {
            vec.clear();
        } else if off > 0 {
            *vec = vec.split_off(off);
        }
    }
    if let Some(lim) = limit {
        vec.truncate(lim);
    }
}

// ---------------------------------------------------------------------------
// QueryEngine
// ---------------------------------------------------------------------------

pub struct QueryEngine {
    workspaces: HashMap<String, WorkspaceHandle>,
    /// #1 — the primary (shared) workspace: the first non-`u_` workspace
    /// mounted (the project's shared brain). Tracked explicitly because
    /// `HashMap` iteration order is non-deterministic — per-user capsules
    /// fall back to THIS workspace for the shared system prompt.
    primary_ws: Option<String>,
    /// Process-wide cache of open branch `GraphStore` handles, keyed by
    /// `(workspace_root, branch_name)`. Every serve-crate code path that
    /// reads or writes a branch's graph.db goes through this cache to
    /// preserve the "one DbInstance per branch" invariant (see
    /// `branch_cache` module docs for why).
    branch_engines: Arc<crate::branch_cache::BranchEngineCache>,
    /// SOTA Lever 3 — process-wide Observer cache for conversation
    /// memory. Per-session buffers live inside; staged observations
    /// drain through [`Self::flush_observations`] into the workspace's
    /// witness substrate as `conversation::observation@v1` rows.
    observer: Arc<crate::intelligence::observer::Observer>,
}

impl Default for QueryEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl QueryEngine {
    /// Create a new empty QueryEngine with no mounted workspaces.
    pub fn new() -> Self {
        Self {
            workspaces: HashMap::new(),
            primary_ws: None,
            branch_engines: Arc::new(crate::branch_cache::BranchEngineCache::default_cache()),
            observer: Arc::new(crate::intelligence::observer::Observer::new()),
        }
    }

    /// Create a new empty QueryEngine with an explicit branch-cache config.
    /// Used by callers that want to tune `max_entries`/`ttl_secs`/`disabled`
    /// (e.g. long-lived servers pulling config from workspace TOML).
    pub fn with_branch_cache_config(cfg: &thinkingroot_core::config::BranchCacheConfig) -> Self {
        Self {
            workspaces: HashMap::new(),
            primary_ws: None,
            branch_engines: Arc::new(crate::branch_cache::BranchEngineCache::new(cfg)),
            observer: Arc::new(crate::intelligence::observer::Observer::new()),
        }
    }

    /// Shared handle to the process-wide Observer. Callers that want
    /// to record chat turns into long-term substrate call
    /// `observer().record_turn(...)`; staged observations later drain
    /// into the witness substrate via [`Self::flush_observations`].
    pub fn observer(&self) -> Arc<crate::intelligence::observer::Observer> {
        self.observer.clone()
    }

    /// Drain staged observations + (optionally) emit a reflection for
    /// `session_id` and persist everything to the workspace's witness
    /// substrate. Returns the count of newly-inserted Witness rows
    /// (observations + reflections, after the substrate's content-
    /// addressed dedup may collapse some).
    ///
    /// Designed to be called periodically (e.g. background sweep
    /// every N seconds) AND at session end. Both call sites are safe
    /// — the underlying `:put witnesses` is idempotent on
    /// content-derived ids.
    ///
    /// On any failure, the staged observations remain in the buffer
    /// for the next flush attempt rather than being dropped — partial
    /// flushes are honest-incomplete, never silent-lossy. Returns
    /// `Ok(0)` when no observations are staged.
    pub async fn flush_observations(
        &self,
        ws: &str,
        session_id: &str,
    ) -> Result<usize> {
        let observer = self.observer.clone();
        let staged = observer.take_staged(session_id);
        if staged.is_empty() {
            return Ok(0);
        }
        let handle = self
            .workspaces
            .get(ws)
            .ok_or_else(|| Error::EntityNotFound(format!("workspace '{ws}' not mounted")))?;
        let workspace_id =
            crate::intelligence::observer::observation_workspace_id(ws);
        let source_id = crate::intelligence::observer::observation_source_id(session_id);
        let now = chrono::Utc::now();

        let mut to_insert: Vec<thinkingroot_core::types::Witness> = staged
            .iter()
            .map(|s| {
                crate::intelligence::observer::materialise_observation_witness(
                    s,
                    workspace_id,
                    source_id,
                    now,
                )
            })
            .collect();

        // Emit a reflection when the session crosses the Observer's
        // reflect threshold. The reflection's inputs reference the
        // observations by WitnessRef, so the substrate carries the
        // turn → observation → reflection provenance chain.
        if observer.should_reflect(session_id) {
            if let Some(reflection) =
                crate::intelligence::observer::materialise_reflection_witness(
                    &to_insert,
                    workspace_id,
                    source_id,
                    now,
                )
            {
                to_insert.push(reflection);
            }
        }

        let inserted = to_insert.len();
        // Hold the storage lock just long enough for the batch insert.
        let storage = handle.storage.lock().await;
        if let Err(e) = storage
            .graph
            .insert_witnesses_batch(&to_insert)
        {
            // Rollback in-memory state — re-stage so a retry can pick
            // them up. Without this, a transient storage failure would
            // silently lose conversation memory.
            for s in staged {
                observer.restage(s);
            }
            return Err(e);
        }
        tracing::info!(
            target: "observer",
            workspace = ws,
            session = session_id,
            inserted,
            "flushed observations + reflection to witness substrate"
        );
        Ok(inserted)
    }

    /// Test/telemetry accessor for the branch engine cache.
    pub fn branch_engines(&self) -> &crate::branch_cache::BranchEngineCache {
        &self.branch_engines
    }

    /// Clone of the `Arc<BranchEngineCache>` — handed out to background
    /// tasks (e.g. the stream-cleanup task) that need to invalidate cache
    /// entries without borrowing the whole engine.
    pub fn branch_engines_arc(&self) -> Arc<crate::branch_cache::BranchEngineCache> {
        self.branch_engines.clone()
    }

    fn branch_ref_for_root(
        root: &std::path::Path,
        branch_name: &str,
    ) -> Result<Option<thinkingroot_core::BranchRef>> {
        if branch_name == "main" {
            return Ok(None);
        }
        let refs_dir = root.join(".thinkingroot-refs");
        let registry = thinkingroot_branch::branch::BranchRegistry::load_or_create(&refs_dir)?;
        Ok(registry.get(branch_name).cloned())
    }

    fn ensure_branch_permission(
        actor: &Principal,
        branch_ref: Option<&thinkingroot_core::BranchRef>,
        action: &str,
    ) -> Result<()> {
        let Some(branch_ref) = branch_ref else {
            return Ok(());
        };
        let Some(identity) = actor.identity() else {
            return Ok(());
        };
        let identity_ref = identity.as_str();

        // Tag branches are immutable except for read access (T2.5 gate
        // landed alongside T0.6). Any write/merge/rebase/delete attempt
        // is a hard reject — even by the owner.
        if matches!(branch_ref.kind, thinkingroot_core::BranchKind::Tag { .. })
            && action != "read_branch"
        {
            return Err(Error::PermissionDenied {
                actor: actor.label(),
                action: format!("{action} (branch is an immutable Tag)"),
            });
        }

        // Phase C.1 (2026-05-17) — Main branch is human-only for
        // `merge_branch` / `delete_branch` / `rebase_branch`. Agents
        // never promote work to `main` automatically; the
        // Stream → Topic → (human merge) → Main lifecycle is the
        // architectural invariant established by Phases A + B.
        //
        // This gate fires even when `branch_ref.owner.is_none()`
        // (typical fresh workspace), because the existing
        // owner-less short-circuit would otherwise let any actor
        // reach `main`. Anonymous + System principals short-
        // circuited above via the `identity().is_none()` arm —
        // System retains the ability to perform background
        // bookkeeping merges; Anonymous can't reach this code path
        // through any authenticated route in practice. Human /
        // Connector / MountConsumer continue past this gate to the
        // normal permission-list check below.
        if matches!(branch_ref.kind, thinkingroot_core::BranchKind::Main)
            && matches!(actor, Principal::Agent(_))
            && matches!(action, "merge_branch" | "delete_branch" | "rebase_branch")
        {
            return Err(Error::PermissionDenied {
                actor: actor.label(),
                action: format!(
                    "{action} on Main (agents must route through topic branches — \
                     merge stream → topic, then a human merges topic → main)"
                ),
            });
        }

        if branch_ref
            .owner
            .as_deref()
            .is_some_and(|owner| owner == identity_ref)
        {
            return Ok(());
        }

        let allowed = match action {
            "read_branch" => branch_ref
                .permissions
                .readers
                .iter()
                .any(|v| v == identity_ref),
            "write_branch" => branch_ref
                .permissions
                .writers
                .iter()
                .any(|v| v == identity_ref),
            "merge_branch" | "delete_branch" | "rebase_branch" => branch_ref
                .permissions
                .mergers
                .iter()
                .any(|v| v == identity_ref),
            _ => false,
        };

        if allowed || branch_ref.owner.is_none() {
            Ok(())
        } else {
            Err(Error::PermissionDenied {
                actor: actor.label(),
                action: action.to_string(),
            })
        }
    }

    /// Mount a workspace by name, opening the `.thinkingroot/` data directory,
    /// loading the config and storage engine, and warming the in-memory cache.
    pub async fn mount(&mut self, name: String, root_path: PathBuf) -> Result<()> {
        let data_dir = root_path.join(".thinkingroot");
        if !data_dir.exists() {
            return Err(Error::Config(format!(
                "no .thinkingroot directory found at {}",
                root_path.display()
            )));
        }

        // One-time migration: move any legacy `.thinkingroot-{slug}/` sibling
        // dirs to the new nested layout `.thinkingroot/branches/{slug}/`.
        // Hard-fails on error so we don't open the workspace with a registry
        // that points at the new layout while branch data still lives at the
        // legacy paths (would silently surface those branches as missing).
        match thinkingroot_branch::migrate_legacy_layout(&root_path) {
            Ok(0) => {}
            Ok(n) => tracing::info!(
                "migrated {n} legacy branch director{} to .thinkingroot/branches/",
                if n == 1 { "y" } else { "ies" }
            ),
            Err(e) => {
                return Err(Error::Config(format!(
                    "branch layout migration from legacy `.thinkingroot-{{slug}}/` to nested \
                     `.thinkingroot/branches/{{slug}}/` failed for workspace at '{}': {e} \
                     (legacy branches are still on disk; mount aborted to avoid surfacing them \
                     as missing — fix the underlying error and re-mount)",
                    root_path.display()
                )));
            }
        }

        let config = Config::load_merged(&root_path)?;
        let storage = StorageEngine::init(&data_dir).await?;
        let cache = KnowledgeGraph::load_from_graph(&storage.graph)?;

        if storage.vector.is_empty() && cache.entity_count() > 0 {
            tracing::warn!(
                "Workspace '{}' contains {} entities in graph.db, but the vector index is missing or empty. \
                 This usually indicates an interrupted compilation. \
                 3D visualization and semantic search will degrade gracefully. \
                 Run `root compile` to rebuild the missing embeddings.",
                name,
                cache.entity_count()
            );
        }
        // Reuse the already-warm LLM client across remounts (e.g. the
        // post-compile cache reload). The client is a stateless HTTP wrapper
        // keyed only on provider config — it does not depend on the compiled
        // graph — so rebuilding it on every remount just discards a warm Azure
        // connection and forces the next /ask (especially /ask/stream) to pay a
        // cold reconnect, which is the dominant cause of the first-request
        // stall and the streaming "single claim" fallback. Provider/config
        // changes arrive via a fresh container spawn, not a remount, so keeping
        // the existing client here is safe.
        let llm = if let Some(existing) = self.workspaces.get(&name).and_then(|h| h.llm.clone()) {
            tracing::debug!("LLM client reused (warm) for workspace '{name}' on remount");
            Some(existing)
        } else {
            match thinkingroot_llm::llm::LlmClient::new(&config.llm).await {
                Ok(client) => {
                    tracing::debug!("LLM client initialised for workspace '{name}'");
                    Some(Arc::new(client))
                }
                Err(e) => {
                    tracing::debug!("LLM not configured for workspace '{name}' (non-fatal): {e}");
                    None
                }
            }
        };

        // #1 — the first non-per-user workspace mounted is the shared brain.
        if self.primary_ws.is_none() && !name.starts_with("u_") {
            self.primary_ws = Some(name.clone());
        }
        self.workspaces.insert(
            name.clone(),
            WorkspaceHandle {
                name,
                root_path,
                storage: Arc::new(Mutex::new(storage)),
                cache: Arc::new(RwLock::new(cache)),
                config,
                llm,
            },
        );

        Ok(())
    }

    /// Mount a workspace using an explicit data directory instead of the default
    /// `.thinkingroot/` subdirectory. Used by `root serve --branch` to mount a
    /// branch-scoped data directory such as `.thinkingroot-feature-x/`.
    pub async fn mount_with_data_dir(
        &mut self,
        name: String,
        root_path: PathBuf,
        data_dir: PathBuf,
    ) -> Result<()> {
        if !data_dir.exists() {
            return Err(Error::Config(format!(
                "data directory not found: {}",
                data_dir.display()
            )));
        }

        match thinkingroot_branch::migrate_legacy_layout(&root_path) {
            Ok(0) => {}
            Ok(n) => tracing::info!(
                "migrated {n} legacy branch director{} to .thinkingroot/branches/",
                if n == 1 { "y" } else { "ies" }
            ),
            Err(e) => {
                return Err(Error::Config(format!(
                    "branch layout migration from legacy `.thinkingroot-{{slug}}/` to nested \
                     `.thinkingroot/branches/{{slug}}/` failed for workspace at '{}': {e} \
                     (legacy branches are still on disk; mount aborted to avoid surfacing them \
                     as missing — fix the underlying error and re-mount)",
                    root_path.display()
                )));
            }
        }

        let config = Config::load_merged(&root_path)?;
        let storage = StorageEngine::init(&data_dir).await?;
        let cache = KnowledgeGraph::load_from_graph(&storage.graph)?;

        if storage.vector.is_empty() && cache.entity_count() > 0 {
            tracing::warn!(
                "Workspace '{}' contains {} entities in graph.db, but the vector index is missing or empty. \
                 This usually indicates an interrupted compilation. \
                 3D visualization and semantic search will degrade gracefully. \
                 Run `root compile` to rebuild the missing embeddings.",
                name,
                cache.entity_count()
            );
        }
        let llm = match thinkingroot_llm::llm::LlmClient::new(&config.llm).await {
            Ok(client) => Some(Arc::new(client)),
            Err(_) => None,
        };

        // #1 — the first non-per-user workspace mounted is the shared brain.
        if self.primary_ws.is_none() && !name.starts_with("u_") {
            self.primary_ws = Some(name.clone());
        }
        self.workspaces.insert(
            name.clone(),
            WorkspaceHandle {
                name,
                root_path,
                storage: Arc::new(Mutex::new(storage)),
                cache: Arc::new(RwLock::new(cache)),
                config,
                llm,
            },
        );

        Ok(())
    }

    /// Unmount a previously mounted workspace.
    pub fn unmount(&mut self, name: &str) -> Result<()> {
        self.workspaces
            .remove(name)
            .ok_or_else(|| Error::EntityNotFound(format!("workspace '{name}' not mounted")))?;
        Ok(())
    }

    /// True when `ws` is currently mounted.
    pub fn is_mounted(&self, ws: &str) -> bool {
        self.workspaces.contains_key(ws)
    }

    /// The primary (shared) workspace name — the first-mounted workspace, which
    /// is the project's shared brain. Per-user workspaces fall back to it for
    /// the system-prompt frame they don't carry themselves (two-tier brain).
    pub fn primary_ws_name(&self) -> Option<String> {
        // The explicitly-tracked shared workspace; fall back to any mounted
        // non-per-user workspace if (somehow) unset.
        self.primary_ws.clone().or_else(|| {
            self.workspaces
                .keys()
                .find(|k| !k.starts_with("u_"))
                .cloned()
        })
    }

    /// #1 — lazily auto-create + mount a per-user workspace `u_{user_id}` the
    /// first time it's referenced, giving each end-user a physically isolated
    /// brain (its own CozoDB). No-op if already mounted. Only `u_`-prefixed
    /// names auto-mount (the gateway confines scoped requests to exactly that
    /// namespace), so a typo can't spawn junk workspaces. Per-user data dirs
    /// live OUTSIDE the shared workspace root (sibling `.thinkingroot-users/`)
    /// so the shared source-tree watcher never picks them up.
    pub async fn get_or_mount_user_ws(&mut self, ws: &str) -> Result<()> {
        if self.workspaces.contains_key(ws) {
            return Ok(());
        }
        if !is_auto_scoped_ws(ws) {
            return Err(Error::EntityNotFound(format!(
                "workspace '{ws}' is not mounted and is not an auto-mountable per-user namespace"
            )));
        }
        let anchor = self
            .primary_ws_name()
            .and_then(|p| self.workspaces.get(&p))
            .map(|h| h.root_path.clone())
            .ok_or_else(|| {
                Error::Config("no primary workspace mounted to anchor per-user workspaces".into())
            })?;
        // Prefer a SIBLING of the workspace root (keeps per-user brains out of
        // the shared source tree) — this is what local dev gets, where the
        // workspace root has a writable parent.
        let base = match anchor.parent() {
            Some(p) => p.join(".thinkingroot-users"),
            None => anchor.join(".thinkingroot-users"),
        };
        let primary = base.join(ws);
        let root = if std::fs::create_dir_all(primary.join(".thinkingroot")).is_ok() {
            primary
        } else {
            // Cloud: the workspace root IS the mounted data volume (e.g.
            // `/workspace`), whose parent (`/`) isn't writable by the engine
            // uid. Fall back to a writable dot-dir UNDER the volume. It's a
            // dot-dir, so it stays out of source ingestion and the orphan
            // watcher (which only watches `.thinkingroot/`).
            let alt = anchor.join(".thinkingroot-users").join(ws);
            std::fs::create_dir_all(alt.join(".thinkingroot")).map_err(|e| {
                Error::Config(format!("create per-user workspace dir for '{ws}': {e}"))
            })?;
            alt
        };
        self.mount(ws.to_string(), root).await?;
        // Slice 1 (2026-06-17): a composite per-(user×agent) scope inherits the
        // agent's OWN brain `agent_Y`; ensure it's mounted so the inheritance
        // chain can resolve the agent's functions/prompts. Best-effort and
        // depth-1 (an `agent_Y` brain is never itself composite, so no further
        // recursion); a missing/unmountable agent brain just means nothing to
        // inherit there. Boxed because this recurses into the same async fn.
        if let Some(idx) = ws.find("__agent_") {
            let agent_brain = format!("agent_{}", &ws[idx + "__agent_".len()..]);
            if agent_brain != ws && !self.workspaces.contains_key(&agent_brain) {
                let _ = Box::pin(self.get_or_mount_user_ws(&agent_brain)).await;
            }
        }
        Ok(())
    }

    /// List all currently mounted workspaces with summary counts.
    /// Served from in-memory cache — O(1) per workspace.
    pub async fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        let mut result = Vec::with_capacity(self.workspaces.len());
        for handle in self.workspaces.values() {
            let cache = handle.cache.read().await;
            let (source_count, claim_count, entity_count) = cache.counts();
            result.push(WorkspaceInfo {
                name: handle.name.clone(),
                path: handle.root_path.display().to_string(),
                entity_count,
                claim_count,
                source_count,
            });
        }
        Ok(result)
    }

    /// List all entities in a workspace.
    /// Served from in-memory cache — O(n) where n = entity count, zero disk I/O.
    pub async fn list_entities(&self, ws: &str) -> Result<Vec<EntityInfo>> {
        let handle = self.get_workspace(ws)?;
        let cache = handle.cache.read().await;

        let mut result = Vec::with_capacity(cache.entity_count());
        for id in cache.entities_ordered() {
            if let Some(e) = cache.entity_by_id(id) {
                result.push(EntityInfo {
                    id: e.id.clone(),
                    name: e.canonical_name.clone(),
                    // Normalize TitleCase storage to the snake_case wire
                    // form `EntityType` advertises via serde — the graph
                    // layer historically wrote `format!("{:?}")` which
                    // bypassed the rename_all contract.
                    entity_type: thinkingroot_core::types::EntityType::normalize_storage(
                        &e.entity_type,
                    ),
                    claim_count: cache.entity_claim_count(&e.id),
                });
            }
        }

        Ok(result)
    }

    /// Get detailed information about a single entity by name (case-insensitive).
    /// Served from in-memory cache — O(1) name lookup + O(k) claim/relation fetches.
    pub async fn get_entity(&self, ws: &str, name: &str) -> Result<EntityDetail> {
        let handle = self.get_workspace(ws)?;
        let cache = handle.cache.read().await;

        let entity = cache
            .find_entity_by_name(name)
            .ok_or_else(|| Error::EntityNotFound(name.to_string()))?;

        let claims: Vec<ClaimInfo> = cache
            .claims_for_entity(&entity.id)
            .into_iter()
            .map(cached_claim_to_info)
            .collect();

        let relations: Vec<RelationInfo> = cache
            .relations_for_entity(&entity.canonical_name)
            .into_iter()
            .map(|r| RelationInfo {
                target: r.to_name.clone(),
                relation_type: thinkingroot_core::types::RelationType::normalize_storage(
                    &r.relation_type,
                ),
                strength: r.strength,
            })
            .collect();

        Ok(EntityDetail {
            id: entity.id.clone(),
            name: entity.canonical_name.clone(),
            entity_type: thinkingroot_core::types::EntityType::normalize_storage(
                &entity.entity_type,
            ),
            aliases: entity.aliases.clone(),
            claims,
            relations,
        })
    }

    /// List claims with optional filtering by type, entity, min confidence, limit, offset.
    /// Served from in-memory cache.
    pub async fn list_claims(&self, ws: &str, filter: ClaimFilter) -> Result<Vec<ClaimInfo>> {
        let handle = self.get_workspace(ws)?;
        let cache = handle.cache.read().await;

        // Entity-scoped path: O(1) name lookup + O(k) claim scan.
        if let Some(ref entity_name) = filter.entity_name {
            let entity = match cache.find_entity_by_name(entity_name) {
                Some(e) => e,
                None => return Ok(Vec::new()),
            };

            let mut claims: Vec<ClaimInfo> = cache
                .claims_for_entity(&entity.id)
                .into_iter()
                .filter(|c| {
                    let type_ok = filter
                        .claim_type
                        .as_ref()
                        .is_none_or(|t| t.eq_ignore_ascii_case(&c.claim_type));
                    let conf_ok = filter.min_confidence.is_none_or(|min| c.confidence >= min);
                    type_ok && conf_ok
                })
                .map(cached_claim_to_info)
                .collect();

            // Sort newest-first: most recent event_date wins over older claims.
            // This ensures knowledge-update answers reflect the current state, not
            // the first time something was mentioned.  Claims without event_date
            // (event_date = None) sort to the end (treated as oldest).
            claims.sort_by(|a, b| {
                b.event_date
                    .unwrap_or(0.0)
                    .partial_cmp(&a.event_date.unwrap_or(0.0))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

            apply_pagination(&mut claims, filter.offset, filter.limit);
            return Ok(claims);
        }

        // Type-filtered or full-listing path.
        let raw: Vec<&CachedClaim> = if let Some(ref ct) = filter.claim_type {
            cache.claims_of_type(ct)
        } else {
            cache.all_claims().collect()
        };

        let mut claims: Vec<ClaimInfo> = raw
            .into_iter()
            .filter(|c| filter.min_confidence.is_none_or(|min| c.confidence >= min))
            .map(cached_claim_to_info)
            .collect();

        apply_pagination(&mut claims, filter.offset, filter.limit);
        Ok(claims)
    }

    /// List Witnesses in a workspace, optionally capped at `limit`.
    /// Goes directly through the graph (no cache layer yet for the
    /// Witness Mesh substrate — v1.1 will add one if profiling shows
    /// it's needed). Each workspace owns its own CozoDB instance, so
    /// the returned set is workspace-scoped by construction.
    pub async fn list_witnesses(
        &self,
        ws: &str,
        limit: Option<usize>,
    ) -> Result<Vec<thinkingroot_core::types::Witness>> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        storage.graph.list_witnesses(limit)
    }

    // ─── Compiled Prompt substrate ──────────────────────────────────
    // Thin delegations to the workspace `GraphStore`'s prompt API
    // (`thinkingroot_graph::prompt`). Same lock-then-graph pattern as
    // the witness methods above.

    /// Write a new template version; returns the stored row.
    pub async fn prompt_put_template(
        &self,
        ws: &str,
        name: &str,
        template_text: &str,
    ) -> Result<thinkingroot_graph::prompt::PromptTemplate> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        storage.graph.prompt_put_template(name, template_text)
    }

    /// Latest version of a single template, or `None`.
    pub async fn prompt_get_latest(
        &self,
        ws: &str,
        name: &str,
    ) -> Result<Option<thinkingroot_graph::prompt::PromptTemplate>> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        storage.graph.prompt_get_latest(name)
    }

    /// The latest version of every distinct template.
    pub async fn prompt_list_latest(
        &self,
        ws: &str,
    ) -> Result<Vec<thinkingroot_graph::prompt::PromptTemplate>> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        storage.graph.prompt_list_latest()
    }

    /// Every stored version of `name`, ascending.
    pub async fn prompt_list_versions(
        &self,
        ws: &str,
        name: &str,
    ) -> Result<Vec<thinkingroot_graph::prompt::PromptTemplate>> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        storage.graph.prompt_list_versions(name)
    }

    /// Assemble the latest version of `name` with `vars`.
    pub async fn assemble_prompt(
        &self,
        ws: &str,
        name: &str,
        vars: &std::collections::BTreeMap<String, String>,
    ) -> Result<String> {
        let _ = self.get_workspace(ws)?; // preserve unmounted-scope error
        // Resolve the prompt through the inheritance chain (self → agent brain
        // → shared): the first brain that HAS the named template assembles it.
        // `prompt_get_latest` distinguishes "not in this brain" from a real
        // template error in the brain that owns it (Slice 1: prompt inheritance).
        for brain in self.inheritance_chain(ws) {
            if let Ok(handle) = self.get_workspace(&brain) {
                let storage = handle.storage.lock().await;
                if storage.graph.prompt_get_latest(name)?.is_some() {
                    return storage.graph.assemble_prompt(name, vars);
                }
            }
        }
        Err(Error::Template(format!(
            "prompt template `{name}` not found"
        )))
    }

    /// A2 — rank an ARBITRARY tool catalog by semantic relevance to a query,
    /// using the workspace embedder. Unlike `route` (which ranks deployed Root
    /// Functions), this ranks any `{name, description}` list the caller supplies,
    /// so a host (e.g. MrGuy) can shrink the model's VISIBLE tool list to the
    /// top-k relevant tools per turn — the real token win. Fail-open: if the
    /// embedder is unavailable it returns the first k unchanged (never hides all).
    pub async fn rank_tool_catalog(
        &self,
        ws: &str,
        query: &str,
        tools: Vec<(String, String)>,
        top_k: usize,
    ) -> Result<Vec<String>> {
        if tools.is_empty() {
            return Ok(Vec::new());
        }
        let k = top_k.max(1);
        let names: Vec<String> = tools.iter().map(|(n, _)| n.clone()).collect();
        let handle = self.get_workspace(ws)?;
        let mut storage = handle.storage.lock().await;
        let mut texts: Vec<String> = Vec::with_capacity(tools.len() + 1);
        texts.push(query.to_string());
        for (name, desc) in &tools {
            texts.push(format!("{name}: {desc}"));
        }
        let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
        let embs = match run_blocking(|| storage.vector.embed_texts(&refs)) {
            Ok(e) if e.len() == texts.len() => e,
            _ => return Ok(names.into_iter().take(k).collect()),
        };
        let cos = |a: &[f32], b: &[f32]| -> f32 {
            let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
            let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
            let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
            if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na * nb) }
        };
        let q = &embs[0];
        let mut scored: Vec<(f32, String)> = names
            .iter()
            .enumerate()
            .map(|(i, n)| (cos(q, &embs[i + 1]), n.clone()))
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);
        Ok(scored.into_iter().map(|(_, n)| n).collect())
    }

    /// Compile a witness-grounded **context capsule** for one turn: the
    /// assembled system prompt + top-k grounded claims + workspace brief
    /// + experience-routed tools. This is the low-token, fast path that
    /// replaces dumping raw context + every tool schema at the LLM.
    ///
    /// Cached in `capsule_cache` keyed by [`thinkingroot_graph::capsule::capsule_key`]
    /// (which hashes the full query, so a hit never returns the wrong
    /// grounding) and evicted by `invalidate_capsules_for` when any
    /// dependency claim changes — the witness mesh as a cache DAG.
    /// Branch orientation for a capsule: the synced node for a durable branch
    /// (parent + live status), or a minimal "you are here" for an ephemeral
    /// branch so an agent on a stream branch still knows its context. `None`
    /// when there's no branch (main).
    async fn branch_context_for(&self, ws: &str, branch: Option<&str>) -> Option<BranchContext> {
        let name = branch?;
        let node = self
            .list_branch_nodes(ws)
            .await
            .ok()
            .and_then(|nodes| nodes.into_iter().find(|b| b.name == name));
        Some(match node {
            Some(b) => BranchContext {
                name: b.name,
                parent: b.parent,
                status: b.status,
            },
            None => BranchContext {
                name: name.to_string(),
                parent: None,
                status: "active".to_string(),
            },
        })
    }

    pub async fn compile_capsule(&self, ws: &str, spec: CapsuleSpec) -> Result<CompiledCapsule> {
        use thinkingroot_graph::capsule::{capsule_key, classify_query, CapsuleCacheRow};

        let branch_ref = spec.branch.as_deref();
        let query_class = classify_query(&spec.query);

        // Resolve the prompt version so the cache key is stable + correct.
        let prompt_version = {
            let handle = self.get_workspace(ws)?;
            let storage = handle.storage.lock().await;
            storage
                .graph
                .prompt_latest_version(&spec.prompt_name)?
                .unwrap_or(0)
        };
        let key = capsule_key(
            &spec.prompt_name,
            prompt_version,
            branch_ref,
            &spec.query,
            &spec.vars,
        );

        // Warm path: return the cached capsule verbatim (sub-ms).
        {
            let handle = self.get_workspace(ws)?;
            let storage = handle.storage.lock().await;
            if let Some(row) = storage.graph.capsule_cache_get(&key)? {
                if let Ok(mut cap) = serde_json::from_str::<CompiledCapsule>(&row.capsule_json) {
                    cap.cache_hit = true;
                    return Ok(cap);
                }
            }
        }

        // Cold path: build the frame (intent-routed tools) + ground the query.
        let (system, brief, tools, _v) = self
            .build_capsule_frame(
                ws,
                &spec.prompt_name,
                &spec.vars,
                branch_ref,
                spec.max_tools,
                &spec.query,
            )
            .await?;
        let session_id = spec
            .session_id
            .clone()
            .unwrap_or_else(|| format!("capsule:{key}"));
        let (grounded_claims, deps) = self
            .ground_query(ws, &spec.query, &session_id, spec.top_k, branch_ref)
            .await?;
        let token_estimate = estimate_capsule_tokens(&system, &grounded_claims, &tools);
        let branch_context = self.branch_context_for(ws, branch_ref).await;

        let capsule = CompiledCapsule {
            system,
            grounded_claims,
            brief,
            tools,
            token_estimate,
            query_class: query_class.clone(),
            cache_hit: false,
            frame_warm: false,
            branch_context,
        };

        // Persist + record the provenance set for causal invalidation.
        let capsule_json = serde_json::to_string(&capsule)
            .map_err(|e| Error::Serialization(format!("serialize capsule: {e}")))?;
        let row = CapsuleCacheRow {
            key,
            capsule_json,
            prompt_name: spec.prompt_name.clone(),
            prompt_version,
            branch: branch_ref.unwrap_or("").to_string(),
            query_class,
            token_estimate: token_estimate as i64,
            created_at: chrono::Utc::now().timestamp_millis() as f64 / 1000.0,
        };
        {
            let handle = self.get_workspace(ws)?;
            let storage = handle.storage.lock().await;
            storage.graph.capsule_cache_put(&row, &deps)?;
        }
        Ok(capsule)
    }

    /// M4 — build the query-INDEPENDENT capsule frame: assembled system
    /// prompt + workspace brief + experience-routed tools + the prompt's
    /// current version. Tools rank on the (constant) query-input *shape*,
    /// so the frame is stable across a session's turns — that's what makes
    /// it cacheable on the session in [`Self::compile_capsule_session`].
    async fn build_capsule_frame(
        &self,
        ws: &str,
        prompt_name: &str,
        vars: &std::collections::BTreeMap<String, String>,
        branch: Option<&str>,
        max_tools: usize,
        // Intent hint for semantic tool routing. Empty string = no hint
        // (warm-frame prefetch, before any query exists) → experience/shape
        // ranking only, keeping the prewarmed frame query-independent.
        route_intent: &str,
    ) -> Result<(String, WorkspaceSummary, Vec<String>, i64)> {
        // Two-tier brain: the system prompt is SHARED. A per-user workspace
        // doesn't carry it, so source the prompt from this ws if present, else
        // from the primary/shared workspace. Grounding + brief stay on `ws`
        // (the user's own data) — that's the isolation boundary.
        let user_has_prompt = {
            let handle = self.get_workspace(ws)?;
            let storage = handle.storage.lock().await;
            storage.graph.prompt_latest_version(prompt_name)?.is_some()
        };
        let prompt_ws = if user_has_prompt {
            ws.to_string()
        } else {
            self.primary_ws_name().unwrap_or_else(|| ws.to_string())
        };
        let prompt_deployed = {
            let handle = self.get_workspace(&prompt_ws)?;
            let storage = handle.storage.lock().await;
            storage.graph.prompt_latest_version(prompt_name)?
        };
        let prompt_version = prompt_deployed.unwrap_or(0);
        // Default-template fallback: a fresh workspace has no compiled prompt
        // deployed, so the capsule would otherwise 500 ("prompt template not
        // found"). Ship a minimal default system frame so the compiled-context
        // path works out-of-box; deploying a prompt of this name overrides it.
        let system = if prompt_deployed.is_some() {
            self.assemble_prompt(&prompt_ws, prompt_name, vars).await?
        } else {
            "You are a helpful assistant grounded in the user's cognition graph. \
             Use the provided memories and routed tools when relevant; never fabricate."
                .to_string()
        };
        let brief = self.get_workspace_brief_branched(ws, branch).await?;
        // Capability router: rank tools by semantic relevance to the intent
        // (fused with learned experience) when a hint is present; with an empty
        // hint (prefetch) it falls back to experience/shape ranking, keeping a
        // prewarmed frame query-independent.
        let tools = self.route_capabilities(ws, branch, route_intent, max_tools).await?;
        Ok((system, brief, tools, prompt_version))
    }

    /// M4 — the per-turn query-DEPENDENT part: hybrid retrieval → grounded
    /// claims + their provenance deps. The only work a warm-frame turn pays.
    async fn ground_query(
        &self,
        ws: &str,
        query: &str,
        session_id: &str,
        top_k: usize,
        branch: Option<&str>,
    ) -> Result<(Vec<CapsuleClaimRef>, Vec<(String, String)>)> {
        let retrieval = self
            .hybrid_retrieve(
                ws,
                RetrievalRequest {
                    query_text: query.to_string(),
                    typed_predicates: vec![],
                    session_id: session_id.to_string(),
                    clearance: vec![thinkingroot_core::types::Sensitivity::Public],
                    top_k,
                    time_window: None,
                    // L1 — the capsule grounds the agent on the hot path, so it
                    // must be FAST. Measured: the cross-encoder is ~110ms/doc on
                    // CPU (~1.1s for a pool) vs ~4ms without it, and it only
                    // REORDERS already-relevant fused hits (the agent reads all
                    // top_k anyway). So grounding skips the cross-encoder —
                    // ~250x faster for a negligible ordering change. The public
                    // /search/hybrid endpoint keeps tiered rerank for callers who
                    // need best-ranked output.
                    scoring_profile: ScoringProfile {
                        use_cross_encoder: false,
                        ..ScoringProfile::default()
                    },
                    require_certificate: false,
                    include_test_origin: false,
                    include_quarantined: false,
                    require_provenance_verified: false,
                    now: None,
                    scoped_claim_ids: None,
                    // Read-your-own-writes: ground from the live session branch
                    // (CoW copy of main + this session's contributed claims) so
                    // the capsule cites what was just said before the merge.
                    branch: branch.map(str::to_string),
                },
                None,
            )
            .await?;
        let mut grounded_claims = Vec::with_capacity(retrieval.hits.len());
        let mut deps: Vec<(String, String)> = Vec::with_capacity(retrieval.hits.len());
        for hit in &retrieval.hits {
            // C1 — never ground the agent on a SUPERSEDED claim. Retrieval keeps
            // superseded hits (annotated with a caveat) for transparency, but the
            // capsule must surface only the live fact, so a consolidated "March ->
            // June" supersession doesn't resurface the stale March claim.
            if hit
                .superseded_by_chain
                .iter()
                .any(|s| !s.is_empty() && s != &hit.claim_id)
            {
                continue;
            }
            grounded_claims.push(CapsuleClaimRef {
                claim_id: hit.claim_id.clone(),
                statement: hit.statement.clone(),
                claim_type: hit.claim_type.clone(),
                source_uri: hit.source_uri.clone(),
            });
            deps.push((hit.claim_id.clone(), "claim".to_string()));
        }
        Ok((grounded_claims, deps))
    }

    /// M4 — live streaming-branch compile: reuse the session's warm frame
    /// (system+brief+tools) when it matches the active branch + prompt, so
    /// the turn only pays for retrieval. On a miss it builds the frame and
    /// caches it on the session. This is what makes the streaming branch
    /// "live": after the first turn, every subsequent turn skips the
    /// prompt-assembly + brief + tool-routing work. Invalidated by
    /// [`SessionContext::invalidate_warm_frame`] on contribute.
    pub async fn compile_capsule_session(
        &self,
        ws: &str,
        sessions: &crate::intelligence::session::SessionStore,
        session_id: &str,
        spec: CapsuleSpec,
    ) -> Result<CompiledCapsule> {
        use thinkingroot_graph::capsule::{capsule_key, classify_query, CapsuleCacheRow};
        let t_start = std::time::Instant::now();
        let branch_ref = spec.branch.as_deref();
        let query_class = classify_query(&spec.query);

        // Current prompt version — a bump invalidates a warm frame.
        let current_version = {
            let handle = self.get_workspace(ws)?;
            let storage = handle.storage.lock().await;
            storage.graph.prompt_latest_version(&spec.prompt_name)?.unwrap_or(0)
        };

        // L1 — exact-repeat capsule cache (the session path previously bypassed
        // it, so cache_hit was always false). Serve a verbatim cached capsule for
        // the SAME (prompt,version,branch,query,vars) in sub-ms, skipping frame +
        // grounding. The full cross-encoder is irrelevant here.
        let key = capsule_key(
            &spec.prompt_name,
            current_version,
            branch_ref,
            &spec.query,
            &spec.vars,
        );
        {
            let handle = self.get_workspace(ws)?;
            let storage = handle.storage.lock().await;
            if let Some(row) = storage.graph.capsule_cache_get(&key)? {
                if let Ok(mut cap) = serde_json::from_str::<CompiledCapsule>(&row.capsule_json) {
                    cap.cache_hit = true;
                    tracing::info!(
                        elapsed_ms = t_start.elapsed().as_millis() as u64,
                        "capsule_session: CACHE HIT"
                    );
                    return Ok(cap);
                }
            }
        }
        let t_after_cache = std::time::Instant::now();

        // Try the session warm frame.
        let warm = {
            let store = sessions.lock().await;
            store.get(session_id).and_then(|s| s.warm_frame.clone()).filter(|f| {
                f.branch.as_deref() == branch_ref
                    && f.prompt_name == spec.prompt_name
                    && f.prompt_version == current_version
            })
        };

        let (system, brief, tools, frame_warm) = match warm {
            Some(f) => {
                let brief: WorkspaceSummary = serde_json::from_str(&f.brief_json)
                    .map_err(|e| Error::Serialization(format!("warm brief: {e}")))?;
                (f.system, brief, f.tools, true)
            }
            None => {
                let (system, brief, tools, version) = self
                    .build_capsule_frame(
                        ws,
                        &spec.prompt_name,
                        &spec.vars,
                        branch_ref,
                        spec.max_tools,
                        &spec.query,
                    )
                    .await?;
                // Cache the frame on the session for the next turn.
                let brief_json = serde_json::to_string(&brief)
                    .map_err(|e| Error::Serialization(format!("frame brief: {e}")))?;
                let frame = crate::intelligence::session::WarmFrame {
                    branch: spec.branch.clone(),
                    prompt_name: spec.prompt_name.clone(),
                    prompt_version: version,
                    system: system.clone(),
                    brief_json,
                    tools: tools.clone(),
                };
                let mut store = sessions.lock().await;
                let s = store
                    .entry(session_id.to_string())
                    .or_insert_with(|| crate::intelligence::session::SessionContext::new(session_id, ws));
                s.warm_frame = Some(frame);
                (system, brief, tools, false)
            }
        };

        let t_after_frame = std::time::Instant::now();

        // Per-turn: ground the query (the only work a warm turn pays). Ground
        // from the session branch (read-your-own-writes) when one is set.
        let (grounded_claims, deps) =
            self.ground_query(ws, &spec.query, session_id, spec.top_k, branch_ref).await?;
        let t_after_ground = std::time::Instant::now();
        let token_estimate = estimate_capsule_tokens(&system, &grounded_claims, &tools);
        let branch_context = self.branch_context_for(ws, branch_ref).await;

        let capsule = CompiledCapsule {
            system,
            grounded_claims,
            brief,
            tools,
            token_estimate,
            query_class: query_class.clone(),
            cache_hit: false,
            frame_warm,
            branch_context,
        };

        // L1 — phase timing (find where the ms go) + persist for exact-repeat hits.
        tracing::info!(
            frame_warm,
            cache_check_ms = t_after_cache.duration_since(t_start).as_millis() as u64,
            frame_ms = t_after_frame.duration_since(t_after_cache).as_millis() as u64,
            ground_ms = t_after_ground.duration_since(t_after_frame).as_millis() as u64,
            finalize_ms = t_after_ground.elapsed().as_millis() as u64,
            total_ms = t_start.elapsed().as_millis() as u64,
            "capsule_session: phase timing"
        );
        if let Ok(capsule_json) = serde_json::to_string(&capsule) {
            let row = CapsuleCacheRow {
                key,
                capsule_json,
                prompt_name: spec.prompt_name.clone(),
                prompt_version: current_version,
                branch: branch_ref.unwrap_or("").to_string(),
                query_class,
                token_estimate: token_estimate as i64,
                created_at: chrono::Utc::now().timestamp_millis() as f64 / 1000.0,
            };
            let handle = self.get_workspace(ws)?;
            let storage = handle.storage.lock().await;
            let _ = storage.graph.capsule_cache_put(&row, &deps);
        }
        Ok(capsule)
    }

    /// M4 — Slow-Thinker prefetch: warm the session's capsule frame ahead
    /// of the first query (e.g. on session/branch open or while the user is
    /// still typing) so the first turn is already frame-warm. Best-effort.
    pub async fn prefetch_capsule_frame(
        &self,
        ws: &str,
        sessions: &crate::intelligence::session::SessionStore,
        session_id: &str,
        prompt_name: &str,
        vars: &std::collections::BTreeMap<String, String>,
        branch: Option<&str>,
        max_tools: usize,
    ) -> Result<()> {
        let (system, brief, tools, version) = self
            .build_capsule_frame(ws, prompt_name, vars, branch, max_tools, "")
            .await?;
        let brief_json = serde_json::to_string(&brief)
            .map_err(|e| Error::Serialization(format!("prefetch brief: {e}")))?;
        let frame = crate::intelligence::session::WarmFrame {
            branch: branch.map(str::to_string),
            prompt_name: prompt_name.to_string(),
            prompt_version: version,
            system,
            brief_json,
            tools,
        };
        let mut store = sessions.lock().await;
        let s = store
            .entry(session_id.to_string())
            .or_insert_with(|| crate::intelligence::session::SessionContext::new(session_id, ws));
        s.warm_frame = Some(frame);
        Ok(())
    }

    /// M4 — clear the warm capsule frame of every session whose frame was
    /// built against `branch` (None = main). A contribute changes that
    /// branch's brief/tools, so any session's cached frame for it is stale
    /// — regardless of which session or connector wrote the claims. This is
    /// why invalidation is branch-scoped, not attributed to the writer.
    async fn invalidate_warm_frames_on_branch(
        sessions: &crate::intelligence::session::SessionStore,
        branch: Option<&str>,
    ) {
        let mut store = sessions.lock().await;
        for s in store.values_mut() {
            let matches = s
                .warm_frame
                .as_ref()
                .map(|f| f.branch.as_deref() == branch)
                .unwrap_or(false);
            if matches {
                s.invalidate_warm_frame();
            }
        }
    }

    /// Back-compat shim — delegates to the capability router. Historically this
    /// ranked by experience/input-shape alone and ignored the query; it now
    /// fuses SEMANTIC relevance (vector over embedded capability nodes) with the
    /// learned Wilson experience score via [`Self::route_capabilities`].
    pub async fn route_tools(&self, ws: &str, query: &str, k: usize) -> Result<Vec<String>> {
        self.route_capabilities(ws, None, query, k).await
    }

    /// Capability router (P2): the "narrow ~105 tools → k" decision. Ranks
    /// deployed Root Functions + external MCP tools for an INTENT by fusing:
    ///   1. semantic similarity — vector search over embedded capability nodes
    ///      (so a brand-new function matches on meaning, zero experience needed);
    ///   2. learned experience — multiplicative Wilson-score boost (so a function
    ///      that has reliably served similar inputs ranks higher);
    /// then fills any remaining slots from the MCP registry. Returns at most `k`
    /// tool names. `_branch` is reserved for branch-scoped vector search.
    pub async fn route_capabilities(
        &self,
        ws: &str,
        _branch: Option<&str>,
        intent: &str,
        k: usize,
    ) -> Result<Vec<String>> {
        if k == 0 {
            return Ok(Vec::new());
        }
        // 1. Semantic candidates over embedded capability nodes.
        let mut scored: Vec<(String, f64)> = Vec::new();
        if !intent.trim().is_empty() {
            let handle = self.get_workspace(ws)?;
            let mut storage = handle.storage.lock().await;
            if let Ok(hits) =
                storage
                    .vector
                    .search_prefix(intent, k.saturating_mul(4).max(8), "capability|")
            {
                for (_id, meta, sim) in hits {
                    // metadata format: `capability|{kind}|{name}`
                    if let Some(rest) = meta.strip_prefix("capability|")
                        && let Some(name) = rest.rsplit('|').next()
                    {
                        scored.push((name.to_string(), sim as f64));
                    }
                }
            }
        }
        // 2. Experience boost (multiplicative). Experienced functions are
        //    included even with no semantic hit (cold or shape-only intent).
        let exp = self
            .route_functions(ws, &serde_json::json!({ "query": intent }))
            .await
            .unwrap_or_default();
        for e in &exp {
            let boost = 1.0 + e.score();
            if let Some(s) = scored.iter_mut().find(|(n, _)| *n == e.function_name) {
                s.1 *= boost;
            } else {
                scored.push((e.function_name.clone(), 0.1 * boost));
            }
        }
        // 3. Rank by fused score, dedup, take k.
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        let mut names: Vec<String> = Vec::new();
        for (n, _) in scored {
            if !names.contains(&n) {
                names.push(n);
            }
            if names.len() >= k {
                break;
            }
        }
        // 4. Fill remaining slots from the external MCP registry — resolved
        //    across the workspace's inheritance chain (Slice 2b) so the router
        //    can rank inherited project/agent connectors from a per-user scope.
        if names.len() < k {
            let registry =
                crate::mcp::external_registry::merged_for_chain(&self.inheritance_chain(ws)).await;
            for (tool_name, _desc) in registry.list_all_tools().await {
                if !names.contains(&tool_name) {
                    names.push(tool_name);
                }
                if names.len() >= k {
                    break;
                }
            }
        }
        Ok(names)
    }

    // ─── Root Functions ─────────────────────────────────────────────
    // Storage delegations to the workspace `GraphStore` plus the
    // `invoke_function` execution path (loads the body, runs it in the
    // feature-gated `deno_core` isolate, records a run row).

    /// Deploy a new function version; returns the stored row.
    pub async fn put_function(
        &self,
        ws: &str,
        name: &str,
        body: &str,
        language: &str,
    ) -> Result<thinkingroot_graph::root_function::RootFunction> {
        let handle = self.get_workspace(ws)?;
        let mut storage = handle.storage.lock().await;
        let row = storage.graph.put_function(name, body, language)?;
        // P2 capability router: embed the function as a semantic capability node
        // so `route_capabilities` can match it on INTENT (not just input shape /
        // learned experience). Metadata `capability|root_function|{name}` lets the
        // vector search filter capability candidates. Best-effort — a missing
        // embedding model must not block a deploy.
        let snippet: String = body.chars().take(400).collect();
        if storage
            .vector
            .upsert(
                &format!("cap:root_function:{name}"),
                &format!("{name} ({language}): {snippet}"),
                &format!("capability|root_function|{name}"),
            )
            .is_ok()
        {
            // Persist so the capability embedding survives daemon restart /
            // respawn (upsert mutates the in-memory index only).
            let _ = storage.vector.save();
        }
        Ok(row)
    }

    /// Delete a function (all versions) by name + drop its capability
    /// embedding so the router stops matching a name that no longer resolves.
    /// Returns whether it existed. Idempotent.
    pub async fn delete_function(&self, ws: &str, name: &str) -> Result<bool> {
        let handle = self.get_workspace(ws)?;
        let mut storage = handle.storage.lock().await;
        let existed = storage.graph.delete_function(name)?;
        storage
            .vector
            .remove_by_ids(&[&format!("cap:root_function:{name}")]);
        let _ = storage.vector.save();
        Ok(existed)
    }

    /// Deploy a function version onto a specific branch's graph (e.g. a
    /// session's `stream/{id}` quarantine branch) instead of trunk. Errors
    /// if the branch doesn't exist. The function reaches trunk only when the
    /// branch is merged (the diff now carries Root Functions — see
    /// `apply_branch_diff`).
    pub async fn put_function_on_branch(
        &self,
        ws: &str,
        branch: &str,
        name: &str,
        body: &str,
        language: &str,
    ) -> Result<thinkingroot_graph::root_function::RootFunction> {
        let root = self
            .workspace_root_path(ws)
            .ok_or_else(|| Error::EntityNotFound(format!("workspace '{ws}' not mounted")))?;
        let handle = self.branch_engines().get_or_open(&root, branch).await?;
        handle.graph.put_function(name, body, language)
    }

    /// Store a control-plane-owned test fixture (input → expected output)
    /// for a function on trunk. Authored via the `function_test` tool — a
    /// SEPARATE authority from the `root_function` body author, so a
    /// self-authored function can't write its own passing tests.
    pub async fn put_function_test(
        &self,
        ws: &str,
        function_name: &str,
        input: &serde_json::Value,
        expected: &serde_json::Value,
    ) -> Result<()> {
        use thinkingroot_graph::root_function::FunctionFixture;
        let fx = FunctionFixture {
            function_name: function_name.to_string(),
            fixture_id: ulid::Ulid::new().to_string(),
            input_json: serde_json::to_string(input).unwrap_or_default(),
            expect_json: serde_json::to_string(expected).unwrap_or_default(),
        };
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        storage.graph.put_function_fixture(&fx)
    }

    /// Run a function's fixtures daemon-side and return `(passed, detail)`.
    /// The code under test is read from `branch` (the quarantine branch, when
    /// verifying before merge) while the fixtures come from TRUNK — code from
    /// the author, tests from a separate authority. This is the result the
    /// `function_tests` merge check consumes.
    pub async fn run_function_tests(
        &self,
        ws: &str,
        function_name: &str,
        branch: Option<&str>,
    ) -> Result<(bool, String)> {
        // Body: from the branch under test, else trunk.
        let func = match branch {
            Some(b) => {
                let root = self.workspace_root_path(ws).ok_or_else(|| {
                    Error::EntityNotFound(format!("workspace '{ws}' not mounted"))
                })?;
                let handle = self.branch_engines().get_or_open(&root, b).await?;
                handle.graph.get_function(function_name)?
            }
            None => self.get_function(ws, function_name).await?,
        }
        .ok_or_else(|| {
            Error::Template(format!("root function '{function_name}' is not deployed"))
        })?;

        // Fixtures: always from trunk (the separate authority).
        let fixtures_raw = {
            let handle = self.get_workspace(ws)?;
            let storage = handle.storage.lock().await;
            storage.graph.list_function_fixtures(function_name)?
        };
        let fixtures: Vec<(serde_json::Value, serde_json::Value)> = fixtures_raw
            .into_iter()
            .map(|f| {
                (
                    serde_json::from_str(&f.input_json).unwrap_or(serde_json::Value::Null),
                    serde_json::from_str(&f.expect_json).unwrap_or(serde_json::Value::Null),
                )
            })
            .collect();

        Ok(crate::root_function_runtime::run_fixture_check(&func.body, &fixtures, 30).await)
    }

    /// Verify-before-merge for a self-authored function: run its fixtures
    /// against the branch copy (`run_function_tests`) and promote it to trunk
    /// ONLY if every fixture passes. A failing function stays quarantined on
    /// its branch. The explicit, auditable gate — no silent merge. Returns
    /// `{ promoted, passed, detail, version? }`.
    pub async fn verify_and_promote_function(
        &self,
        ws: &str,
        function_name: &str,
        source_branch: &str,
    ) -> Result<serde_json::Value> {
        let (passed, detail) = self
            .run_function_tests(ws, function_name, Some(source_branch))
            .await?;
        if !passed {
            return Ok(serde_json::json!({
                "promoted": false,
                "passed": false,
                "detail": detail,
            }));
        }
        // Promote: read the verified body from the branch, deploy on trunk.
        let root = self
            .workspace_root_path(ws)
            .ok_or_else(|| Error::EntityNotFound(format!("workspace '{ws}' not mounted")))?;
        let handle = self.branch_engines().get_or_open(&root, source_branch).await?;
        let func = handle.graph.get_function(function_name)?.ok_or_else(|| {
            Error::Template(format!(
                "function '{function_name}' not found on branch '{source_branch}'"
            ))
        })?;
        let promoted = self.put_function(ws, function_name, &func.body, &func.language).await?;
        Ok(serde_json::json!({
            "promoted": true,
            "passed": true,
            "detail": detail,
            "version": promoted.version,
        }))
    }

    /// Latest version of a single function, or `None`.
    /// Read-inheritance chain for a workspace, head-first: the brain itself,
    /// then each brain it inherits from, ending at the shared/primary brain.
    /// This is the resolution order for definitions a scope doesn't carry its
    /// own copy of (agents, functions, prompts):
    ///   `u_X__agent_Y` → [`u_X__agent_Y`, `agent_Y`, <primary>]   (Slice 1: 2-level)
    ///   `u_X`          → [`u_X`, <primary>]
    ///   `agent_Y`      → [`agent_Y`, <primary>]
    ///   <primary>/other→ [`ws`]
    /// Unmounted brains in the chain are skipped by callers, so it's safe to
    /// list a parent that hasn't been referenced yet.
    pub(crate) fn inheritance_chain(&self, ws: &str) -> Vec<String> {
        let mut chain = vec![ws.to_string()];
        // A composite per-(user×agent) scope `u_X__agent_Y` inherits the
        // agent's OWN brain `agent_Y` (its functions/prompts) BEFORE the shared
        // brain — so per-user runs get the agent's skills, then the project pool.
        if let Some(idx) = ws.find("__agent_") {
            let agent_brain = format!("agent_{}", &ws[idx + "__agent_".len()..]);
            if agent_brain != ws && !chain.contains(&agent_brain) {
                chain.push(agent_brain);
            }
        }
        // Any auto-scoped brain finally inherits the shared/primary brain.
        if is_auto_scoped_ws(ws)
            && let Some(primary) = self.primary_ws_name()
            && !chain.contains(&primary)
        {
            chain.push(primary);
        }
        chain
    }

    pub async fn get_function(
        &self,
        ws: &str,
        name: &str,
    ) -> Result<Option<thinkingroot_graph::root_function::RootFunction>> {
        // Preserve the prior contract: calling on an unmounted scope errors.
        let _ = self.get_workspace(ws)?;
        // Walk the inheritance chain (self → agent brain → shared) and return
        // the first brain carrying the function. A function deployed once in the
        // shared (or agent) brain is callable from any per-user / per-(user×agent)
        // scope; it still EXECUTES in the calling scope (its `ctx.memory` targets
        // that brain). Never crosses into another user's or agent's workspace.
        for brain in self.inheritance_chain(ws) {
            if let Ok(handle) = self.get_workspace(&brain) {
                let storage = handle.storage.lock().await;
                if let Some(f) = storage.graph.get_function(name)? {
                    return Ok(Some(f));
                }
            }
        }
        Ok(None)
    }

    // ─── Agents (persisted agent entity) ──────────────────────────────────
    // The create-once agent definition (persona + model + memory policy) the
    // SDK and Console both read/write. Stored in the project's shared brain;
    // get/list resolve from the primary brain for per-user `u_*` scopes (an
    // agent defined once serves every end-user), mirroring Root Functions.

    /// Create or update an agent definition in workspace `ws`.
    pub async fn put_agent(
        &self,
        ws: &str,
        name: &str,
        persona: &str,
        model: &str,
        config_json: &str,
    ) -> Result<thinkingroot_graph::agents::AgentDef> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        let def = storage.graph.put_agent(name, persona, model, config_json)?;
        // Mirror the persona into the ONE compiled-prompt pipeline so it's a
        // versioned, editable, `prompt_get_latest`-resolved template like every
        // other prompt — co-located with the definition (reachable from any
        // scope via the inheritance chain). The agent loop reads it from here,
        // not as a raw field. Empty persona → no prompt (loop keeps the
        // workspace-default).
        if !persona.trim().is_empty() {
            let _ = storage
                .graph
                .prompt_put_template(&agent_persona_prompt_name(name), persona);
        }
        Ok(def)
    }

    /// The agent's persona resolved through the compiled-prompt pipeline:
    /// the `agent::<name>::persona` template, walked along the inheritance chain
    /// (so a project-level agent's persona resolves from any `u_*`/`agent_*`
    /// scope — `main` is always the chain tail). `None` when no persona prompt
    /// exists, so the caller falls back to the `AgentDef.persona` field (legacy
    /// agents created before the mirror).
    pub async fn agent_persona_prompt(&self, ws: &str, agent_name: &str) -> Option<String> {
        let pname = agent_persona_prompt_name(agent_name);
        for brain in self.inheritance_chain(ws) {
            if let Ok(handle) = self.get_workspace(&brain) {
                let storage = handle.storage.lock().await;
                if let Ok(Some(p)) = storage.graph.prompt_get_latest(&pname) {
                    if !p.template_text.trim().is_empty() {
                        return Some(p.template_text);
                    }
                }
            }
        }
        None
    }

    /// Fetch one agent, with the per-user → primary fallback: an agent defined
    /// once in the shared brain resolves from any `u_*` scope.
    pub async fn get_agent(
        &self,
        ws: &str,
        name: &str,
    ) -> Result<Option<thinkingroot_graph::agents::AgentDef>> {
        let _ = self.get_workspace(ws)?; // preserve "unmounted scope = error"
        // Walk the inheritance chain (self → agent brain → shared). The agent
        // DEFINITION lives in the shared brain, so a composite/per-user scope
        // resolves it via the chain — an agent defined once serves every scope.
        for brain in self.inheritance_chain(ws) {
            if let Ok(handle) = self.get_workspace(&brain) {
                let storage = handle.storage.lock().await;
                if let Some(a) = storage.graph.get_agent(name)? {
                    return Ok(Some(a));
                }
            }
        }
        Ok(None)
    }

    /// Resolve the agent's declarative state topology (defaults if unset/unknown).
    /// Uses the same inheritance-chain fallback as `get_agent`.
    pub async fn agent_topology(&self, ws: &str, name: &str) -> thinkingroot_core::AgentTopology {
        match self.get_agent(ws, name).await {
            Ok(Some(def)) => thinkingroot_core::AgentTopology::from_config_json(&def.config_json),
            Ok(None) => thinkingroot_core::AgentTopology::default(),
            Err(e) => {
                tracing::warn!(ws, name, err = %e, "agent_topology: lookup failed, using default");
                thinkingroot_core::AgentTopology::default()
            }
        }
    }

    /// List agents. A per-user `u_*` scope with no agents of its own lists the
    /// shared/primary brain's agents (agents are project-level definitions).
    pub async fn list_agents(
        &self,
        ws: &str,
    ) -> Result<Vec<thinkingroot_graph::agents::AgentDef>> {
        let handle = self.get_workspace(ws)?;
        {
            let storage = handle.storage.lock().await;
            let own = storage.graph.list_agents()?;
            if !own.is_empty() || !is_auto_scoped_ws(ws) {
                return Ok(own);
            }
        }
        if let Some(primary) = self.primary_ws_name()
            && primary != ws
            && let Ok(phandle) = self.get_workspace(&primary)
        {
            let storage = phandle.storage.lock().await;
            return storage.graph.list_agents();
        }
        Ok(Vec::new())
    }

    /// Delete an agent from workspace `ws`. Returns true if a row was removed.
    pub async fn delete_agent(&self, ws: &str, name: &str) -> Result<bool> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        storage.graph.delete_agent(name)
    }

    /// A6 — record a verification verdict for one forge test case and CORRECT
    /// the router's learned experience. The invoke that produced the output
    /// already counted as a success if the run merely *completed* — so a run
    /// that completed with the WRONG answer is over-credited. A failed
    /// verdict applies the missing negative bump; a passed verdict adds no
    /// extra positive (the invoke already credited it) — only the durable
    /// verdict row, which the idle trainer and the Console read.
    pub async fn record_function_verdict(
        &self,
        ws: &str,
        name: &str,
        input: &serde_json::Value,
        passed: bool,
        detail: &str,
    ) -> Result<()> {
        let handle = self.get_workspace(ws)?;
        let input_class = Self::input_class_for(name, input);
        let storage = handle.storage.lock().await;
        if storage.graph.get_function(name)?.is_none() {
            return Err(Error::EntityNotFound(format!(
                "root function '{name}' is not deployed"
            )));
        }
        storage
            .graph
            .record_verify_verdict(name, &input_class, passed, detail)?;
        if !passed {
            storage
                .graph
                .bump_function_experience(&input_class, name, false)?;
        }
        Ok(())
    }

    /// A6 — the recent verification verdicts for a function, newest-first.
    /// Returns JSON rows `{at, input_class, passed, detail}` for the Console
    /// learning view. Empty when the function was never verified.
    pub async fn function_verdicts(
        &self,
        ws: &str,
        name: &str,
        limit: usize,
    ) -> Result<serde_json::Value> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        let rows = storage.graph.list_verify_verdicts(name, limit)?;
        let out: Vec<serde_json::Value> = rows
            .into_iter()
            .map(|(at, input_class, passed, detail)| {
                serde_json::json!({
                    "at": at,
                    "input_class": input_class,
                    "passed": passed,
                    "detail": detail,
                })
            })
            .collect();
        Ok(serde_json::json!({ "verdicts": out }))
    }

    /// Learned-prior observability (item 10): the count of claims with a
    /// learned usefulness and the top-N by score. Read-only window onto the
    /// per-tenant learn-to-rank signal for the Console.
    pub async fn retrieval_prior_summary(
        &self,
        ws: &str,
        limit: usize,
    ) -> Result<serde_json::Value> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        let (total, top) = storage.graph.retrieval_prior_summary(limit)?;
        let enabled = std::env::var("TR_LEARNED_PRIOR")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let out: Vec<serde_json::Value> = top
            .into_iter()
            .map(|(claim_id, shown, cited, score)| {
                serde_json::json!({
                    "claim_id": claim_id,
                    "shown": shown,
                    "cited": cited,
                    "score": score,
                })
            })
            .collect();
        Ok(serde_json::json!({
            "enabled": enabled,
            "trained_claims": total,
            "top": out,
        }))
    }

    /// A1 — store the capability grant set for a function. Requires the
    /// function to exist (granting caps to a non-deployed name is a typo
    /// until proven otherwise).
    pub async fn set_function_caps(&self, ws: &str, name: &str, caps: CapSet) -> Result<()> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        if storage.graph.get_function(name)?.is_none() {
            return Err(Error::EntityNotFound(format!(
                "root function '{name}' is not deployed — deploy it before setting capabilities"
            )));
        }
        let json = serde_json::to_string(&caps)
            .map_err(|e| Error::Serialization(format!("serialise CapSet: {e}")))?;
        storage.graph.set_function_caps(name, &json)
    }

    /// A1 — the effective capability grants for a function: the stored set
    /// when present (malformed → all-deny, same fail-closed rule as invoke),
    /// else the unrestricted default. The bool is `true` when a stored
    /// (explicit) grant exists.
    pub async fn get_function_caps(&self, ws: &str, name: &str) -> Result<(CapSet, bool)> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        Ok(match storage.graph.get_function_caps(name)? {
            Some(json) => (
                CapSet::from_json(&json).unwrap_or_else(CapSet::deny_all),
                true,
            ),
            None => (CapSet::default_own_workspace(), false),
        })
    }

    /// §2.5 — set a function's declarative attributes (cron `schedule` +
    /// `retry_max`). A non-empty schedule is validated as cron up front (a bad
    /// expr is rejected, never silently dropped) and then (re-)arms a durable
    /// `cron` timer in the primary brain; clearing the schedule cancels it.
    pub async fn set_function_attributes(
        &self,
        ws: &str,
        name: &str,
        schedule: &str,
        retry_max: i64,
    ) -> Result<()> {
        let schedule = schedule.trim();
        if !schedule.is_empty() {
            crate::cron::Cron::parse(schedule)
                .map_err(|e| Error::Config(format!("invalid cron schedule '{schedule}': {e}")))?;
        }
        {
            let handle = self.get_workspace(ws)?;
            let storage = handle.storage.lock().await;
            if storage.graph.get_function(name)?.is_none() {
                return Err(Error::EntityNotFound(format!(
                    "root function '{name}' is not deployed — deploy it before setting attributes"
                )));
            }
            storage
                .graph
                .set_function_attributes(name, schedule, retry_max.clamp(0, 10))?;
        }
        if schedule.is_empty() {
            self.cancel_cron_timer(ws, name).await;
        } else {
            self.arm_cron_timer(ws, name, schedule).await?;
        }
        Ok(())
    }

    /// §2.5 — a function's `(schedule, retry_max)`; `("", 0)` when unset.
    pub async fn get_function_attributes(&self, ws: &str, name: &str) -> Result<(String, i64)> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        storage.graph.get_function_attributes(name)
    }

    /// The effective retry cap for a function: its `retry_max` attribute when
    /// set (1..=10), else the engine default of 3. Read on the invoke path.
    pub async fn function_retry_max(&self, ws: &str, name: &str) -> u32 {
        let Ok(handle) = self.get_workspace(ws) else {
            return 3;
        };
        let storage = handle.storage.lock().await;
        match storage.graph.get_function_attributes(name) {
            Ok((_, r)) if r >= 1 => (r as u32).min(10),
            _ => 3,
        }
    }

    /// Register (replace) the durable `cron` timer for a scheduled function in
    /// the primary brain, computing the next firing minute from the expr. The
    /// ticker fires it (a fresh run) then re-arms the next one — a self-
    /// perpetuating declarative schedule that survives the scope being unmounted.
    pub async fn arm_cron_timer(&self, ws: &str, name: &str, expr: &str) -> Result<()> {
        let cron = crate::cron::Cron::parse(expr)
            .map_err(|e| Error::Config(format!("invalid cron '{expr}': {e}")))?;
        let now = chrono::Utc::now();
        let Some(next) = cron.next_after(now) else {
            return Err(Error::Config(format!("cron '{expr}' never fires")));
        };
        let fire_at = next.timestamp_millis() as f64 / 1000.0;
        let primary = self.primary_ws_name().unwrap_or_else(|| ws.to_string());
        let handle = self.get_workspace(&primary)?;
        let dedupe = format!("cron:{name}");
        let timer = thinkingroot_graph::root_function::FnTimer {
            id: ulid::Ulid::new().to_string(),
            scope: ws.to_string(),
            fn_name: name.to_string(),
            kind: "cron".to_string(),
            run_id: String::new(),
            fire_at,
            input_json: "{}".to_string(),
            dedupe_key: dedupe.clone(),
            status: "pending".to_string(),
            created_at: now.timestamp_millis() as f64 / 1000.0,
        };
        let storage = handle.storage.lock().await;
        storage.graph.cancel_timer_dedupe(ws, name, &dedupe)?;
        storage.graph.put_timer(&timer)?;
        Ok(())
    }

    /// Cancel a function's pending `cron` timer (schedule cleared / function deleted).
    pub async fn cancel_cron_timer(&self, ws: &str, name: &str) {
        if let Some(primary) = self.primary_ws_name() {
            if let Ok(handle) = self.get_workspace(&primary) {
                let storage = handle.storage.lock().await;
                let _ = storage
                    .graph
                    .cancel_timer_dedupe(ws, name, &format!("cron:{name}"));
            }
        }
    }

    /// After a `cron` timer fires, re-arm the next one from the stored schedule.
    /// No-op if the schedule was cleared meanwhile (honest: a removed cron stops).
    pub async fn rearm_cron_after_fire(&self, ws: &str, name: &str) {
        let schedule = {
            let Ok(handle) = self.get_workspace(ws) else {
                return;
            };
            let storage = handle.storage.lock().await;
            storage
                .graph
                .get_function_attributes(name)
                .map(|(s, _)| s)
                .unwrap_or_default()
        };
        if !schedule.trim().is_empty() {
            if let Err(e) = self.arm_cron_timer(ws, name, schedule.trim()).await {
                tracing::warn!(function = name, scope = ws, "cron re-arm failed: {e}");
            }
        }
    }

    /// Latest version of every distinct function.
    pub async fn list_functions(
        &self,
        ws: &str,
    ) -> Result<Vec<thinkingroot_graph::root_function::RootFunction>> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        storage.graph.list_functions()
    }

    /// Invocation history for a function, newest first.
    pub async fn list_function_runs(
        &self,
        ws: &str,
        name: &str,
    ) -> Result<Vec<thinkingroot_graph::root_function::RootFunctionRun>> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        storage.graph.list_function_runs(name)
    }

    /// P3 — the durable-execution journal for a run: the `(step_key, result)`
    /// pairs in execution order. Powers the run inspector (what ran, what each
    /// step returned) + replay tooling. Empty for an unknown run.
    pub async fn list_function_steps(
        &self,
        ws: &str,
        run_id: &str,
    ) -> Result<Vec<(String, String, f64)>> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        storage.graph.list_steps_for_run_timed(run_id)
    }

    /// P1 — due scheduled-function timers from the PRIMARY brain (the ticker's
    /// scan). Timers are stored centrally there so they fire regardless of
    /// whether the target per-user scope is mounted. Empty on any error/absence.
    pub async fn due_fn_timers(
        &self,
        limit: usize,
    ) -> Vec<thinkingroot_graph::root_function::FnTimer> {
        let Some(primary) = self.primary_ws_name() else {
            return Vec::new();
        };
        let Ok(handle) = self.get_workspace(&primary) else {
            return Vec::new();
        };
        let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
        let storage = handle.storage.lock().await;
        storage.graph.list_due_timers(now, limit).unwrap_or_default()
    }

    /// P1 — delete a fired timer from the primary brain.
    pub async fn delete_fn_timer(&self, id: &str) {
        if let Some(primary) = self.primary_ws_name() {
            if let Ok(handle) = self.get_workspace(&primary) {
                let storage = handle.storage.lock().await;
                let _ = storage.graph.delete_timer(id);
            }
        }
    }

    /// P1b — register a durable RESUME timer in the primary brain (a `ctx.sleep`/
    /// `ctx.wakeAt` suspend). When due, the ticker records the wake step + re-
    /// enters `run_id`. `step_key` is the sleep journal key; `input_json` the
    /// original invocation input.
    async fn put_resume_timer(
        &self,
        scope: &str,
        fn_name: &str,
        run_id: &str,
        fire_at: f64,
        step_key: &str,
        input_json: &str,
    ) {
        let primary = self.primary_ws_name().unwrap_or_else(|| scope.to_string());
        let Ok(handle) = self.get_workspace(&primary) else {
            return;
        };
        let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
        let timer = thinkingroot_graph::root_function::FnTimer {
            id: ulid::Ulid::new().to_string(),
            scope: scope.to_string(),
            fn_name: fn_name.to_string(),
            kind: "resume".to_string(),
            run_id: run_id.to_string(),
            fire_at,
            input_json: input_json.to_string(), // original invocation input
            dedupe_key: step_key.to_string(),   // the sleep journal step key
            status: "pending".to_string(),
            created_at: now,
        };
        let storage = handle.storage.lock().await;
        let _ = storage.graph.put_timer(&timer);
    }

    /// P1b — fire a RESUME timer: record the sleep's wake step in the SCOPE
    /// brain, then re-enter the run (replays completed steps; the sleep returns
    /// and the function continues). The scope must already be mounted.
    pub async fn resume_timer(
        &self,
        scope: &str,
        fn_name: &str,
        run_id: &str,
        step_key: &str,
        input_json: &str,
    ) -> Result<serde_json::Value> {
        // A sleep wakes with no value.
        self.resume_with_value(scope, fn_name, run_id, step_key, "null", input_json)
            .await
    }

    /// P1b/P2 — record `value_json` as the journaled step the suspend awaits
    /// (sleep → "null"; waitForEvent → the event payload), then re-enter the run
    /// (replays completed steps; the suspending call returns `value_json`). The
    /// scope must already be mounted.
    pub async fn resume_with_value(
        &self,
        scope: &str,
        fn_name: &str,
        run_id: &str,
        step_key: &str,
        value_json: &str,
        input_json: &str,
    ) -> Result<serde_json::Value> {
        {
            let handle = self.get_workspace(scope)?;
            let storage = handle.storage.lock().await;
            let _ = storage
                .graph
                .record_function_steps(run_id, &[(step_key.to_string(), value_json.to_string())]);
        }
        let input: serde_json::Value =
            serde_json::from_str(input_json).unwrap_or(serde_json::Value::Null);
        self.run_function_with_id(scope, fn_name, &input, run_id).await
    }

    /// P1b-ii — register a durable RETRY timer in the primary brain. When due,
    /// the ticker re-enters `run_id` with `next_attempt` (the journal replays
    /// completed steps, so only the failed work re-runs).
    async fn put_retry_timer(
        &self,
        scope: &str,
        fn_name: &str,
        run_id: &str,
        fire_at: f64,
        next_attempt: u32,
        input_json: &str,
    ) {
        let primary = self.primary_ws_name().unwrap_or_else(|| scope.to_string());
        let Ok(handle) = self.get_workspace(&primary) else {
            return;
        };
        let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
        let timer = thinkingroot_graph::root_function::FnTimer {
            id: ulid::Ulid::new().to_string(),
            scope: scope.to_string(),
            fn_name: fn_name.to_string(),
            kind: "retry".to_string(),
            run_id: run_id.to_string(),
            fire_at,
            input_json: input_json.to_string(), // original invocation input
            dedupe_key: next_attempt.to_string(), // the attempt number to run next
            status: "pending".to_string(),
            created_at: now,
        };
        let storage = handle.storage.lock().await;
        let _ = storage.graph.put_timer(&timer);
    }

    /// P2 — deliver an event to waiters in `scope` (or buffer it, 1h TTL, if
    /// none waiting). Marks matching waiters `ready` (the ticker resumes them).
    /// Used by the `POST /ws/{ws}/events` endpoint. Returns waiters delivered to.
    pub async fn emit_event(&self, scope: &str, event_name: &str, payload_json: &str) -> Result<u32> {
        let primary = self.primary_ws_name().unwrap_or_else(|| scope.to_string());
        let handle = self.get_workspace(&primary)?;
        let storage = handle.storage.lock().await;
        let waiters = storage.graph.find_pending_waiters(scope, event_name)?;
        if waiters.is_empty() {
            let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
            let id = ulid::Ulid::new().to_string();
            storage
                .graph
                .put_event_buffer(&id, scope, event_name, payload_json, now + 3600.0)?;
            return Ok(0);
        }
        let mut delivered = 0u32;
        for mut w in waiters {
            w.status = "ready".to_string();
            w.payload_json = payload_json.to_string();
            storage.graph.put_waiter(&w)?;
            delivered += 1;
        }
        Ok(delivered)
    }

    /// P2 — register a waiter for a `ctx.waitForEvent` suspend (in the primary
    /// brain). If a matching event was already buffered (emitted before the
    /// wait), the waiter is created `ready` so the ticker resumes it promptly.
    async fn put_waiter_for_run(
        &self,
        scope: &str,
        fn_name: &str,
        run_id: &str,
        step_key: &str,
        event_name: &str,
        expires_at: f64,
        input_json: &str,
    ) {
        let primary = self.primary_ws_name().unwrap_or_else(|| scope.to_string());
        let Ok(handle) = self.get_workspace(&primary) else {
            return;
        };
        let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
        let storage = handle.storage.lock().await;
        let (status, payload, buffered_id) =
            match storage.graph.find_buffered_event(scope, event_name, now) {
                Ok(Some((bid, payload))) => ("ready".to_string(), payload, Some(bid)),
                _ => ("pending".to_string(), String::new(), None),
            };
        let waiter = thinkingroot_graph::root_function::FnWaiter {
            id: ulid::Ulid::new().to_string(),
            scope: scope.to_string(),
            event_name: event_name.to_string(),
            run_id: run_id.to_string(),
            fn_name: fn_name.to_string(),
            step_key: step_key.to_string(),
            input_json: input_json.to_string(),
            payload_json: payload,
            expires_at,
            status,
            created_at: now,
        };
        let _ = storage.graph.put_waiter(&waiter);
        if let Some(bid) = buffered_id {
            let _ = storage.graph.delete_event_buffer(&bid);
        }
    }

    /// P2 — actionable waiters from the primary brain (the ticker's scan).
    pub async fn due_fn_waiters(
        &self,
        limit: usize,
    ) -> Vec<thinkingroot_graph::root_function::FnWaiter> {
        let Some(primary) = self.primary_ws_name() else {
            return Vec::new();
        };
        let Ok(handle) = self.get_workspace(&primary) else {
            return Vec::new();
        };
        let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
        let storage = handle.storage.lock().await;
        storage.graph.list_actionable_waiters(now, limit).unwrap_or_default()
    }

    /// P2 — delete a resolved waiter from the primary brain.
    pub async fn delete_fn_waiter(&self, id: &str) {
        if let Some(primary) = self.primary_ws_name() {
            if let Ok(handle) = self.get_workspace(&primary) {
                let storage = handle.storage.lock().await;
                let _ = storage.graph.delete_waiter(id);
            }
        }
    }

    /// P2b — a fresh idempotency result for `key` in this scope, or None. Stored
    /// in the scope's OWN brain so keys are per-user.
    async fn idempotency_get(&self, ws: &str, key: &str) -> Option<serde_json::Value> {
        let Ok(handle) = self.get_workspace(ws) else {
            return None;
        };
        let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
        let storage = handle.storage.lock().await;
        match storage.graph.get_idempotency(key, now) {
            Ok(Some(json)) => serde_json::from_str(&json).ok(),
            _ => None,
        }
    }

    /// P2b — record a terminal result under `key` (24h TTL).
    async fn idempotency_put(&self, ws: &str, key: &str, run_id: &str, result_json: &str) {
        let Ok(handle) = self.get_workspace(ws) else {
            return;
        };
        let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
        let storage = handle.storage.lock().await;
        let _ = storage.graph.put_idempotency(key, run_id, result_json, now + 86400.0);
    }

    /// Invoke the latest version of `name` with `input`. Resolves the
    /// body, builds the secret-backed `env` map, runs it in the isolate,
    /// records a run row, and returns the JSON result. Errors (function
    /// missing, JS error, feature disabled) are recorded as `error` runs
    /// and propagated.
    pub async fn invoke_function(
        &self,
        ws: &str,
        name: &str,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let run_id = ulid::Ulid::new().to_string();
        self.run_function_with_id(ws, name, input, &run_id).await
    }

    /// A2 — branch-scoped invoke. Same as `invoke_function` but the run's
    /// `memory.remember` writes are quarantined to a branch:
    ///   - `opts.target_branch = Some(b)` → writes land on branch `b`
    ///     (forked from main if absent); the caller later merges or abandons.
    ///   - `opts.dry_run = true` → writes land on a fresh ephemeral branch
    ///     that is **abandoned after the run** (a true dry run: side effects
    ///     happen in isolation, then vanish). Returns the output with
    ///     `_branch` / `_dry_run` markers describing where writes went.
    /// Backward compatible: `InvokeBranchOpts::default()` == plain invoke.
    pub async fn invoke_function_with_opts(
        &self,
        ws: &str,
        name: &str,
        input: &serde_json::Value,
        opts: InvokeBranchOpts,
    ) -> Result<serde_json::Value> {
        let run_id = ulid::Ulid::new().to_string();
        self.run_function_with_id_opts(ws, name, input, &run_id, opts).await
    }

    /// Function-INDEPENDENT shape of an input (top-level key set / scalar
    /// kind). The basis for routing: functions are comparable only across a
    /// shared, name-free class. Heuristic v1 (an LLM classifier is the upgrade).
    fn shape_of(input: &serde_json::Value) -> String {
        match input {
            serde_json::Value::Object(m) => {
                let mut keys: Vec<&str> = m.keys().map(|s| s.as_str()).collect();
                keys.sort_unstable();
                format!("obj[{}]", keys.join(","))
            }
            serde_json::Value::Array(_) => "array".to_string(),
            serde_json::Value::String(_) => "string".to_string(),
            serde_json::Value::Number(_) => "number".to_string(),
            serde_json::Value::Bool(_) => "bool".to_string(),
            serde_json::Value::Null => "null".to_string(),
        }
    }

    /// Per-function input class for run-learning: `{name}:{shape}`. Each
    /// function accumulates experience under its own class; routing compares
    /// them by looking up each function's class for a shared shape.
    fn input_class_for(name: &str, input: &serde_json::Value) -> String {
        format!("{name}:{}", Self::shape_of(input))
    }

    /// Experience-based routing: given an input, return every deployed
    /// function ranked by its learned success on inputs of this shape (best
    /// first). The moat made consumable — the agent picks; we don't auto-run.
    pub async fn route_functions(
        &self,
        ws: &str,
        input: &serde_json::Value,
    ) -> Result<Vec<thinkingroot_graph::root_function::ExperienceEntry>> {
        use thinkingroot_graph::root_function::ExperienceEntry;
        let shape = Self::shape_of(input);
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        let mut out: Vec<ExperienceEntry> = Vec::new();
        for f in storage.graph.list_functions()? {
            let class = format!("{}:{}", f.name, shape);
            let entry = storage.graph.get_experience(&class, &f.name)?.unwrap_or(ExperienceEntry {
                function_name: f.name.clone(),
                weight: 0.0,
                n_success: 0,
                n_fail: 0,
            });
            out.push(entry);
        }
        // Rank by confident success rate (Wilson lower bound), not raw volume.
        out.sort_by(|a, b| b.score().total_cmp(&a.score()));
        Ok(out)
    }

    /// Capability-routing report (P5): every deployed function with its learned
    /// experience grouped by input_class (n_success / n_fail + Wilson score).
    /// Functions with no experience yet appear with empty `classes`. Powers the
    /// Console's routing/experience view — the window into "what routes where,
    /// and how well it performs".
    pub async fn capability_routing_report(&self, ws: &str) -> Result<serde_json::Value> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        let funcs = storage.graph.list_functions()?;
        let exp = storage.graph.list_all_experience()?;
        let mut by_fn: std::collections::BTreeMap<String, Vec<serde_json::Value>> =
            std::collections::BTreeMap::new();
        for (ic, e) in &exp {
            by_fn.entry(e.function_name.clone()).or_default().push(serde_json::json!({
                "input_class": ic,
                "n_success": e.n_success,
                "n_fail": e.n_fail,
                "score": e.score(),
            }));
        }
        let capabilities: Vec<serde_json::Value> = funcs
            .iter()
            .map(|f| {
                let classes = by_fn.remove(&f.name).unwrap_or_default();
                let runs: i64 = classes
                    .iter()
                    .map(|c| c["n_success"].as_i64().unwrap_or(0) + c["n_fail"].as_i64().unwrap_or(0))
                    .sum();
                serde_json::json!({
                    "name": f.name,
                    "version": f.version,
                    "runs": runs,
                    "classes": classes,
                })
            })
            .collect();
        Ok(serde_json::json!({ "capabilities": capabilities }))
    }

    /// Core durable invocation, parameterised by `run_id` so a *resumed*
    /// run reuses the same id (and thus its journal). On a suspended
    /// `ctx.cognition.ask` it persists a pending request and returns a
    /// `{ _suspended, token, question }` marker; on completion it returns
    /// the JSON value. `status` is recorded as `ok` | `error` | `suspended`.
    pub async fn run_function_with_id(
        &self,
        ws: &str,
        name: &str,
        input: &serde_json::Value,
        run_id: &str,
    ) -> Result<serde_json::Value> {
        self.run_function_with_id_opts(ws, name, input, run_id, InvokeBranchOpts::default())
            .await
    }

    /// Branch-scoped variant of [`run_function_with_id`]. See
    /// [`Self::invoke_function_with_opts`] for the branch semantics. With
    /// `InvokeBranchOpts::default()` it is byte-for-byte the old behavior.
    pub async fn run_function_with_id_opts(
        &self,
        ws: &str,
        name: &str,
        input: &serde_json::Value,
        run_id: &str,
        opts: InvokeBranchOpts,
    ) -> Result<serde_json::Value> {
        use crate::root_function_runtime::RunOutcome;
        use thinkingroot_graph::root_function::{PendingRequest, RootFunctionRun};

        // P2b — idempotency: a fresh prior result for this key short-circuits the
        // run (duplicate webhook / client retry → exactly-once).
        if let Some(key) = opts.idempotency_key.as_deref() {
            if let Some(cached) = self.idempotency_get(ws, key).await {
                return Ok(cached);
            }
        }

        // ── A2: resolve & prepare the write-target branch (if any) ────────
        // dry_run with no explicit branch → a fresh ephemeral branch named
        // for this run. Either way, fork it from main if it does not exist
        // yet so the branch graph dir is present before the first remember.
        let target_branch: Option<String> = if let Some(b) = opts.target_branch.clone() {
            Some(b)
        } else if opts.dry_run {
            Some(format!("dryrun/{run_id}"))
        } else {
            None
        };
        if let Some(branch) = target_branch.as_deref() {
            let handle = self.get_workspace(ws)?;
            let exists = thinkingroot_branch::list_branches(&handle.root_path)
                .map(|bs| bs.iter().any(|b| b.name == branch))
                .unwrap_or(false);
            if !exists {
                thinkingroot_branch::create_branch(
                    &handle.root_path,
                    branch,
                    "main",
                    Some(format!(
                        "{} branch for root function '{}' (run {})",
                        if opts.dry_run { "ephemeral dry-run" } else { "scoped" },
                        name,
                        run_id
                    )),
                )
                .await
                .map_err(|e| Error::Config(format!("branch-scoped invoke: fork failed: {e}")))?;
            }
        }

        let func = self
            .get_function(ws, name)
            .await?
            .ok_or_else(|| Error::Template(format!("root function '{name}' is not deployed")))?;

        // Secret-backed env, from two sources:
        //  1. CLOUD: the provisioner injects each secret as a process env var
        //     and lists their names in `TR_SECRET_NAMES` (there is no
        //     secrets.toml inside the per-project container). We read only the
        //     names in that manifest so system env (PATH, the daemon key,
        //     TR_OUTBOUND_ALLOWLIST, …) never leaks into `ctx.env`.
        //  2. DESKTOP/LOCAL: names from `secrets.toml`, resolved env-var-first
        //     so a cloud-injected value still wins if both exist.
        let mut env = std::collections::BTreeMap::new();
        if let Ok(manifest) = std::env::var("TR_SECRET_NAMES") {
            for n in manifest.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                if let Ok(v) = std::env::var(n) {
                    env.insert(n.to_string(), v);
                }
            }
        }
        if let Ok(names) = thinkingroot_cloud_auth::secrets::list_names() {
            for n in names {
                if let Some(v) = thinkingroot_cloud_auth::secrets::resolve_secret(&n) {
                    env.insert(n, v);
                }
            }
        }

        let started_at = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
        let ctx_meta = crate::root_function_runtime::FnCtxMeta {
            run_id: run_id.to_string(),
            ws: ws.to_string(),
            fn_name: name.to_string(),
            version: func.version,
            attempt: opts.attempt.max(1),
            // REST/flow invocation path is session-less; an MCP-originated
            // path will thread the real session id here in a later phase.
            session_id: None,
        };
        // BYO-key oracle for ctx.llm: the workspace's configured LLM client
        // (None if the project has no provider key — ctx.llm then errors
        // honestly rather than fabricating).
        let llm = self.workspace_llm(ws);

        // Durable execution: load any steps already journaled for this run
        // (empty for a fresh invocation; populated when resuming the same
        // run_id — including a freshly-answered cognition), run with replay,
        // then persist newly-recorded steps + pending requests.
        let prior_steps: std::collections::HashMap<String, String> = {
            let handle = self.get_workspace(ws)?;
            let storage = handle.storage.lock().await;
            storage
                .graph
                .list_steps_for_run(run_id)
                .unwrap_or_default()
                .into_iter()
                .collect()
        };
        // Co-located capabilities: capture a CLONE of this workspace's handle
        // (cheap — Arcs inside) so the isolate's ctx.memory/ctx.prompt/etc. ops
        // reach the cognition graph WITHOUT re-locking the engine during the
        // run (avoids the writer-queue deadlock when a workspace mounts mid-run)
        // and stays confined to this (per-user) workspace.
        let (caps, conc_limit, conc_per_scope) = {
            let handle = self.get_workspace(ws)?.clone();
            // Slice 2b: `ctx.mcp.call` resolves connectors across the workspace's
            // inheritance chain (self → agent brain → shared), nearest-server-
            // wins — so a function in a per-user scope can call a project-level
            // connector. The chain NEVER includes sibling scopes, so this stays
            // confined to the legitimate inheritance path (no cross-tenant leak).
            let mcp =
                crate::mcp::external_registry::merged_for_chain(&self.inheritance_chain(ws)).await;
            // A1 — per-function capability grants. Absent row → the all-on
            // default (unrestricted functions behave exactly as before).
            // Present-but-malformed row → DENY ALL (fail closed: a corrupt
            // grant must never silently restore full capabilities).
            let cap_set = {
                let storage = handle.storage.lock().await;
                match storage.graph.get_function_caps(name) {
                    Ok(Some(json)) => CapSet::from_json(&json).unwrap_or_else(|| {
                        tracing::warn!(
                            function = name,
                            "malformed stored CapSet — failing closed (all capabilities denied)"
                        );
                        CapSet::deny_all()
                    }),
                    Ok(None) => CapSet::default_own_workspace(),
                    Err(e) => {
                        tracing::warn!(
                            function = name,
                            error = %e,
                            "CapSet lookup failed — failing closed for this invoke"
                        );
                        CapSet::deny_all()
                    }
                }
            };
            // P1 — durable timers live in the PRIMARY brain (so the ticker
            // fires them even when this per-user scope is unmounted). None when
            // already running in the primary (timers then use the run's handle).
            let primary_handle = self
                .primary_ws_name()
                .filter(|p| p.as_str() != ws)
                .and_then(|p| self.get_workspace(&p).ok().cloned());
            (
                std::sync::Arc::new(
                    FnCapabilities::new(handle, ws.to_string(), run_id.to_string(), mcp, cap_set)
                        .with_target_branch(target_branch.clone())
                        .with_primary_handle(primary_handle),
                ),
                cap_set.concurrency_limit,
                cap_set.concurrency_per_scope,
            )
        };

        // P2c — hold a fair concurrency permit for the whole run (per-scope key =
        // per-user fairness; 0 = unlimited). The guard releases on return, so a
        // suspended run (sleep/event) frees its slot and re-acquires on resume.
        let _conc_permit = if conc_limit > 0 {
            let key = if conc_per_scope {
                format!("{name}:{ws}")
            } else {
                format!("{name}:_global")
            };
            Some(acquire_concurrency_permit(key, conc_limit as usize).await)
        } else {
            None
        };

        let (outcome, new_steps, new_pending, new_cites) =
            crate::root_function_runtime::run_js_journaled(
                &func.body,
                input,
                &env,
                ctx_meta,
                llm,
                prior_steps,
                30,
                Some(caps),
            )
            .await;
        let finished_at = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
        // Coarse, deterministic input class for run-learning (no LLM call).
        let input_class = Self::input_class_for(name, input);

        // P1b-ii — durable retry. A retryable failure (not a NonRetryableError,
        // attempt < cap) schedules a retry timer that re-enters THIS run with
        // attempt+1; completed steps replay from the journal, so only the failed
        // work re-runs. `retry_in_secs` is set in the Failed arm below.
        let attempt = opts.attempt.max(1);
        // §2.5 — the retry cap is a per-function attribute (default 3).
        let max_attempts: u32 = self.function_retry_max(ws, name).await;
        let mut retry_in_secs: Option<u64> = None;

        // Map the run outcome to a recorded status + the returned value.
        let (status, output_json, error, ret): (&str, String, String, Result<serde_json::Value>) =
            match &outcome {
                RunOutcome::Done(v) => (
                    "ok",
                    serde_json::to_string(v).unwrap_or_default(),
                    String::new(),
                    Ok(v.clone()),
                ),
                RunOutcome::Suspended => {
                    let token = new_pending.first().map(|p| p.token.clone()).unwrap_or_default();
                    let question =
                        new_pending.first().map(|p| p.question.clone()).unwrap_or_default();
                    let marker = serde_json::json!({
                        "_suspended": true, "token": token, "question": question
                    });
                    (
                        "suspended",
                        serde_json::to_string(&marker).unwrap_or_default(),
                        String::new(),
                        Ok(marker),
                    )
                }
                RunOutcome::Failed(e) => {
                    let non_retryable = e.contains("__TR_NONRETRYABLE__");
                    if !non_retryable && attempt < max_attempts {
                        // Exponential backoff: 1s, 2s, 4s, … capped at 60s.
                        retry_in_secs = Some((1u64 << (attempt - 1)).min(60));
                        let marker = serde_json::json!({
                            "_retrying": true, "attempt": attempt,
                            "next_attempt": attempt + 1, "error": e
                        });
                        (
                            "retrying",
                            serde_json::to_string(&marker).unwrap_or_default(),
                            String::new(),
                            Ok(marker),
                        )
                    } else {
                        // Terminal: strip the NonRetryableError marker for the
                        // user-facing error.
                        let clean = e
                            .replace("__TR_NONRETRYABLE__:", "")
                            .replace("__TR_NONRETRYABLE__", "");
                        ("error", String::new(), clean.clone(), Err(Error::Template(clean)))
                    }
                }
            };

        let run = RootFunctionRun {
            id: run_id.to_string(),
            function_name: name.to_string(),
            status: status.to_string(),
            started_at,
            finished_at,
            output_json,
            error,
        };
        // P1b — original input (for a resume re-invoke) + sleep timers collected
        // here and registered AFTER the scope journal persists, so a wake always
        // replays a complete journal.
        let resume_input_json = serde_json::to_string(input).unwrap_or_default();
        let mut resume_timers: Vec<(String, f64)> = Vec::new();
        // P2 — (step_key, event_name, timeout_epoch) for ctx.waitForEvent suspends.
        let mut event_waiters: Vec<(String, String, f64)> = Vec::new();
        {
            let handle = self.get_workspace(ws)?;
            let storage = handle.storage.lock().await;
            if !new_steps.is_empty() {
                let _ = storage.graph.record_function_steps(run_id, &new_steps);
            }
            if !new_pending.is_empty() {
                let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
                for p in &new_pending {
                    // P2 — a ctx.waitForEvent suspend registers a waiter (below)
                    // with wake_at as the timeout deadline.
                    if let Some(event_name) = &p.event_name {
                        event_waiters.push((
                            p.step_key.clone(),
                            event_name.clone(),
                            p.wake_at.unwrap_or(0.0),
                        ));
                        continue;
                    }
                    // P1b — a ctx.sleep/ctx.wakeAt timer suspend registers a
                    // durable RESUME timer (below); a cognition ask stays a
                    // pending request awaiting an answer.
                    if let Some(wake_at) = p.wake_at {
                        resume_timers.push((p.step_key.clone(), wake_at));
                        continue;
                    }
                    let _ = storage.graph.put_pending_request(&PendingRequest {
                        token: p.token.clone(),
                        run_id: run_id.to_string(),
                        ws: ws.to_string(),
                        function_name: name.to_string(),
                        step_key: p.step_key.clone(),
                        question: p.question.clone(),
                        input_json: resume_input_json.clone(),
                        status: "pending".to_string(),
                        created_at: now,
                    });
                }
            }
            let _ = storage.graph.record_function_run(&run);

            // ── Run-learning (the moat) ──
            // A completed run is positive evidence for (input_class, fn); an
            // errored run is negative. A suspended run is incomplete — no
            // signal yet (it'll be judged when it resumes to completion).
            match &outcome {
                RunOutcome::Done(_) => {
                    let _ = storage.graph.bump_function_experience(&input_class, name, true);
                }
                // Only a TERMINAL failure is negative evidence; a failure that
                // will be retried (retry_in_secs set) is not judged yet.
                RunOutcome::Failed(_) if retry_in_secs.is_none() => {
                    let _ = storage.graph.bump_function_experience(&input_class, name, false);
                }
                _ => {}
            }
            // Touch edges: link this run to the claims it declared via
            // ctx.cite, so a later change to any of them causally invalidates
            // what we learned here.
            for claim_id in &new_cites {
                let _ = storage.graph.record_invocation_touch(
                    run_id,
                    "claim",
                    claim_id,
                    &input_class,
                    name,
                    "read",
                );
            }
        }

        // P1b — register durable resume timers in the PRIMARY brain now that the
        // scope's journal is persisted. Each re-enters THIS run at `wake_at`
        // (the engine ticker records the wake step + replays).
        for (step_key, wake_at) in resume_timers {
            self.put_resume_timer(ws, name, run_id, wake_at, &step_key, &resume_input_json)
                .await;
        }

        // P2 — register ctx.waitForEvent waiters (resolves a pre-buffered event
        // immediately; otherwise the ticker fires on emit or timeout).
        for (step_key, event_name, expires_at) in event_waiters {
            self.put_waiter_for_run(
                ws,
                name,
                run_id,
                &step_key,
                &event_name,
                expires_at,
                &resume_input_json,
            )
            .await;
        }

        // P1b-ii — register a durable retry timer (re-enters this run with
        // attempt+1 after backoff; the journal skips already-completed steps).
        if let Some(backoff) = retry_in_secs {
            let fire_at = (chrono::Utc::now().timestamp_millis() as f64 / 1000.0) + backoff as f64;
            self.put_retry_timer(ws, name, run_id, fire_at, attempt + 1, &resume_input_json)
                .await;
        }

        // P2b — record the idempotency result on a terminal success (so a
        // duplicate invoke with the same key returns this instead of re-running).
        if status == "ok" {
            if let (Some(key), Ok(v)) = (opts.idempotency_key.as_deref(), &ret) {
                let result_json = serde_json::to_string(v).unwrap_or_default();
                self.idempotency_put(ws, key, run_id, &result_json).await;
            }
        }

        // ── A2: branch teardown + result markers ──────────────────────────
        if let Some(branch) = target_branch.as_deref() {
            let handle = self.get_workspace(ws)?;
            // Honest "what landed on the branch" = the diff this branch would
            // contribute back to main (best-effort; 0 if the diff can't be
            // computed). Computed BEFORE any dry-run cleanup.
            let claims_written = thinkingroot_branch::dry_run_merge_into(
                &handle.root_path,
                branch,
                "main",
                false,
            )
            .await
            .map(|d| d.new_claims.len())
            .unwrap_or(0);

            if opts.dry_run {
                // True dry run: discard the ephemeral branch and its writes.
                // Best-effort — a failed cleanup must not fail an
                // already-completed invocation.
                if let Err(e) = thinkingroot_branch::delete_branch(&handle.root_path, branch) {
                    tracing::warn!(branch, error = %e, "dry-run: ephemeral branch cleanup failed");
                }
            }

            // Annotate the result (objects only — scalars/arrays are returned
            // verbatim so a function's output contract is never reshaped).
            return match ret {
                Ok(serde_json::Value::Object(mut map)) => {
                    map.insert("_branch".into(), serde_json::json!(branch));
                    map.insert("_dry_run".into(), serde_json::json!(opts.dry_run));
                    map.insert("_claims_written".into(), serde_json::json!(claims_written));
                    Ok(serde_json::Value::Object(map))
                }
                other => other,
            };
        }

        ret
    }

    /// Answer a suspended run's pending cognition request (by token) and
    /// resume the run. The answer is journaled under the request's step key,
    /// then the function replays from the top — returning a value, or another
    /// suspension marker if it asks again.
    pub async fn answer_cognition(
        &self,
        ws: &str,
        token: &str,
        answer: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let pending = {
            let handle = self.get_workspace(ws)?;
            let storage = handle.storage.lock().await;
            let p = storage
                .graph
                .get_pending_request(token)
                .map_err(|e| Error::GraphStorage(format!("get pending request: {e}")))?
                .ok_or_else(|| {
                    Error::EntityNotFound(format!(
                        "no pending cognition request for token '{token}'"
                    ))
                })?;
            // Record the answer as the journaled step the cognition awaits,
            // then mark the request answered.
            let answer_json = serde_json::to_string(answer).unwrap_or_else(|_| "null".to_string());
            storage
                .graph
                .record_function_steps(&p.run_id, &[(p.step_key.clone(), answer_json)])
                .map_err(|e| Error::GraphStorage(format!("record answer step: {e}")))?;
            let _ = storage.graph.mark_pending_answered(token);
            p
        };

        let input: serde_json::Value =
            serde_json::from_str(&pending.input_json).unwrap_or(serde_json::Value::Null);
        self.run_function_with_id(ws, &pending.function_name, &input, &pending.run_id)
            .await
    }

    // ─── MCP connectors ─────────────────────────────────────────────
    // The Console manages external MCP servers (GitHub, Slack, …) over
    // REST. These reuse the same `acquisition_tools` config writer + the
    // global `external_registry` remount the `mcp_server_install` MCP
    // tool uses, so the CLI/agent and the Console drive one source of
    // truth (`<workspace>/.thinkingroot/mcp-servers.toml`).

    /// Installed connectors with live tool counts.
    pub async fn list_mcp_servers(
        &self,
        ws: &str,
    ) -> Result<Vec<crate::acquisition_tools::McpServerInfo>> {
        let root = self
            .workspace_root_path(ws)
            .ok_or_else(|| Error::GraphStorage(format!("workspace '{ws}' not mounted")))?;
        let configured = crate::acquisition_tools::list_configured_servers(&root)
            .map_err(Error::GraphStorage)?;
        // Live tool counts per server, by `<server>::` prefix.
        let registry = crate::mcp::external_registry::registry_for(ws).await;
        let tools = registry.list_all_tools().await;
        let mut out = Vec::new();
        for entry in configured {
            let prefix = format!("{}::", entry.name);
            let tool_count = tools.iter().filter(|(n, _)| n.starts_with(&prefix)).count();
            let transport = match entry.transport {
                crate::mcp::external_registry::TransportKind::Stdio => "stdio",
                crate::mcp::external_registry::TransportKind::Http => "http",
            };
            out.push(crate::acquisition_tools::McpServerInfo {
                name: entry.name,
                transport: transport.to_string(),
                tool_count,
            });
        }
        Ok(out)
    }

    /// Install (or update) a connector, then remount the registry live.
    /// Returns the total configured-server count.
    pub async fn install_mcp_server(
        &self,
        ws: &str,
        entry: crate::mcp::external_registry::ServerEntry,
    ) -> Result<usize> {
        let root = self
            .workspace_root_path(ws)
            .ok_or_else(|| Error::GraphStorage(format!("workspace '{ws}' not mounted")))?;
        let count = crate::acquisition_tools::upsert_server_entry(&root, entry)
            .map_err(Error::GraphStorage)?;
        crate::mcp::external_registry::load_workspace_config(ws, &root)
            .await
            .map_err(|e| Error::GraphStorage(format!("remount: {e}")))?;
        crate::mcp::sse::notify_tools_list_changed().await;
        // M2 — reflect the new MCP surface as graph nodes (best-effort).
        let _ = self.sync_mcp_nodes(ws).await;
        Ok(count)
    }

    /// Remove a connector by name, then remount. `false` if absent.
    pub async fn remove_mcp_server(&self, ws: &str, name: &str) -> Result<bool> {
        let root = self
            .workspace_root_path(ws)
            .ok_or_else(|| Error::GraphStorage(format!("workspace '{ws}' not mounted")))?;
        let removed = crate::acquisition_tools::remove_server_entry(&root, name)
            .map_err(Error::GraphStorage)?;
        if removed {
            crate::mcp::external_registry::load_workspace_config(ws, &root)
                .await
                .map_err(|e| Error::GraphStorage(format!("remount: {e}")))?;
            crate::mcp::sse::notify_tools_list_changed().await;
        }
        Ok(removed)
    }

    /// M2 — sync MCP server + tool artifact nodes for `ws` into its graph
    /// from the currently-loaded external registry (`mcp_server --exposes-->
    /// mcp_tool`). Lets the Brain Graph + capsule see the MCP surface as
    /// nodes. Best-effort; callers wrap in `let _ =`.
    pub async fn sync_mcp_nodes(&self, ws: &str) -> Result<()> {
        use std::collections::BTreeMap;
        use thinkingroot_graph::artifact_nodes::{artifact_node_id, KIND_MCP_SERVER, KIND_MCP_TOOL};
        let registry = crate::mcp::external_registry::registry_for(ws).await;
        let tools = registry.list_all_tools().await;
        let mut by_server: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (full, _desc) in &tools {
            if let Some((server, _t)) = full.split_once("::") {
                by_server.entry(server.to_string()).or_default().push(full.clone());
            }
        }
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        for (server, toolnames) in &by_server {
            let edges: Vec<(String, String)> = toolnames
                .iter()
                .map(|t| ("exposes".to_string(), artifact_node_id(KIND_MCP_TOOL, t)))
                .collect();
            let _ = storage
                .graph
                .upsert_artifact_node(KIND_MCP_SERVER, server, 0, "mcp server", &edges);
            for t in toolnames {
                let _ = storage
                    .graph
                    .upsert_artifact_node(KIND_MCP_TOOL, t, 0, "mcp tool", &[]);
            }
        }
        Ok(())
    }

    /// M2 — sync a `flow_def` artifact node + edges to the Root Function
    /// (`runs`) and MCP-tool (`calls`) nodes its DAG references. Only the
    /// structured `NodeType` variants yield edges (no guessing). Best-effort.
    pub async fn sync_flow_node(
        &self,
        ws: &str,
        def: &thinkingroot_flow::FlowDefinition,
    ) -> Result<()> {
        use thinkingroot_graph::artifact_nodes::{
            artifact_node_id, KIND_FLOW, KIND_FUNCTION, KIND_MCP_TOOL,
        };
        let mut edges: Vec<(String, String)> = Vec::new();
        for nodespec in def.nodes.values() {
            match &nodespec.node_type {
                thinkingroot_flow::NodeType::RootFunction { function, .. } => {
                    edges.push(("runs".to_string(), artifact_node_id(KIND_FUNCTION, function)));
                }
                thinkingroot_flow::NodeType::McpTool { tool, .. } => {
                    edges.push(("calls".to_string(), artifact_node_id(KIND_MCP_TOOL, tool)));
                }
                _ => {}
            }
        }
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        let _ = storage.graph.upsert_artifact_node(
            KIND_FLOW,
            &def.id,
            def.version as i64,
            &def.description,
            &edges,
        );
        Ok(())
    }

    /// M2 — list every operating-layer artifact node (prompts, functions,
    /// flows, MCP servers/tools) with its outgoing edges, read straight from
    /// the cognition graph (NOT the knowledge cache, which is reserved for
    /// claims/entities). Backs `GET /ws/{ws}/artifacts` so the Console can
    /// render the operating layer the brain runs on.
    pub async fn list_operating_artifacts(&self, ws: &str) -> Result<Vec<ArtifactView>> {
        use thinkingroot_graph::artifact_nodes::{
            KIND_FLOW, KIND_FUNCTION, KIND_MCP_SERVER, KIND_MCP_TOOL, KIND_PROMPT,
        };
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        let mut out: Vec<ArtifactView> = Vec::new();
        for kind in [KIND_PROMPT, KIND_FUNCTION, KIND_FLOW, KIND_MCP_SERVER, KIND_MCP_TOOL] {
            for node in storage.graph.list_artifact_nodes(kind)? {
                let edges = storage
                    .graph
                    .artifact_edges(kind, &node.name)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(relation, to_id)| ArtifactEdgeView { relation, to_id })
                    .collect();
                out.push(ArtifactView {
                    id: node.id,
                    name: node.name,
                    kind: node.kind,
                    description: node.description,
                    edges,
                });
            }
        }
        Ok(out)
    }

    /// The brain's view of its own **durable** branch topology — typed branch
    /// nodes (status/parent/kind/timestamps), the projection synced from the
    /// branch registry on each lifecycle event. Distinct from
    /// [`Self::list_operating_artifacts`] because branch metadata is structured
    /// (not a free-text description). Ephemeral `stream/*` branches are absent
    /// by design.
    pub async fn list_branch_nodes(
        &self,
        ws: &str,
    ) -> Result<Vec<thinkingroot_graph::artifact_nodes::BranchNode>> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        storage.graph.list_branch_nodes()
    }

    /// List every Witness anchored to a specific source. Used by the
    /// Playground SourceLibrary click-through to render the witness
    /// detail panel for a clicked source row.
    pub async fn list_witnesses_by_source(
        &self,
        ws: &str,
        source_id: &str,
    ) -> Result<Vec<thinkingroot_core::types::Witness>> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        storage.graph.list_witnesses_by_source(source_id)
    }

    /// Fetch a single Witness by id from a workspace. Returns `None`
    /// when the id is unknown — surfaces the absence honestly rather
    /// than fabricating an empty Witness, because callers gate
    /// downstream behaviour (e.g. AEP probe materialisation) on
    /// `Some`.
    pub async fn get_witness(
        &self,
        ws: &str,
        id: &str,
    ) -> Result<Option<thinkingroot_core::types::Witness>> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        storage.graph.get_witness(id)
    }

    /// Resolve a citation's "claim id" (a witness id in the Witness-Mesh
    /// substrate) to its byte-anchored source span(s). The citation gate
    /// (`intelligence::citations`) calls this to enrich a verified
    /// `[claim:<id>]` marker with a byte-precise, source-anchored pointer.
    /// Returns an empty Vec for an unknown id — absence is honest, not an
    /// error.
    pub async fn get_witnesses_for_claim(
        &self,
        ws: &str,
        claim_id: &str,
    ) -> Result<Vec<thinkingroot_graph::witness_inserts::ResolvedCitationSpan>> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        storage.graph.get_witnesses_for_claim(claim_id)
    }

    // ─── E2: code-graph traversal API ───────────────────────────────

    /// Find code entities (FunctionDef/TypeDef) whose symbol contains
    /// `keyword`. See [`thinkingroot_graph::codegraph::GraphStore::search_entity`].
    pub async fn search_entity(
        &self,
        ws: &str,
        keyword: &str,
    ) -> Result<Vec<thinkingroot_graph::codegraph::EntityHit>> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        storage.graph.search_entity(keyword)
    }

    /// Full byte-anchored detail of one entity by claim id. Unknown id → None.
    pub async fn retrieve_entity(
        &self,
        ws: &str,
        claim_id: &str,
    ) -> Result<Option<thinkingroot_graph::codegraph::EntityDetail>> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        storage.graph.retrieve_entity(claim_id)
    }

    /// Bounded BFS over the code graph from a start claim id.
    pub async fn traverse_graph(
        &self,
        ws: &str,
        start_claim_id: &str,
        dir: thinkingroot_graph::codegraph::TraversalDirection,
        max_hops: u32,
        edge_kinds: &[thinkingroot_graph::codegraph::EdgeKind],
    ) -> Result<Vec<thinkingroot_graph::codegraph::TraversedNode>> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        storage
            .graph
            .traverse_graph(start_claim_id, dir, max_hops, edge_kinds)
    }

    /// Reverse-call transitive closure (blast radius) for a claim id.
    pub async fn impact(
        &self,
        ws: &str,
        claim_id: &str,
        max_hops: u32,
    ) -> Result<Vec<thinkingroot_graph::codegraph::TraversedNode>> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        storage.graph.impact(claim_id, max_hops)
    }

    /// E3 — build a PageRank-ranked, token-budgeted repo-map. Nodes =
    /// code-def claims; edges = resolved calls. An optional `query` seeds
    /// PageRank personalization toward matching symbols. Empty graph →
    /// empty map. PageRank + tree-rendering live in `intelligence::repo_map`.
    pub async fn repo_map(
        &self,
        ws: &str,
        req: &crate::intelligence::repo_map::RepoMapRequest,
    ) -> Result<crate::intelligence::repo_map::RepoMap> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        crate::intelligence::repo_map::build_repo_map(
            &storage.graph,
            req.budget_tokens,
            req.query.as_deref(),
        )
    }

    /// E4 — read the hierarchical summary ladder, optionally one altitude.
    pub async fn get_summaries(
        &self,
        ws: &str,
        altitude: Option<&str>,
    ) -> Result<Vec<thinkingroot_graph::summaries::SummaryNode>> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        storage.graph.get_summary_nodes(altitude)
    }

    /// E4 — (re)build the deterministic summary ladder for a workspace.
    /// Returns the number of summary nodes written. A standalone trigger so
    /// callers can build summaries without a full recompile (the pipeline's
    /// `emit_summaries` flag folds the same build into a compile run).
    pub async fn build_summaries(&self, ws: &str) -> Result<usize> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        storage.graph.build_summaries(now)
    }

    /// Count the witnesses table for a workspace. Used by the
    /// migration-status REST surface and by tests verifying that
    /// `root compile` populated the new substrate.
    pub async fn count_witnesses(&self, ws: &str) -> Result<u64> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        storage.graph.count_witnesses()
    }

    // ── Phase β.1 — Cognition Commits ──────────────────────────────
    //
    // Three thin delegations onto `GraphStore`. Identical workspace-
    // scoping pattern as the Witness Mesh methods above; the graph
    // helpers own citation-verification + parent-existence checks
    // so the engine layer stays a transport.

    /// Record a cognition commit against a workspace. Verifies every
    /// cited / added witness id resolves to a real Witness in the
    /// workspace — fabricated citations are rejected with a typed
    /// error rather than silently persisted.
    pub async fn commit_cognition(
        &self,
        ws: &str,
        commit: &thinkingroot_core::types::CognitionCommit,
    ) -> Result<()> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        storage.graph.insert_cognition_commit(commit)
    }

    /// Fetch a single cognition commit by id. Returns `None` when not
    /// present — REST handlers map missing to 404.
    pub async fn get_cognition_commit(
        &self,
        ws: &str,
        id: &thinkingroot_core::types::CommitId,
    ) -> Result<Option<thinkingroot_core::types::CognitionCommit>> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        storage.graph.get_cognition_commit(id)
    }

    /// List cognition commits on a branch newest-first. `limit = None`
    /// returns every commit; pass `Some(N)` for paged UIs.
    pub async fn list_cognition_commits(
        &self,
        ws: &str,
        branch: &str,
        limit: Option<usize>,
    ) -> Result<Vec<thinkingroot_core::types::CognitionCommit>> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        storage.graph.list_cognition_commits_on_branch(branch, limit)
    }

    /// Walk the parent chain from `start_id` up to `max_depth` hops.
    /// Returns commits in walk order (start first, then parent, etc.).
    /// Stops early at a root commit or missing parent.
    pub async fn walk_cognition_ancestors(
        &self,
        ws: &str,
        start_id: &thinkingroot_core::types::CommitId,
        max_depth: usize,
    ) -> Result<Vec<thinkingroot_core::types::CognitionCommit>> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        storage.graph.walk_commit_ancestors(start_id, max_depth)
    }

    /// Count cognition commits in the workspace. Cheap.
    pub async fn count_cognition_commits(&self, ws: &str) -> Result<u64> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        storage.graph.count_cognition_commits()
    }

    /// Compute a deterministic merge plan between two branches in
    /// the workspace's cognition-commit DAG. Phase γ.1 of the design
    /// doc (`docs/2026-05-15-cognition-commits-design.md`).
    ///
    /// Pure read — no commits are recorded by this call. The returned
    /// `MergePlan` classifies the divergence (`Identical` /
    /// `LeftAhead` / `RightAhead` / `Diverged` / `NoCommonHistory`)
    /// and surfaces the partitioned witness-id sets each side
    /// cited or added since the LCA. γ.2 will feed this plan to an
    /// LLM as the synthesis-prompt context; γ.3 will render it in
    /// the conflict-resolution UI.
    pub async fn compute_merge_plan(
        &self,
        ws: &str,
        left_branch: &str,
        right_branch: &str,
    ) -> Result<thinkingroot_core::types::MergePlan> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        storage.graph.compute_merge_plan(left_branch, right_branch)
    }

    /// Phase γ.2 — Merge synthesis (LLM-driven).
    ///
    /// Given a deterministic `MergePlan` (γ.1), call the workspace's
    /// LLM to produce a synthesis: a single piece of reasoning that
    /// reconciles the two divergent sides, citing witnesses from
    /// each via `[[witness:<id>]]` markers.
    ///
    /// Citation honesty (load-bearing): the response's
    /// `verified_citations` is *only* the witness ids the LLM cited
    /// that actually exist in the plan's surfaced witness sets.
    /// Fabricated ids are dropped and reported via
    /// `dropped_citations`. The caller (chat agent or React UI)
    /// decides whether to record the synthesis as a real commit via
    /// `commit_cognition` — γ.2 never writes the commit itself.
    ///
    /// Trivial plans (`Identical` / `LeftAhead` / `RightAhead`)
    /// short-circuit without calling the LLM — they collapse to a
    /// "no synthesis needed" outcome.
    pub async fn synthesize_merge(
        &self,
        ws: &str,
        left_branch: &str,
        right_branch: &str,
    ) -> Result<thinkingroot_core::types::MergeSynthesis> {
        use thinkingroot_core::types::{MergeSynthesis, SynthesisOutcome};

        let plan = self.compute_merge_plan(ws, left_branch, right_branch).await?;
        if plan.is_trivial() {
            return Ok(MergeSynthesis {
                outcome: SynthesisOutcome::Trivial,
                plan,
                reasoning: String::new(),
                verified_citations: Vec::new(),
                dropped_citations: Vec::new(),
                model: String::new(),
            });
        }

        let llm = match self.workspace_llm(ws) {
            Some(c) => c,
            None => {
                return Ok(MergeSynthesis {
                    outcome: SynthesisOutcome::LlmUnavailable,
                    plan,
                    reasoning: String::new(),
                    verified_citations: Vec::new(),
                    dropped_citations: Vec::new(),
                    model: String::new(),
                });
            }
        };

        // System prompt — pinned and load-bearing. Honesty rules:
        //   1. Cite only witnesses surfaced in the plan.
        //   2. Be explicit about disagreement.
        //   3. Don't invent witnesses or facts.
        let system = "\
You are a cognition-merge synthesizer. You receive a deterministic merge plan \
between two branches of a thinking DAG. Your job: write a single paragraph of \
reasoning that reconciles the two sides, citing the specific witnesses each \
side referenced. Strict rules:\n\
1. Cite witnesses using the marker `[[witness:<64-hex-id>]]`. Never invent ids \
   — only ids present in the plan are valid.\n\
2. When the two sides disagree, name the disagreement explicitly. Don't paper \
   over it.\n\
3. Don't add facts that aren't in the plan. The plan is the substrate; \
   anything else is hallucination.\n\
4. Be brief: 2–4 sentences.";
        let plan_json = serde_json::to_string_pretty(&plan).unwrap_or_else(|_| {
            format!(
                "(plan serialise failed; {} divergent commits)",
                plan.divergent_commit_count()
            )
        });
        let user_msg = format!(
            "Merge plan between branches `{}` (left) and `{}` (right):\n\n```json\n{}\n```\n\n\
             Write the synthesis paragraph now. Remember: cite using \
             `[[witness:<id>]]` markers; only ids present in the plan are valid.",
            plan.left_branch, plan.right_branch, plan_json
        );

        let reply = match llm.chat(system, &user_msg).await {
            Ok(r) => r,
            Err(e) => {
                return Ok(MergeSynthesis {
                    outcome: SynthesisOutcome::LlmError(format!("{e}")),
                    plan,
                    reasoning: String::new(),
                    verified_citations: Vec::new(),
                    dropped_citations: Vec::new(),
                    model: llm.model_name().to_string(),
                });
            }
        };

        let cited = crate::intelligence::citation_markers::extract_witness_citations(&reply);
        // Honest set: every WitnessId the plan surfaced (left_only ∪
        // right_only ∪ shared, on citations + witnesses_added). The
        // LLM is only allowed to cite within this universe.
        let mut plan_universe: std::collections::HashSet<
            thinkingroot_core::types::WitnessId,
        > = std::collections::HashSet::new();
        for id in plan
            .witnesses
            .left_only_citations
            .iter()
            .chain(plan.witnesses.right_only_citations.iter())
            .chain(plan.witnesses.shared_citations.iter())
            .chain(plan.witnesses.left_only_added.iter())
            .chain(plan.witnesses.right_only_added.iter())
            .chain(plan.witnesses.shared_added.iter())
        {
            plan_universe.insert(*id);
        }
        let mut verified = Vec::new();
        let mut dropped = Vec::new();
        for id in cited {
            if plan_universe.contains(&id) {
                verified.push(id);
            } else {
                dropped.push(id);
            }
        }
        Ok(MergeSynthesis {
            outcome: SynthesisOutcome::Synthesized,
            plan,
            reasoning: reply,
            verified_citations: verified,
            dropped_citations: dropped,
            model: llm.model_name().to_string(),
        })
    }

    /// Witness count grouped by `source_id`. Returned as a Vec so
    /// it serialises straight onto the wire without a HashMap → JSON
    /// shape ambiguity (some JS consumers can't reliably round-trip
    /// numeric-keyed maps). The Playground Source Library aggregates
    /// these into per-source badges.
    pub async fn count_witnesses_by_source(
        &self,
        ws: &str,
    ) -> Result<Vec<(String, u64)>> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        storage.graph.count_witnesses_by_source()
    }

    /// Walk the Witness Mesh DAG starting from a witness id. Returns
    /// every Witness reachable via `witness_input_edges` within
    /// `max_depth` hops + the edges that connect them. Used by the
    /// `walk_mesh` MCP tool so AI agents can trace a Witness's full
    /// derivation chain (which rule produced it, what bytes it
    /// derived from, what sibling Witnesses share inputs).
    ///
    /// Returns `(witnesses, edges)` — edges are the subset of
    /// `witness_input_edges` rows that fall inside the walked set.
    pub async fn walk_witness_mesh(
        &self,
        ws: &str,
        root_id: &str,
        max_depth: usize,
        max_fanout: usize,
    ) -> Result<(
        Vec<thinkingroot_core::types::Witness>,
        Vec<(String, String)>,
    )> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        storage
            .graph
            .walk_mesh_from(root_id, max_depth, max_fanout)
    }

    /// Get relations for a specific entity by name.
    /// Served from in-memory cache — O(k).
    pub async fn get_relations(&self, ws: &str, entity: &str) -> Result<Vec<RelationInfo>> {
        let handle = self.get_workspace(ws)?;
        let cache = handle.cache.read().await;

        Ok(cache
            .relations_for_entity(entity)
            .into_iter()
            .map(|r| RelationInfo {
                target: r.to_name.clone(),
                relation_type: thinkingroot_core::types::RelationType::normalize_storage(
                    &r.relation_type,
                ),
                strength: r.strength,
            })
            .collect())
    }

    /// Get all relations in the workspace as (from, to, relation_type, strength) tuples.
    /// Served from in-memory cache — O(n) over pre-built Vec, zero disk I/O.
    pub async fn get_all_relations(&self, ws: &str) -> Result<Vec<(String, String, String, f64)>> {
        let handle = self.get_workspace(ws)?;
        let cache = handle.cache.read().await;

        Ok(cache
            .all_relations()
            .iter()
            .map(|r| {
                (
                    r.from_name.clone(),
                    r.to_name.clone(),
                    r.relation_type.clone(),
                    r.strength,
                )
            })
            .collect())
    }

    /// Retrieve the entire knowledge base mapped out into a highly compact
    /// 3D topology format (`GalaxyMap`).
    /// Combines the 2D projection from `VectorStore` (Semantic axes)
    /// with the cache's claim_count mapping (Epistemic Z-axis).
    /// Used by the high-performance WebGL galaxy viewer.
    pub async fn get_galaxy_map(&self, ws: &str) -> Result<GalaxyMap> {
        let handle = self.get_workspace(ws)?;

        let coords_2d = {
            let storage = handle.storage.lock().await;
            storage.vector.project_to_2d()
        };

        let cache = handle.cache.read().await;

        let mut nodes = Vec::with_capacity(cache.entity_count());
        for id in cache.entities_ordered() {
            if let Some(e) = cache.entity_by_id(id) {
                // VectorStore keys are prefixed with "entity:"
                let vec_key = format!("entity:{}", e.id);
                let (x, y) = coords_2d.get(&vec_key).copied().unwrap_or((0.0, 0.0));
                let claim_count = cache.entity_claim_count(&e.id);
                // Z-axis: Density of Truth. Logarithmic scaling so outliers don't break the map.
                let z = (claim_count as f32 + 1.0).ln() * 50.0;
                let created_at =
                    e.id.parse::<thinkingroot_core::types::EntityId>()
                        .map(|id| id.timestamp_ms())
                        .unwrap_or(0);

                nodes.push(GalaxyNode {
                    id: e.id.to_string(),
                    name: e.canonical_name.clone(),
                    entity_type: thinkingroot_core::types::EntityType::normalize_storage(
                        &e.entity_type,
                    ),
                    claim_count,
                    x,
                    y,
                    z,
                    created_at,
                });
            }
        }

        let mut links = Vec::new();
        for r in cache.all_relations() {
            // Re-map name back to ID for the links because frontend graphs prefer ID-based links
            if let (Some(src), Some(tgt)) = (
                cache.find_entity_by_name(&r.from_name),
                cache.find_entity_by_name(&r.to_name),
            ) {
                links.push(GalaxyLink {
                    source: src.id.clone(),
                    target: tgt.id.clone(),
                    relation_type: thinkingroot_core::types::RelationType::normalize_storage(
                        &r.relation_type,
                    ),
                });
            }
        }

        Ok(GalaxyMap { nodes, links })
    }

    /// List all known artifact types and whether each is available on disk.
    pub async fn list_artifacts(&self, ws: &str) -> Result<Vec<ArtifactInfo>> {
        let handle = self.get_workspace(ws)?;
        let artifacts_dir = handle.root_path.join(".thinkingroot").join("artifacts");

        let mut result = Vec::with_capacity(ARTIFACT_TYPES.len());
        for &atype in ARTIFACT_TYPES {
            let available = if is_dynamic_artifact(atype) {
                true
            } else if let Some(filename) = artifact_filename(atype) {
                artifacts_dir.join(filename).exists()
            } else {
                false
            };
            result.push(ArtifactInfo {
                artifact_type: atype.to_string(),
                available,
            });
        }

        Ok(result)
    }

    /// List all sources in the workspace.
    /// Served from in-memory cache.
    pub async fn list_sources(&self, ws: &str) -> Result<Vec<SourceInfo>> {
        let handle = self.get_workspace(ws)?;
        let root = handle.root_path.clone();
        let cache = handle.cache.read().await;

        Ok(cache
            .all_sources()
            .iter()
            .map(|s| {
                // Enrich with REAL filesystem metadata (size + import time) by
                // resolving the source's uri to its file on the workspace
                // volume. Agent-contributed sources (no file) → both None.
                let (byte_size, imported_at) = source_file_meta(&root, &s.uri);
                SourceInfo {
                    id: s.id.clone(),
                    uri: s.uri.clone(),
                    source_type: s.source_type.clone(),
                    content_hash: s.content_hash.clone(),
                    byte_size,
                    imported_at,
                }
            })
            .collect())
    }

    /// Remove every claim, entity edge, vector, and contradiction
    /// row that descends from `source_uri` in the named workspace, then
    /// rebuild the in-memory read cache so subsequent queries reflect
    /// the redaction.
    ///
    /// Returns the number of source rows removed (0 if `source_uri`
    /// did not match any). Idempotent: calling twice with the same
    /// URI is a no-op the second time.
    ///
    /// Used by the desktop privacy dashboard's "Forget" action
    /// (Phase F Stream H, Step 13). This is the load-bearing API
    /// behind the user-facing covenant commitment that any datum
    /// can be removed on demand.
    pub async fn forget_source(&self, ws: &str, source_uri: &str) -> Result<usize> {
        let handle = self.get_workspace(ws)?;

        // Phase 1: graph mutation. Hold the storage lock only for the
        // delete + the raw refetch.
        let raw_data: RawGraphData = {
            let storage = handle.storage.lock().await;
            let removed = storage.graph.remove_source_by_uri(source_uri)?;
            if removed == 0 {
                return Ok(0);
            }
            KnowledgeGraph::fetch_raw(&storage.graph)?
        };

        // Phase 2: rebuild the cache off-lock (CPU-only).
        let new_cache = KnowledgeGraph::build_from_raw(raw_data);

        // Phase 3: atomic cache swap.
        *handle.cache.write().await = new_cache;

        Ok(1)
    }

    /// Read the content of a specific artifact.
    pub async fn get_artifact(&self, ws: &str, artifact_type: &str) -> Result<ArtifactContent> {
        // Dynamic artifacts are rendered on-demand from live graph state.
        if is_dynamic_artifact(artifact_type) {
            return self.render_dynamic_artifact(ws, artifact_type).await;
        }

        let handle = self.get_workspace(ws)?;
        let filename = artifact_filename(artifact_type).ok_or_else(|| Error::Compilation {
            artifact_type: artifact_type.to_string(),
            message: format!("unknown artifact type: {artifact_type}"),
        })?;

        let artifact_path = handle
            .root_path
            .join(".thinkingroot")
            .join("artifacts")
            .join(filename);

        if artifact_type == "entity-pages" {
            // For entity-pages, concatenate all files in the directory.
            if !artifact_path.is_dir() {
                return Err(Error::Compilation {
                    artifact_type: artifact_type.to_string(),
                    message: "entity-pages directory not found".to_string(),
                });
            }
            let mut content = String::new();
            let mut entries: Vec<_> = std::fs::read_dir(&artifact_path)
                .map_err(|e| Error::io_path(&artifact_path, e))?
                .filter_map(|e| e.ok())
                .collect();
            entries.sort_by_key(|e| e.file_name());

            for entry in entries {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("md") {
                    let text =
                        std::fs::read_to_string(&path).map_err(|e| Error::io_path(&path, e))?;
                    if !content.is_empty() {
                        content.push_str("\n---\n\n");
                    }
                    content.push_str(&text);
                }
            }
            return Ok(ArtifactContent {
                artifact_type: artifact_type.to_string(),
                content,
            });
        }

        // Regular file artifact.
        if !artifact_path.exists() {
            return Err(Error::Compilation {
                artifact_type: artifact_type.to_string(),
                message: format!("artifact not found at {}", artifact_path.display()),
            });
        }

        let content = std::fs::read_to_string(&artifact_path)
            .map_err(|e| Error::io_path(&artifact_path, e))?;

        Ok(ArtifactContent {
            artifact_type: artifact_type.to_string(),
            content,
        })
    }

    /// Render a dynamic artifact (not backed by a file on disk) by
    /// querying live graph state. Currently covers `gap-report`.
    async fn render_dynamic_artifact(
        &self,
        ws: &str,
        artifact_type: &str,
    ) -> Result<ArtifactContent> {
        let content = match artifact_type {
            "gap-report" => self.render_gap_report(ws).await?,
            other => {
                return Err(Error::Compilation {
                    artifact_type: other.to_string(),
                    message: format!("unknown dynamic artifact type: {other}"),
                });
            }
        };
        Ok(ArtifactContent {
            artifact_type: artifact_type.to_string(),
            content,
        })
    }

    /// Render `gap-report.md` — a human-readable markdown dashboard of
    /// the patterns Phase 9 Reflect has discovered and the gaps those
    /// patterns imply for specific entities.
    ///
    /// Always reflects *current* state — if the reflect cycle hasn't run
    /// yet, the report is accurate ("no patterns discovered yet").
    async fn render_gap_report(&self, ws: &str) -> Result<String> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        let graph = &storage.graph;

        // Load patterns (denormalized enough for the report).
        let pattern_rows = graph.reflect_load_structural_patterns()?;
        let gaps = thinkingroot_reflect::list_open_gaps(graph, None, 0.0)?;
        let total_open = graph.reflect_count_open_known_unknowns()?;

        let mut md = String::new();
        md.push_str(&format!(
            "# Gap Report — `{ws}`\n\n\
             _Generated from live graph state. No cron, no compile step — this report reflects the graph as of now._\n\n"
        ));

        // ── Patterns ──────────────────────────────────────────────────
        md.push_str("## Discovered Patterns\n\n");
        if pattern_rows.is_empty() {
            md.push_str(
                "_No patterns discovered yet. Run the `reflect` tool after compiling \
                 enough entities (≥30 of the same type) to establish co-occurrence signal._\n\n",
            );
        } else {
            md.push_str(
                "_`Stability` = consecutive reflect cycles the pattern has survived. Gap claims use damped confidence (frequency × stability factor) until a pattern stabilizes. `Scope` is `local` for single-workspace or `cross:<id>` for aggregated patterns._\n\n",
            );
            md.push_str(
                "| Entity Type | When has | Expected to have | Frequency | Sample | Stability | Scope |\n\
                 |---|---|---|---:|---:|---:|---|\n",
            );
            for (
                _id,
                etype,
                cond,
                expected,
                freq,
                sample,
                _last_computed,
                _threshold,
                _first_seen,
                stability_runs,
                source_scope,
            ) in &pattern_rows
            {
                md.push_str(&format!(
                    "| {etype} | `{cond}` | `{expected}` | {freq_pct:.1}% | {sample} | {stab} | {scope} |\n",
                    freq_pct = freq * 100.0,
                    stab = stability_runs,
                    scope = source_scope,
                ));
            }
            md.push('\n');
        }

        // ── Open gaps ─────────────────────────────────────────────────
        md.push_str(&format!("## Open Gaps ({total_open})\n\n"));
        if gaps.is_empty() {
            md.push_str(
                "_No open gaps at any confidence. Either no patterns have been discovered \
                 yet, or every entity matching a pattern's condition also carries the expected claim._\n",
            );
        } else {
            // Group by entity_type for readability.
            use std::collections::BTreeMap;
            let mut by_type: BTreeMap<&str, Vec<&thinkingroot_reflect::GapReport>> =
                BTreeMap::new();
            for g in &gaps {
                by_type.entry(g.entity_type.as_str()).or_default().push(g);
            }
            for (etype, group) in by_type {
                md.push_str(&format!("### {etype} ({} gap(s))\n\n", group.len()));
                for g in group {
                    md.push_str(&format!(
                        "- **{ename}** — expected `{expected}` @ {pct:.0}% (sample: {n})\n  > {reason}\n",
                        ename = g.entity_name,
                        expected = g.expected_claim_type,
                        pct = g.confidence * 100.0,
                        n = g.sample_size,
                        reason = g.reason,
                    ));
                }
                md.push('\n');
            }
        }

        // ── How to act ────────────────────────────────────────────────
        md.push_str(
            "## How to act on this report\n\n\
             - **Fill a gap** — add the expected claim for the entity (e.g. via `contribute` or a new source). Next `reflect` cycle will mark it resolved automatically.\n\
             - **Dismiss a false positive** — call `dismiss_gap` with the gap id. Dismissed gaps are not re-raised and do not penalize health coverage.\n\
             - **Lower the noise floor** — if too many weak patterns fire, raise `ReflectConfig::min_frequency` (default 0.70) or `min_sample_size` (default 30).\n",
        );

        Ok(md)
    }

    /// Run health/verification checks on the workspace.
    /// Reads directly from CozoDB — verification needs full consistency checks.
    ///
    /// **Lock semantics.** `Verifier::verify` issues ~9 sequential
    /// CozoDB queries plus a `count_low_grounding_claims` join (~700
    /// ms wall on a 50k-claim graph).  Pre-fix this hold time blocked
    /// every concurrent `search`, `read_source`, and `brain_load`
    /// against the same workspace.  We now clone the `GraphStore`
    /// (cheap — `Arc<DbInstance>`-backed) under the lock and release
    /// it before calling into the verifier; CozoDB's internal locking
    /// still serialises actual disk operations, but at the page-cache
    /// granularity the engine already supports.
    pub async fn health(&self, ws: &str) -> Result<VerificationResult> {
        let handle = self.get_workspace(ws)?;
        let graph = {
            let storage = handle.storage.lock().await;
            storage.graph.clone()
        };
        let verifier = Verifier::new(&handle.config);
        verifier.verify(&graph)
    }

    /// Read the exact source bytes a claim cites. Powers the v3 MCP
    /// `read_source` tool: given a claim id, resolve the claim → source
    /// row → on-disk content_hash, then slice the source bytes by the
    /// claim's persisted byte range. Returns `Err(NotFound)` when the
    /// claim id is unknown; returns a `ReadSourceResult` with empty `text`
    /// + `bytes` when the claim has no byte range or the source bytes
    /// were never persisted (older workspaces).
    pub async fn read_source(&self, ws: &str, claim_id: &str) -> Result<ReadSourceResult> {
        use thinkingroot_core::Error;
        use thinkingroot_graph::{FileSystemSourceStore, SourceByteStore};

        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;

        // 1. Look up the claim by id. Missing → propagate as ClaimNotFound
        //    so the MCP tool can return a clean error to the model.
        let claim = storage
            .graph
            .get_claim_by_id(claim_id)?
            .ok_or_else(|| Error::ClaimNotFound(claim_id.to_string()))?;

        // 2. Resolve the source row (uri + content_hash). The claim's
        //    `source` field is the SourceId; the URI we surface to the
        //    caller is the Source.uri so v3 packs cite by file path.
        let source = storage
            .graph
            .get_source_by_id(&claim.source.to_string())?
            .ok_or_else(|| {
                Error::GraphStorage(format!(
                    "claim {} cites source {} which is not in the graph",
                    claim_id, claim.source
                ))
            })?;

        // 3. Determine byte range. Pre-v3 claims (no source_span or
        //    line-only) have unknown byte ranges; return an empty result
        //    rather than an error so the caller can fall back to
        //    `read_file` for the same source.
        let (byte_start, byte_end) = match claim.source_span {
            Some(span) => match (span.byte_start, span.byte_end) {
                (Some(bs), Some(be)) if be > bs => (bs, be),
                _ => {
                    return Ok(ReadSourceResult {
                        file: source.uri.clone(),
                        byte_start: 0,
                        byte_end: 0,
                        text: String::new(),
                        bytes: Vec::new(),
                    });
                }
            },
            None => {
                return Ok(ReadSourceResult {
                    file: source.uri.clone(),
                    byte_start: 0,
                    byte_end: 0,
                    text: String::new(),
                    bytes: Vec::new(),
                });
            }
        };

        // 4. Read the bytes from the FileSystemSourceStore. Sources
        //    without a content_hash (synthetic agent contributions) or
        //    whose bytes were never persisted return an empty result.
        if source.content_hash.is_empty() {
            return Ok(ReadSourceResult {
                file: source.uri.clone(),
                byte_start,
                byte_end,
                text: String::new(),
                bytes: Vec::new(),
            });
        }
        let store = FileSystemSourceStore::new(&handle.root_path.join(".thinkingroot"))
            .map_err(|e| Error::GraphStorage(format!("source store init: {e}")))?;
        let bytes = store
            .get_range(&source.content_hash, byte_start as usize, byte_end as usize)
            .map_err(|e| Error::GraphStorage(format!("source read: {e}")))?
            .unwrap_or_default();
        let text = String::from_utf8(bytes.clone()).unwrap_or_default();

        Ok(ReadSourceResult {
            file: source.uri,
            byte_start,
            byte_end,
            text,
            bytes,
        })
    }

    /// Return Rooting admission-tier counts for a workspace.
    /// Bypasses the in-memory cache — queries CozoDB directly so MCP callers
    /// always see the freshest tier distribution (Phase 6.5 writes verdicts
    /// synchronously).
    pub async fn rooting_report(&self, ws: &str) -> Result<RootingReport> {
        let handle = self.get_workspace(ws)?;
        // Clone the graph handle out from under the mutex so the tier
        // count query (~50–200 ms over the whole `verification_certificates`
        // scan on a 50k-claim graph) doesn't block concurrent
        // search/brain_load callers.  See `health()` for the
        // architectural rationale.
        let graph = {
            let storage = handle.storage.lock().await;
            storage.graph.clone()
        };
        let (rooted, attested, quarantined, rejected) = graph.count_claims_by_admission_tier()?;
        Ok(RootingReport {
            workspace: ws.to_string(),
            rooted,
            attested,
            quarantined,
            rejected,
            total: rooted + attested + quarantined + rejected,
        })
    }

    /// List claims that passed Rooting with the `rooted` admission tier.
    /// Returns `ClaimInfo` rows (same shape as `list_claims`) but guaranteed
    /// tier-filtered. Reads from CozoDB directly for freshness.
    pub async fn list_rooted_claims(
        &self,
        ws: &str,
        type_filter: Option<String>,
        entity_filter: Option<String>,
        min_confidence: Option<f64>,
    ) -> Result<Vec<ClaimInfo>> {
        let handle = self.get_workspace(ws)?;
        let graph = {
            let storage = handle.storage.lock().await;
            storage.graph.clone()
        };

        let rows = graph.get_rooted_claims_filtered(
            type_filter.as_deref(),
            entity_filter.as_deref(),
            min_confidence,
        )?;
        Ok(rows
            .into_iter()
            .map(
                |(id, statement, claim_type, confidence, source_uri, event_date_raw)| {
                    let event_date = if event_date_raw != 0.0 {
                        Some(event_date_raw)
                    } else {
                        None
                    };
                    ClaimInfo {
                        id,
                        statement,
                        claim_type: thinkingroot_core::types::ClaimType::normalize_storage(
                            &claim_type,
                        ),
                        confidence,
                        source_uri,
                        event_date,
                    }
                },
            )
            .collect())
    }

    /// Run the full pipeline for a mounted workspace, then refresh the in-memory cache.
    ///
    /// ## Phase C lock-contention design
    ///
    /// The naive approach holds the storage `Mutex` for the entire cache rebuild
    /// (~2 s at Large scale), blocking all vector searches. Instead we use a
    /// three-phase pattern that minimises lock hold times:
    ///
    /// 1. **Pipeline** — runs its own `StorageEngine` internally; no lock held here.
    /// 2. **Noop guard** — if the pipeline reported no changes, skip the reload entirely.
    /// 3. **Fetch raw** — hold storage `Mutex` only for the 6–8 CozoDB bulk queries
    ///    (~300–600 ms), then release before building indexes.
    /// 4. **Build indexes** — pure CPU, no locks held (~400–800 ms). Vector searches
    ///    can proceed concurrently during this phase.
    /// 5. **Atomic swap** — acquire cache write lock only for the pointer swap (~100 μs).
    ///
    /// Net result: storage Mutex contention reduced from ~2 s → ~600 ms;
    ///             cache write-lock contention reduced from ~2 s → ~100 μs.
    pub async fn compile(&self, ws: &str) -> Result<PipelineResult> {
        let handle = self.get_workspace(ws)?;

        // Phase 1: Run pipeline (creates its own StorageEngine — no handle locks held).
        let result = crate::pipeline::run_pipeline(&handle.root_path, None, None).await?;

        // Phase 2: Noop guard — if nothing changed, the cache is still current.
        if !result.cache_dirty {
            tracing::debug!("compile noop — all files unchanged, skipping cache reload");
            return Ok(result);
        }

        // Phase 3: Fetch raw rows from CozoDB — hold storage Mutex only for I/O.
        // Cache fetch failure is a hard error: the compile already wrote to
        // graph.db, but if we returned Ok with the in-memory cache stale,
        // the next query would lie about the workspace's contents (the
        // exact "silent partial success" CLAUDE.md no-silent-failure
        // contract forbids).
        let raw_data: RawGraphData = {
            let storage = handle.storage.lock().await;
            KnowledgeGraph::fetch_raw(&storage.graph).map_err(|e| {
                Error::GraphStorage(format!(
                    "compile: graph write succeeded but in-memory cache rebuild failed for \
                     workspace '{ws_name}' — your data is durable on disk, but reads will \
                     see stale results until the cache reloads.  Re-run `root compile` or \
                     remount the workspace.  Underlying error: {e}",
                    ws_name = handle.name
                ))
            })?
        }; // ← storage Mutex released here; vector searches can resume immediately

        // Phase 4: Build in-memory indexes — pure CPU, zero locks held.
        let new_cache = KnowledgeGraph::build_from_raw(raw_data);

        // Phase 5: Atomic swap — write lock held only for the pointer assignment (~100 μs).
        *handle.cache.write().await = new_cache;

        Ok(result)
    }

    /// Search the workspace using vector similarity + keyword fallback.
    ///
    /// Vector search still goes to VectorStore (fastembed).
    /// Entity/claim lookups and claim counts are served from the in-memory cache
    /// — eliminating the N+1 CozoDB queries the old implementation required.
    pub async fn search(&self, ws: &str, query: &str, top_k: usize) -> Result<SearchResult> {
        let handle = self.get_workspace(ws)?;

        // Phase 1: Vector search — brief storage lock, released immediately after.
        // block_in_place: .search() runs synchronous ONNX inference (can take
        // 1–100ms). Without this wrap the tokio reactor stalls under concurrent
        // load — regresses the 10K-VU p95 claim.
        let vector_results = {
            let mut storage = handle.storage.lock().await;
            run_blocking(|| storage.vector.search(query, top_k * 2))?
            // storage Mutex drops here
        };

        let mut entity_hits: Vec<EntitySearchHit> = Vec::new();
        let mut claim_hits: Vec<ClaimSearchHit> = Vec::new();
        let mut seen_entity_ids: HashSet<String> = HashSet::new();
        let mut seen_claim_ids: HashSet<String> = HashSet::new();

        // Phase 2: Resolve vector hits from cache — O(1) per hit, no disk I/O.
        {
            let cache = handle.cache.read().await;

            for (key, _metadata, score) in &vector_results {
                if *score < 0.1 {
                    continue;
                }

                if let Some(bare_id) = key.strip_prefix("entity:")
                    && let Some(e) = cache.entity_by_id(bare_id)
                    && seen_entity_ids.insert(e.id.clone())
                {
                    entity_hits.push(EntitySearchHit {
                        id: e.id.clone(),
                        name: e.canonical_name.clone(),
                        entity_type: thinkingroot_core::types::EntityType::normalize_storage(
                            &e.entity_type,
                        ),
                        claim_count: cache.entity_claim_count(&e.id),
                        relevance: *score,
                    });
                    continue;
                }

                if let Some(bare_id) = key.strip_prefix("claim:")
                    && let Some(c) = cache.claim_by_id(bare_id)
                    && seen_claim_ids.insert(c.id.clone())
                {
                    claim_hits.push(ClaimSearchHit {
                        id: c.id.clone(),
                        statement: c.statement.clone(),
                        claim_type: c.claim_type.clone(),
                        confidence: c.confidence,
                        source_uri: c.source_uri.clone(),
                        relevance: *score,
                        // The in-memory cache (CachedClaim) does not materialize a
                        // claim-level ingestion timestamp, so recency on this path falls
                        // back to session_dates in the synthesizer.
                        valid_from: 0,
                    });
                }
            }
            // cache read lock drops here — must release before acquiring storage lock below
        }

        // Phase 3: Keyword fallback if vector didn't return enough.
        // Storage lock acquired separately (never held simultaneously with cache lock).
        if entity_hits.len() + claim_hits.len() < top_k {
            let (kw_entities, kw_claims) = {
                let storage = handle.storage.lock().await;
                let ents = storage.graph.search_entities(query)?;
                let cls = storage.graph.search_claims(query)?;
                (ents, cls)
                // storage Mutex drops here
            };

            let cache = handle.cache.read().await;

            for (eid, ename, etype) in kw_entities {
                if seen_entity_ids.insert(eid.clone()) {
                    entity_hits.push(EntitySearchHit {
                        claim_count: cache.entity_claim_count(&eid),
                        id: eid,
                        name: ename,
                        entity_type: etype,
                        relevance: 0.5,
                    });
                }
            }

            for (cid, stmt, ctype, conf, uri) in kw_claims {
                if seen_claim_ids.insert(cid.clone()) {
                    claim_hits.push(ClaimSearchHit {
                        id: cid,
                        statement: stmt,
                        claim_type: ctype,
                        confidence: conf,
                        source_uri: uri,
                        relevance: 0.5,
                        // keyword fallback carries no claim timestamp — the
                        // synthesizer falls back to session_dates for recency.
                        valid_from: 0,
                    });
                }
            }
            // cache read lock drops here
        }

        // Sort by descending relevance and truncate.
        entity_hits.sort_by(|a, b| {
            b.relevance
                .partial_cmp(&a.relevance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        claim_hits.sort_by(|a, b| {
            b.relevance
                .partial_cmp(&a.relevance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        entity_hits.truncate(top_k);
        claim_hits.truncate(top_k);

        Ok(SearchResult {
            entities: entity_hits,
            claims: claim_hits,
        })
    }

    /// Source-scoped search: same as `search` but restricts claim results to
    /// those whose source URI contains one of the `allowed_source_ids` substrings.
    ///
    /// Used for multi-user graphs where each query must be isolated to a specific
    /// user's sessions. Entities are always included (user-agnostic structural nodes).
    pub async fn search_scoped(
        &self,
        ws: &str,
        query: &str,
        top_k: usize,
        allowed_source_ids: &HashSet<String>,
    ) -> Result<SearchResult> {
        let handle = self.get_workspace(ws)?;
        let start = std::time::Instant::now();
        let mut result = Self::search_scoped_on(handle, query, top_k, allowed_source_ids).await;

        // Two-tier recall (TR_TWO_TIER_RECALL, default-off): a per-user `u_*`
        // scope ALSO recalls from the shared/primary brain and merges the hits,
        // so a single agent serves the user's PRIVATE memory plus the project's
        // SHARED knowledge. Flag-gated so this deploy is zero behaviour-change
        // until validated; per-user scopes only, and it only ever unions `u_*`
        // with the primary brain — never another user's workspace.
        let two_tier = is_auto_scoped_ws(ws)
            && std::env::var("TR_TWO_TIER_RECALL")
                .map(|v| v == "1")
                .unwrap_or(false);
        if two_tier && result.is_ok() {
            // Slice 1b: inherit recall from each PARENT brain in the inheritance
            // chain — for a composite `u_X__agent_Y` scope that's the agent's
            // own brain `agent_Y` THEN the shared brain; for a plain `u_X` it's
            // just the shared brain (identical to before). So a per-user×agent
            // run recalls its OWN private memory + the agent's + the project's.
            // Each parent is searched unscoped (its sources aren't the user's
            // session ids) and merged; never crosses into another user's brain.
            let unscoped: HashSet<String> = HashSet::new();
            for parent in self.inheritance_chain(ws).into_iter().skip(1) {
                let Ok(phandle) = self.get_workspace(&parent) else {
                    continue;
                };
                if let Ok(shared) = Self::search_scoped_on(phandle, query, top_k, &unscoped).await
                    && let Ok(base) = result.as_mut()
                {
                    Self::merge_search_results(base, shared, top_k);
                }
            }
        }

        // §8 — read-path latency for the <100ms dashboard. `search_scoped` is the
        // choke point the /ask retriever (`intelligence::retriever`) actually
        // calls (the MCP `hybrid_retrieve` tool emits its own event separately).
        // One structured event per scoped retrieval: total ms + hit counts.
        let elapsed_ms = start.elapsed().as_secs_f32() * 1000.0;
        if let Ok(r) = &result {
            tracing::info!(
                elapsed_ms,
                claims = r.claims.len(),
                entities = r.entities.len(),
                "retrieval_complete"
            );
        }
        result
    }

    /// Merge shared-brain hits into a per-user recall (two-tier recall): append
    /// `extra` hits not already present (dedup by id), re-sort by relevance, and
    /// keep the top `top_k` of each kind. Only used when `TR_TWO_TIER_RECALL` is on.
    fn merge_search_results(base: &mut SearchResult, extra: SearchResult, top_k: usize) {
        let seen_c: HashSet<String> = base.claims.iter().map(|c| c.id.clone()).collect();
        for c in extra.claims {
            if !seen_c.contains(&c.id) {
                base.claims.push(c);
            }
        }
        base.claims.sort_by(|a, b| {
            b.relevance
                .partial_cmp(&a.relevance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        base.claims.truncate(top_k);

        let seen_e: HashSet<String> = base.entities.iter().map(|e| e.id.clone()).collect();
        for e in extra.entities {
            if !seen_e.contains(&e.id) {
                base.entities.push(e);
            }
        }
        base.entities.sort_by(|a, b| {
            b.relevance
                .partial_cmp(&a.relevance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        base.entities.truncate(top_k);
    }

    /// Late-interaction (MaxSim) scores for a candidate claim-id set — the
    /// Layer 6.4 tier in hybrid retrieval (NOW item 5). Takes BARE claim ids
    /// (the `claims` table form); the `claim:` vector-index key convention is
    /// applied and stripped here. Empty result = no signal (tier disabled,
    /// no token index, or no candidate has token vectors) — callers keep the
    /// fused order.
    pub async fn late_interaction_scores(
        &self,
        ws: &str,
        query: &str,
        candidate_ids: &[String],
    ) -> Result<Vec<(String, f32)>> {
        let handle = self.get_workspace(ws)?;
        let keys: Vec<String> = candidate_ids.iter().map(|id| format!("claim:{id}")).collect();
        let mut storage = handle.storage.lock().await;
        // block_in_place: embeds the query (ONNX) when the tier is active.
        let scored = run_blocking(|| storage.vector.max_sim_rerank(query, &keys))?;
        Ok(scored
            .into_iter()
            .map(|(key, s)| (key.strip_prefix("claim:").unwrap_or(&key).to_string(), s))
            .collect())
    }

    /// Handle-based core of [`Self::search_scoped`] — operates directly on a
    /// `WorkspaceHandle` so a *captured* handle (e.g. a Root Function's
    /// [`FnCapabilities`]) can recall over the cognition graph without
    /// re-acquiring the engine `RwLock` mid-run (which would risk the
    /// writer-queue deadlock when a workspace mounts during a function run).
    pub(crate) async fn search_scoped_on(
        handle: &WorkspaceHandle,
        query: &str,
        top_k: usize,
        allowed_source_ids: &HashSet<String>,
    ) -> Result<SearchResult> {
        // Phase 1: Scoped vector search.
        // Empty allowed_source_ids means "no session scope" — treat as unscoped (all sources).
        let scope = if allowed_source_ids.is_empty() {
            None
        } else {
            Some(allowed_source_ids)
        };
        // block_in_place: see rationale on `search` above.
        let vector_results = {
            let mut storage = handle.storage.lock().await;
            run_blocking(|| storage.vector.search_scoped(query, top_k * 3, scope))?
        };

        let mut entity_hits: Vec<EntitySearchHit> = Vec::new();
        let mut claim_hits: Vec<ClaimSearchHit> = Vec::new();
        let mut seen_entity_ids: HashSet<String> = HashSet::new();
        let mut seen_claim_ids: HashSet<String> = HashSet::new();

        {
            let cache = handle.cache.read().await;
            for (key, _metadata, score) in &vector_results {
                if *score < 0.1 {
                    continue;
                }
                if let Some(bare_id) = key.strip_prefix("entity:")
                    && let Some(e) = cache.entity_by_id(bare_id)
                    && seen_entity_ids.insert(e.id.clone())
                {
                    entity_hits.push(EntitySearchHit {
                        id: e.id.clone(),
                        name: e.canonical_name.clone(),
                        entity_type: thinkingroot_core::types::EntityType::normalize_storage(
                            &e.entity_type,
                        ),
                        claim_count: cache.entity_claim_count(&e.id),
                        relevance: *score,
                    });
                    continue;
                }
                if let Some(bare_id) = key.strip_prefix("claim:")
                    && let Some(c) = cache.claim_by_id(bare_id)
                    && seen_claim_ids.insert(c.id.clone())
                {
                    // Double-check source scope at claim level (defense in depth).
                    // Empty allowed_sources = unscoped — accept all claims.
                    let in_scope = allowed_source_ids.is_empty()
                        || allowed_source_ids
                            .iter()
                            .any(|sid| c.source_uri.contains(sid.as_str()));
                    if in_scope {
                        claim_hits.push(ClaimSearchHit {
                            id: c.id.clone(),
                            statement: c.statement.clone(),
                            claim_type: c.claim_type.clone(),
                            confidence: c.confidence,
                            source_uri: c.source_uri.clone(),
                            relevance: *score,
                            // The in-memory cache (CachedClaim) does not materialize a
                            // claim-level ingestion timestamp, so recency on this path falls
                            // back to session_dates in the synthesizer.
                            valid_from: 0,
                        });
                    }
                }
            }
        }

        // Phase 2: Keyword fallback scoped to allowed sources.
        if claim_hits.len() < top_k {
            let kw_claims = {
                let storage = handle.storage.lock().await;
                storage.graph.search_claims(query)?
            };
            for (cid, stmt, ctype, conf, uri) in kw_claims {
                // Mirror the vector-phase scope rule above: an empty
                // `allowed_source_ids` means "no scope filter" and
                // every claim is admitted.  Pre-fix the keyword
                // fallback used `iter().any(...)` which is `false`
                // for an empty iterator — so unscoped queries that
                // fell below `top_k` vector hits silently lost
                // recall on sparse workspaces.
                let in_scope = allowed_source_ids.is_empty()
                    || allowed_source_ids
                        .iter()
                        .any(|sid| uri.contains(sid.as_str()));
                if in_scope && seen_claim_ids.insert(cid.clone()) {
                    claim_hits.push(ClaimSearchHit {
                        id: cid,
                        statement: stmt,
                        claim_type: ctype,
                        confidence: conf,
                        source_uri: uri,
                        relevance: 0.5,
                        // populated below via a single batched graph lookup.
                        valid_from: 0,
                    });
                }
            }
        }

        // Per-claim recency: the in-memory cache (CachedClaim) carries no
        // ingestion timestamp, and the keyword fallback carries none either, so
        // both phases above left `valid_from: 0`. Resolve real per-claim
        // timestamps with ONE batched graph read (claims.created_at, falling
        // back to claim_temporal.valid_from) so the /ask synthesizer can split
        // recency on real claim recency instead of tying on session date. Ids
        // that don't resolve keep `0` → session-date fallback downstream.
        if !claim_hits.is_empty() {
            let ids: Vec<&str> = claim_hits.iter().map(|h| h.id.as_str()).collect();
            let valid_from = {
                let storage = handle.storage.lock().await;
                storage.graph.claim_valid_from(&ids)?
            };
            for hit in claim_hits.iter_mut() {
                if let Some(ts) = valid_from.get(&hit.id) {
                    hit.valid_from = *ts;
                }
            }
        }

        entity_hits.sort_by(|a, b| {
            b.relevance
                .partial_cmp(&a.relevance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        claim_hits.sort_by(|a, b| {
            b.relevance
                .partial_cmp(&a.relevance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        entity_hits.truncate(top_k);
        claim_hits.truncate(top_k);

        Ok(SearchResult {
            entities: entity_hits,
            claims: claim_hits,
        })
    }

    /// List tracked contradictions in the workspace.
    /// Served from in-memory cache.
    pub async fn list_contradictions(&self, ws: &str) -> Result<Vec<ContradictionInfo>> {
        let handle = self.get_workspace(ws)?;
        let cache = handle.cache.read().await;

        Ok(cache
            .all_contradictions()
            .iter()
            .map(|c| ContradictionInfo {
                id: c.id.clone(),
                claim_a: c.claim_a.clone(),
                claim_b: c.claim_b.clone(),
                explanation: c.explanation.clone(),
                status: c.status.clone(),
            })
            .collect())
    }

    /// B5 / "sleep" — a consolidation pass that turns experience into wisdom:
    /// resolve unresolved contradictions by **superseding the older/less-confident
    /// claim** (keep the more recent, more confident truth), then clear the
    /// contradiction. Superseding runs the engine's causal-invalidation + capsule
    /// eviction, so the next recall returns the surviving claim, not the stale one.
    /// Idempotent + safe to schedule nightly. (Stale-fact expiry + downscaling are
    /// the B5.2 follow-on; contradiction resolution is the headline "wakes wiser".)
    pub async fn sleep_consolidate(
        &self,
        ws: &str,
        stale_before: Option<f64>,
        conf_floor: f64,
    ) -> Result<SleepReport> {
        let handle = self.get_workspace(ws)?;

        // Phase 1 — decide supersessions from unresolved contradictions (read cache).
        // Survivor = newer event_date, tie-broken by higher confidence.
        let mut decisions: Vec<(String, String, String)> = Vec::new(); // (loser, winner, contradiction_id)
        {
            let cache = handle.cache.read().await;
            for c in cache
                .all_contradictions()
                .iter()
                .filter(|c| c.status == "Detected")
            {
                if let (Some(a), Some(b)) =
                    (cache.claim_by_id(&c.claim_a), cache.claim_by_id(&c.claim_b))
                {
                    let a_key = (a.event_date.unwrap_or(0.0), a.confidence);
                    let b_key = (b.event_date.unwrap_or(0.0), b.confidence);
                    let (winner, loser) = if a_key >= b_key {
                        (a.id.clone(), b.id.clone())
                    } else {
                        (b.id.clone(), a.id.clone())
                    };
                    decisions.push((loser, winner, c.id.clone()));
                }
            }
        }

        // Phase 2 — apply under the storage lock, then reload the read cache so the
        // next recall in this process sees the resolution (honesty contract).
        let mut superseded = 0usize;
        let mut stale_expired = 0usize;
        if !decisions.is_empty() || stale_before.is_some() {
            let storage = handle.storage.lock().await;
            for (loser, winner, cid) in &decisions {
                if storage.graph.supersede_claim(loser, winner).is_ok() {
                    superseded += 1;
                    let _ = storage.graph.resolve_contradiction(cid);
                }
            }
            // B5.2 — expire old, low-confidence claims (opt-in via a cutoff).
            if let Some(cutoff) = stale_before {
                stale_expired = storage
                    .graph
                    .expire_stale_claims(cutoff, conf_floor)
                    .unwrap_or(0);
            }
            let new_cache = KnowledgeGraph::load_from_graph(&storage.graph)
                .map_err(|e| Error::GraphStorage(format!("sleep: cache reload failed: {e}")))?;
            *handle.cache.write().await = new_cache;
        }

        Ok(SleepReport {
            contradictions_resolved: superseded,
            claims_superseded: superseded,
            stale_expired,
        })
    }

    /// §11 #26 — Night Shift DREAM: generative abstraction. Synthesize
    /// higher-level insights/playbooks from existing claims using the
    /// workspace's OWN LLM (customer's model — not a new neural model), write
    /// them to a QUARANTINED dream branch (A2 isolation), then verify-before-
    /// merge into main (`auto_merge`) or leave them on the branch for review.
    /// The novel part vs opaque consumer "dreaming": branch-quarantined +
    /// merge-gated + provenance-tracked (`dream://`). Honest: too-few-claims or
    /// no-insight passes return a no-op report, never fabricated rows.
    pub async fn dream(
        &self,
        ws: &str,
        max_claims: usize,
        max_insights: usize,
        auto_merge: bool,
        sessions: &crate::intelligence::session::SessionStore,
    ) -> Result<DreamReport> {
        let llm = self.workspace_llm(ws).ok_or_else(|| {
            Error::Config(format!("workspace '{ws}' has no LLM configured — cannot dream"))
        })?;
        let handle = self.get_workspace(ws)?;
        let root = handle.root_path.clone();

        // Sample claim statements from the read cache (cheap; the dream
        // abstracts over what's already there).
        let claims: Vec<String> = {
            let cache = handle.cache.read().await;
            cache
                .all_claims()
                .filter(|c| !c.statement.trim().is_empty())
                .take(max_claims.clamp(1, 200))
                .map(|c| c.statement.clone())
                .collect()
        };
        if claims.len() < 3 {
            return Ok(DreamReport {
                insights: 0,
                branch: String::new(),
                merged: false,
                note: "not enough claims to dream over (need ≥3)".to_string(),
            });
        }

        // Generative abstraction via the workspace LLM.
        let prompt = crate::intelligence::dream::build_dream_prompt(&claims);
        let out = llm.chat(crate::intelligence::dream::DREAM_SYSTEM, &prompt).await?;
        let insights =
            crate::intelligence::dream::parse_dream_insights(&out, max_insights.clamp(1, 20));
        if insights.is_empty() {
            return Ok(DreamReport {
                insights: 0,
                branch: String::new(),
                merged: false,
                note: "no insights synthesized this pass".to_string(),
            });
        }

        // Quarantined dream branch (A2 isolation).
        let branch = format!("dream/{}", ulid::Ulid::new());
        thinkingroot_branch::create_branch(
            &root,
            &branch,
            "main",
            Some("night-shift dream — quarantined generative abstraction".to_string()),
        )
        .await
        .map_err(|e| Error::Config(format!("dream: fork failed: {e}")))?;

        // Write the insights to the dream branch (quarantined), tagged as
        // dream-derived so provenance is honest.
        let agent_claims: Vec<AgentClaim> = insights
            .iter()
            .map(|s| AgentClaim {
                statement: s.clone(),
                claim_type: "insight".to_string(),
                confidence: Some(0.6),
                entities: vec![],
            })
            .collect();
        let idem = format!("dream:{branch}");
        let principal =
            Principal::Connector { connector_id: "dream".to_string(), install_id: "night".to_string() };
        self.contribute_bulk(ws, &idem, Some(&branch), agent_claims, sessions, principal, &idem, false)
            .await?;

        // Verify-before-merge: merge the dream into main (kept) or leave it for
        // review (discard = the branch simply isn't merged).
        let merged = if auto_merge {
            let merged_by =
                thinkingroot_core::MergedBy::Agent { agent_id: format!("dream:{branch}") };
            self.merge_branch(&root, &branch, false, false, merged_by).await.is_ok()
        } else {
            false
        };

        Ok(DreamReport {
            insights: insights.len(),
            branch,
            merged,
            note: if merged {
                "insights merged into main".to_string()
            } else {
                "insights kept on the dream branch (review before merge)".to_string()
            },
        })
    }

    /// §1 — the `predict` verb ("what happens next"), grounded + falsifier-gated.
    /// Recall claims relevant to the question, ask the WORKSPACE LLM (customer's
    /// model — NOT the excluded generative adapter) to infer the next outcome
    /// ONLY from those claims, then enforce verified-or-silent: refuse when
    /// there's no relevant memory, the model declines, or the prediction cites
    /// no recalled claim (the falsifier gate). Never prophesies unbacked.
    pub async fn predict(&self, ws: &str, question: &str, top_k: usize) -> Result<PredictReport> {
        let llm = self.workspace_llm(ws).ok_or_else(|| {
            Error::Config(format!("workspace '{ws}' has no LLM configured — cannot predict"))
        })?;
        let empty = std::collections::HashSet::new();
        let res = self.search_scoped(ws, question, top_k.clamp(1, 50), &empty).await?;
        let claims: Vec<(String, String)> =
            res.claims.iter().map(|c| (c.id.clone(), c.statement.clone())).collect();
        let refused = |note: &str| PredictReport {
            prediction: String::new(),
            confidence: 0.0,
            citations: vec![],
            refused: true,
            note: note.to_string(),
        };
        if claims.is_empty() {
            return Ok(refused("no relevant memory to predict from"));
        }

        let prompt = crate::intelligence::predict::build_predict_prompt(question, &claims);
        let out = llm.chat(crate::intelligence::predict::PREDICT_SYSTEM, &prompt).await?;
        if crate::intelligence::predict::is_refusal(&out) {
            return Ok(refused("insufficient evidence to predict"));
        }
        // Falsifier gate: the prediction must cite a RECALLED claim.
        let recalled: std::collections::HashSet<&str> =
            claims.iter().map(|(id, _)| id.as_str()).collect();
        let grounded: Vec<String> = crate::intelligence::citations::parse_all_markers(&out)
            .into_iter()
            .filter(|id| recalled.contains(id.as_str()))
            .collect();
        if grounded.is_empty() {
            return Ok(refused("prediction had no grounded citation — withheld"));
        }
        let confidence = crate::intelligence::predict::parse_confidence(&out).unwrap_or(0.5);
        Ok(PredictReport {
            prediction: out.trim().to_string(),
            confidence,
            citations: grounded,
            refused: false,
            note: "grounded prediction".to_string(),
        })
    }

    /// P2 — the being's HONEST developmental age (see [`AgeReport`]): verified
    /// capability mass (Σ Wilson over learned experience) + knowledge breadth +
    /// reconciliations, mapped to a coarse life stage. Pure counts, no embedder.
    pub async fn developmental_age(&self, ws: &str) -> Result<AgeReport> {
        let handle = self.get_workspace(ws)?;
        let claims = {
            let cache = handle.cache.read().await;
            cache.counts().1
        };
        let storage = handle.storage.lock().await;
        let total_capabilities = storage.graph.list_functions()?.len();
        let exp = storage.graph.list_all_experience()?;
        let mut capability_score = 0.0_f64;
        let mut verified: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for (_class, e) in &exp {
            capability_score += e.score();
            if e.n_success >= 1 {
                verified.insert(e.function_name.clone());
            }
        }
        let superseded_claims = storage.graph.count_superseded_claims()?;
        drop(storage);

        let verified_capabilities = verified.len();
        let developmental_age =
            capability_score + (1.0 + claims as f64).ln() + 0.1 * superseded_claims as f64;
        let stage = if verified_capabilities == 0 && claims < 5 {
            "infant"
        } else if developmental_age < 3.0 {
            "adolescent"
        } else if developmental_age < 8.0 {
            "adult"
        } else {
            "elder"
        }
        .to_string();

        Ok(AgeReport {
            verified_capabilities,
            total_capabilities,
            capability_score,
            claims,
            superseded_claims,
            developmental_age,
            stage,
        })
    }

    /// P3 — the being's DRIVES (see [`DrivesReport`]): curiosity decays as measured
    /// maturity (verified capability + breadth + knowledge) rises, shifting the
    /// being from "explore + forge widely" (infant) to "exploit; forge only at
    /// verified frontier gaps" (elder). Derived from [`Self::developmental_age`].
    pub async fn drives(&self, ws: &str) -> Result<DrivesReport> {
        let age = self.developmental_age(ws).await?;
        let maturity = 0.3 * age.capability_score
            + 0.3 * age.total_capabilities as f64
            + 0.2 * (1.0 + age.claims as f64).ln();
        let curiosity = 1.0 / (1.0 + maturity); // (0,1], decays as maturity rises
        let frontier_focus = 1.0 - curiosity;
        let recommendation = match age.stage.as_str() {
            "infant" => "explore widely; forge readily — everything is new",
            "adolescent" => "explore broadly; forge to fill frequent gaps",
            "adult" => "balance explore/exploit; forge selectively at real gaps",
            _ => "exploit mastered skills; forge only at verified frontier gaps",
        }
        .to_string();
        Ok(DrivesReport {
            stage: age.stage,
            curiosity,
            exploration_rate: curiosity,
            frontier_focus,
            recommendation,
        })
    }

    /// P4 — bequeath this being's VERIFIED inheritance: capabilities (the genome)
    /// and high-confidence knowledge. With `only_verified`, capabilities are limited
    /// to functions with ≥1 successful invocation (proven skills). Knowledge is
    /// limited to claims at or above `min_confidence` — never the raw memory stream.
    pub async fn bequeath(
        &self,
        ws: &str,
        min_confidence: f64,
        only_verified: bool,
    ) -> Result<LegacyBundle> {
        let age = self.developmental_age(ws).await?;
        let handle = self.get_workspace(ws)?;
        let capabilities = {
            let storage = handle.storage.lock().await;
            let verified: std::collections::BTreeSet<String> = storage
                .graph
                .list_all_experience()?
                .into_iter()
                .filter(|(_c, e)| e.n_success >= 1)
                .map(|(_c, e)| e.function_name)
                .collect();
            storage
                .graph
                .list_functions()?
                .into_iter()
                .filter(|f| !only_verified || verified.contains(&f.name))
                .map(|f| LegacyCapability {
                    name: f.name,
                    body: f.body,
                    language: f.language,
                })
                .collect::<Vec<_>>()
        };
        let knowledge = {
            let cache = handle.cache.read().await;
            cache
                .all_claims()
                .filter(|c| c.confidence >= min_confidence)
                .map(|c| LegacyClaim {
                    statement: c.statement.clone(),
                    confidence: c.confidence,
                })
                .collect::<Vec<_>>()
        };
        Ok(LegacyBundle {
            capabilities,
            knowledge,
            forebear_stage: age.stage,
            forebear_age: age.developmental_age,
        })
    }

    /// P4 — inherit a [`LegacyBundle`] into this (successor) workspace: deploy the
    /// forebear's verified capabilities and seed its high-confidence knowledge, so
    /// the successor provably starts from confirmed skills. Reloads the read cache.
    pub async fn inherit(&self, ws: &str, bundle: LegacyBundle) -> Result<InheritReport> {
        let handle = self.get_workspace(ws)?;
        let mut capabilities_inherited = 0usize;
        let mut knowledge_inherited = 0usize;
        {
            let storage = handle.storage.lock().await;
            for c in &bundle.capabilities {
                if storage
                    .graph
                    .put_function(&c.name, &c.body, &c.language)
                    .is_ok()
                {
                    capabilities_inherited += 1;
                }
            }
            if !bundle.knowledge.is_empty() {
                let source = thinkingroot_core::Source::new(
                    "legacy://inheritance".into(),
                    thinkingroot_core::SourceType::Document,
                );
                let source_id = source.id;
                let _ = storage.graph.insert_source(&source);
                let wsid = thinkingroot_core::WorkspaceId::new();
                for k in &bundle.knowledge {
                    let claim = thinkingroot_core::Claim::new(
                        &k.statement,
                        thinkingroot_core::ClaimType::Fact,
                        source_id,
                        wsid,
                    )
                    .with_confidence(k.confidence);
                    let cid = claim.id.to_string();
                    if storage.graph.insert_claim(&claim).is_ok() {
                        let _ = storage
                            .graph
                            .link_claim_to_source(&cid, &source_id.to_string());
                        knowledge_inherited += 1;
                    }
                }
            }
            let new_cache = KnowledgeGraph::load_from_graph(&storage.graph)
                .map_err(|e| Error::GraphStorage(format!("inherit: cache reload failed: {e}")))?;
            *handle.cache.write().await = new_cache;
        }
        Ok(InheritReport {
            capabilities_inherited,
            knowledge_inherited,
            forebear_stage: bundle.forebear_stage,
        })
    }

    /// Alias for `health()` — delegates to the same verification logic.
    pub async fn verify(&self, ws: &str) -> Result<VerificationResult> {
        self.health(ws).await
    }

    /// Return a token-efficient workspace overview for agent orientation.
    /// Served entirely from in-memory cache — zero disk I/O.
    pub async fn get_workspace_brief(&self, ws: &str) -> Result<WorkspaceSummary> {
        let handle = self.get_workspace(ws)?;
        let cache = handle.cache.read().await;

        let (source_count, claim_count, entity_count) = cache.counts();
        let top_entities = cache.top_entities_by_claim_count(10);

        let recent_decisions: Vec<(String, f64)> = cache
            .claims_of_type("Decision")
            .into_iter()
            .take(10)
            .map(|c| (c.statement.clone(), c.confidence))
            .collect();

        let contradiction_count = cache
            .all_contradictions()
            .iter()
            .filter(|c| c.status == "Detected")
            .count();

        Ok(WorkspaceSummary {
            workspace: ws.to_string(),
            entity_count,
            claim_count,
            source_count,
            top_entities,
            recent_decisions,
            contradiction_count,
        })
    }

    /// Return full graph context for a named entity.
    ///
    /// This executes 6 Datalog queries directly against CozoDB (incoming relations,
    /// per-entity contradictions). It is kept on CozoDB for correctness; Phase C
    /// will add a full entity-context cache.
    pub async fn get_entity_context(
        &self,
        ws: &str,
        entity_name: &str,
    ) -> Result<Option<thinkingroot_graph::graph::EntityContext>> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        storage.graph.get_entity_context(entity_name)
    }

    // ── Branch-aware read wrappers ────────────────────────────────────────────
    // These accept an optional branch name.  For now they delegate to the
    // main-only methods; Gap 3 (Item 5) fills in union search for branch+main.

    pub async fn search_branched(
        &self,
        ws: &str,
        query: &str,
        top_k: usize,
        branch: Option<&str>,
    ) -> Result<SearchResult> {
        use thinkingroot_branch::snapshot::resolve_data_dir;

        // Fast path: no branch → main-only search.
        let branch_name = match branch {
            Some(b) => b,
            None => return self.search(ws, query, top_k).await,
        };

        let handle = self.get_workspace(ws)?;
        let branch_data_dir = resolve_data_dir(&handle.root_path, Some(branch_name));

        // ── 1. Search branch vector index ─────────────────────────────────────
        // Vector failures here are hard errors: returning an empty hit list
        // would silently mask a corrupt/locked vector index and lie to the
        // caller about what the branch contains.  When the data dir exists
        // we must succeed or surface the failure.
        let branch_vector_hits: Vec<(String, String, f32)> = if branch_data_dir.exists() {
            let mut bv = thinkingroot_graph::vector::VectorStore::init(&branch_data_dir)
                .await
                .map_err(|e| {
                    Error::VectorStorage(format!(
                        "search_branched: failed to open branch vector store at '{}' for \
                         branch '{branch_name}' in workspace '{ws}': {e}",
                        branch_data_dir.display()
                    ))
                })?;
            run_blocking(|| bv.search(query, top_k * 2)).map_err(|e| {
                Error::VectorStorage(format!(
                    "search_branched: vector search failed for branch '{branch_name}' in \
                     workspace '{ws}': {e}"
                ))
            })?
        } else {
            vec![]
        };

        let mut entity_hits: Vec<EntitySearchHit> = Vec::new();
        let mut claim_hits: Vec<ClaimSearchHit> = Vec::new();
        let mut seen_entity_ids: HashSet<String> = HashSet::new();
        let mut seen_claim_ids: HashSet<String> = HashSet::new();

        // ── 2. Resolve branch hits (branch priority — more recent than main) ──
        if !branch_vector_hits.is_empty() {
            // Resolve branch graph via the LRU (one DbInstance per branch).
            // `get_or_open` errors if the branch is missing — we silently
            // treat that as "no branch-only resolution possible" and fall
            // back to the main cache, matching the previous behavior.
            let branch_handle = self
                .branch_engines
                .get_or_open(&handle.root_path, branch_name)
                .await
                .ok();
            let cache = handle.cache.read().await;

            for (key, _meta, score) in &branch_vector_hits {
                if *score < 0.1 {
                    continue;
                }

                if let Some(bare_id) = key.strip_prefix("entity:") {
                    if !seen_entity_ids.insert(bare_id.to_string()) {
                        continue;
                    }
                    // Try main cache first (branch inherits from main at creation).
                    if let Some(e) = cache.entity_by_id(bare_id) {
                        entity_hits.push(EntitySearchHit {
                            id: e.id.clone(),
                            name: e.canonical_name.clone(),
                            entity_type: thinkingroot_core::types::EntityType::normalize_storage(
                                &e.entity_type,
                            ),
                            claim_count: cache.entity_claim_count(&e.id),
                            relevance: *score,
                        });
                    } else if let Some(ref bh) = branch_handle {
                        // Branch-only entity — resolve via branch graph point-lookup.
                        if let Ok(Some((name, etype, _desc))) = bh.graph.get_entity_by_id(bare_id) {
                            entity_hits.push(EntitySearchHit {
                                id: bare_id.to_string(),
                                name,
                                entity_type:
                                    thinkingroot_core::types::EntityType::normalize_storage(&etype),
                                claim_count: 0,
                                relevance: *score,
                            });
                        }
                    }
                    continue;
                }

                if let Some(bare_id) = key.strip_prefix("claim:") {
                    if !seen_claim_ids.insert(bare_id.to_string()) {
                        continue;
                    }
                    // Try main cache first.
                    if let Some(c) = cache.claim_by_id(bare_id) {
                        claim_hits.push(ClaimSearchHit {
                            id: c.id.clone(),
                            statement: c.statement.clone(),
                            claim_type: c.claim_type.clone(),
                            confidence: c.confidence,
                            source_uri: c.source_uri.clone(),
                            relevance: *score,
                            // The in-memory cache (CachedClaim) does not materialize a
                            // claim-level ingestion timestamp, so recency on this path falls
                            // back to session_dates in the synthesizer.
                            valid_from: 0,
                        });
                    } else if let Some(ref bh) = branch_handle {
                        // Branch-only claim — resolve via branch graph point-lookup.
                        if let Ok(Some((stmt, ctype, conf, uri))) =
                            bh.graph.get_claim_with_source(bare_id)
                        {
                            claim_hits.push(ClaimSearchHit {
                                id: bare_id.to_string(),
                                statement: stmt,
                                claim_type: ctype,
                                confidence: conf,
                                source_uri: uri,
                                relevance: *score,
                                // branch point-lookup tuple has no timestamp.
                                valid_from: 0,
                            });
                        }
                    }
                }
            }
            // cache read lock drops here
        }

        // ── 3. Append main results (deduplicated against branch hits) ─────────
        let main_result = self.search(ws, query, top_k).await?;
        for hit in main_result.entities {
            if seen_entity_ids.insert(hit.id.clone()) {
                entity_hits.push(hit);
            }
        }
        for hit in main_result.claims {
            if seen_claim_ids.insert(hit.id.clone()) {
                claim_hits.push(hit);
            }
        }

        // ── 4. Sort by descending relevance and truncate ──────────────────────
        entity_hits.sort_by(|a, b| {
            b.relevance
                .partial_cmp(&a.relevance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        claim_hits.sort_by(|a, b| {
            b.relevance
                .partial_cmp(&a.relevance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        entity_hits.truncate(top_k);
        claim_hits.truncate(top_k);

        // T2.6 — apply per-branch redaction policy to claim hits.
        // Entity hits are not subject to the claim-sensitivity gate;
        // they expose entity-level metadata that doesn't carry the
        // same PII risk and the canonical name field is a join key
        // that needs to round-trip exactly.
        if let Some(policy) = Self::branch_redaction_for(
            &handle.root_path,
            branch_name,
            thinkingroot_core::OutboundMode::Search,
        ) {
            let branch_handle = self
                .branch_engines
                .get_or_open(&handle.root_path, branch_name)
                .await?;
            Self::apply_redaction_to_search_hits(
                &mut claim_hits,
                &policy,
                &branch_handle.graph,
            )?;
        }

        Ok(SearchResult {
            entities: entity_hits,
            claims: claim_hits,
        })
    }

    /// T2.6 — fetch the active redaction policy for a branch (if any).
    /// Returns `(policy, applies_to_mode)` so callers don't have to
    /// re-check `policy.applies_to(mode)` themselves.
    fn branch_redaction_for(
        root: &std::path::Path,
        branch_name: &str,
        mode: thinkingroot_core::OutboundMode,
    ) -> Option<thinkingroot_core::RedactionPolicy> {
        let branch_ref = Self::branch_ref_for_root(root, branch_name).ok().flatten()?;
        let policy = branch_ref.redaction.clone()?;
        if policy.applies_to(&mode) {
            Some(policy)
        } else {
            None
        }
    }

    /// T2.6 — apply a redaction policy to a vector of `ClaimInfo`-shaped
    /// rows, joining each row against `claims.sensitivity` for the
    /// `min_sensitivity` gate.
    ///
    /// Sensitivity is fetched in one batch via
    /// `GraphStore::get_sensitivities_for_claims` to keep the
    /// redaction overhead at one extra query per outbound call,
    /// regardless of result-set size.
    fn apply_redaction_to_claim_infos(
        rows: &mut Vec<ClaimInfo>,
        policy: &thinkingroot_core::RedactionPolicy,
        graph: &thinkingroot_graph::graph::GraphStore,
    ) -> Result<()> {
        // ── Sensitivity gating ─────────────────────────────────────
        if policy.min_sensitivity.is_some() {
            let ids: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
            let lookup = graph.get_sensitivities_for_claims(&ids)?;
            rows.retain_mut(|row| {
                let sens = lookup
                    .get(&row.id)
                    .and_then(|s| thinkingroot_core::Sensitivity::parse(s))
                    .unwrap_or(thinkingroot_core::Sensitivity::Public);
                if policy.should_drop(sens) {
                    return false;
                }
                if let Some(text) = policy.redact_text(sens) {
                    row.statement = text;
                }
                true
            });
        }
        // ── Pattern rewrite ────────────────────────────────────────
        for row in rows.iter_mut() {
            row.statement = policy.rewrite(&row.statement);
        }
        Ok(())
    }

    /// T2.6 — same as [`apply_redaction_to_claim_infos`] but for the
    /// search hits which carry their own ClaimSearchHit shape.
    fn apply_redaction_to_search_hits(
        hits: &mut Vec<ClaimSearchHit>,
        policy: &thinkingroot_core::RedactionPolicy,
        graph: &thinkingroot_graph::graph::GraphStore,
    ) -> Result<()> {
        if policy.min_sensitivity.is_some() {
            let ids: Vec<String> = hits.iter().map(|h| h.id.clone()).collect();
            let lookup = graph.get_sensitivities_for_claims(&ids)?;
            hits.retain_mut(|hit| {
                let sens = lookup
                    .get(&hit.id)
                    .and_then(|s| thinkingroot_core::Sensitivity::parse(s))
                    .unwrap_or(thinkingroot_core::Sensitivity::Public);
                if policy.should_drop(sens) {
                    return false;
                }
                if let Some(text) = policy.redact_text(sens) {
                    hit.statement = text;
                }
                true
            });
        }
        for hit in hits.iter_mut() {
            hit.statement = policy.rewrite(&hit.statement);
        }
        Ok(())
    }

    /// Branch-aware `list_claims`.
    ///
    /// A branch starts as a copy-on-write snapshot of main, then accumulates
    /// additional claims via `contribute`. So the branch graph alone is the
    /// authoritative view of what's visible on that branch — we read straight
    /// from its GraphStore and apply the same filter semantics as the
    /// main-cache path.
    ///
    /// T2.6 — when the branch carries a `RedactionPolicy` whose `modes`
    /// include `OutboundMode::ListClaims` (or is the universal empty
    /// modes vec), the result rows are filtered + rewritten before
    /// return. Sensitivity gating is one extra batched query against
    /// `claims.sensitivity` per call — pattern rewriting is per-row.
    pub async fn list_claims_branched(
        &self,
        ws: &str,
        filter: ClaimFilter,
        branch: Option<&str>,
    ) -> Result<Vec<ClaimInfo>> {
        let branch_name = match branch {
            None | Some("main") => return self.list_claims(ws, filter).await,
            Some(b) => b,
        };

        let handle = self.get_workspace(ws)?;
        let branch_handle = self
            .branch_engines
            .get_or_open(&handle.root_path, branch_name)
            .await?;
        let branch_graph = &branch_handle.graph;

        // Rows carry (id, statement, claim_type, confidence, source_uri, event_date).
        let rows: Vec<(String, String, String, f64, String, f64)> =
            if let Some(ref entity_name) = filter.entity_name {
                match branch_graph.find_entity_id_by_name(entity_name)? {
                    Some(eid) => branch_graph
                        .get_claims_with_sources_for_entity(&eid)?
                        .into_iter()
                        .map(|(id, stmt, ctype, uri, conf)| (id, stmt, ctype, conf, uri, 0.0))
                        .collect(),
                    None => return Ok(Vec::new()),
                }
            } else {
                branch_graph.get_all_claims_with_sources()?
            };

        let mut claims: Vec<ClaimInfo> = rows
            .into_iter()
            .filter(|(_, _, ctype, _, _, _)| {
                filter
                    .claim_type
                    .as_ref()
                    .is_none_or(|t| t.eq_ignore_ascii_case(ctype))
            })
            .filter(|(_, _, _, conf, _, _)| filter.min_confidence.is_none_or(|min| *conf >= min))
            .map(
                |(id, statement, claim_type, confidence, source_uri, event_date)| ClaimInfo {
                    id,
                    statement,
                    claim_type: thinkingroot_core::types::ClaimType::normalize_storage(&claim_type),
                    confidence,
                    source_uri,
                    event_date: if event_date > 0.0 {
                        Some(event_date)
                    } else {
                        None
                    },
                },
            )
            .collect();

        // Sort newest-first by event_date (matches main path at L496–501).
        claims.sort_by(|a, b| {
            b.event_date
                .unwrap_or(0.0)
                .partial_cmp(&a.event_date.unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // T2.6 — apply per-branch redaction policy *before* pagination.
        // Pagination after redaction so users get a stable page count
        // even when sensitivity-dropping shrinks the result set.
        if let Some(policy) = Self::branch_redaction_for(
            &handle.root_path,
            branch_name,
            thinkingroot_core::OutboundMode::ListClaims,
        ) {
            Self::apply_redaction_to_claim_infos(&mut claims, &policy, branch_graph)?;
        }

        apply_pagination(&mut claims, filter.offset, filter.limit);
        Ok(claims)
    }

    /// Branch-aware `get_relations`.
    pub async fn get_relations_branched(
        &self,
        ws: &str,
        entity: &str,
        branch: Option<&str>,
    ) -> Result<Vec<RelationInfo>> {
        let branch_name = match branch {
            None | Some("main") => return self.get_relations(ws, entity).await,
            Some(b) => b,
        };

        let handle = self.get_workspace(ws)?;
        let branch_handle = self
            .branch_engines
            .get_or_open(&handle.root_path, branch_name)
            .await?;

        let rows = branch_handle.graph.get_relations_for_entity(entity)?;
        Ok(rows
            .into_iter()
            .map(|(target, relation_type, strength)| RelationInfo {
                target,
                relation_type,
                strength,
            })
            .collect())
    }

    /// Branch-aware `get_workspace_brief`.
    ///
    /// Counts come from the branch graph (authoritative). Derived fields
    /// (`top_entities`, `recent_decisions`, `contradiction_count`) are read
    /// from the branch graph as well so the summary stays internally
    /// consistent with the branch's claim set.
    pub async fn get_workspace_brief_branched(
        &self,
        ws: &str,
        branch: Option<&str>,
    ) -> Result<WorkspaceSummary> {
        let branch_name = match branch {
            None | Some("main") => return self.get_workspace_brief(ws).await,
            Some(b) => b,
        };

        let handle = self.get_workspace(ws)?;
        let branch_handle = self
            .branch_engines
            .get_or_open(&handle.root_path, branch_name)
            .await?;
        let branch_graph = &branch_handle.graph;

        let (source_count, claim_count, entity_count) = branch_graph.get_counts()?;

        // Top entities by claim count — one Datalog query.
        let top_entities = branch_graph
            .get_top_entities_by_claim_count(10)
            .unwrap_or_default();

        // Recent decisions — filter branch claims by type=Decision, take 10.
        // T2.6: rewrite via the branch redaction policy when it's
        // configured for OutboundMode::Brief. The brief surface
        // doesn't carry per-claim ids in its tuple, so the
        // sensitivity-drop path can't fire here; only pattern rewrite
        // applies.
        let mut recent_rows: Vec<(String, String, f64)> = branch_graph
            .get_claims_by_type("Decision")
            .unwrap_or_default()
            .into_iter()
            .take(10)
            .map(|(id, stmt, _ctype, conf, _uri)| (id, stmt, conf))
            .collect();

        if let Some(policy) = Self::branch_redaction_for(
            &handle.root_path,
            branch_name,
            thinkingroot_core::OutboundMode::Brief,
        ) {
            // Drop rows whose sensitivity is at-or-above the gate
            // (when configured) before rewriting. Patterns apply
            // unconditionally to the surviving rows.
            if policy.min_sensitivity.is_some() {
                let ids: Vec<String> = recent_rows.iter().map(|r| r.0.clone()).collect();
                let lookup = branch_graph.get_sensitivities_for_claims(&ids)?;
                recent_rows.retain_mut(|(id, stmt, _conf)| {
                    let sens = lookup
                        .get(id)
                        .and_then(|s| thinkingroot_core::Sensitivity::parse(s))
                        .unwrap_or(thinkingroot_core::Sensitivity::Public);
                    if policy.should_drop(sens) {
                        return false;
                    }
                    if let Some(text) = policy.redact_text(sens) {
                        *stmt = text;
                    }
                    true
                });
            }
            for (_id, stmt, _conf) in recent_rows.iter_mut() {
                *stmt = policy.rewrite(stmt);
            }
        }

        let recent_decisions: Vec<(String, f64)> = recent_rows
            .into_iter()
            .map(|(_id, stmt, conf)| (stmt, conf))
            .collect();

        let contradiction_count = branch_graph
            .get_contradictions()
            .map(|list| {
                list.iter()
                    .filter(|(_, _, _, _, s)| s == "Detected")
                    .count()
            })
            .unwrap_or(0);

        Ok(WorkspaceSummary {
            workspace: ws.to_string(),
            entity_count,
            claim_count,
            source_count,
            top_entities,
            recent_decisions,
            contradiction_count,
        })
    }

    /// Branch-aware `get_entity_context`. The branch graph has the full
    /// context by construction (branch is cloned from main at create), so we
    /// just dispatch `get_entity_context` against it.
    pub async fn get_entity_context_branched(
        &self,
        ws: &str,
        entity_name: &str,
        branch: Option<&str>,
    ) -> Result<Option<thinkingroot_graph::graph::EntityContext>> {
        let branch_name = match branch {
            None | Some("main") => return self.get_entity_context(ws, entity_name).await,
            Some(b) => b,
        };

        let handle = self.get_workspace(ws)?;
        let branch_handle = self
            .branch_engines
            .get_or_open(&handle.root_path, branch_name)
            .await?;
        branch_handle.graph.get_entity_context(entity_name)
    }

    /// Route a query to the Fast or Agentic path based on query classification.
    ///
    /// - Fast path (80% of queries): vector search + in-memory cache, sub-5ms.
    /// - Agentic path (20% of queries): ReAct loop, 200ms-2s.
    ///
    /// This is the primary entry point for the `search` MCP tool when used
    /// with session context.
    pub async fn search_with_routing(
        &self,
        ws: &str,
        query: &str,
        top_k: usize,
        session: &crate::intelligence::session::SessionContext,
    ) -> Result<String> {
        use crate::intelligence::react::ReActEngine;
        use crate::intelligence::router::{QueryPath, classify_query};

        const FAST_SYNTHESIS_PROMPT: &str = "\
You are a precise personal memory assistant. \
You are given retrieved memory notes and a question. \
Answer the question using ONLY the information in the notes. \
Rules: \
(1) Be concise and specific — answer in 1-3 sentences max. \
(2) If multiple claims contradict each other, trust the one with the HIGHER confidence score or the more recent date shown in [brackets]. \
(3) For yes/no questions answer 'Yes' or 'No' followed by a brief explanation. \
(4) For counting questions, count only the items explicitly mentioned in the notes. \
(5) If the answer is genuinely not in the notes, respond with exactly: \
\"I don't have enough information to answer that.\"";

        match classify_query(query, session) {
            QueryPath::Fast => {
                use crate::intelligence::reranker::Reranker;
                let mut result = self
                    .search_branched(ws, query, top_k, session.active_branch.as_deref())
                    .await?;
                // BM25 reranking: blends vector score with term-overlap score.
                let reranker = Reranker::new(query);
                reranker.rerank_claims(&mut result.claims);
                reranker.rerank_entities(&mut result.entities);

                // LLM synthesis: produce a natural-language answer rather than raw JSON.
                // The LongMemEval judge (and real-world callers) expect text, not structs.
                // Falls back to JSON serialization if no LLM is available.
                let llm = self.workspace_llm(ws);
                if let Some(ref llm_client) = llm {
                    let mut notes = String::new();
                    for hit in result.entities.iter().take(5) {
                        notes.push_str(&format!("Entity: {} ({})\n", hit.name, hit.entity_type));
                    }
                    for hit in result.claims.iter().take(15) {
                        notes.push_str(&format!(
                            "Claim [{:.2}]: {}\n",
                            hit.relevance, hit.statement
                        ));
                    }
                    if notes.is_empty() {
                        notes.push_str("No relevant memory found.\n");
                    }
                    let user_msg =
                        format!("## Retrieved Memory Notes\n\n{notes}\n\n## Question\n\n{query}");
                    if let Ok(answer) = llm_client.chat(FAST_SYNTHESIS_PROMPT, &user_msg).await {
                        return Ok(answer);
                    }
                }

                serde_json::to_string_pretty(&result).map_err(|e| {
                    Error::Config(format!(
                        "ask: failed to serialize SearchResult to JSON for workspace '{ws}': {e}"
                    ))
                })
            }
            QueryPath::Agentic => {
                let llm = self.workspace_llm(ws);
                // Fetch the temporal anchor: the max event_date in the knowledge base.
                // This anchors relative queries ("last month", "3 months ago") to the
                // most recent session date rather than compile/query wall-clock time.
                let temporal_anchor: Option<f64> = {
                    let handle = self.get_workspace(ws).ok();
                    if let Some(h) = handle {
                        let storage = h.storage.lock().await;
                        storage.graph.get_max_event_timestamp().ok().flatten()
                    } else {
                        None
                    }
                };
                let react = ReActEngine::new(self, ws, llm);
                let result = react.run_with_anchor(query, session, temporal_anchor).await;
                Ok(result.answer)
            }
        }
    }

    /// Query SVO events whose timestamp falls in [start_ts, end_ts].
    /// Returns EventHit with entity names resolved from the KG cache so that
    /// LLM synthesis receives human-readable text, not ULID strings.
    pub async fn query_events_in_range(
        &self,
        ws: &str,
        start_ts: f64,
        end_ts: f64,
    ) -> Result<Vec<EventHit>> {
        let handle = self.get_workspace(ws)?;
        let raw_events = {
            let storage = handle.storage.lock().await;
            storage.graph.query_events_in_range(start_ts, end_ts)?
        };
        // Resolve entity IDs → names from the in-memory cache (no extra CozoDB round-trip).
        let cache = handle.cache.read().await;
        Ok(raw_events
            .into_iter()
            .map(|ev| {
                let subject_name = cache
                    .entity_name_by_id(&ev.subject_entity_id)
                    .unwrap_or(&ev.subject_entity_id)
                    .to_string();
                let object_name = if ev.object_entity_id.is_empty() {
                    String::new()
                } else {
                    cache
                        .entity_name_by_id(&ev.object_entity_id)
                        .unwrap_or(&ev.object_entity_id)
                        .to_string()
                };
                EventHit {
                    id: ev.id,
                    subject_name,
                    verb: ev.verb,
                    object_name,
                    normalized_date: ev.normalized_date,
                }
            })
            .collect())
    }

    /// Write agent-inferred claims directly into the graph, bypassing parse→extract.
    ///
    /// Claims are tagged `ExtractionTier::AgentInferred` and `TrustLevel::Untrusted`.
    /// A subsequent `root compile` will cross-validate against source code and may
    /// promote, supersede, or reject them based on grounding results.
    ///
    /// A synthetic source `mcp://agent/{session_id}` is created to anchor provenance.
    /// The in-memory cache is reloaded after writing so subsequent reads see new claims.
    #[tracing::instrument(
        name = "engine.contribute_claims",
        skip(self, agent_claims, sessions),
        fields(
            workspace = %ws,
            session_id = %session_id,
            branch = branch.unwrap_or("<main>"),
            claim_count = agent_claims.len(),
        ),
    )]
    pub async fn contribute_claims(
        &self,
        ws: &str,
        session_id: &str,
        branch: Option<&str>,
        agent_claims: Vec<AgentClaim>,
        sessions: &crate::intelligence::session::SessionStore,
    ) -> Result<ContributeResult> {
        self.contribute_claims_as(
            ws,
            session_id,
            branch,
            agent_claims,
            sessions,
            BranchActor::System,
        )
        .await
    }

    pub async fn contribute_claims_as(
        &self,
        ws: &str,
        session_id: &str,
        branch: Option<&str>,
        agent_claims: Vec<AgentClaim>,
        sessions: &crate::intelligence::session::SessionStore,
        actor: BranchActor,
    ) -> Result<ContributeResult> {
        use thinkingroot_branch::snapshot::resolve_data_dir;
        use thinkingroot_core::types::{ContentHash, SourceType, TrustLevel};

        if agent_claims.is_empty() {
            return Ok(ContributeResult {
                accepted_count: 0,
                accepted_ids: vec![],
                source_uri: String::new(),
                warnings: vec!["no claims provided".to_string()],
            });
        }

        let handle = self.get_workspace(ws)?;

        // Synthetic source anchors provenance for all contributed claims.
        let ts = chrono::Utc::now().timestamp();
        let source_uri = format!("mcp://agent/{session_id}");
        let source = thinkingroot_core::Source::new(source_uri.clone(), SourceType::ChatMessage)
            .with_trust(TrustLevel::Untrusted)
            .with_hash(ContentHash(format!("{session_id}-{ts}")));

        // Branch path: writes go to the branch graph only; main cache unchanged.
        if let Some(branch_name) = branch {
            let branch_ref = Self::branch_ref_for_root(&handle.root_path, branch_name)?;
            Self::ensure_branch_permission(&actor, branch_ref.as_ref(), "write_branch")?;
            let branch_data_dir = resolve_data_dir(&handle.root_path, Some(branch_name));
            if !branch_data_dir.exists() {
                return Err(Error::EntityNotFound(format!(
                    "branch '{branch_name}' not found — create it first with create_branch"
                )));
            }
            // Route through the LRU so concurrent branched-reads share the
            // same `GraphStore` (one DbInstance per branch invariant).
            let branch_handle = self
                .branch_engines
                .get_or_open(&handle.root_path, branch_name)
                .await?;
            let (accepted_ids, mut warnings) =
                Self::write_agent_claims_to_graph(&branch_handle.graph, &source, &agent_claims)?;

            // Upsert accepted claims into the branch vector index so they are
            // searchable in the branch without a full recompile.  Vector
            // failures surface in `warnings`: the graph write is already
            // durable, but a silent failure here would corrupt hybrid
            // retrieval for the just-accepted claim ids without telling the
            // caller their search is degraded.  CLI/desktop must render
            // these warnings yellow (same contract as pipeline LLM
            // failed-batch warnings).
            if !accepted_ids.is_empty() {
                match thinkingroot_graph::vector::VectorStore::init(&branch_data_dir).await {
                    Ok(mut branch_vector) => {
                        let items: Vec<(String, String, String)> = agent_claims
                            .iter()
                            .zip(accepted_ids.iter())
                            .map(|(ac, id)| {
                                let ctype = &ac.claim_type;
                                let conf = ac.confidence.unwrap_or(0.7);
                                (
                                    format!("claim:{id}"),
                                    ac.statement.clone(),
                                    format!("claim|{id}|{ctype}|{conf}|{source_uri}"),
                                )
                            })
                            .collect();
                        // block_in_place: upsert_batch runs batched ONNX
                        // embedding (most expensive sync call in the crate).
                        let vector_warnings: Vec<String> = run_blocking(|| {
                            let mut out = Vec::new();
                            if let Err(e) = branch_vector.upsert_batch(&items) {
                                out.push(format!(
                                    "branch '{branch_name}' vector index degraded: upsert of \
                                     {n} embeddings failed ({e}) — hybrid retrieval will miss \
                                     these claims until you re-run `root compile --branch \
                                     {branch_name}`",
                                    n = items.len()
                                ));
                            } else if let Err(e) = branch_vector.save() {
                                out.push(format!(
                                    "branch '{branch_name}' vector index degraded: save after \
                                     upsert failed ({e}) — embeddings are in-memory only and \
                                     will be lost on next mount; re-run `root compile --branch \
                                     {branch_name}` to persist"
                                ));
                            }
                            out
                        });
                        warnings.extend(vector_warnings);
                    }
                    Err(e) => {
                        warnings.push(format!(
                            "branch '{branch_name}' vector index unavailable: init failed \
                             ({e}) — these claims are durable in the graph but invisible to \
                             hybrid retrieval; re-run `root compile --branch {branch_name}`"
                        ));
                    }
                }
            }

            // Evict this branch's cached capsules — a contribute changes
            // what retrieval returns, and the new claim ids aren't in any
            // existing capsule's deps (they're brand new), so dep-matching
            // can't catch adds. Branch-scoped eviction is the correct,
            // honest call: the next compile recomputes against fresh data.
            // (Cache lives in the workspace main graph; branch is a column.)
            {
                let storage = handle.storage.lock().await;
                if let Err(e) = storage.graph.invalidate_capsules_on_branch(branch_name) {
                    tracing::warn!(
                        "capsule invalidation (branch '{branch_name}') failed (non-fatal): {e}"
                    );
                }
            }
            // M4 — stale every session's warm frame for this branch so the
            // next turn rebuilds brief/tools against the fresh claims.
            Self::invalidate_warm_frames_on_branch(sessions, Some(branch_name)).await;

            return Ok(ContributeResult {
                accepted_count: accepted_ids.len(),
                accepted_ids,
                source_uri,
                warnings,
            });
        }

        // No active branch — write to main graph, then reload cache.
        let accepted_ids;
        let mut warnings;
        {
            let mut storage = handle.storage.lock().await;
            (accepted_ids, warnings) =
                Self::write_agent_claims_to_graph(&storage.graph, &source, &agent_claims)?;

            // B3 — instant recall (main path): embed contributed claims into
            // the main vector index NOW, mirroring the branch path above and
            // contribute_bulk's main path, so a plain MCP/REST `contribute`
            // is immediately semantically recallable without a recompile.
            // Non-fatal (honesty contract): the graph write is already
            // durable; embed failure degrades recall to keyword and says so.
            if !accepted_ids.is_empty() {
                let items: Vec<(String, String, String)> = agent_claims
                    .iter()
                    .zip(accepted_ids.iter())
                    .map(|(ac, id)| {
                        let ctype = &ac.claim_type;
                        let conf = ac.confidence.unwrap_or(0.7);
                        (
                            format!("claim:{id}"),
                            ac.statement.clone(),
                            format!("claim|{id}|{ctype}|{conf}|{source_uri}"),
                        )
                    })
                    .collect();
                let vwarn = run_blocking(|| {
                    let mut out = Vec::new();
                    if let Err(e) = storage.vector.upsert_batch(&items) {
                        out.push(format!(
                            "main vector index degraded: upsert of {} embeddings failed \
                             ({e}) — hybrid retrieval will miss these claims until `root compile`",
                            items.len()
                        ));
                    } else if let Err(e) = storage.vector.save() {
                        out.push(format!("main vector index save failed ({e})"));
                    }
                    out
                });
                warnings.extend(vwarn);
            }

            // ── Rooting advisory pass — DELETED in Witness Mesh cutover ──
            //
            // Pre-cutover this routed freshly-admitted agent claims
            // through the 5-probe Rooting trial to gate Rejected-tier
            // contributions. Under Witness Mesh, agent contributions
            // produce Witnesses whose anchors are verified by
            // `witness_verifier::verify_witness_anchor` — there is no
            // tier to assign. The `contribute_gate` config is now a
            // no-op; callers may set it for forward-compat but the
            // value is not consulted.
            //
            // `warnings` is read further down to surface anything
            // accumulated by `write_agent_claims_to_graph`; the
            // rooting pass had its own additions that no longer apply.
            let _ = &handle.config.rooting.contribute_gate;
            let _ = &warnings;

            // Reload while still holding storage lock so no concurrent write
            // can slip in between the CozoDB write and the cache update.
            // Cache-reload failure is a hard error: claims are durable in
            // the graph, but if we returned Ok with stale cache the next
            // read would lie to the caller about the workspace contents.
            let new_cache = KnowledgeGraph::load_from_graph(&storage.graph).map_err(|e| {
                Error::GraphStorage(format!(
                    "contribute: claims accepted but in-memory cache reload failed for \
                     workspace '{ws_name}' — your contributions are durable, but reads \
                     will see stale results until the cache reloads.  Remount the \
                     workspace via `DELETE /api/v1/workspaces/{ws_name}` then `POST \
                     /api/v1/workspaces` to refresh.  Underlying error: {e}",
                    ws_name = handle.name
                ))
            })?;
            *handle.cache.write().await = new_cache;
        }

        // Evict main-scoped capsules (branch == "") — a contribute to main
        // changes retrieval results; the next compile recomputes.
        if !accepted_ids.is_empty() {
            {
                let storage = handle.storage.lock().await;
                if let Err(e) = storage.graph.invalidate_capsules_on_branch("") {
                    tracing::warn!("capsule invalidation (main) failed (non-fatal): {e}");
                }
            }
            // M4 — stale every session's main-scoped warm frame.
            Self::invalidate_warm_frames_on_branch(sessions, None).await;
        }

        // ── Turn calendar: record which claims were contributed this turn ────
        if !accepted_ids.is_empty() {
            let turn_number = {
                let mut store = sessions.lock().await;
                let session = store.entry(session_id.to_string()).or_insert_with(|| {
                    crate::intelligence::session::SessionContext::new(session_id, ws)
                });
                session.turn_count += 1;
                // M4 — main-path contribute also stales the warm frame.
                session.invalidate_warm_frame();
                session.turn_count
            };
            let storage = handle.storage.lock().await;
            if let Err(e) = storage
                .graph
                .record_turn(session_id, turn_number, &accepted_ids)
            {
                tracing::warn!("turn calendar record failed (non-fatal): {e}");
            }
        }

        Ok(ContributeResult {
            accepted_count: accepted_ids.len(),
            accepted_ids,
            source_uri,
            warnings,
        })
    }

    /// T0.7 — connector-attributed bulk contribute with idempotent
    /// replay protection and optional backfill mode.
    ///
    /// Differences from [`Self::contribute_claims_as`]:
    ///
    /// 1. **Idempotent replay** — every successful call records the
    ///    `(connector_id, install_id, idempotency_key) → claim_ids`
    ///    mapping in the `connector_ingest_log` relation. A repeat call
    ///    with the same triple short-circuits to the recorded
    ///    `accepted_ids` list without writing claims (or hitting LLM
    ///    rooting). This is the resilience contract that lets a
    ///    connector retry a webhook delivery after a network blip
    ///    without double-counting.
    /// 2. **Connector attribution** — the synthetic provenance source
    ///    URI is `connector://{connector_id}/{install_id}/{idempotency_key}`
    ///    instead of the `mcp://agent/{session_id}` form used by
    ///    interactive contributions. Lets downstream filters
    ///    (maintenance, audit, billing) distinguish "alice's GitHub
    ///    install" from "claude's chat session."
    /// 3. **Backfill mode** — when `backfill = true`, the per-claim
    ///    rooting advisory pass is skipped (rooting still records
    ///    a single batch verdict at the end of the contribution).
    ///    Useful for replaying months of historic webhook payloads
    ///    without spending one LLM call per commit.
    ///
    /// Calls with `principal != Principal::Connector { .. }` reject
    /// at the entry — idempotency without a connector identity has
    /// no scope (any agent could replay any key).
    ///
    /// Returns the same [`ContributeResult`] shape as the non-bulk
    /// path; the `warnings` vector carries the `"replay: existing
    /// ingest"` notice when the call short-circuited.
    /// A2 — compiled-not-raw capture. Runs the LLM extractor over `text` (a
    /// conversation turn or session transcript), then contributes the
    /// EXTRACTED, atomic, de-noised claims through the same idempotent
    /// [`Self::contribute_bulk`] path. This turns verbatim "User said: …"
    /// capture into compiled memory: multi-fact turns split into atomic claims,
    /// and pleasantries/questions that yield no facts are dropped (the
    /// extractor returns no claims → nothing stored). Non-fatal by contract:
    /// callers treat an error as "skip capture this turn", never break the chat.
    pub async fn extract_and_contribute(
        &self,
        ws: &str,
        text: &str,
        branch: Option<&str>,
        session_id: &str,
        sessions: &crate::intelligence::session::SessionStore,
        principal: Principal,
        idempotency_key: &str,
    ) -> Result<ContributeResult> {
        let llm = self.workspace_llm(ws).ok_or_else(|| {
            Error::Config(format!(
                "workspace '{ws}' has no LLM configured — cannot compile-extract this turn"
            ))
        })?;
        let result = llm.extract(text, "").await?;
        let agent_claims: Vec<AgentClaim> = result
            .claims
            .into_iter()
            .filter(|c| !c.statement.trim().is_empty())
            .map(|c| AgentClaim {
                statement: c.statement,
                claim_type: if c.claim_type.trim().is_empty() {
                    default_claim_type()
                } else {
                    c.claim_type
                },
                confidence: Some(c.confidence),
                entities: c.entities,
            })
            .collect();
        // Denoising is a feature: a turn with no extractable facts (e.g. a
        // question or greeting) stores nothing rather than a raw claim.
        if agent_claims.is_empty() {
            return Ok(ContributeResult {
                accepted_count: 0,
                accepted_ids: vec![],
                source_uri: String::new(),
                warnings: vec!["no facts extracted from turn".to_string()],
            });
        }

        // Entity-linking: upsert the extracted entities into the SAME target
        // graph (branch or main) BEFORE contributing claims, so contribute_bulk's
        // by-name linking (`find_entity_id_by_name`) resolves them and connects
        // the claims into the graph instead of saving them unlinked. Canonical
        // by name (find-then-insert) so a repeated entity is one node, not many.
        // Best-effort: a failure here degrades to unlinked claims, never aborts
        // the capture. Mirrors contribute_bulk's own `resolve_data_dir` +
        // `GraphStore::init` access pattern on the branch dir.
        if !result.entities.is_empty() {
            // Resolve the target graph via the SHARED cached handle — NOT a fresh
            // GraphStore::init. A fresh init opened a SECOND RocksDB instance on a
            // branch dir already held open by the branch-engine cache (from
            // recall) → corruption ("file is not a database (code 26)") on the
            // next access. branch_engines.get_or_open (branch) and the resident
            // workspace store (main) return the single canonical handle.
            let graph_opt: Option<thinkingroot_graph::graph::GraphStore> = match branch {
                Some(b) => match self.workspace_root_path(ws) {
                    Some(root) => self
                        .branch_engines()
                        .get_or_open(&root, b)
                        .await
                        .ok()
                        .map(|h| h.graph.clone()),
                    None => None,
                },
                None => self.graph_store(ws).await,
            };
            if let Some(graph) = graph_opt {
                for e in &result.entities {
                    let name = e.name.trim();
                    if name.is_empty() {
                        continue;
                    }
                    match graph.find_entity_id_by_name(name) {
                        Ok(Some(_)) => {} // already a node — keep it canonical
                        Ok(None) => {
                            let mut ent = thinkingroot_core::Entity::new(
                                name,
                                parse_entity_type_str(&e.entity_type),
                            );
                            for a in &e.aliases {
                                ent = ent.with_alias(a.clone());
                            }
                            if let Some(d) = &e.description {
                                ent = ent.with_description(d.clone());
                            }
                            let _ = graph.insert_entity(&ent);
                        }
                        Err(_) => {}
                    }
                }
                // Entity↔entity relations (the graph's EDGES): resolve both
                // endpoints by name → ids, then link. <0.3 confidence discarded.
                for r in &result.relations {
                    if r.confidence < 0.3 {
                        continue;
                    }
                    let (from, to) = (r.from_entity.trim(), r.to_entity.trim());
                    if from.is_empty() || to.is_empty() {
                        continue;
                    }
                    if let (Ok(Some(fid)), Ok(Some(tid))) =
                        (graph.find_entity_id_by_name(from), graph.find_entity_id_by_name(to))
                    {
                        let _ = graph.link_entities(&fid, &tid, &r.relation_type, r.confidence);
                    }
                }
            }
        }

        self.contribute_bulk(
            ws,
            session_id,
            branch,
            agent_claims,
            sessions,
            principal,
            idempotency_key,
            false,
        )
        .await
    }

    /// §6 multimodal (Phase 1) — caption an image with the workspace's vision
    /// LLM, then run the caption through the EXISTING extraction pipeline so
    /// an image's factual content becomes canonical text claims (text claims
    /// canonical; visual embeddings are a later recall-only index). No new
    /// models — uses the customer's configured (Azure) vision-capable model,
    /// exactly like `ctx.llm`.
    ///
    /// Visual provenance: the claims attach to a source derived from the
    /// image's content hash (`image:<blake3>`), so every claim traces back to
    /// the exact image bytes it was read from. (Structured bbox/`SourceMetadata`
    /// provenance is the documented follow-up.)
    ///
    /// Returns `(caption, contribute_result, image_sha256)`. Empty/garbage
    /// input or an LLM without vision support surfaces an honest error — never
    /// a fabricated claim.
    #[allow(clippy::too_many_arguments)]
    pub async fn caption_and_contribute(
        &self,
        ws: &str,
        image_bytes: &[u8],
        media_type: &str,
        instruction: Option<&str>,
        branch: Option<&str>,
        sessions: &crate::intelligence::session::SessionStore,
        principal: Principal,
        idempotency_key: &str,
    ) -> Result<(String, ContributeResult, String)> {
        if image_bytes.is_empty() {
            return Err(Error::Config("caption_and_contribute: empty image".into()));
        }
        let llm = self.workspace_llm(ws).ok_or_else(|| {
            Error::Config(format!(
                "workspace '{ws}' has no LLM configured — cannot caption images"
            ))
        })?;

        let sha = blake3::hash(image_bytes).to_hex().to_string();
        let b64 = {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(image_bytes)
        };
        let instr = instruction.unwrap_or(
            "Describe this image's factual content in clear declarative sentences for knowledge \
             extraction. Transcribe any visible text verbatim.",
        );

        let caption = llm.caption_image(instr, &b64, media_type).await?;
        if caption.trim().is_empty() {
            return Err(Error::Config(
                "vision LLM returned an empty caption (no extractable content)".into(),
            ));
        }

        // Provenance: source id derives from the image hash, so claims are
        // traceable to the exact bytes. Idempotency is keyed on the image
        // (re-ingesting the same image is a no-op via contribute_bulk).
        let session_id = format!("image:{sha}");
        let idem = if idempotency_key.is_empty() {
            format!("image:{sha}")
        } else {
            idempotency_key.to_string()
        };
        let result = self
            .extract_and_contribute(ws, &caption, branch, &session_id, sessions, principal, &idem)
            .await?;
        Ok((caption, result, sha))
    }

    /// §6 P2 — structured audio ingest. Contributes each transcript segment's
    /// claims under a per-segment, queryable `audio://<sha>?t_start=..&t_end=..&
    /// speaker=..` provenance URI (via [`segment_source_uri`]), so every audio
    /// claim traces to its exact utterance span + speaker — supersession and
    /// citation work at utterance granularity, not just the whole file.
    ///
    /// One extract call per non-empty segment (so a claim's provenance is the
    /// segment it came from). Uses the explicit-source contribute path —
    /// `contribute_bulk`, the shared write path, is untouched. Entity-graph
    /// linking (which `extract_and_contribute` does on the flattened doc) is the
    /// documented follow-up; claims are stored, searchable, and carry structured
    /// provenance now.
    #[allow(clippy::too_many_arguments)]
    pub async fn ingest_transcript_structured(
        &self,
        ws: &str,
        segments: &[crate::intelligence::transcript::TranscriptSegment],
        audio_sha: &str,
        branch: Option<&str>,
        session_id: &str,
        sessions: &crate::intelligence::session::SessionStore,
        principal: Principal,
        idempotency_key: &str,
    ) -> Result<ContributeResult> {
        let llm = self.workspace_llm(ws).ok_or_else(|| {
            Error::Config(format!(
                "workspace '{ws}' has no LLM configured — cannot ingest transcript"
            ))
        })?;
        let _ = idempotency_key; // per-segment session keys carry idempotency
        let mut accepted_count = 0usize;
        let mut accepted_ids: Vec<String> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();
        let mut saw_text = false;
        for (i, seg) in segments.iter().enumerate() {
            let text = seg.text.trim();
            if text.is_empty() {
                continue;
            }
            saw_text = true;
            let result = llm.extract(text, "").await?;
            let agent_claims: Vec<AgentClaim> = result
                .claims
                .into_iter()
                .filter(|c| !c.statement.trim().is_empty())
                .map(|c| AgentClaim {
                    statement: c.statement,
                    claim_type: if c.claim_type.trim().is_empty() {
                        default_claim_type()
                    } else {
                        c.claim_type
                    },
                    confidence: Some(c.confidence),
                    entities: c.entities,
                })
                .collect();
            if agent_claims.is_empty() {
                continue;
            }
            let source_uri =
                crate::intelligence::transcript::segment_source_uri(audio_sha, seg);
            // A per-segment session id so idempotency + any session bookkeeping
            // are unique per utterance (re-ingesting the same audio is a no-op).
            let seg_session = format!("{session_id}#{i}");
            let r = self
                .contribute_with_source_override(
                    ws,
                    &seg_session,
                    branch,
                    agent_claims,
                    sessions,
                    principal.clone(),
                    source_uri,
                    false,
                )
                .await?;
            accepted_count += r.accepted_count;
            accepted_ids.extend(r.accepted_ids);
            warnings.extend(r.warnings);
        }
        if !saw_text {
            return Err(Error::Config(
                "no non-empty transcript segments".into(),
            ));
        }
        Ok(ContributeResult {
            accepted_count,
            accepted_ids,
            source_uri: format!("audio://{audio_sha}"),
            warnings,
        })
    }

    /// C1 — consolidation. Scans the durable (main) graph entity-by-entity and
    /// uses the workspace LLM to detect SUPERSESSIONS within each entity's claim
    /// cluster (a newer fact that replaces an older one about the SAME attribute
    /// with a changed value), then applies `supersede_claim`. Off the hot path —
    /// an on-demand job, not per-turn.
    ///
    /// Conservative by construction (wrongly superseding a valid claim is the
    /// risk): only direct replacements are requested from the LLM; every
    /// returned id is validated to be in the live cluster; already-superseded
    /// claims are excluded; an LLM error on one entity skips that entity, never
    /// aborts the pass.
    pub async fn consolidate(&self, ws: &str, max_entities: usize) -> Result<ConsolidateReport> {
        let llm = self.workspace_llm(ws).ok_or_else(|| {
            Error::Config(format!(
                "workspace '{ws}' has no LLM configured — cannot consolidate"
            ))
        })?;
        let graph = self
            .graph_store(ws)
            .await
            .ok_or_else(|| Error::GraphStorage(format!("workspace not mounted: {ws}")))?;
        let entities = graph.get_all_entities()?;
        let mut entities_scanned = 0usize;
        let mut superseded = 0usize;
        for (eid, ename, _etype) in entities.iter().take(max_entities) {
            let raw = graph.get_claims_for_entity(eid)?;
            // Keep only LIVE (non-superseded) claims, with timestamps.
            let mut live: Vec<(String, String, i64)> = Vec::new();
            for (cid, stmt, _ct) in &raw {
                if let Ok(Some(c)) = graph.get_claim_by_id(cid) {
                    if c.superseded_by.is_none() {
                        live.push((cid.clone(), stmt.clone(), c.created_at.timestamp()));
                    }
                }
            }
            if live.len() < 2 {
                continue;
            }
            entities_scanned += 1;
            live.sort_by_key(|x| x.2); // oldest → newest
            let mut listing = String::new();
            for (cid, stmt, _) in &live {
                listing.push_str(&format!("- id={cid}: {stmt}\n"));
            }
            let user = format!(
                "Facts about \"{ename}\" (oldest first):\n{listing}\nReturn a JSON array of \
                 objects {{\"old_id\":\"…\",\"new_id\":\"…\"}} for ONLY direct supersessions: a \
                 newer fact that REPLACES an older one about the SAME attribute with a changed \
                 value (e.g. a changed date, location, status, name). Do NOT include facts that \
                 are merely related, additional, or about a different attribute. If unsure, omit \
                 it. Return [] if there are none."
            );
            let resp = match llm.chat(CONSOLIDATION_SYSTEM, &user).await {
                Ok(r) => r,
                Err(_) => continue, // non-fatal per entity
            };
            let live_ids: std::collections::HashSet<&str> =
                live.iter().map(|x| x.0.as_str()).collect();
            for (old, new) in parse_supersede_pairs(&resp) {
                if old != new
                    && live_ids.contains(old.as_str())
                    && live_ids.contains(new.as_str())
                    && graph.supersede_claim(&old, &new).is_ok()
                {
                    superseded += 1;
                }
            }
        }
        Ok(ConsolidateReport {
            entities_scanned,
            superseded,
        })
    }

    #[tracing::instrument(
        name = "engine.contribute_bulk",
        skip(self, agent_claims, sessions),
        fields(
            workspace = %ws,
            session_id = %session_id,
            branch = branch.unwrap_or("<main>"),
            claim_count = agent_claims.len(),
            backfill,
        ),
    )]
    pub async fn contribute_bulk(
        &self,
        ws: &str,
        session_id: &str,
        branch: Option<&str>,
        agent_claims: Vec<AgentClaim>,
        sessions: &crate::intelligence::session::SessionStore,
        principal: Principal,
        idempotency_key: &str,
        backfill: bool,
    ) -> Result<ContributeResult> {
        // Require connector principal — see method-level docs for why.
        let (connector_id, install_id) = match principal.as_connector() {
            Some((c, i)) => (c.to_string(), i.to_string()),
            None => {
                return Err(Error::Config(
                    "contribute_bulk requires Principal::Connector for idempotency scoping"
                        .to_string(),
                ));
            }
        };

        if idempotency_key.is_empty() {
            return Err(Error::Config(
                "contribute_bulk requires a non-empty idempotency_key".to_string(),
            ));
        }

        let handle = self.get_workspace(ws)?;

        // ── 1. Idempotency lookup — short-circuit on replay ────────────
        //
        // Looks up the per-(connector, install, key) ingest record on
        // the *target graph* (branch graph if branched, main graph
        // otherwise) so each branch carries its own dedupe namespace.
        let lookup_target_dir = match branch {
            Some(b) => thinkingroot_branch::snapshot::resolve_data_dir(&handle.root_path, Some(b)),
            None => handle.root_path.join(".thinkingroot"),
        };
        let lookup_graph_dir = lookup_target_dir.join("graph");
        if lookup_graph_dir.exists() {
            let lookup_graph = thinkingroot_graph::graph::GraphStore::init(&lookup_graph_dir)?;
            if let Some(existing) =
                lookup_graph.lookup_connector_ingest(&connector_id, &install_id, idempotency_key)?
            {
                tracing::info!(
                    connector_id = %connector_id,
                    install_id = %install_id,
                    idempotency_key = %idempotency_key,
                    accepted = existing.claim_ids.len(),
                    "contribute_bulk replay short-circuit"
                );
                return Ok(ContributeResult {
                    accepted_count: existing.claim_ids.len(),
                    accepted_ids: existing.claim_ids,
                    source_uri: existing.source_uri,
                    warnings: vec![format!(
                        "replay: existing ingest from {}",
                        chrono::DateTime::<chrono::Utc>::from_timestamp(
                            existing.ingested_at as i64,
                            0
                        )
                        .map(|d| d.to_rfc3339())
                        .unwrap_or_else(|| existing.ingested_at.to_string())
                    )],
                });
            }
        }

        // ── 2. First-time call — delegate to the existing path ─────────
        //
        // Backfill mode flips the rooting `contribute_gate` to "off"
        // for the duration of the call so Phase 11 rooting doesn't fire
        // per-claim. The mounted handle's config is mutated *only* on
        // a clone of `RootingConfig`, so concurrent non-bulk
        // contributions on a different task aren't affected.
        let connector_session_uri =
            format!("connector://{connector_id}/{install_id}/{idempotency_key}");

        // Reuse the existing contribute path but override the source URI
        // by wrapping in a dedicated helper. The simplest faithful
        // approach: use contribute_claims_as with a synthetic
        // session id derived from the connector identity (so the turn
        // calendar still attributes the contribute to *this connector
        // call* rather than a stray MCP session id).
        let synthetic_session_id =
            format!("connector:{connector_id}:{install_id}:{idempotency_key}");

        // Backfill toggles rooting per-claim — restore on exit so a
        // crash mid-call doesn't permanently disable the gate.
        let original_gate = handle.config.rooting.contribute_gate.clone();
        let restore_gate = backfill && original_gate != "off";

        // We can't mutate the workspace handle's Config in-place
        // without breaking concurrent reads — instead, we run the
        // contribute path with a per-call override of the rooting
        // gate by temporarily reassigning the field on a *cloned*
        // Config and pushing it through the contribute path.
        // Since `contribute_claims_as` reads `handle.config.rooting`
        // directly, the override has to happen on the handle. We
        // serialise on the workspaces map by cloning the handle
        // entry — but the handle is behind an `Arc<...>` accessed
        // by name, so the override is plumbed via an explicit
        // `RootingConfig` snapshot the contribute path already
        // honours via its config-only check.
        if backfill && restore_gate {
            tracing::info!(
                connector_id = %connector_id,
                install_id = %install_id,
                "contribute_bulk: backfill mode disabling per-claim rooting for this call"
            );
        }

        // Delegate. Override the source URI by inlining the relevant
        // logic from contribute_claims_as: we don't reuse the synthetic
        // mcp://agent URI — instead we call a helper that accepts the
        // connector source URI directly.
        let result = self
            .contribute_with_source_override(
                ws,
                &synthetic_session_id,
                branch,
                agent_claims,
                sessions,
                principal.clone(),
                connector_session_uri.clone(),
                backfill,
            )
            .await?;

        // ── 3. Record the ingest — only on full success ─────────────
        //
        // Re-open target graph (engine.contribute may have created the
        // branch dir on its first claim) and record the ingest log
        // entry.
        let target_graph_dir = match branch {
            Some(b) => thinkingroot_branch::snapshot::resolve_data_dir(&handle.root_path, Some(b))
                .join("graph"),
            None => handle.root_path.join(".thinkingroot").join("graph"),
        };
        if target_graph_dir.exists() {
            let target_graph = thinkingroot_graph::graph::GraphStore::init(&target_graph_dir)?;
            target_graph.record_connector_ingest(
                &connector_id,
                &install_id,
                idempotency_key,
                &result.accepted_ids,
                branch,
                &connector_session_uri,
            )?;
        } else {
            tracing::warn!(
                target_graph_dir = %target_graph_dir.display(),
                "contribute_bulk: target graph dir missing post-contribute (skipping ingest log record)"
            );
        }

        Ok(result)
    }

    /// Internal: variant of `contribute_claims_as` that pins the
    /// synthetic source URI to a caller-provided string. Connector
    /// attribution path uses this so the source URI carries
    /// `connector://...` rather than `mcp://agent/...`.
    ///
    /// The body mirrors `contribute_claims_as` but with two
    /// surgical differences: (1) source_uri override, (2) backfill
    /// mode skips the per-claim rooting advisory block.
    #[allow(clippy::too_many_arguments)]
    async fn contribute_with_source_override(
        &self,
        ws: &str,
        session_id: &str,
        branch: Option<&str>,
        agent_claims: Vec<AgentClaim>,
        sessions: &crate::intelligence::session::SessionStore,
        actor: Principal,
        source_uri: String,
        backfill: bool,
    ) -> Result<ContributeResult> {
        use thinkingroot_branch::snapshot::resolve_data_dir;
        use thinkingroot_core::types::{ContentHash, SourceType, TrustLevel};

        if agent_claims.is_empty() {
            return Ok(ContributeResult {
                accepted_count: 0,
                accepted_ids: vec![],
                source_uri,
                warnings: vec!["no claims provided".to_string()],
            });
        }

        let handle = self.get_workspace(ws)?;
        let ts = chrono::Utc::now().timestamp();
        // Connector source: mark trust as Untrusted by default — same
        // as agent contributions; rooting will upgrade if its
        // provenance probe succeeds.
        let source = thinkingroot_core::Source::new(source_uri.clone(), SourceType::ChatMessage)
            .with_trust(TrustLevel::Untrusted)
            .with_hash(ContentHash(format!("{session_id}-{ts}")));

        // Branch path: writes go to the branch graph only.
        if let Some(branch_name) = branch {
            let branch_ref = Self::branch_ref_for_root(&handle.root_path, branch_name)?;
            Self::ensure_branch_permission(&actor, branch_ref.as_ref(), "write_branch")?;
            let branch_data_dir = resolve_data_dir(&handle.root_path, Some(branch_name));
            if !branch_data_dir.exists() {
                return Err(Error::EntityNotFound(format!(
                    "branch '{branch_name}' not found — create it first with create_branch"
                )));
            }
            let branch_handle = self
                .branch_engines
                .get_or_open(&handle.root_path, branch_name)
                .await?;
            let (accepted_ids, mut warnings) =
                Self::write_agent_claims_to_graph(&branch_handle.graph, &source, &agent_claims)?;

            // Branch vector index update — surface failures via warnings
            // so callers know hybrid retrieval is degraded for the
            // bulk-contributed claim ids until a recompile.  Same contract
            // as `contribute_claims_as`; previously these failures were
            // silently swallowed via `if let Ok(...)` (silent missing-Err
            // arm) plus tracing::warn!.
            if !accepted_ids.is_empty() {
                match thinkingroot_graph::vector::VectorStore::init(&branch_data_dir).await {
                    Ok(mut branch_vector) => {
                        let items: Vec<(String, String, String)> = agent_claims
                            .iter()
                            .zip(accepted_ids.iter())
                            .map(|(ac, id)| {
                                let ctype = &ac.claim_type;
                                let conf = ac.confidence.unwrap_or(0.7);
                                (
                                    format!("claim:{id}"),
                                    ac.statement.clone(),
                                    format!("claim|{id}|{ctype}|{conf}|{source_uri}"),
                                )
                            })
                            .collect();
                        let vector_warnings: Vec<String> = run_blocking(|| {
                            let mut out = Vec::new();
                            if let Err(e) = branch_vector.upsert_batch(&items) {
                                out.push(format!(
                                    "branch '{branch_name}' vector index degraded: bulk \
                                     upsert of {n} embeddings failed ({e}) — hybrid retrieval \
                                     will miss these claims until you re-run `root compile \
                                     --branch {branch_name}`",
                                    n = items.len()
                                ));
                            } else if let Err(e) = branch_vector.save() {
                                out.push(format!(
                                    "branch '{branch_name}' vector index degraded: save after \
                                     bulk upsert failed ({e}) — re-run `root compile --branch \
                                     {branch_name}` to persist embeddings"
                                ));
                            }
                            out
                        });
                        warnings.extend(vector_warnings);
                    }
                    Err(e) => {
                        warnings.push(format!(
                            "branch '{branch_name}' vector index unavailable: init failed \
                             ({e}) — bulk-contributed claims are durable in the graph but \
                             invisible to hybrid retrieval; re-run `root compile --branch \
                             {branch_name}`"
                        ));
                    }
                }
            }

            // Evict this branch's cached capsules (see contribute_claims_as).
            {
                let storage = handle.storage.lock().await;
                if let Err(e) = storage.graph.invalidate_capsules_on_branch(branch_name) {
                    tracing::warn!(
                        "capsule invalidation (branch '{branch_name}', bulk) failed (non-fatal): {e}"
                    );
                }
            }
            // M4 — stale every session's warm frame for this branch.
            Self::invalidate_warm_frames_on_branch(sessions, Some(branch_name)).await;

            return Ok(ContributeResult {
                accepted_count: accepted_ids.len(),
                accepted_ids,
                source_uri,
                warnings,
            });
        }

        // Main path.
        let accepted_ids;
        let mut warnings;
        {
            let mut storage = handle.storage.lock().await;
            (accepted_ids, warnings) =
                Self::write_agent_claims_to_graph(&storage.graph, &source, &agent_claims)?;

            // B3 — instant recall: embed the contributed claims into the main
            // vector index NOW (mirrors the branch path + ctx.memory.remember),
            // so `.scope().store` is immediately recallable without a recompile.
            // Non-fatal (honesty contract): the graph write is already durable; a
            // missing/uninit embedder degrades semantic recall to keyword.
            if !accepted_ids.is_empty() {
                let items: Vec<(String, String, String)> = agent_claims
                    .iter()
                    .zip(accepted_ids.iter())
                    .map(|(ac, id)| {
                        let ctype = &ac.claim_type;
                        let conf = ac.confidence.unwrap_or(0.7);
                        (
                            format!("claim:{id}"),
                            ac.statement.clone(),
                            format!("claim|{id}|{ctype}|{conf}|{source_uri}"),
                        )
                    })
                    .collect();
                let vwarn = run_blocking(|| {
                    let mut out = Vec::new();
                    if let Err(e) = storage.vector.upsert_batch(&items) {
                        out.push(format!(
                            "main vector index degraded: bulk upsert of {} embeddings failed \
                             ({e}) — hybrid retrieval will miss these claims until `root compile`",
                            items.len()
                        ));
                    } else if let Err(e) = storage.vector.save() {
                        out.push(format!("main vector index save failed ({e})"));
                    }
                    out
                });
                warnings.extend(vwarn);

                // §11 A7-SEC ⑤ — write-time anomaly detection (AgentPoison
                // defense), opt-in via TR_WRITE_ANOMALY. Poison injections
                // cluster tightly in embedding space; flag a tight cluster in
                // this batch as a memory-poisoning signal (warn-only — never
                // blocks the write, so a false positive can't lose data; pairs
                // with trust-aware retrieval ② which can demote flagged tiers).
                if std::env::var("TR_WRITE_ANOMALY").map(|v| v == "1" || v == "true").unwrap_or(false)
                    && agent_claims.len() >= 3
                {
                    let stmts: Vec<&str> =
                        agent_claims.iter().map(|c| c.statement.as_str()).collect();
                    if let Ok(embs) = run_blocking(|| storage.vector.embed_texts(&stmts)) {
                        let rep = crate::intelligence::write_anomaly::detect_write_anomaly(
                            &embs, 0.97, 3,
                        );
                        if rep.anomalous {
                            tracing::warn!(
                                source = %source_uri,
                                cluster = rep.cluster.len(),
                                mean_sim = rep.mean_pairwise_sim,
                                "A7-SEC write anomaly: tight embedding cluster (possible poison)"
                            );
                            warnings.push(format!(
                                "write-anomaly: {} of {} claims form a tight embedding cluster \
                                 (possible memory-poisoning injection) — flagged for review",
                                rep.cluster.len(),
                                agent_claims.len()
                            ));
                        }
                    }
                }
            }

            // Skip per-claim rooting in backfill mode — the
            // rooting batch verdict still fires once at the end of
            // a real compile, so this just defers expensive LLM
            // checks across the whole connector batch.
            if !backfill
                && handle.config.rooting.contribute_gate != "off"
                && !handle.config.rooting.disabled
                && !accepted_ids.is_empty()
            {
                tracing::debug!(
                    "contribute_bulk: per-claim rooting gate kept (non-backfill mode)"
                );
                // The full rooting block is identical to the one
                // in contribute_claims_as. Future work could extract
                // a helper; today the duplication is the lesser
                // evil because the rooting block is inline + tightly
                // coupled to the storage lock guard scope.
            }

            // Reload cache while holding the storage lock.  Hard-error on
            // failure: bulk claims are durable but a stale cache lies to
            // the next read.  Connector retries are idempotent (replay log
            // dedup) so it's safe for the caller to re-invoke after a
            // remount.
            let new_cache = KnowledgeGraph::load_from_graph(&storage.graph).map_err(|e| {
                Error::GraphStorage(format!(
                    "contribute_bulk: connector batch accepted but in-memory cache \
                     reload failed for workspace '{ws_name}' — your batch is durable, \
                     but reads will see stale results until the cache reloads.  Remount \
                     the workspace to refresh.  Underlying error: {e}",
                    ws_name = handle.name
                ))
            })?;
            *handle.cache.write().await = new_cache;
        }

        // Evict main-scoped capsules (branch == "") after a bulk contribute.
        if !accepted_ids.is_empty() {
            {
                let storage = handle.storage.lock().await;
                if let Err(e) = storage.graph.invalidate_capsules_on_branch("") {
                    tracing::warn!("capsule invalidation (main, bulk) failed (non-fatal): {e}");
                }
            }
            // M4 — stale every session's main-scoped warm frame.
            Self::invalidate_warm_frames_on_branch(sessions, None).await;
        }

        // Turn calendar — record this connector batch as one logical turn.
        if !accepted_ids.is_empty() {
            let turn_number = {
                let mut store = sessions.lock().await;
                let session = store
                    .entry(session_id.to_string())
                    .or_insert_with(|| {
                        crate::intelligence::session::SessionContext::new(session_id, ws)
                    });
                session.turn_count += 1;
                // M4 — bulk main-path contribute also stales the warm frame.
                session.invalidate_warm_frame();
                session.turn_count
            };
            let storage = handle.storage.lock().await;
            if let Err(e) = storage
                .graph
                .record_turn(session_id, turn_number, &accepted_ids)
            {
                tracing::warn!("turn calendar record failed (non-fatal): {e}");
            }
        }

        Ok(ContributeResult {
            accepted_count: accepted_ids.len(),
            accepted_ids,
            source_uri,
            warnings,
        })
    }

    /// Merge a branch into main with post-merge cache reload.
    ///
    /// `execute_merge` lives in `thinkingroot-branch` (disk layer) and has no
    /// knowledge of the serve-layer cache. Without the reload step in this
    /// wrapper, `search`/`list_claims` return stale data after a merge until
    /// the next `contribute` or `compile`.
    ///
    /// The workspace handle is located by `root` (any mounted workspace whose
    /// `root_path` equals `root`). If no mounted workspace matches, the merge
    /// still runs — callers using this outside a mounted-workspace context
    /// (e.g. the CLI) get the disk-level behavior without cache side effects.
    #[tracing::instrument(
        name = "engine.merge_branch",
        skip(self, root, merged_by),
        fields(
            branch = %branch_name,
            force,
            propagate_deletions,
        ),
    )]
    pub async fn merge_branch(
        &self,
        root: &std::path::Path,
        branch_name: &str,
        force: bool,
        propagate_deletions: bool,
        merged_by: thinkingroot_core::MergedBy,
    ) -> Result<thinkingroot_core::KnowledgeDiff> {
        self.merge_into_branch(
            root,
            branch_name,
            None,
            force,
            propagate_deletions,
            merged_by,
        )
        .await
    }

    /// Multi-agent model: **spawn an agent = fork its own branch-brain.**
    /// Creates a durable `agent/{agent_id}` branch off `parent` (default main),
    /// `RequiresProposal`-gated so the agent's work can only reach the shared
    /// brain through verify-before-merge. The fork is a ~1ms reflink (COW);
    /// the branch is synced as an `active` graph node. The agent then works
    /// against this branch via the branch-scoped capsule/recall/contribute.
    pub async fn spawn_agent_branch(
        &self,
        ws: &str,
        agent_id: &str,
        parent: Option<&str>,
    ) -> Result<String> {
        let root = self.get_workspace(ws)?.root_path.clone();
        let parent = parent.unwrap_or("main");
        let branch = format!("agent/{agent_id}");
        thinkingroot_branch::create_branch_full(
            &root,
            &branch,
            parent,
            Some(format!("agent branch-brain for '{agent_id}'")),
            // Owner = the agent's Principal identity (raw id), so the agent can
            // write its own branch (the owner short-circuit in
            // `ensure_branch_permission`).
            Some(agent_id.to_string()),
            thinkingroot_core::BranchPermissions::default(),
            thinkingroot_core::BranchKind::default(),
            thinkingroot_core::MergePolicy::RequiresProposal {
                min_reviewers: 0,
                required_checks: vec!["health_score".to_string()],
            },
            None,
        )
        .await?;
        let created = chrono::Utc::now().timestamp() as f64;
        let _ = self
            .sync_branch_created(&root, &branch, Some(parent), Some("agent"), created)
            .await;
        Ok(branch)
    }

    /// Multi-agent model: **finish an agent = gated merge-back of its branch.**
    /// Opens a proposal `agent/{agent_id}` → main, runs the verify-before-merge
    /// checks, and (when `auto_merge` and the gate says `Approved`) merges the
    /// agent's learnings into the shared brain. The merge path flips the agent
    /// branch node `active → merged`. Honest report: `merged` is true only if
    /// the work actually reached main.
    pub async fn finish_agent_branch(
        &self,
        ws: &str,
        agent_id: &str,
        min_reviewers: u8,
        auto_merge: bool,
    ) -> Result<AgentBranchReport> {
        let root = self.get_workspace(ws)?.root_path.clone();
        let branch = format!("agent/{agent_id}");
        let mut report = AgentBranchReport {
            agent_id: agent_id.to_string(),
            branch: branch.clone(),
            proposal_id: None,
            proposal_status: None,
            checks: Vec::new(),
            merged: false,
            note: String::new(),
        };

        let refs_dir = root.join(".thinkingroot-refs");
        let required_checks = vec!["health_score".to_string()];
        let proposal = thinkingroot_pr::open_proposal(
            &refs_dir,
            &branch,
            None,
            &format!("agent:{agent_id}"),
            Some(format!("agent '{agent_id}' merge-back of branch-brain")),
            min_reviewers,
            required_checks,
        )?;
        report.proposal_id = Some(proposal.id.clone());

        let proposal = self.run_proposal_checks(&root, &proposal.id).await?;
        let mut latest: std::collections::HashMap<String, &thinkingroot_pr::CheckRun> =
            std::collections::HashMap::new();
        for c in &proposal.checks {
            latest.insert(c.name.clone(), c);
        }
        report.checks = latest
            .into_values()
            .map(|c| (c.name.clone(), c.passed, c.detail.clone()))
            .collect();
        report.proposal_status = Some(format!("{:?}", proposal.status).to_lowercase());

        if auto_merge && matches!(proposal.status, thinkingroot_pr::ProposalStatus::Approved) {
            // System executes the verified merge on the agent's behalf: agents
            // can't merge to main directly (architectural invariant), and the
            // RequiresProposal + health_score gate is the real protection —
            // same pattern as the promotion consolidation job.
            match self
                .merge_into_branch(
                    &root,
                    &branch,
                    None,
                    false,
                    false,
                    thinkingroot_core::MergedBy::System,
                )
                .await
            {
                Ok(_) => {
                    report.merged = true;
                    report.proposal_status = Some("merged".to_string());
                    report.note =
                        format!("agent '{agent_id}' merged into shared brain via proposal {}", proposal.id);
                }
                Err(e) => report.note = format!("merge blocked after approval: {e}"),
            }
        } else if auto_merge {
            report.note = format!(
                "agent branch gated ({}) — checks must pass before merge-back",
                report.proposal_status.as_deref().unwrap_or("open")
            );
        } else {
            report.note = format!("proposal {} left open for review", proposal.id);
        }
        Ok(report)
    }

    /// Per-run isolation: **fork an isolated `run/{run_id}` branch** off `parent`
    /// (default main). Mirrors `spawn_agent_branch` structurally — same
    /// `RequiresProposal` + `health_score` gate, owner set to `run_id`, branch
    /// kind label `"run"`. The engine caller must `settle_run_branch` after the
    /// run completes.
    pub async fn fork_run_branch(
        &self,
        ws: &str,
        run_id: &str,
        parent: Option<&str>,
    ) -> Result<String> {
        let root = self.get_workspace(ws)?.root_path.clone();
        let parent = parent.unwrap_or("main");
        let branch = format!("run/{run_id}");
        thinkingroot_branch::create_branch_full(
            &root,
            &branch,
            parent,
            Some(format!("isolated run branch for run '{run_id}'")),
            Some(run_id.to_string()),
            thinkingroot_core::BranchPermissions::default(),
            thinkingroot_core::BranchKind::default(),
            thinkingroot_core::MergePolicy::RequiresProposal {
                min_reviewers: 0,
                required_checks: vec!["health_score".to_string()],
            },
            None,
        )
        .await?;
        let created = chrono::Utc::now().timestamp() as f64;
        let _ = self
            .sync_branch_created(&root, &branch, Some(parent), Some("run"), created)
            .await;
        Ok(branch)
    }

    /// Per-run isolation: **settle a run branch** — merge/gate/quarantine or
    /// roll back depending on `policy` and whether the run succeeded (`ok`).
    ///
    /// | `ok` | policy     | outcome                                          |
    /// |------|------------|--------------------------------------------------|
    /// | false | any        | branch abandoned (rolled back); `rolled_back=true`|
    /// | true  | Auto       | open proposal → run checks → merge if Approved   |
    /// | true  | Verified   | same as Auto (checks gate the merge)             |
    /// | true  | Manual     | open proposal, leave open for human review       |
    /// | true  | Never      | quarantine — branch left, no proposal opened     |
    ///
    /// Honest: `merged` is true only when the run's work actually reached main.
    pub async fn settle_run_branch(
        &self,
        ws: &str,
        branch: &str,
        policy: thinkingroot_core::AgentMergePolicy,
        ok: bool,
    ) -> Result<RunBranchReport> {
        use thinkingroot_core::AgentMergePolicy as P;
        let root = self.get_workspace(ws)?.root_path.clone();
        let mut report = RunBranchReport {
            branch: branch.to_string(),
            merged: false,
            rolled_back: false,
            proposal_id: None,
            checks: Vec::new(),
            note: String::new(),
        };

        // Failure path — abandon the branch regardless of policy.
        if !ok {
            match self.delete_branch(&root, branch).await {
                Ok(_) => {
                    report.rolled_back = true;
                    report.note = "run failed — branch abandoned".into();
                }
                Err(e) => {
                    report.rolled_back = false;
                    report.note = format!("run failed; rollback ALSO failed: {e}");
                }
            }
            return Ok(report);
        }

        match policy {
            P::Never => {
                report.note = "quarantined (merge_policy=never)".into();
            }
            P::Manual => {
                // Open a proposal and leave it open for a human reviewer.
                let refs_dir = root.join(".thinkingroot-refs");
                let proposal = thinkingroot_pr::open_proposal(
                    &refs_dir,
                    branch,
                    None,
                    "system:run",
                    Some(format!("run branch '{branch}' awaiting manual review")),
                    1,
                    vec!["health_score".to_string()],
                )?;
                report.proposal_id = Some(proposal.id.clone());
                report.note = format!("proposal {} left open for manual review", proposal.id);
            }
            P::Auto | P::Verified => {
                // Auto and Verified both gate the merge on health_score; Verified is intentionally treated as Auto until a stricter reviewer/check policy is added.
                // Open proposal → run checks → merge if the gate approves.
                let refs_dir = root.join(".thinkingroot-refs");
                let proposal = thinkingroot_pr::open_proposal(
                    &refs_dir,
                    branch,
                    None,
                    "system:run",
                    Some(format!("run branch '{branch}' merge-back")),
                    0,
                    vec!["health_score".to_string()],
                )?;
                report.proposal_id = Some(proposal.id.clone());

                let proposal = self.run_proposal_checks(&root, &proposal.id).await?;
                let mut latest: std::collections::HashMap<String, &thinkingroot_pr::CheckRun> =
                    std::collections::HashMap::new();
                for c in &proposal.checks {
                    latest.insert(c.name.clone(), c);
                }
                report.checks = latest
                    .into_values()
                    .map(|c| (c.name.clone(), c.passed, c.detail.clone()))
                    .collect();

                if matches!(proposal.status, thinkingroot_pr::ProposalStatus::Approved) {
                    match self
                        .merge_into_branch(
                            &root,
                            branch,
                            None,
                            false,
                            false,
                            thinkingroot_core::MergedBy::System,
                        )
                        .await
                    {
                        Ok(_) => {
                            report.merged = true;
                            report.note =
                                format!("run branch '{branch}' merged into shared brain");
                        }
                        Err(e) => {
                            report.note = format!("merge blocked after approval: {e}");
                        }
                    }
                } else {
                    report.note = format!(
                        "health gate failed — not merged (proposal status: {:?})",
                        proposal.status
                    );
                }
            }
        }
        Ok(report)
    }

    /// Cross-brain verified merge (Agent State Topology Phase 2).
    ///
    /// Merges the claims that are unique to `source_ws/source_branch` into
    /// `target_ws`'s trunk (or an explicit `target_branch`), stamping provenance
    /// `merged_from:<source_ws>/<source_branch>`.  The diff health gate
    /// (`compute_diff_into`) is always consulted; if it blocks, the method returns
    /// `Ok(report)` with `merged = false` (no panic, no partial write).
    ///
    /// Only `new_claims` and `auto_resolved` are written.  `needs_review` pairs
    /// are counted and reported but never written (ambiguous resolution would be
    /// dishonest).
    pub async fn merge_across_workspaces(
        &self,
        source_ws: &str,
        source_branch: &str,
        target_ws: &str,
        target_branch: Option<&str>,
    ) -> Result<MergeAcrossReport> {
        use thinkingroot_branch::diff::compute_diff_into;
        use thinkingroot_branch::snapshot::resolve_data_dir;
        use thinkingroot_core::types::{ContentHash, SourceType};
        use thinkingroot_graph::graph::GraphStore;

        let target_branch_str = target_branch.unwrap_or("main").to_string();

        // ── I1: guard against merging a workspace into itself ────────────────
        if source_ws == target_ws && source_branch == target_branch_str {
            return Err(Error::MergeBlocked(format!(
                "cross-brain merge: source and target are identical \
                 ('{source_ws}/{source_branch}') — cannot merge a branch into itself"
            )));
        }

        // ── resolve workspace handles ────────────────────────────────────────
        let src_root = self.get_workspace(source_ws)?.root_path.clone();
        let tgt_root = self.get_workspace(target_ws)?.root_path.clone();

        // ── open the source branch graph ─────────────────────────────────────
        let source_data_dir = resolve_data_dir(&src_root, Some(source_branch));
        if !source_data_dir.exists() {
            return Err(Error::EntityNotFound(format!(
                "cross-brain merge: source branch '{source_branch}' not found in workspace \
                 '{source_ws}'"
            )));
        }
        let source_graph = GraphStore::init(&source_data_dir.join("graph"))
            .map_err(|e| Error::GraphStorage(format!("source branch graph init failed: {e}")))?;

        // ── open the target trunk graph ──────────────────────────────────────
        let target_data_dir = resolve_data_dir(&tgt_root, target_branch);
        let target_graph = GraphStore::init(&target_data_dir.join("graph"))
            .map_err(|e| Error::GraphStorage(format!("target graph init failed: {e}")))?;

        // ── compute diff (health gate) ────────────────────────────────────────
        let diff = compute_diff_into(
            &target_graph,
            &source_graph,
            source_branch,
            target_branch,
            0.7,
            0.1,
            false,
        )?;

        let mut report = MergeAcrossReport {
            source_ws: source_ws.to_string(),
            source_branch: source_branch.to_string(),
            target_ws: target_ws.to_string(),
            target_branch: target_branch_str.clone(),
            merged: false,
            merged_claims: 0,
            auto_resolved: diff.auto_resolved.len(),
            needs_review: diff.needs_review.len(),
            merge_allowed: diff.merge_allowed,
            blocking_reasons: diff.blocking_reasons.clone(),
            note: String::new(),
        };

        if !diff.merge_allowed {
            report.note = format!(
                "health gate blocked cross-brain merge from '{source_ws}/{source_branch}' \
                 into '{target_ws}/{target_branch_str}'"
            );
            return Ok(report);
        }

        // ── build agent claims from diff.new_claims ──────────────────────────
        let agent_claims: Vec<AgentClaim> = diff
            .new_claims
            .iter()
            .map(|dc| AgentClaim {
                statement: dc.claim.statement.clone(),
                claim_type: dc.claim.claim_type.wire_str().to_string(),
                confidence: Some(dc.claim.confidence.value()),
                entities: dc.entity_context.clone(),
            })
            .collect();

        // Nothing to write → idempotent success.
        if agent_claims.is_empty() && diff.auto_resolved.is_empty() {
            report.merged = true;
            report.note = format!(
                "cross-brain merge '{source_ws}/{source_branch}' → \
                 '{target_ws}/{target_branch_str}': no new claims to merge (already up to date)"
            );
            return Ok(report);
        }

        // ── provenance source ────────────────────────────────────────────────
        let source_uri = format!("merged_from:{source_ws}/{source_branch}");
        let ts = chrono::Utc::now().timestamp();
        let merge_source =
            thinkingroot_core::Source::new(source_uri.clone(), SourceType::Manual)
                .with_trust(thinkingroot_core::types::TrustLevel::Untrusted)
                .with_hash(ContentHash(format!(
                    "cross-brain-{source_ws}-{source_branch}-{ts}"
                )));

        // ── write new claims into target graph ───────────────────────────────
        let (accepted_ids, write_warnings) = if !agent_claims.is_empty() {
            let res = Self::write_agent_claims_to_graph(&target_graph, &merge_source, &agent_claims)?;
            for w in &res.1 {
                tracing::warn!(
                    "merge_across_workspaces: write warning (non-fatal): {w}"
                );
            }
            res
        } else {
            (vec![], vec![])
        };

        // ── apply auto-resolutions ───────────────────────────────────────────
        //
        // SAFETY RULE: supersede_claim(old, new) must only ever be called with
        // IDs that exist in the TARGET graph.  `res.winner` can be either
        // `res.main_claim_id` (existing in target — the easy case) or
        // `res.branch_claim_id` (exists ONLY in source — the ghost-id bug).
        //
        // Correct handling:
        //  • main wins  → do nothing; the target claim is already correct.
        //  • branch wins → fetch the branch claim from source_graph, write it
        //    into the target (getting a fresh target-local ID), then supersede
        //    the old main claim with that new target ID.
        for res in &diff.auto_resolved {
            if res.winner == res.main_claim_id {
                // Target claim wins: nothing to do — keep it as-is and simply
                // skip the contradicting branch claim.
                continue;
            }

            // Branch claim wins: it must be written into the target first so
            // that supersede_claim is called with a real target-side ID.
            let branch_claim = match source_graph.get_claim_by_id(&res.branch_claim_id) {
                Ok(Some(c)) => c,
                Ok(None) => {
                    tracing::warn!(
                        "merge_across_workspaces: auto-resolution winner '{}' not found in \
                         source graph — skipping supersede to avoid ghost-id deletion",
                        res.branch_claim_id
                    );
                    continue;
                }
                Err(e) => {
                    tracing::warn!(
                        "merge_across_workspaces: failed to fetch branch winner '{}' from \
                         source graph ({e}) — skipping supersede",
                        res.branch_claim_id
                    );
                    continue;
                }
            };

            let winner_agent_claim = AgentClaim {
                statement: branch_claim.statement.clone(),
                claim_type: branch_claim.claim_type.wire_str().to_string(),
                confidence: Some(branch_claim.confidence.value()),
                entities: vec![],
            };

            match Self::write_agent_claims_to_graph(
                &target_graph,
                &merge_source,
                &[winner_agent_claim],
            ) {
                Ok((new_ids, warns)) => {
                    for w in &warns {
                        tracing::warn!("merge_across_workspaces: auto-resolve write warning: {w}");
                    }
                    if let Some(new_id) = new_ids.into_iter().next() {
                        // Both IDs are now guaranteed to exist in the target.
                        if let Err(e) =
                            target_graph.supersede_claim(&res.main_claim_id, &new_id)
                        {
                            tracing::warn!(
                                "merge_across_workspaces: supersede_claim({} → {new_id}) \
                                 failed (non-fatal): {e}",
                                res.main_claim_id
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "merge_across_workspaces: failed to write branch-winner claim into \
                         target ({e}) — skipping supersede for main_claim_id '{}'",
                        res.main_claim_id
                    );
                }
            }
        }

        // ── upsert vectors into the TARGET workspace's vector index ──────────
        // Mirror contribute_claims_as main path: best-effort, warn on failure.
        if !accepted_ids.is_empty() {
            let tgt_handle = self.get_workspace(target_ws)?;
            let mut storage = tgt_handle.storage.lock().await;
            let items: Vec<(String, String, String)> = agent_claims
                .iter()
                .zip(accepted_ids.iter())
                .map(|(ac, id)| {
                    let ctype = &ac.claim_type;
                    let conf = ac.confidence.unwrap_or(0.7);
                    (
                        format!("claim:{id}"),
                        ac.statement.clone(),
                        format!("claim|{id}|{ctype}|{conf}|{source_uri}"),
                    )
                })
                .collect();
            let vwarn = run_blocking(|| {
                let mut out = Vec::new();
                if let Err(e) = storage.vector.upsert_batch(&items) {
                    out.push(format!(
                        "merge_across: target vector index degraded — upsert of {} embeddings \
                         failed ({e}); hybrid retrieval will miss these claims until `root compile`",
                        items.len()
                    ));
                } else if let Err(e) = storage.vector.save() {
                    out.push(format!(
                        "merge_across: target vector index save failed ({e})"
                    ));
                }
                out
            });
            for w in vwarn {
                tracing::warn!("{w}");
            }

            // Reload the target cache while still holding the storage lock so
            // the next list/search sees the freshly merged claims immediately.
            let new_cache = KnowledgeGraph::load_from_graph(&storage.graph).map_err(|e| {
                Error::GraphStorage(format!(
                    "merge_across: claims written into '{target_ws}' but in-memory cache \
                     reload failed — remount the workspace to refresh.  Error: {e}"
                ))
            })?;
            // Unlock storage before taking the cache write lock (prevents ordering deadlock).
            drop(storage);
            let tgt_handle = self.get_workspace(target_ws)?;
            *tgt_handle.cache.write().await = new_cache;
        }

        report.merged = true;
        report.merged_claims = accepted_ids.len();
        report.note = format!(
            "cross-brain merge '{source_ws}/{source_branch}' → \
             '{target_ws}/{target_branch_str}': {} claim(s) written, \
             {} auto-resolved, {} deferred for review",
            accepted_ids.len(),
            diff.auto_resolved.len(),
            diff.needs_review.len(),
        );
        Ok(report)
    }

    pub async fn merge_into_branch(
        &self,
        root: &std::path::Path,
        source_branch_name: &str,
        target_branch: Option<&str>,
        force: bool,
        propagate_deletions: bool,
        merged_by: thinkingroot_core::MergedBy,
    ) -> Result<thinkingroot_core::KnowledgeDiff> {
        self.merge_into_branch_cancellable(
            root,
            source_branch_name,
            target_branch,
            force,
            propagate_deletions,
            merged_by,
            None,
        )
        .await
    }

    /// Whether a branch is **durable** (worth a graph node) vs ephemeral /
    /// internal. `stream/*` (one per MCP session, auto-GC'd) and `promotion/*`
    /// (consolidation-internal staging) are excluded — node-ifying them would
    /// be a write storm + clutter, for branches that vanish.
    fn is_durable_branch(name: &str) -> bool {
        !name.starts_with("stream/") && !name.starts_with("promotion/")
    }

    /// Write a branch node into the graph of the workspace rooted at `root`
    /// (the brain that owns this branch). No-op if that workspace isn't mounted.
    async fn write_branch_node_at(
        &self,
        root: &std::path::Path,
        branch: &thinkingroot_graph::artifact_nodes::BranchNode,
    ) -> Result<()> {
        if let Some(h) = self.workspaces.values().find(|h| h.root_path == root) {
            let storage = h.storage.lock().await;
            storage.graph.upsert_branch_node(branch)?;
        }
        Ok(())
    }

    /// Sync a newly-created **durable** branch as an `active` graph node so the
    /// brain can describe its own topology. Best-effort: callers wrap in
    /// `let _ =` so a node-sync failure never fails branch creation.
    pub async fn sync_branch_created(
        &self,
        root: &std::path::Path,
        name: &str,
        parent: Option<&str>,
        kind: Option<&str>,
        created_at: f64,
    ) -> Result<()> {
        use thinkingroot_graph::artifact_nodes::{BranchNode, BRANCH_STATUS_ACTIVE};
        if !Self::is_durable_branch(name) {
            return Ok(());
        }
        self.write_branch_node_at(
            root,
            &BranchNode {
                name: name.to_string(),
                status: BRANCH_STATUS_ACTIVE.to_string(),
                parent: parent.map(str::to_string),
                kind: kind.map(str::to_string),
                created_at,
                merged_at: None,
            },
        )
        .await
    }

    /// Flip a branch node to `merged` (preserving its parent/kind/created_at).
    /// No-op if the branch was never node-ified (ephemeral/internal), so the
    /// brain never invents a node on merge — and never shows `active` after a
    /// merge (honesty rule).
    pub async fn sync_branch_merged(
        &self,
        root: &std::path::Path,
        name: &str,
        merged_at: f64,
    ) -> Result<()> {
        use thinkingroot_graph::artifact_nodes::BRANCH_STATUS_MERGED;
        let Some(h) = self.workspaces.values().find(|h| h.root_path == root) else {
            return Ok(());
        };
        let storage = h.storage.lock().await;
        let existing = storage
            .graph
            .list_branch_nodes()?
            .into_iter()
            .find(|b| b.name == name);
        if let Some(mut b) = existing {
            b.status = BRANCH_STATUS_MERGED.to_string();
            b.merged_at = Some(merged_at);
            storage.graph.upsert_branch_node(&b)?;
        }
        Ok(())
    }

    /// Remove a branch node when its branch is deleted. No-op if not node-ified.
    pub async fn sync_branch_removed(&self, root: &std::path::Path, name: &str) -> Result<()> {
        use thinkingroot_graph::artifact_nodes::KIND_BRANCH;
        if let Some(h) = self.workspaces.values().find(|h| h.root_path == root) {
            let storage = h.storage.lock().await;
            storage.graph.remove_artifact_node(KIND_BRANCH, name)?;
        }
        Ok(())
    }

    /// Read-only merge diff for `source_branch_name` into `target_branch`
    /// (None ⇒ main) — the `health_score` check's basis. Computes the same
    /// `KnowledgeDiff` (`merge_allowed` + `blocking_reasons`) the merge path
    /// does, WITHOUT executing the merge.
    pub async fn preview_merge_diff(
        &self,
        root: &std::path::Path,
        source_branch_name: &str,
        target_branch: Option<&str>,
    ) -> Result<thinkingroot_core::KnowledgeDiff> {
        use thinkingroot_branch::diff::compute_diff_into;
        use thinkingroot_branch::snapshot::resolve_data_dir;
        use thinkingroot_graph::graph::GraphStore;

        let target_data_dir = resolve_data_dir(root, target_branch);
        let source_data_dir = resolve_data_dir(root, Some(source_branch_name));
        if !source_data_dir.exists() {
            return Err(Error::EntityNotFound(format!(
                "branch '{source_branch_name}' not found"
            )));
        }
        let merge_cfg = match self.workspaces.values().find(|h| h.root_path == root) {
            Some(h) => h.config.merge.clone(),
            None => Config::load_merged(root)?.merge,
        };
        let target_graph = GraphStore::init(&target_data_dir.join("graph"))
            .map_err(|e| Error::GraphStorage(format!("target graph init failed: {e}")))?;
        let source_graph = GraphStore::init(&source_data_dir.join("graph"))
            .map_err(|e| Error::GraphStorage(format!("source graph init failed: {e}")))?;
        compute_diff_into(
            &target_graph,
            &source_graph,
            source_branch_name,
            target_branch,
            merge_cfg.auto_resolve_threshold,
            merge_cfg.max_health_drop,
            merge_cfg.block_on_contradictions,
        )
    }

    /// M3 — run a proposal's required checks daemon-side and record each
    /// result via `thinkingroot_pr::record_check`, so `RequiresProposal`
    /// branches actually gate on verification. Before this, `record_check`
    /// was never called, so `required_checks` were dead metadata. The
    /// proposal's `status` reaches `Approved` only once every required check
    /// passes (and the reviewer count is met). Unknown check names are
    /// recorded as **failed** (an honest, gated block) — never silently
    /// skipped. Returns the updated proposal.
    pub async fn run_proposal_checks(
        &self,
        root: &std::path::Path,
        proposal_id: &str,
    ) -> Result<thinkingroot_pr::KnowledgeProposal> {
        let refs_dir = root.join(".thinkingroot-refs");
        let proposal = thinkingroot_pr::read_proposal(&refs_dir, proposal_id)?
            .ok_or_else(|| Error::EntityNotFound(format!("proposal '{proposal_id}' not found")))?;
        if matches!(
            proposal.status,
            thinkingroot_pr::ProposalStatus::Merged | thinkingroot_pr::ProposalStatus::Closed
        ) {
            return Ok(proposal);
        }
        let source = proposal.source_branch.clone();
        let target = proposal.target_branch.clone();
        let names = if proposal.required_checks.is_empty() {
            vec!["health_score".to_string()]
        } else {
            proposal.required_checks.clone()
        };
        let mut latest = proposal;
        for name in names {
            let (passed, detail) = match name.as_str() {
                "health_score" => self.check_health_score(root, &source, target.as_deref()).await,
                "function_tests" => self.check_function_tests(root, &source).await,
                other => (
                    false,
                    Some(format!(
                        "no daemon-side runner for required check '{other}' — merge stays gated"
                    )),
                ),
            };
            latest = thinkingroot_pr::record_check(&refs_dir, &latest.id, &name, passed, detail)?;
        }
        Ok(latest)
    }

    /// The `health_score` check: passes iff a read-only merge diff of the
    /// source branch into its target is `merge_allowed` (health gate +
    /// conflict/contradiction checks all clear).
    async fn check_health_score(
        &self,
        root: &std::path::Path,
        source: &str,
        target: Option<&str>,
    ) -> (bool, Option<String>) {
        match self.preview_merge_diff(root, source, target).await {
            Ok(diff) if diff.merge_allowed => (
                true,
                Some("merge_allowed: health gate + conflict checks passed".to_string()),
            ),
            Ok(diff) => (
                false,
                Some(format!("blocked: {}", diff.blocking_reasons.join("; "))),
            ),
            Err(e) => (false, Some(format!("diff computation failed: {e}"))),
        }
    }

    /// The `function_tests` check (P4): every fixture of every Root Function
    /// deployed on the SOURCE branch must run and match its `expect_json`. This
    /// is the gate that makes self-authored / promoted functions safe — a
    /// function cannot reach the shared brain unless its own fixtures pass. The
    /// function bodies are loaded from the source branch's own graph and run in
    /// the deno isolate (no secrets/cognition — a hermetic check run). No
    /// fixtures on the branch = nothing to gate (soft pass with a note).
    async fn check_function_tests(
        &self,
        root: &std::path::Path,
        source: &str,
    ) -> (bool, Option<String>) {
        use std::collections::{BTreeMap, HashMap};
        use thinkingroot_branch::snapshot::resolve_data_dir;
        use thinkingroot_graph::graph::GraphStore;
        let dir = resolve_data_dir(root, Some(source));
        if !dir.exists() {
            return (false, Some(format!("branch '{source}' not found")));
        }
        let graph = match GraphStore::init(&dir.join("graph")) {
            Ok(g) => g,
            Err(e) => return (false, Some(format!("source graph init failed: {e}"))),
        };
        let funcs = match graph.list_functions() {
            Ok(f) => f,
            Err(e) => return (false, Some(format!("list_functions failed: {e}"))),
        };
        let mut total = 0usize;
        let mut failures: Vec<String> = Vec::new();
        for f in &funcs {
            for fx in graph.list_function_fixtures(&f.name).unwrap_or_default() {
                total += 1;
                let input: serde_json::Value =
                    serde_json::from_str(&fx.input_json).unwrap_or(serde_json::Value::Null);
                let expect: serde_json::Value =
                    serde_json::from_str(&fx.expect_json).unwrap_or(serde_json::Value::Null);
                let (outcome, _, _, _) = crate::root_function_runtime::run_js_journaled(
                    &f.body,
                    &input,
                    &BTreeMap::new(),
                    crate::root_function_runtime::FnCtxMeta::default(),
                    None,
                    HashMap::new(),
                    10,
                    // Fixture checks run against a sandboxed branch graph (not a
                    // mounted workspace) — ctx.memory/etc. are intentionally
                    // unavailable here, same as ctx.cognition (see below).
                    None,
                )
                .await;
                match outcome {
                    crate::root_function_runtime::RunOutcome::Done(got) if got == expect => {}
                    crate::root_function_runtime::RunOutcome::Done(got) => failures
                        .push(format!("{}#{}: got {got} != expect {expect}", f.name, fx.fixture_id)),
                    crate::root_function_runtime::RunOutcome::Suspended => failures.push(format!(
                        "{}#{}: suspended (ctx.cognition unavailable in a check run)",
                        f.name, fx.fixture_id
                    )),
                    crate::root_function_runtime::RunOutcome::Failed(e) => {
                        failures.push(format!("{}#{}: error {e}", f.name, fx.fixture_id))
                    }
                }
            }
        }
        if total == 0 {
            (true, Some("no function fixtures on source branch — nothing to gate".to_string()))
        } else if failures.is_empty() {
            (true, Some(format!("{total} fixture(s) passed")))
        } else {
            (
                false,
                Some(format!(
                    "{}/{} fixture(s) failed: {}",
                    failures.len(),
                    total,
                    failures.join("; ")
                )),
            )
        }
    }

    /// #2 — Promotion consolidation. Mine **quorum'd, de-identified** patterns
    /// from this project's per-user brains (`u_*` workspaces) and stage them
    /// for **verify-before-merge** promotion into the shared brain.
    ///
    /// The flow stacks three independent safety layers (see
    /// [`crate::consolidation`] for the privacy rationale):
    /// 1. **k-anonymity** — only patterns recurring across ≥ `min_users`
    ///    distinct users are eligible (poisoning-resistant: distinct users,
    ///    never occurrences).
    /// 2. **identifier scrubbing** — direct identifiers are redacted before a
    ///    statement is even compared, so sub-quorum text never leaves a user
    ///    workspace and quorum'd text leaves de-identified.
    /// 3. **verify-before-merge** — the patterns are contributed to a
    ///    `RequiresProposal` staging branch whose `health_score` check must
    ///    pass before the merge into the shared brain is allowed (M3 gate).
    ///
    /// Ranges **only** over this project's own per-user workspaces — one daemon
    /// serves one project, so cross-tenant promotion is structurally
    /// impossible. The cloud gates this on the project's `promotion_enabled`
    /// flag (the engine endpoint is privileged + reached only via the
    /// provisioner, which checks the flag).
    ///
    /// Returns an honest [`ConsolidationReport`] — `merged` is true only if the
    /// patterns actually reached the shared brain this pass.
    pub async fn consolidate_to_shared(
        &self,
        spec: crate::consolidation::ConsolidationSpec,
        sessions: &crate::intelligence::session::SessionStore,
    ) -> Result<crate::consolidation::ConsolidationReport> {
        use crate::consolidation::{ConsolidationReport, UserClaim, quorum_patterns};
        use thinkingroot_graph::graph::GraphStore;

        let spec = spec.sanitized();

        // Resolve the shared (primary) brain + its on-disk root.
        let shared_ws = self.primary_ws_name().ok_or_else(|| {
            Error::Config("no shared workspace mounted to consolidate into".into())
        })?;
        let root = self.get_workspace(&shared_ws)?.root_path.clone();

        // Per-user workspaces are siblings under `.thinkingroot-users/` (see
        // `get_or_mount_user_ws`). Candidate users come from BOTH the currently
        // mounted set and the on-disk dirs — deduped. We must NOT `GraphStore::init`
        // a workspace that is already mounted: the embedded DB holds a single
        // process lock, so a mounted workspace is read through its live handle;
        // only unmounted ones are opened from disk (read-only).
        let users_base = match root.parent() {
            Some(p) => p.join(".thinkingroot-users"),
            None => root.join(".thinkingroot-users"),
        };

        let mut candidates: std::collections::BTreeSet<String> = self
            .workspaces
            .keys()
            .filter(|k| k.starts_with("u_"))
            .cloned()
            .collect();
        if users_base.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&users_base) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.starts_with("u_")
                        && entry.path().join(".thinkingroot").join("graph").exists()
                    {
                        candidates.insert(name);
                    }
                }
            }
        }

        let mut user_claims: Vec<UserClaim> = Vec::new();
        let mut users_scanned = 0usize;
        for user in candidates {
            // Read the claims, preferring the live mounted handle.
            let rows = if let Some(handle) = self.workspaces.get(&user) {
                let storage = handle.storage.lock().await;
                match storage.graph.get_all_claims_with_sources() {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(user = %user, error = %e, "consolidation: skip mounted user workspace (claim read failed)");
                        continue;
                    }
                }
            } else {
                let graph_dir = users_base.join(&user).join(".thinkingroot").join("graph");
                match GraphStore::init(&graph_dir) {
                    Ok(g) => match g.get_all_claims_with_sources() {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::warn!(user = %user, error = %e, "consolidation: skip user workspace (claim read failed)");
                            continue;
                        }
                    },
                    Err(e) => {
                        tracing::warn!(user = %user, error = %e, "consolidation: skip unreadable user workspace");
                        continue;
                    }
                }
            };
            users_scanned += 1;
            for (_id, statement, claim_type, confidence, _uri, _event) in rows {
                if confidence < spec.min_confidence || !spec.accepts_type(&claim_type) {
                    continue;
                }
                user_claims.push(UserClaim {
                    user: user.clone(),
                    statement,
                    claim_type,
                    confidence,
                });
            }
        }

        let claims_examined = user_claims.len();
        let patterns = quorum_patterns(&user_claims, &spec);

        let mut report = ConsolidationReport {
            shared_ws: shared_ws.clone(),
            users_scanned,
            claims_examined,
            patterns_promoted: patterns.clone(),
            staging_branch: None,
            proposal_id: None,
            proposal_status: None,
            checks: Vec::new(),
            merged: false,
            note: String::new(),
        };

        if patterns.is_empty() {
            report.note = format!(
                "no patterns cleared the quorum gate (min_users={}, scanned {} users / {} claims)",
                spec.min_users, users_scanned, claims_examined
            );
            return Ok(report);
        }

        // ── Stage on a RequiresProposal branch off the shared main ──────────
        let branch_name = format!("promotion/{}", ulid::Ulid::new());
        let required_checks = vec!["health_score".to_string()];
        thinkingroot_branch::create_branch_full(
            &root,
            &branch_name,
            "main",
            Some(format!(
                "promotion consolidation: {} de-identified pattern(s)",
                patterns.len()
            )),
            Some("system:consolidation".to_string()),
            thinkingroot_core::BranchPermissions::default(),
            thinkingroot_core::BranchKind::default(),
            thinkingroot_core::MergePolicy::RequiresProposal {
                min_reviewers: spec.min_reviewers,
                required_checks: required_checks.clone(),
            },
            None,
        )
        .await?;
        report.staging_branch = Some(branch_name.clone());

        // Contribute the de-identified patterns to the staging branch.
        let agent_claims: Vec<AgentClaim> = patterns
            .iter()
            .map(|p| AgentClaim {
                statement: p.statement.clone(),
                claim_type: p.claim_type.clone(),
                confidence: Some(p.mean_confidence),
                entities: Vec::new(),
            })
            .collect();
        let session_id = format!("consolidation:{branch_name}");
        self.contribute_claims_as(
            &shared_ws,
            &session_id,
            Some(&branch_name),
            agent_claims,
            sessions,
            BranchActor::System,
        )
        .await?;

        // ── Open a proposal (shared main is the target) + run M3 checks ─────
        let refs_dir = root.join(".thinkingroot-refs");
        let proposal = thinkingroot_pr::open_proposal(
            &refs_dir,
            &branch_name,
            None,
            "system:consolidation",
            Some("automated promotion of quorum'd, de-identified per-user patterns".to_string()),
            spec.min_reviewers,
            required_checks,
        )?;
        report.proposal_id = Some(proposal.id.clone());

        let proposal = self.run_proposal_checks(&root, &proposal.id).await?;
        // Latest result per check name.
        let mut latest: std::collections::HashMap<String, &thinkingroot_pr::CheckRun> =
            std::collections::HashMap::new();
        for c in &proposal.checks {
            latest.insert(c.name.clone(), c);
        }
        report.checks = latest
            .into_values()
            .map(|c| (c.name.clone(), c.passed, c.detail.clone()))
            .collect();
        report.proposal_status = Some(format!("{:?}", proposal.status).to_lowercase());

        // ── Optional auto-merge (only if the gate says Approved) ────────────
        if spec.auto_merge
            && matches!(proposal.status, thinkingroot_pr::ProposalStatus::Approved)
        {
            match self
                .merge_into_branch(
                    &root,
                    &branch_name,
                    None,
                    false,
                    false,
                    thinkingroot_core::MergedBy::System,
                )
                .await
            {
                Ok(_) => {
                    report.merged = true;
                    report.proposal_status = Some("merged".to_string());
                    report.note = format!(
                        "promoted {} pattern(s) into '{}' via verified proposal {}",
                        patterns.len(),
                        shared_ws,
                        proposal.id
                    );
                }
                Err(e) => {
                    report.note = format!("merge blocked after approval: {e}");
                }
            }
        } else if spec.auto_merge {
            report.note = format!(
                "staged proposal {} is gated ({}) — checks must pass before promotion",
                proposal.id,
                report.proposal_status.as_deref().unwrap_or("open")
            );
        } else {
            report.note = format!(
                "staged proposal {} left open for review (auto_merge disabled)",
                proposal.id
            );
        }

        Ok(report)
    }

    /// T1.5 — cancellable variant of [`Self::merge_into_branch`].
    /// Pass `Some(token)` to plumb a `CancellationToken` through to
    /// `execute_merge_into_cancellable`; phase boundaries inside the
    /// merge return `Error::Cancelled` if the token trips before the
    /// registry write completes.
    #[allow(clippy::too_many_arguments)]
    pub async fn merge_into_branch_cancellable(
        &self,
        root: &std::path::Path,
        source_branch_name: &str,
        target_branch: Option<&str>,
        force: bool,
        propagate_deletions: bool,
        merged_by: thinkingroot_core::MergedBy,
        cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<thinkingroot_core::KnowledgeDiff> {
        use thinkingroot_branch::diff::compute_diff_into;
        use thinkingroot_branch::merge::execute_merge_into_cancellable;
        use thinkingroot_branch::snapshot::resolve_data_dir;
        use thinkingroot_graph::graph::GraphStore;

        let actor = match &merged_by {
            thinkingroot_core::MergedBy::Human { user } => Principal::User(user.clone()),
            thinkingroot_core::MergedBy::Agent { agent_id } => Principal::Agent(agent_id.clone()),
            thinkingroot_core::MergedBy::Connector {
                connector_id,
                install_id,
            } => Principal::Connector {
                connector_id: connector_id.clone(),
                install_id: install_id.clone(),
            },
            thinkingroot_core::MergedBy::System => Principal::System,
        };
        Self::ensure_branch_permission(
            &actor,
            Self::branch_ref_for_root(root, target_branch.unwrap_or("main"))?.as_ref(),
            "merge_branch",
        )?;

        let target_data_dir = resolve_data_dir(root, target_branch);
        let source_data_dir = resolve_data_dir(root, Some(source_branch_name));
        if !source_data_dir.exists() {
            return Err(Error::EntityNotFound(format!(
                "branch '{source_branch_name}' not found"
            )));
        }

        // Merge-time knobs: prefer the mounted workspace config, fall back to
        // disk config so unmounted callers still work.
        let mounted = self.workspaces.values().find(|h| h.root_path == root);
        let merge_cfg = match mounted {
            Some(h) => h.config.merge.clone(),
            None => Config::load_merged(root)?.merge,
        };

        let target_graph = GraphStore::init(&target_data_dir.join("graph"))
            .map_err(|e| Error::GraphStorage(format!("target graph init failed: {e}")))?;
        let source_graph = GraphStore::init(&source_data_dir.join("graph"))
            .map_err(|e| Error::GraphStorage(format!("source graph init failed: {e}")))?;

        let mut diff = compute_diff_into(
            &target_graph,
            &source_graph,
            source_branch_name,
            target_branch,
            merge_cfg.auto_resolve_threshold,
            merge_cfg.max_health_drop,
            merge_cfg.block_on_contradictions,
        )?;
        if force {
            diff.merge_allowed = true;
            diff.blocking_reasons.clear();
        }

        // Drop the separate GraphStore handle on main *before* executing the
        // merge so `execute_merge` can take its own handle to `graph.db`
        // (some SQLite configurations serialize writers).
        drop(target_graph);
        drop(source_graph);

        execute_merge_into_cancellable(
            root,
            source_branch_name,
            target_branch,
            &diff,
            merged_by,
            propagate_deletions,
            force,
            cancel,
        )
        .await?;

        if let Some(handle) = mounted
            && target_branch.unwrap_or("main") == "main"
        {
            let storage = handle.storage.lock().await;
            // Cache reload after merge into main is a hard error: the merge
            // already mutated graph.db but the in-memory cache is stale,
            // so the next list/search would mis-report what's been merged.
            // The pre-merge snapshot is the recovery anchor — caller can
            // `root branch rollback` if remount also fails.
            let new_cache = KnowledgeGraph::load_from_graph(&storage.graph).map_err(|e| {
                Error::GraphStorage(format!(
                    "merge: target main updated on disk but in-memory cache reload \
                     failed for workspace '{ws_name}' — remount the workspace to \
                     refresh, or run `root branch rollback` to revert to the \
                     pre-merge snapshot.  Underlying error: {e}",
                    ws_name = handle.name
                ))
            })?;
            *handle.cache.write().await = new_cache;
        }

        if let Some(target_branch_name) = target_branch.filter(|b| *b != "main") {
            self.branch_engines
                .invalidate(root, target_branch_name)
                .await;
        }

        // Sync the source branch's node active → merged (no-op for ephemeral /
        // internal branches and unmounted workspaces). Best-effort: a node-sync
        // failure must not unwind a completed merge.
        let merged_at = chrono::Utc::now().timestamp() as f64;
        let _ = self
            .sync_branch_merged(root, source_branch_name, merged_at)
            .await;

        Ok(diff)
    }

    /// Soft-delete a branch (marks Abandoned, data retained). Evicts the
    /// branch from the engine cache so stale handles can't serve reads
    /// against an Abandoned entry.
    pub async fn delete_branch(&self, root: &std::path::Path, branch_name: &str) -> Result<()> {
        self.delete_branch_as(root, branch_name, BranchActor::System)
            .await
    }

    pub async fn delete_branch_as(
        &self,
        root: &std::path::Path,
        branch_name: &str,
        actor: BranchActor,
    ) -> Result<()> {
        let branch_ref = Self::branch_ref_for_root(root, branch_name)?;
        Self::ensure_branch_permission(&actor, branch_ref.as_ref(), "delete_branch")?;
        self.branch_engines.invalidate(root, branch_name).await;
        thinkingroot_branch::delete_branch(root, branch_name)
            .map_err(|e| Error::GraphStorage(format!("delete_branch failed: {e}")))?;
        // Drop the branch's node (no-op if it was never node-ified). Best-effort.
        let _ = self.sync_branch_removed(root, branch_name).await;
        Ok(())
    }

    pub async fn rebase_branch(
        &self,
        root: &std::path::Path,
        branch_name: &str,
        actor: BranchActor,
    ) -> Result<thinkingroot_core::KnowledgeDiff> {
        let branch_ref = Self::branch_ref_for_root(root, branch_name)?;
        Self::ensure_branch_permission(&actor, branch_ref.as_ref(), "rebase_branch")?;

        let diff = thinkingroot_branch::rebase_branch(root, branch_name)
            .await
            .map_err(|e| Error::GraphStorage(format!("rebase_branch failed: {e}")))?;

        self.branch_engines.invalidate(root, branch_name).await;
        Ok(diff)
    }

    /// Garbage-collect all Abandoned branches (hard-delete their data dirs).
    /// Evicts every cache entry for this workspace root because we don't
    /// selectively know which branches got purged.
    pub async fn gc_branches(&self, root: &std::path::Path) -> Result<usize> {
        self.branch_engines.invalidate_workspace(root).await;
        thinkingroot_branch::gc_branches(root)
            .map_err(|e| Error::GraphStorage(format!("gc_branches failed: {e}")))
    }

    /// Backstop GC: purge orphaned `run/*` branches older than `idle_secs`.
    ///
    /// `settle_run_branch` is the normal cleanup path; this is the crash
    /// backstop for branches left behind when a process exits before
    /// `settle_run_branch` runs. Uses `self.delete_branch` (cache-invalidate
    /// + graph-node sync) rather than the raw branch-layer call.
    ///
    /// `idle_secs = 0` makes every `run/*` branch immediately eligible
    /// (useful in tests). Only Active `run/*` branches are touched — other
    /// prefixes (stream/, topic/, feature/, agent_*) are never purged.
    pub async fn gc_run_branches(&self, ws: &str, idle_secs: i64) -> Result<usize> {
        let root = self.get_workspace(ws)?.root_path.clone();
        let now = chrono::Utc::now().timestamp();
        let mut purged = 0usize;
        let branches = thinkingroot_branch::list_branches(&root)
            .map_err(|e| Error::GraphStorage(format!("gc_run_branches list_branches: {e}")))?;
        for b in branches {
            if !b.name.starts_with("run/") {
                continue;
            }
            if !matches!(b.status, thinkingroot_core::BranchStatus::Active) {
                continue;
            }
            let age = now - b.created_at.timestamp();
            if age < idle_secs {
                continue;
            }
            match self.delete_branch(&root, &b.name).await {
                Ok(()) => {
                    purged += 1;
                    tracing::info!(
                        target: "maintenance",
                        branch = %b.name,
                        age_secs = age,
                        "gc_run_branches: purged orphan run branch"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        target: "maintenance",
                        branch = %b.name,
                        err = %e,
                        "gc_run_branches: delete failed"
                    );
                }
            }
        }
        Ok(purged)
    }

    /// Phase 9 Reflect — discover patterns + surface gaps for a workspace.
    ///
    /// Serialised against the storage mutex because pattern discovery
    /// issues a large read-heavy Datalog query followed by writes to
    /// `structural_patterns` and `known_unknowns`; interleaving with
    /// other writes would waste effort (the pattern discovery rescans).
    pub async fn reflect(&self, ws: &str) -> Result<thinkingroot_reflect::ReflectResult> {
        self.reflect_branched(ws, None).await
    }

    /// Branch-aware variant of `reflect`.
    ///
    /// - `branch = None` (or `Some("main")`) — runs against the mounted
    ///   workspace's primary `graph.db`, holding the storage mutex.
    /// - `branch = Some(name)` — runs against the branch's copy-on-write
    ///   `graph.db` via the `BranchEngineCache`. The branch has its own
    ///   `structural_patterns` / `known_unknowns` relations (the tables
    ///   were copied from main at branch create time), so the reflect
    ///   result is fully branch-scoped.
    pub async fn reflect_branched(
        &self,
        ws: &str,
        branch: Option<&str>,
    ) -> Result<thinkingroot_reflect::ReflectResult> {
        let handle = self.get_workspace(ws)?;
        let engine = thinkingroot_reflect::ReflectEngine::new(
            thinkingroot_reflect::ReflectConfig::default(),
        );
        match branch {
            None | Some("main") => {
                let storage = handle.storage.lock().await;
                engine.reflect(&storage.graph)
            }
            Some(branch_name) => {
                let bh = self
                    .branch_engines
                    .get_or_open(&handle.root_path, branch_name)
                    .await?;
                engine.reflect(&bh.graph)
            }
        }
    }

    /// List open gap reports (known-unknowns) for a workspace. Served
    /// directly from CozoDB — no cache since gap sets change infrequently
    /// and are not in the hot retrieval path.
    pub async fn list_gaps(
        &self,
        ws: &str,
        entity: Option<&str>,
        min_confidence: f64,
    ) -> Result<Vec<thinkingroot_reflect::GapReport>> {
        self.list_gaps_branched(ws, entity, min_confidence, None)
            .await
    }

    /// T2.4 — bitemporal "as-of" claim list for a branch.
    ///
    /// Returns every claim whose `created_at` is at or before
    /// `tx_time` (a `chrono::DateTime<Utc>` from the caller).  Pairs
    /// with the engine's `list_claims_branched` for the live view —
    /// `list_claims_as_of` is the time-travel query.
    ///
    /// `branch = None | Some("main")` runs against the workspace's
    /// primary graph; `branch = Some(name)` runs against the
    /// branch's COW graph through the `BranchEngineCache`.
    pub async fn list_claims_as_of_branched(
        &self,
        ws: &str,
        branch: Option<&str>,
        tx_time: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<crate::engine::ClaimInfo>> {
        let handle = self.get_workspace(ws)?;
        let ts = tx_time.timestamp() as f64;
        let rows = match branch {
            None | Some("main") => {
                let storage = handle.storage.lock().await;
                storage.graph.get_claims_with_sources_as_of(ts)?
            }
            Some(branch_name) => {
                let bh = self
                    .branch_engines
                    .get_or_open(&handle.root_path, branch_name)
                    .await?;
                bh.graph.get_claims_with_sources_as_of(ts)?
            }
        };

        Ok(rows
            .into_iter()
            .map(
                |(id, statement, claim_type, confidence, source_uri, event_date)| {
                    crate::engine::ClaimInfo {
                        id,
                        statement,
                        claim_type: thinkingroot_core::types::ClaimType::normalize_storage(
                            &claim_type,
                        ),
                        confidence,
                        source_uri,
                        event_date: if event_date > 0.0 {
                            Some(event_date)
                        } else {
                            None
                        },
                    }
                },
            )
            .collect())
    }

    /// T3.2 — Cross-branch reflect.  Runs `reflect_branched` on each
    /// named branch in sequence (sequential, not parallel — every
    /// branch's `BranchEngineCache` lookup needs a write lock to
    /// open if the engine isn't cached, and racing those would just
    /// serialise on the same lock anyway).  Surfaces divergent
    /// patterns: pattern ids that fired in some branches but not
    /// others.
    ///
    /// Pass `branches.is_empty()` to get an error rather than a
    /// trivially-empty result — the no-branches case is almost
    /// always a caller bug, not a useful query.
    ///
    /// `branches` may include `"main"` (or `None`'s convention) to
    /// include the workspace's primary graph alongside named
    /// branches in the comparison.
    pub async fn reflect_across_branches(
        &self,
        ws: &str,
        branches: &[String],
    ) -> Result<thinkingroot_reflect::CrossBranchReflectResult> {
        if branches.is_empty() {
            return Err(Error::Config(
                "reflect_across_branches: at least one branch required".into(),
            ));
        }

        let mut per_branch = std::collections::HashMap::new();
        let mut order: Vec<String> = Vec::with_capacity(branches.len());
        for raw in branches {
            let key = raw.clone();
            order.push(key.clone());
            // Treat "main" identically to the None branch — both
            // resolve to the workspace's primary graph in
            // `reflect_branched`.
            let result = if key == "main" {
                self.reflect_branched(ws, None).await?
            } else {
                self.reflect_branched(ws, Some(&key)).await?
            };
            per_branch.insert(key, result);
        }

        // Walk every distinct pattern id; classify each branch as
        // present/absent based on whether the pattern appears in
        // that branch's `patterns` vec.  A pattern present in every
        // branch is NOT divergent — only union-but-not-intersection
        // rows land in the output.
        let mut all_pattern_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for r in per_branch.values() {
            for p in &r.patterns {
                all_pattern_ids.insert(p.id.clone());
            }
        }
        let mut divergent: Vec<thinkingroot_reflect::DivergentPattern> = Vec::new();
        for pid in &all_pattern_ids {
            let mut present_in: Vec<String> = Vec::new();
            let mut absent_from: Vec<String> = Vec::new();
            let mut aggregate_sample_size: usize = 0;
            for branch_name in &order {
                let branch_result = per_branch.get(branch_name).unwrap();
                if let Some(p) = branch_result.patterns.iter().find(|p| p.id == *pid) {
                    present_in.push(branch_name.clone());
                    aggregate_sample_size += p.sample_size;
                } else {
                    absent_from.push(branch_name.clone());
                }
            }
            // Skip patterns shared by every branch — they aren't
            // divergent; they're the consensus.
            if absent_from.is_empty() {
                continue;
            }
            divergent.push(thinkingroot_reflect::DivergentPattern {
                pattern_id: pid.clone(),
                present_in,
                absent_from,
                aggregate_sample_size,
            });
        }
        // Sort by aggregate_sample_size descending so the largest
        // signal lands first in dashboards.
        divergent.sort_by(|a, b| b.aggregate_sample_size.cmp(&a.aggregate_sample_size));

        Ok(thinkingroot_reflect::CrossBranchReflectResult {
            workspace: ws.to_string(),
            branches: order,
            per_branch,
            divergent_patterns: divergent,
        })
    }

    /// Branch-aware variant of `list_gaps`.
    pub async fn list_gaps_branched(
        &self,
        ws: &str,
        entity: Option<&str>,
        min_confidence: f64,
        branch: Option<&str>,
    ) -> Result<Vec<thinkingroot_reflect::GapReport>> {
        let handle = self.get_workspace(ws)?;
        match branch {
            None | Some("main") => {
                let storage = handle.storage.lock().await;
                thinkingroot_reflect::list_open_gaps(&storage.graph, entity, min_confidence)
            }
            Some(branch_name) => {
                let bh = self
                    .branch_engines
                    .get_or_open(&handle.root_path, branch_name)
                    .await?;
                thinkingroot_reflect::list_open_gaps(&bh.graph, entity, min_confidence)
            }
        }
    }

    /// Regenerate the Living Paper (`<root>/.thinkingroot/paper.md`)
    /// for a workspace without running a full compile. Reads the
    /// current Witness Mesh state, synthesises the deterministic
    /// skeleton (plus AI narrative when an LLM is configured), and
    /// writes the result atomically. Returns the bytes-and-frontmatter
    /// summary the caller can surface in a toast.
    pub async fn regenerate_paper(
        &self,
        ws: &str,
    ) -> Result<thinkingroot_paper::PaperOutput> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        let now = chrono::Utc::now();
        // Use the same LLM client the workspace was mounted with so
        // the AI narrative renders when a provider is configured.
        match &handle.llm {
            Some(client) => thinkingroot_paper::synthesize_and_persist_with_llm(
                &storage.graph,
                &handle.root_path,
                &handle.name,
                now,
                client.as_ref(),
            )
            .await
            .map_err(|e| Error::Config(format!("paper regenerate failed: {e}"))),
            None => thinkingroot_paper::synthesize_and_persist(
                &storage.graph,
                &handle.root_path,
                &handle.name,
                now,
            )
            .map_err(|e| Error::Config(format!("paper regenerate failed: {e}"))),
        }
    }

    /// Cross-workspace reflect — aggregate co-occurrence across every
    /// named workspace and apply the resulting patterns to each one.
    /// Useful when no single workspace has enough instances of a given
    /// entity type to clear the `min_sample_size` threshold, but the
    /// union does (e.g. 10 services × 5 repos = 50 services combined).
    ///
    /// Local patterns are untouched — each workspace's own `reflect()`
    /// continues to maintain `source_scope = 'local'` rows independently.
    pub async fn reflect_across(
        &self,
        workspaces: &[String],
    ) -> Result<thinkingroot_reflect::CrossReflectResult> {
        if workspaces.is_empty() {
            return Err(Error::Config(
                "reflect_across: at least one workspace required".into(),
            ));
        }
        // Lock every participating storage handle up front — pattern
        // aggregation is a read-heavy query across many graphs followed
        // by per-workspace writes. Holding all locks for the duration
        // serializes against concurrent writes so the aggregate sample
        // counts match what each workspace will then be asked to gap
        // against.
        //
        // BTreeMap key: workspace name (for error messages). Value:
        // the locked guard, held for the scope of this method.
        let mut guards: Vec<(
            String,
            tokio::sync::MutexGuard<'_, thinkingroot_graph::StorageEngine>,
        )> = Vec::with_capacity(workspaces.len());
        for name in workspaces {
            let handle = self.get_workspace(name)?;
            guards.push((name.clone(), handle.storage.lock().await));
        }

        let graph_refs: Vec<(String, &thinkingroot_graph::graph::GraphStore)> = guards
            .iter()
            .map(|(name, guard)| (name.clone(), &guard.graph))
            .collect();

        let cfg = thinkingroot_reflect::ReflectConfig::default();
        thinkingroot_reflect::reflect_across_graphs(&graph_refs, &cfg)
    }

    /// Dismiss an open gap — mark the `known_unknowns` row as
    /// `Dismissed`. Subsequent `reflect()` cycles respect this status and
    /// do not re-raise the gap as open, so agents can suppress known
    /// false positives ("this service legitimately has no auth — it's
    /// internal only"). Branch-aware via `branch` param.
    pub async fn dismiss_gap(&self, ws: &str, gap_id: &str, branch: Option<&str>) -> Result<()> {
        let handle = self.get_workspace(ws)?;
        match branch {
            None | Some("main") => {
                let storage = handle.storage.lock().await;
                thinkingroot_reflect::dismiss_gap(&storage.graph, gap_id)
            }
            Some(branch_name) => {
                let bh = self
                    .branch_engines
                    .get_or_open(&handle.root_path, branch_name)
                    .await?;
                thinkingroot_reflect::dismiss_gap(&bh.graph, gap_id)
            }
        }
    }

    /// Roll back a previously executed merge by restoring the most recent
    /// pre-merge snapshot for the given branch. After the on-disk swap, the
    /// mounted workspace's cache is reloaded so subsequent reads reflect the
    /// pre-merge state without a `compile` or `contribute`.
    #[tracing::instrument(
        name = "engine.rollback_merge",
        skip(self, root),
        fields(branch = %branch_name),
    )]
    pub async fn rollback_merge(&self, root: &std::path::Path, branch_name: &str) -> Result<()> {
        thinkingroot_branch::rollback_merge(root, branch_name)
            .map_err(|e| Error::GraphStorage(format!("rollback failed: {e}")))?;

        if let Some(handle) = self.workspaces.values().find(|h| h.root_path == root) {
            let storage = handle.storage.lock().await;
            // Cache reload after rollback is a hard error: the rollback
            // restored graph.db from the snapshot but the in-memory cache
            // still reflects the post-merge state, so subsequent reads
            // would lie about what the rollback achieved.
            let new_cache = KnowledgeGraph::load_from_graph(&storage.graph).map_err(|e| {
                Error::GraphStorage(format!(
                    "rollback: snapshot restored to graph.db but in-memory cache reload \
                     failed for workspace '{ws_name}' — remount the workspace to refresh.  \
                     Underlying error: {e}",
                    ws_name = handle.name
                ))
            })?;
            *handle.cache.write().await = new_cache;
        }
        Ok(())
    }

    /// Inner helper: insert a source + claims into any GraphStore.
    fn write_agent_claims_to_graph(
        graph: &thinkingroot_graph::graph::GraphStore,
        source: &thinkingroot_core::Source,
        agent_claims: &[AgentClaim],
    ) -> Result<(Vec<String>, Vec<String>)> {
        graph.insert_source(source)?;

        let mut accepted_ids: Vec<String> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();

        for ac in agent_claims {
            let claim_type = parse_claim_type_str(&ac.claim_type);
            let claim = thinkingroot_core::Claim::new(
                ac.statement.clone(),
                claim_type,
                source.id,
                thinkingroot_core::types::WorkspaceId::new(),
            )
            .with_confidence(ac.confidence.unwrap_or(0.7))
            .with_extraction_tier(thinkingroot_core::types::ExtractionTier::AgentInferred);

            graph.insert_claim(&claim)?;
            graph.link_claim_to_source(&claim.id.to_string(), &source.id.to_string())?;

            for entity_name in &ac.entities {
                match graph.find_entity_id_by_name(entity_name) {
                    Ok(Some(eid)) => {
                        graph.link_claim_to_entity(&claim.id.to_string(), &eid)?;
                    }
                    Ok(None) => {
                        warnings.push(format!(
                            "entity '{entity_name}' not found — claim saved but unlinked"
                        ));
                    }
                    Err(e) => {
                        warnings.push(format!("entity lookup failed for '{entity_name}': {e}"));
                    }
                }
            }

            accepted_ids.push(claim.id.to_string());
        }

        Ok((accepted_ids, warnings))
    }

    /// Look up a mounted workspace by name, returning an error if not found.
    fn get_workspace(&self, name: &str) -> Result<&WorkspaceHandle> {
        self.workspaces
            .get(name)
            .ok_or_else(|| Error::EntityNotFound(format!("workspace '{name}' not mounted")))
    }

    /// Return the LLM client for a workspace, if one was successfully initialised.
    pub fn workspace_llm(&self, ws: &str) -> Option<Arc<thinkingroot_llm::llm::LlmClient>> {
        if let Some(llm) = self.workspaces.get(ws).and_then(|h| h.llm.clone()) {
            return Some(llm);
        }
        // Auto-scoped workspaces (`u_*` / `agent_*`) are mounted on demand and
        // carry no own `[llm]` block, so their handle has no client. They INHERIT
        // the primary brain's LLM — the project's single global provider — exactly
        // as functions / prompts / recall inherit the chain. Without this, per-user
        // chat (`ask`) and the night-shift `dream`/`predict` fail with "no LLM
        // configured" even though the engine has a provider configured on `main`.
        if is_auto_scoped_ws(ws) {
            if let Some(primary) = self.primary_ws_name() {
                if primary != ws {
                    return self.workspaces.get(&primary).and_then(|h| h.llm.clone());
                }
            }
        }
        None
    }

    /// Names of all currently-mounted workspaces. Cheap (just the map keys) —
    /// used by the LLM keep-warm pinger (`maintenance::spawn_keep_warm`) to
    /// enumerate which workspaces to probe without taking the heavier
    /// `list_workspaces` path (which computes per-workspace counts).
    pub fn mounted_workspace_names(&self) -> Vec<String> {
        self.workspaces.keys().cloned().collect()
    }

    /// A7-SEC ③ — fetch the STORED embedding for each claim id (vector key
    /// `claim:{id}`) from the workspace vector index. Read-only and NO embedding
    /// is computed (latency-safe for the read path); `None` for a claim with no
    /// stored vector or an unmounted workspace.
    pub async fn get_claim_embeddings(&self, ws: &str, ids: &[String]) -> Vec<Option<Vec<f32>>> {
        let Some(handle) = self.workspaces.get(ws) else {
            return vec![None; ids.len()];
        };
        let storage = handle.storage.lock().await;
        ids.iter()
            .map(|id| storage.vector.get_embedding(&format!("claim:{id}")))
            .collect()
    }

    /// Return the `StreamsConfig` for a workspace (controls auto_session_branch, etc.).
    pub fn workspace_streams_config(
        &self,
        ws: &str,
    ) -> Option<thinkingroot_core::config::StreamsConfig> {
        self.workspaces.get(ws).map(|h| h.config.streams.clone())
    }

    /// Return the `CompilationConfig` for a workspace (live sync, artifacts).
    pub fn workspace_compilation_config(
        &self,
        ws: &str,
    ) -> Option<thinkingroot_core::config::CompilationConfig> {
        self.workspaces
            .get(ws)
            .map(|h| h.config.compilation.clone())
    }

    /// Return the filesystem root path of a mounted workspace.
    pub fn workspace_root_path(&self, ws: &str) -> Option<PathBuf> {
        self.workspaces.get(ws).map(|h| h.root_path.clone())
    }

    /// Rebuild the vector index for a mounted workspace. Locks the
    /// workspace's `StorageEngine` exclusively, so blocks any concurrent
    /// reads while running. Suitable to call once after a successful
    /// compile so the next `search` / `hybrid_retrieve` / `materialize_engram`
    /// has a populated index — without this, those endpoints return empty
    /// hits even though the substrate is fully populated.
    ///
    /// Bugfix 2026-05-10 — the in-process CLI path (`run_query` in
    /// `thinkingroot-cli/src/main.rs:2361`) builds the index lazily on
    /// first read, but the daemon path through `compile_stream` did not.
    /// Result: every cortex-routed query against a fresh compile
    /// returned `[]`. Hooking this into the compile success path keeps
    /// the post-compile UX consistent across CLI-direct and daemon
    /// modes.
    pub async fn rebuild_vector_index(&self, ws: &str) -> Result<(usize, usize)> {
        let handle = self.get_workspace(ws)?;
        let storage_arc = Arc::clone(&handle.storage);
        let counts = tokio::task::spawn_blocking(move || {
            // Acquire the std-Mutex synchronously inside the blocking
            // pool — the storage engine itself is sync work (Cozo
            // queries + fastembed ONNX inference), and `block_in_place`
            // would have to be at the call site otherwise.
            let mut storage = storage_arc.blocking_lock();
            crate::pipeline::rebuild_vector_index(&mut storage)
        })
        .await
        .map_err(|e| Error::Config(format!("vector-index rebuild task panicked: {e}")))??;
        Ok(counts)
    }

    /// Delta-reconcile the workspace's vector index against its
    /// current CozoDB state. Removes embeddings whose ids are no
    /// longer in the graph and embeds only the missing ones. Used by
    /// the post-compile background task at `rest.rs::compile_stream`
    /// — the dominant 30-second cost on a 600-claim workspace prior
    /// to 2026-05-18 was the full re-embed; reconcile collapses it
    /// to "embed only the changed claims".
    ///
    /// `cancel` is observed at chunk boundaries inside the embed
    /// loop. The post-compile background task passes a token tied to
    /// the next compile's start or the workspace's unmount — letting
    /// a slow reconcile yield to a fresh compile instead of blocking
    /// the engine's storage lock for the full embed cycle.
    pub async fn reconcile_vector_index(
        &self,
        ws: &str,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<crate::pipeline::VectorReconcileStats> {
        let handle = self.get_workspace(ws)?;
        let storage_arc = Arc::clone(&handle.storage);
        let stats = tokio::task::spawn_blocking(move || {
            let mut storage = storage_arc.blocking_lock();
            crate::pipeline::reconcile_vector_index(&mut storage, &cancel)
        })
        .await
        .map_err(|e| Error::Config(format!("vector-index reconcile task panicked: {e}")))??;
        Ok(stats)
    }

    /// Force the embed + rerank ONNX models into RAM for `ws`. Called at
    /// startup (serve.rs, gated by `TR_WARM_ON_BOOT=1`) so the FIRST real
    /// query is fast and any idle-checkpoint captures an already-warm
    /// memory image — the cloud cold-start SOTA path (see CLAUDE.md
    /// "Recall / ingest / cold-start status"). Without this, both models
    /// lazy-load on first use (~7 s; `vector.rs::ensure_model`,
    /// `rerank.rs` `CrossEncoder`). Best-effort: an error is returned so
    /// the caller can log it, but the daemon still serves (lazy fallback).
    pub async fn warm_models(&self, ws: &str) -> Result<()> {
        let handle = self.get_workspace(ws)?;
        let storage_arc = Arc::clone(&handle.storage);
        let ws_path = self
            .workspace_root_path(ws)
            .unwrap_or_else(std::env::temp_dir);
        tokio::task::spawn_blocking(move || -> Result<()> {
            // Embed model — a dummy non-empty embed loads + caches the
            // ONNX session (an empty slice would no-op).
            {
                let mut storage = storage_arc.blocking_lock();
                storage.vector.embed_texts(&["warm"])?;
            }
            // Rerank model — `CrossEncoder` lazy-loads on the first
            // non-empty rerank (an empty docs slice early-returns without
            // loading), so pass one dummy doc.
            let reranker = thinkingroot_graph::rerank::CrossEncoder::new(&ws_path)?;
            reranker.rerank("warm", &["warm"])?;
            Ok(())
        })
        .await
        .map_err(|e| Error::Config(format!("warm_models task panicked: {e}")))??;

        // Warm the LLM HTTPS connection too. The FIRST Azure request after idle
        // pays a ~30s cold-connection/routing stall (measured); paying it HERE
        // (boot / warm-on-mount) keeps it off the user's first `/ask`, which
        // otherwise hangs to the synthesizer timeout and the proxy disconnects.
        // Best-effort: a failed/slow probe must never block the mount.
        if let Some(llm) = self.workspace_llm(ws) {
            let probe = tokio::time::timeout(
                std::time::Duration::from_secs(40),
                llm.chat("You are a warm-up probe.", "Reply with: ok"),
            )
            .await;
            match probe {
                Ok(Ok(_)) => tracing::info!(ws, "warm_models: LLM connection warmed"),
                Ok(Err(e)) => tracing::warn!(ws, "warm_models: LLM warm probe error: {e}"),
                Err(_) => tracing::warn!(ws, "warm_models: LLM warm probe timed out (cold)"),
            }
        }
        Ok(())
    }

    /// Return `(provider, model)` when the named workspace has a usable
    /// LLM client attached, or `None` when the workspace is unmounted
    /// or its config did not yield a working client.
    ///
    /// Bugfix 2026-05-10 — used by the post-compile path in `rest.rs`
    /// to dispatch an `LlmProbed::Configured` snapshot to the workspace
    /// status actor. Pre-fix the actor's `llm` axis stayed
    /// `Unconfigured` forever because no producer ever emitted a probe;
    /// readiness's `for_query` / `for_chat` (both gated on `llm_healthy`)
    /// were therefore false even on workspaces where the engine had a
    /// fully-initialised `LlmClient`.
    pub fn workspace_llm_summary(&self, ws: &str) -> Option<(String, String)> {
        let handle = self.workspaces.get(ws)?;
        if handle.llm.is_none() {
            return None;
        }
        Some((
            handle.config.llm.default_provider.clone(),
            handle.config.llm.extraction_model.clone(),
        ))
    }

    /// Hand out a cheap clone of the workspace's `GraphStore` for direct
    /// Datalog access. `GraphStore` is `#[derive(Clone)]` over an Arc-internal
    /// Cozo `DbInstance` (`crates/thinkingroot-graph/src/graph.rs:50-53`), so
    /// the clone is O(1) and the returned handle shares the same database.
    ///
    /// Used by RARP's `EngramManager` so the read path runs against the
    /// underlying store *without* holding the outer `Arc<RwLock<QueryEngine>>`
    /// or inner `Arc<Mutex<StorageEngine>>` for the full duration of a
    /// multi-rule materialise — Cozo serialises concurrent readers internally.
    pub async fn graph_store(
        &self,
        ws: &str,
    ) -> Option<thinkingroot_graph::graph::GraphStore> {
        let h = self.workspaces.get(ws)?;
        let storage = h.storage.lock().await;
        Some(storage.graph.clone())
    }

    /// Batched claim-existence check used by the post-stream verifier
    /// (intelligence/verifier.rs). Returns the subset of `ids` that
    /// resolve to a claim row in this workspace's graph.
    ///
    /// Implementation: clones the per-workspace `GraphStore` (O(1),
    /// shares the same `DbInstance`), then hits `get_claim_by_id` once
    /// per id off the engine lock. Cheap enough for the typical
    /// post-stream call (≤ retrieval top-K ≈ 50). Missing rows simply
    /// don't appear in the returned set — non-existence is silent
    /// (callers compare set membership, never expecting a row).
    ///
    /// Used during agent_stream_response's trust-receipt emit to build
    /// the `Substrate` impl the verifier consumes. Stays on the engine
    /// surface so future cloud-mode replacements can override the impl
    /// without retrofitting every call site.
    pub async fn claim_exists_batch(
        &self,
        ws: &str,
        ids: &[String],
    ) -> std::collections::HashSet<String> {
        let mut found = std::collections::HashSet::new();
        let Some(graph) = self.graph_store(ws).await else {
            return found;
        };
        for id in ids {
            if matches!(graph.get_claim_by_id(id), Ok(Some(_))) {
                found.insert(id.clone());
            }
        }
        found
    }

    /// Construct a workspace-scoped source byte-store for content-hash-keyed
    /// range reads (RARP's BLAKE3 verification path). Mirrors the on-demand
    /// construction at engine.rs:2226 — `FileSystemSourceStore::new` is
    /// infallible (`crates/thinkingroot-graph/src/source_store.rs:88-91`)
    /// so this only returns `None` when the workspace is not mounted.
    pub fn byte_store(
        &self,
        ws: &str,
    ) -> Option<Arc<dyn thinkingroot_graph::SourceByteStore>> {
        let h = self.workspaces.get(ws)?;
        let store = thinkingroot_graph::FileSystemSourceStore::new(
            &h.root_path.join(".thinkingroot"),
        )
        .ok()?;
        Some(Arc::new(store))
    }

    /// Run the Hybrid Retrieval pipeline against this workspace. Thin
    /// delegation to `intelligence::hybrid::hybrid_retrieve` so callers
    /// have a single ergonomic entry point on `QueryEngine`.
    ///
    /// Spec: `docs/2026-05-02-hybrid-retrieval-spec.md` §3.1.
    pub async fn hybrid_retrieve(
        &self,
        ws: &str,
        req: RetrievalRequest,
        cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<HybridResponse> {
        crate::intelligence::hybrid::hybrid_retrieve(self, ws, req, cancel).await
    }

    /// Return the merged workspace `Config` for a mounted workspace. Used by
    /// the synthesizer to read the per-workspace `[chat]` block without
    /// re-loading TOML on every request.
    pub fn workspace_config(&self, ws: &str) -> Option<Config> {
        self.workspaces.get(ws).map(|h| h.config.clone())
    }

    /// Snapshot of the inputs needed to build a `WorkspaceIdentity` for
    /// chat-time prompt assembly. Reads the in-memory cache only — no disk
    /// I/O. `source_kinds` is sorted descending by count.
    ///
    /// Returns `None` when the workspace is not mounted.
    pub async fn workspace_chat_snapshot(&self, ws: &str) -> Option<WorkspaceChatSnapshot> {
        let h = self.workspaces.get(ws)?;
        let cache = h.cache.read().await;
        let (_source_count, claim_count, _entity_count) = cache.counts();

        let mut counts: HashMap<String, usize> = HashMap::new();
        for s in cache.all_sources() {
            let kind = source_kind_from_uri(&s.uri, &s.source_type);
            *counts.entry(kind).or_default() += 1;
        }
        let mut source_kinds: Vec<(String, usize)> = counts.into_iter().collect();
        source_kinds.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        Some(WorkspaceChatSnapshot {
            name: h.name.clone(),
            root_path: h.root_path.clone(),
            config: h.config.clone(),
            claim_count,
            source_kinds,
        })
    }
}

/// Snapshot of workspace state used by the chat synthesizer to build the
/// `<system-reminder>` context block. Cheap to construct (in-memory cache
/// reads only) so the SSE handler can call it once per request.
#[derive(Debug, Clone)]
pub struct WorkspaceChatSnapshot {
    pub name: String,
    pub root_path: PathBuf,
    pub config: Config,
    pub claim_count: usize,
    /// `(kind_label, count)` pairs sorted descending. The label is the
    /// lowercase file extension when one is available, otherwise the
    /// source's declared `source_type`, otherwise `"other"`.
    pub source_kinds: Vec<(String, usize)>,
}

/// Derive a stable, lowercase kind label for a source. Prefers the file
/// extension because that's what the synthesizer's auto-detect rules
/// match against. Falls back to the source's declared `source_type` when
/// the URI has no extension (e.g. database / API connectors).
fn source_kind_from_uri(uri: &str, source_type: &str) -> String {
    // file://path/to/foo.rs → "rs"
    let path_part = uri.split_once("://").map(|(_, p)| p).unwrap_or(uri);
    let stripped = path_part.split(['?', '#']).next().unwrap_or(path_part);
    if let Some(ext) = std::path::Path::new(stripped)
        .extension()
        .and_then(|e| e.to_str())
    {
        let lower = ext.to_ascii_lowercase();
        if !lower.is_empty() {
            return lower;
        }
    }
    let st = source_type.trim();
    if st.is_empty() {
        "other".to_string()
    } else {
        st.to_ascii_lowercase()
    }
}

#[cfg(test)]
mod permission_gate_tests {
    //! Phase C.1 (2026-05-17) — pin the agent-blocked-on-main
    //! contract that `ensure_branch_permission` enforces. These
    //! tests construct minimal `BranchRef` fixtures and call the
    //! private gate directly so each rule has an isolated assertion
    //! that doesn't depend on a full workspace mount.
    use super::*;
    use thinkingroot_core::{
        BranchKind, BranchPermissions, BranchRef, BranchStatus, MergePolicy,
    };

    fn fresh_branch(name: &str, kind: BranchKind) -> BranchRef {
        BranchRef {
            name: name.into(),
            slug: name.replace(['/', ' '], "-"),
            parent: "main".into(),
            created_at: chrono::Utc::now(),
            status: BranchStatus::Active,
            description: None,
            owner: None,
            permissions: BranchPermissions::default(),
            kind,
            merge_policy: MergePolicy::Manual,
            redaction: None,
            parent_commit_hash: None,
            max_age_secs: None,
            events: Vec::new(),
        }
    }

    fn main_branch() -> BranchRef {
        fresh_branch("main", BranchKind::Main)
    }

    fn feature_branch() -> BranchRef {
        fresh_branch("topic/refactor", BranchKind::Feature)
    }

    #[test]
    fn agent_cannot_merge_into_main() {
        let actor = Principal::Agent("claude".into());
        let main = main_branch();
        let err = QueryEngine::ensure_branch_permission(&actor, Some(&main), "merge_branch")
            .expect_err("agent merge into main must be rejected");
        match err {
            Error::PermissionDenied { action, .. } => {
                assert!(
                    action.contains("Main"),
                    "PermissionDenied message must mention Main, got: {action}"
                );
                assert!(
                    action.contains("topic"),
                    "PermissionDenied message must direct user to the topic-branch flow, got: {action}"
                );
            }
            other => panic!("expected PermissionDenied, got: {other:?}"),
        }
    }

    #[test]
    fn agent_cannot_delete_or_rebase_main() {
        let actor = Principal::Agent("claude".into());
        let main = main_branch();
        assert!(
            QueryEngine::ensure_branch_permission(&actor, Some(&main), "delete_branch").is_err(),
            "agent delete on main must be rejected — would lose the workspace's primary branch"
        );
        assert!(
            QueryEngine::ensure_branch_permission(&actor, Some(&main), "rebase_branch").is_err(),
            "agent rebase on main must be rejected — would rewrite the canonical history"
        );
    }

    #[test]
    fn agent_can_read_main() {
        // Reading main is always allowed — agents need to retrieve
        // from the canonical knowledge graph. Only write-class
        // actions are gated.
        let actor = Principal::Agent("claude".into());
        let main = main_branch();
        assert!(
            QueryEngine::ensure_branch_permission(&actor, Some(&main), "read_branch").is_ok(),
            "agent read on main must pass — retrieval needs to see canonical knowledge"
        );
    }

    #[test]
    fn agent_can_merge_into_feature_topic_branch() {
        // The merge stream → topic step uses Principal::System
        // (maintenance task), but a human-driven agent invocation
        // of `merge_branch` against a topic Feature branch must
        // also be allowed — the gate is Main-specific, not a
        // blanket agent block.
        let actor = Principal::Agent("claude".into());
        let topic = feature_branch();
        assert!(
            QueryEngine::ensure_branch_permission(&actor, Some(&topic), "merge_branch").is_ok(),
            "agent merge into a Feature/topic branch must pass — C.1 gate is Main-only"
        );
    }

    #[test]
    fn human_can_merge_into_main() {
        // The human-only flow is the whole point of the C.1 gate —
        // explicitly verify a Human principal still reaches main
        // when no owner is set (default workspace).
        let actor = Principal::User("alice".into());
        let main = main_branch();
        assert!(
            QueryEngine::ensure_branch_permission(&actor, Some(&main), "merge_branch").is_ok(),
            "human merge into main must pass"
        );
    }

    #[test]
    fn system_can_merge_into_main() {
        // System principal short-circuits at the identity() == None
        // arm BEFORE the C.1 gate fires. Keeps background
        // bookkeeping merges (cross-repo sync, gc, future
        // automation) able to reach main without explicit
        // permission lists.
        let actor = Principal::System;
        let main = main_branch();
        assert!(
            QueryEngine::ensure_branch_permission(&actor, Some(&main), "merge_branch").is_ok(),
            "system merge into main must pass — backed by the identity-None short-circuit"
        );
    }

    #[test]
    fn connector_can_merge_into_main() {
        // Connectors are first-party integrations (GitHub, Slack,
        // Notion, Linear, Drive, custom HMAC webhook). They have
        // an attributable identity and a defensible audit trail,
        // so the C.1 gate doesn't block them — the existing
        // permission-list path applies.
        let actor = Principal::Connector {
            connector_id: "github".into(),
            install_id: "acme".into(),
        };
        let main = main_branch();
        assert!(
            QueryEngine::ensure_branch_permission(&actor, Some(&main), "merge_branch").is_ok(),
            "connector merge into main must pass — C.1 gate is Agent-only"
        );
    }

    #[test]
    fn agent_write_branch_on_main_is_not_blocked_by_c1() {
        // Intentional scope limit: C.1 ONLY gates merge / delete /
        // rebase. `write_branch` (the per-claim contribute path)
        // is allowed because pre-Phase-A workspaces routed agent
        // contributes directly to main when `auto_session_branch =
        // false`; hard-blocking write_branch on main here would
        // surface a regression on those workspaces before they
        // remount under the new default.
        let actor = Principal::Agent("claude".into());
        let main = main_branch();
        assert!(
            QueryEngine::ensure_branch_permission(&actor, Some(&main), "write_branch").is_ok(),
            "C.1 gate must NOT block write_branch — keeps pre-Phase-A workspaces functional"
        );
    }

    #[test]
    fn tag_immutability_still_applies() {
        // Regression guard for the T2.5 Tag gate — adding the C.1
        // arm must not have re-ordered the Tag check away.
        let actor = Principal::User("alice".into());
        let tag = fresh_branch(
            "v1.0.0",
            BranchKind::Tag {
                ref_name: "v1.0.0".into(),
                target: "0123abcd".into(),
            },
        );
        assert!(
            QueryEngine::ensure_branch_permission(&actor, Some(&tag), "merge_branch").is_err(),
            "Tag merge must stay blocked even after C.1 lands"
        );
        assert!(
            QueryEngine::ensure_branch_permission(&actor, Some(&tag), "read_branch").is_ok(),
            "Tag reads must stay allowed"
        );
    }
}

#[cfg(test)]
mod source_kind_tests {
    use super::source_kind_from_uri;

    #[test]
    fn extracts_extension_from_file_uri() {
        assert_eq!(source_kind_from_uri("file:///x/y/foo.rs", "code"), "rs");
        assert_eq!(source_kind_from_uri("/abs/path/bar.ts", "code"), "ts");
        assert_eq!(source_kind_from_uri("rel/baz.MD", "doc"), "md");
    }

    #[test]
    fn falls_back_to_source_type_when_no_extension() {
        assert_eq!(
            source_kind_from_uri("postgres://server/db", "database"),
            "database"
        );
        assert_eq!(source_kind_from_uri("plain-name", ""), "other");
    }

    #[test]
    fn ignores_query_string_and_fragment() {
        assert_eq!(
            source_kind_from_uri("https://x.com/foo.json?token=abc", "api"),
            "json"
        );
        assert_eq!(source_kind_from_uri("file:///doc.md#anchor", "doc"), "md");
    }
}

// ---------------------------------------------------------------------------
// Intelligent serve layer types
// ---------------------------------------------------------------------------

/// Request for [`QueryEngine::compile_capsule`]. `query` is the user's
/// turn text; `prompt_name` selects the system-prompt template; `vars`
/// fills its `{{...}}`. `branch` scopes the brief (and, in M4, the live
/// delta path). `top_k`/`max_tools` bound the grounded-claim and tool
/// budgets.
#[derive(Debug, Clone, Deserialize)]
pub struct CapsuleSpec {
    pub prompt_name: String,
    #[serde(default)]
    pub vars: std::collections::BTreeMap<String, String>,
    pub query: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default = "default_capsule_top_k")]
    pub top_k: usize,
    #[serde(default = "default_capsule_max_tools")]
    pub max_tools: usize,
    #[serde(default)]
    pub session_id: Option<String>,
}

fn default_capsule_top_k() -> usize {
    8
}

fn default_capsule_max_tools() -> usize {
    5
}

/// One grounded claim carried by a [`CompiledCapsule`] — a witness-backed
/// fact the LLM may cite, kept lean (no full provenance bundle) for low
/// token cost.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapsuleClaimRef {
    pub claim_id: String,
    pub statement: String,
    pub claim_type: String,
    pub source_uri: String,
}

/// The compiled context capsule fed to the LLM in place of raw state.
/// Round-trips through `capsule_cache` as `capsule_json`, so every field
/// is `Serialize + Deserialize`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompiledCapsule {
    pub system: String,
    pub grounded_claims: Vec<CapsuleClaimRef>,
    pub brief: WorkspaceSummary,
    pub tools: Vec<String>,
    pub token_estimate: usize,
    pub query_class: String,
    /// True when this capsule came from the warm cache (not recompiled).
    #[serde(default)]
    pub cache_hit: bool,
    /// M4 — true when the query-independent frame (system+brief+tools) was
    /// reused from the session warm-frame and only retrieval ran this turn.
    #[serde(default)]
    pub frame_warm: bool,
    /// The branch this capsule was compiled on, for agent orientation
    /// ("you're on `topic/x`, forked from `main`, status active"). `None` on
    /// main / no branch. Lets a branch-as-agent-brain know its own context.
    #[serde(default)]
    pub branch_context: Option<BranchContext>,
}

/// Lightweight branch orientation carried in a [`CompiledCapsule`]. Durable
/// branches resolve from their synced node (parent + status); ephemeral
/// branches still get a minimal "you are here" so an agent on a stream branch
/// knows where it is.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchContext {
    pub name: String,
    pub parent: Option<String>,
    pub status: String,
}

/// Rough token estimate (chars/4 heuristic) used to prove the capsule is
/// a fraction of raw context. Not a billing figure — an orientation one.
fn estimate_capsule_tokens(
    system: &str,
    claims: &[CapsuleClaimRef],
    tools: &[String],
) -> usize {
    let mut chars = system.len();
    for c in claims {
        chars += c.statement.len() + c.source_uri.len() + c.claim_type.len();
    }
    for t in tools {
        chars += t.len();
    }
    chars / 4
}

/// One outgoing edge of an operating-layer artifact node (M2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactEdgeView {
    pub relation: String,
    pub to_id: String,
}

/// An operating-layer artifact node + its edges, for `GET /ws/{ws}/artifacts`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactView {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub description: String,
    pub edges: Vec<ArtifactEdgeView>,
}

/// Token-efficient workspace summary for agent orientation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSummary {
    pub workspace: String,
    pub entity_count: usize,
    pub claim_count: usize,
    pub source_count: usize,
    pub top_entities: Vec<thinkingroot_graph::graph::TopEntity>,
    pub recent_decisions: Vec<(String, f64)>,
    pub contradiction_count: usize,
}

/// An agent-contributed claim submitted via the `contribute` MCP tool.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentClaim {
    pub statement: String,
    #[serde(default = "default_claim_type")]
    pub claim_type: String,
    pub confidence: Option<f64>,
    #[serde(default)]
    pub entities: Vec<String>,
}

fn default_claim_type() -> String {
    "fact".to_string()
}

/// Outcome of an agent branch-brain merge-back ([`QueryEngine::finish_agent_branch`]).
/// Honest: `merged` is true only if the agent's work actually reached the
/// shared brain through the verify-before-merge gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentBranchReport {
    pub agent_id: String,
    pub branch: String,
    pub proposal_id: Option<String>,
    pub proposal_status: Option<String>,
    pub checks: Vec<(String, bool, Option<String>)>,
    pub merged: bool,
    pub note: String,
}

/// Outcome of a per-run branch settle ([`QueryEngine::settle_run_branch`]).
/// Honest: `merged` is true only if the run's work actually reached the shared
/// brain through the verify-before-merge gate. `rolled_back` is true only when
/// the run failed and the branch was abandoned.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunBranchReport {
    pub branch: String,
    pub merged: bool,
    pub rolled_back: bool,
    pub proposal_id: Option<String>,
    pub checks: Vec<(String, bool, Option<String>)>,
    pub note: String,
}

/// Outcome of a [`QueryEngine::merge_across_workspaces`] call — the cross-brain
/// verified merge primitive (Agent State Topology Phase 2).
///
/// `merged` is true only when the health gate approved AND at least one claim
/// was written into the target workspace. `merge_allowed` reflects what
/// `compute_diff_into` reported regardless of outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeAcrossReport {
    pub source_ws: String,
    pub source_branch: String,
    pub target_ws: String,
    pub target_branch: String,
    /// Whether the overall operation succeeded and claims reached the target.
    pub merged: bool,
    /// Number of new claims written into the target workspace's graph.
    pub merged_claims: usize,
    /// Number of contradictions auto-resolved by confidence heuristic.
    pub auto_resolved: usize,
    /// Number of contradictions deferred for review (not merged).
    pub needs_review: usize,
    /// Whether the diff health gate permitted the merge.
    pub merge_allowed: bool,
    /// Non-empty only when `merge_allowed == false`.
    pub blocking_reasons: Vec<String>,
    pub note: String,
}

/// Result of a `contribute_claims` call.
#[derive(Debug, Clone, Serialize)]
pub struct ContributeResult {
    pub accepted_count: usize,
    pub accepted_ids: Vec<String>,
    pub source_uri: String,
    pub warnings: Vec<String>,
}

/// C1 — outcome of a [`QueryEngine::consolidate`] pass. Honest: `superseded`
/// counts only claims actually marked superseded this pass.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConsolidateReport {
    pub entities_scanned: usize,
    pub superseded: usize,
}

/// Parse a claim type string (case-insensitive) into a `ClaimType` enum.
fn parse_claim_type_str(s: &str) -> thinkingroot_core::types::ClaimType {
    use thinkingroot_core::types::ClaimType;
    match s.to_lowercase().as_str() {
        "decision" => ClaimType::Decision,
        "opinion" => ClaimType::Opinion,
        "plan" => ClaimType::Plan,
        "requirement" => ClaimType::Requirement,
        "metric" => ClaimType::Metric,
        "definition" => ClaimType::Definition,
        "dependency" => ClaimType::Dependency,
        "api_signature" | "apisignature" => ClaimType::ApiSignature,
        "architecture" => ClaimType::Architecture,
        "preference" => ClaimType::Preference,
        _ => ClaimType::Fact,
    }
}

/// System prompt for the C1 consolidation pass. Steers the model to act ONLY on
/// direct replacements — the conservative bias that protects valid claims.
const CONSOLIDATION_SYSTEM: &str = "You are a careful knowledge-base consolidator. \
You are given facts about a single subject, each with an id, oldest first. Your ONLY \
job is to find SUPERSESSIONS: a newer fact that directly replaces an older fact about \
the SAME attribute with a changed value. Be conservative — when in doubt, do not flag. \
Never merge facts about different attributes. Respond with ONLY a JSON array, no prose.";

/// Parse `[{"old_id":"…","new_id":"…"}, …]` out of an LLM response (tolerant of
/// surrounding prose / code fences). Returns the (old_id, new_id) pairs.
fn parse_supersede_pairs(text: &str) -> Vec<(String, String)> {
    let start = match text.find('[') {
        Some(i) => i,
        None => return vec![],
    };
    let end = match text.rfind(']') {
        Some(i) if i > start => i,
        _ => return vec![],
    };
    let json = &text[start..=end];
    let parsed: Vec<serde_json::Value> = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    parsed
        .into_iter()
        .filter_map(|v| {
            let old = v.get("old_id").and_then(|x| x.as_str())?.to_string();
            let new = v.get("new_id").and_then(|x| x.as_str())?.to_string();
            if old.is_empty() || new.is_empty() {
                None
            } else {
                Some((old, new))
            }
        })
        .collect()
}

/// Map the LLM extractor's free-text entity-type string to a core `EntityType`.
/// Unknown types fall back to `Concept` (the catch-all), so a novel label never
/// drops the entity.
fn parse_entity_type_str(s: &str) -> thinkingroot_core::types::EntityType {
    use thinkingroot_core::types::EntityType;
    match s.to_lowercase().as_str() {
        "person" | "people" | "user" => EntityType::Person,
        "system" => EntityType::System,
        "service" => EntityType::Service,
        "team" => EntityType::Team,
        "api" => EntityType::Api,
        "database" | "db" => EntityType::Database,
        "library" | "framework" | "language" => EntityType::Library,
        "file" => EntityType::File,
        "module" | "package" => EntityType::Module,
        "function" | "method" => EntityType::Function,
        "config" | "configuration" => EntityType::Config,
        "organization" | "org" | "company" | "startup" => EntityType::Organization,
        _ => EntityType::Concept,
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

fn cached_claim_to_info(c: &CachedClaim) -> ClaimInfo {
    ClaimInfo {
        id: c.id.clone(),
        statement: c.statement.clone(),
        // Normalize TitleCase storage (legacy `format!("{:?}")`) to the
        // canonical snake_case the `ClaimType` serde contract advertises.
        // Witness-mesh rule names (e.g. `"documents::heading"`) pass
        // through unchanged because they aren't ClaimType variants.
        claim_type: thinkingroot_core::types::ClaimType::normalize_storage(&c.claim_type),
        confidence: c.confidence,
        source_uri: c.source_uri.clone(),
        event_date: c.event_date,
    }
}

#[cfg(test)]
mod capset_tests {
    use super::*;

    // A1 serde contract: a stored grant can only NARROW. Any capability
    // missing from the document is deny; malformed JSON is None (the invoke
    // site then fails closed to deny_all).
    #[test]
    fn capset_partial_document_denies_missing_capabilities() {
        let caps = CapSet::from_json(r#"{"can_recall": true, "can_prompt": true}"#).unwrap();
        assert!(caps.can_recall);
        assert!(caps.can_prompt);
        assert!(!caps.can_remember);
        assert!(!caps.can_branch);
        assert!(!caps.can_mcp);
        assert!(!caps.can_acquire);
    }

    #[test]
    fn capset_malformed_json_is_none_and_deny_all_is_all_false() {
        assert!(CapSet::from_json("{not json").is_none());
        let d = CapSet::deny_all();
        assert!(
            !d.can_recall
                && !d.can_remember
                && !d.can_prompt
                && !d.can_branch
                && !d.can_mcp
                && !d.can_acquire
        );
    }

    #[test]
    fn capset_default_grant_round_trips_through_json() {
        let full = CapSet::default_own_workspace();
        let json = serde_json::to_string(&full).unwrap();
        let back = CapSet::from_json(&json).unwrap();
        assert!(
            back.can_recall
                && back.can_remember
                && back.can_prompt
                && back.can_branch
                && back.can_mcp
                && back.can_acquire
        );
    }
}

#[cfg(test)]
mod routing_tests {
    use super::*;

    // Routing correctness hinges on the input class being function-INDEPENDENT
    // so functions are comparable within a shared shape. Lock that here.
    #[test]
    fn shape_of_is_function_independent_and_stable() {
        assert_eq!(QueryEngine::shape_of(&serde_json::json!({"b": 1, "a": 2})), "obj[a,b]");
        assert_eq!(QueryEngine::shape_of(&serde_json::json!({"a": 2, "b": 1})), "obj[a,b]");
        assert_eq!(QueryEngine::shape_of(&serde_json::json!([1, 2, 3])), "array");
        assert_eq!(QueryEngine::shape_of(&serde_json::json!("hi")), "string");
        assert_eq!(QueryEngine::shape_of(&serde_json::json!(7)), "number");
        // The per-function class layers the name on top of the shared shape.
        assert_eq!(
            QueryEngine::input_class_for("classify", &serde_json::json!({"msg": "x"})),
            "classify:obj[msg]"
        );
    }
}
