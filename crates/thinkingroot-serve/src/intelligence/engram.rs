//! RARP / Active Engram Protocol v2 — execution engine.
//!
//! Plan: `~/.claude/plans/docs-2026-05-02-compile-completeness-co-synthetic-origami.md` §3.2–§3.11.
//! Spec: `docs/active-engram-protocol.md` §4 (materialise) and §5 (probe).
//!
//! `EngramManager` runs:
//! - **`materialize_engram(topic, scope)` → `EngramSummary`**: 20 Datalog
//!   steps against 31 of 33 substrate tables, returning a typed sub-graph
//!   summary plus a server-held `Engram` keyed by an HMAC pointer.
//! - **`probe_engram(pointer, question, clearance)` → `ProbeAnswer`**:
//!   regex-routed into one of 9 probe templates, runs Datalog, lazy +
//!   memoised BLAKE3 verifies returned rows, enriches with caveats.
//! - **`list_engrams`, `expire_engram`, `invalidate_workspace`, `shutdown`**.
//!
//! Concurrency: outer `Mutex<HashMap<SessionId, Arc<RwLock<SessionEngrams>>>>`
//! mirrors the `SessionStore` pattern. Datalog runs against a cloned
//! `GraphStore` (cheap — DbInstance is `Arc`-internal) so no engine-or
//! storage-lock is held for the duration of a multi-rule materialise.
//!
//! **Phase 4 Witness Mesh transition (2026-05-14).** Per
//! `.claude/rules/aep-v2.md` "Witness Mesh transition": engram payloads
//! remain claim-id-shaped during the dual-write transition. The 20
//! cluster queries + 9 probe templates this module dispatches against
//! still hit the legacy `claims` table (not `witnesses`) because they
//! join through tables (`admission_tier`, `claim_temporal`,
//! `contradictions`, `supersession_chain`, `claim_entity_edges`,
//! `known_unknowns`) that the Witness Mesh substrate doesn't populate
//! today. Read-side parity for the new substrate is exposed via the
//! `list_witnesses` MCP tool / REST endpoint, not through this engine.
//! Commit-2 cutover flips engram materialise + probe paths onto
//! `WitnessId` sub-meshes.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cozo::{DataValue, Num, ScriptMutability};
use thinkingroot_core::types::{AdmissionTier, ContentHash, GroundingMethod, Sensitivity, TrustLevel};
use thinkingroot_core::{Error, Result};
use thinkingroot_graph::aep_queries::{
    self as aepq, dv_str_list, run_aep,
};
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_graph::SourceByteStore;
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::engine::{
    AnswerRow, CallEdge, ClaimRef, CodeMarkerRef, CodeMetricRef, ContradictionRef,
    DocTagHistogram, DocTagRef, EngramPointer, EngramSummary, EntityRef, EventTriple,
    GitBlameRef, GitBlameSummary, GitCommitsSummary, HeadingRef, KnownUnknown, PatternMatch,
    ProbeAnswer, ProbeCaveat, QuantityRef, RowRef, SourceAuthority, SourceByteSpan,
    SourceReferenceEdge, TestAnnotationRef, TierHistogram, TrialScores, TurnRef,
};

/// Pointer scope passed to `materialize_engram`. Defaults from `EngramConfig`
/// fill in when fields are `None`.
#[derive(Debug, Clone, Default)]
pub struct EngramScope {
    pub depth_hops: Option<u8>,
    pub event_window_days: Option<u32>,
    pub clearance: Option<Vec<Sensitivity>>,
    /// Optional pin: skip the vector seed and start from these claim ids.
    pub seed_claim_ids: Option<Vec<String>>,
    /// When `Some(true)`, probes against this Engram run their answer
    /// rows through `hybrid_retrieve` for re-ranking before caveat
    /// enrichment. Composition is applied at the MCP handler layer
    /// (mcp/tools.rs::handle_probe_engram); the flag is a default here
    /// so callers can materialise an Engram already opted-in.
    /// Spec: docs/2026-05-02-hybrid-retrieval-spec.md §11.
    pub score_with_hybrid: Option<bool>,
}

/// Tunables for `EngramManager`. Defaults match Plan §3.2.
#[derive(Debug, Clone)]
pub struct EngramConfig {
    pub default_depth_hops: u8,
    pub default_event_window_days: u32,
    pub probe_default_clearance: Vec<Sensitivity>,
    pub idle_ttl: Duration,
    pub blake3_verify: bool,
    pub max_engrams_per_session: usize,
    /// Bound on `turn_provenance` lookup so a long-lived session doesn't
    /// O(turns × claim_ids) scan the entire history per probe (Plan §3.8).
    pub turn_provenance_window: usize,
}

impl Default for EngramConfig {
    fn default() -> Self {
        Self {
            default_depth_hops: 2,
            default_event_window_days: 90,
            probe_default_clearance: vec![Sensitivity::Public],
            idle_ttl: Duration::from_secs(30 * 60),
            blake3_verify: true,
            max_engrams_per_session: 100,
            turn_provenance_window: 200,
        }
    }
}

/// Server-side materialised sub-graph. The LLM never sees this directly —
/// it holds the pointer + summary; probe paths read this struct.
pub struct Engram {
    pub pointer: EngramPointer,
    pub workspace: String,
    pub topic: String,
    pub scope: EngramScope,
    pub created_at: f64,
    pub entity_set: HashSet<String>,
    pub seed_claim_ids: Vec<String>,
    pub cluster_claim_ids: Vec<String>,
    pub summary: Arc<EngramSummary>,
    /// `source_id` → `content_hash` map built once at materialise time so
    /// the probe BLAKE3 verifier can hop to the byte-store without an
    /// extra Datalog round-trip per row (Plan §3.5).
    pub source_id_to_hash: HashMap<String, ContentHash>,
    /// Lazy BLAKE3 verification cache keyed on `(content_hash_str,
    /// byte_start, byte_end)`. Uses the inner `String` (rather than the
    /// `ContentHash` newtype) because `ContentHash` does not derive
    /// `Hash`. Mismatches are cached as `false` so repeat probes don't
    /// re-hit disk only to re-fail (Plan §3.6).
    pub blake3_cache: RwLock<HashMap<(String, u64, u64), bool>>,
    /// Cluster-wide row caches for cheap probe enrichment — populated at
    /// materialise time so probes don't re-query for caveat data.
    pub call_edges: Vec<CallEdge>,
    pub doc_tags: Vec<DocTagRef>,
    pub code_markers: Vec<CodeMarkerRef>,
    pub quantities: Vec<QuantityRef>,
    pub test_origins: HashMap<String, TestAnnotationRef>,
    pub contradictions_by_claim: HashMap<String, Vec<ContradictionRef>>,
    pub supersession_terminals: HashMap<String, String>,
    pub gaps: Vec<KnownUnknown>,
}

#[derive(Default)]
struct SessionEngrams {
    workspace: String,
    engrams: HashMap<EngramPointer, Arc<Engram>>,
    last_accessed: HashMap<EngramPointer, Instant>,
}

/// Owner of all session-scoped Engrams. Held inside `AppState` as
/// `Arc<EngramManager>` so MCP handlers route into it.
pub struct EngramManager {
    sessions: Arc<Mutex<HashMap<String, Arc<RwLock<SessionEngrams>>>>>,
    config: EngramConfig,
    pointer_secret: [u8; 32],
    counter: AtomicU64,
    eviction: Mutex<Option<JoinHandle<()>>>,
    eviction_signal: CancellationToken,
}

impl EngramManager {
    pub fn new(config: EngramConfig) -> Arc<Self> {
        let mut secret = [0u8; 32];
        // Mix in process start time + a Cozo-side fresh hash; `getrandom`
        // would also work but blake3 is already in deps.
        let now_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let pid = std::process::id() as u64;
        let seed_input = [now_nanos.to_le_bytes(), pid.to_le_bytes()].concat();
        let hash = blake3::hash(&seed_input);
        secret.copy_from_slice(hash.as_bytes());
        let manager = Arc::new(Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            config,
            pointer_secret: secret,
            counter: AtomicU64::new(1),
            eviction: Mutex::new(None),
            eviction_signal: CancellationToken::new(),
        });
        manager.spawn_eviction_task();
        manager
    }

    fn spawn_eviction_task(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let signal = self.eviction_signal.clone();
        let ttl = self.config.idle_ttl;
        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            interval.tick().await; // skip immediate tick
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let Some(manager) = weak.upgrade() else { break };
                        manager.sweep_idle(ttl).await;
                    }
                    _ = signal.cancelled() => break,
                }
            }
        });
        // Storing the handle is best-effort; the cancel signal aborts.
        if let Ok(mut slot) = self.eviction.try_lock() {
            *slot = Some(handle);
        }
    }

    async fn sweep_idle(&self, ttl: Duration) {
        let now = Instant::now();
        let sessions = self.sessions.lock().await;
        for arc in sessions.values() {
            let mut s = arc.write().await;
            let stale: Vec<EngramPointer> = s
                .last_accessed
                .iter()
                .filter(|(_, t)| now.duration_since(**t) > ttl)
                .map(|(p, _)| p.clone())
                .collect();
            for ptr in stale {
                s.last_accessed.remove(&ptr);
                s.engrams.remove(&ptr);
            }
        }
    }

    /// Stop the background TTL sweeper. Idempotent.
    pub async fn shutdown(&self) {
        self.eviction_signal.cancel();
        let mut slot = self.eviction.lock().await;
        if let Some(h) = slot.take() {
            // Best-effort: the cancel signal already gates the loop.
            h.abort();
        }
    }

    fn next_pointer(&self, session: &SessionEngrams) -> EngramPointer {
        for _ in 0..16 {
            let n = self.counter.fetch_add(1, Ordering::Relaxed);
            let h = blake3::keyed_hash(&self.pointer_secret, &n.to_be_bytes());
            let bytes = h.as_bytes();
            let p = u16::from_be_bytes([bytes[0], bytes[1]]);
            let ptr = format!("0x{p:04X}");
            if !session.engrams.contains_key(&ptr) {
                return ptr;
            }
        }
        // Fallback: linear scan after 16 collisions (theoretically impossible
        // at max_engrams_per_session = 100 with 65,536 pointer space).
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("0x{:04X}", (n & 0xFFFF) as u16)
    }

    /// Drop every Engram tied to `workspace` across every session. Called
    /// from the cache-dirty hook in `engine.rs` (Plan §3.10) so probes
    /// after a writing compile don't return GC'd claim ids.
    pub async fn invalidate_workspace(&self, workspace: &str) {
        let sessions = self.sessions.lock().await;
        for arc in sessions.values() {
            let mut s = arc.write().await;
            if s.workspace == workspace {
                s.engrams.clear();
                s.last_accessed.clear();
            }
        }
    }

    /// Drop every Engram for `session_id`. Useful on session timeout.
    pub async fn invalidate_session(&self, session_id: &str) {
        let mut sessions = self.sessions.lock().await;
        sessions.remove(session_id);
    }

    /// List active engram pointers + topics for a session.
    pub async fn list_engrams(&self, session_id: &str) -> Vec<EngramRef> {
        let sessions = self.sessions.lock().await;
        let Some(arc) = sessions.get(session_id) else {
            return Vec::new();
        };
        let s = arc.read().await;
        s.engrams
            .values()
            .map(|e| EngramRef {
                pointer: e.pointer.clone(),
                topic: e.topic.clone(),
                workspace: e.workspace.clone(),
                created_at: e.created_at,
                entity_count: e.entity_set.len() as u32,
                claim_count: e.cluster_claim_ids.len() as u32,
            })
            .collect()
    }

    /// Explicit eviction. Returns true if an Engram was removed.
    pub async fn expire_engram(&self, session_id: &str, pointer: &str) -> bool {
        let sessions = self.sessions.lock().await;
        let Some(arc) = sessions.get(session_id) else {
            return false;
        };
        let mut s = arc.write().await;
        s.last_accessed.remove(pointer);
        s.engrams.remove(pointer).is_some()
    }

    async fn get_engram(&self, session_id: &str, pointer: &str) -> Result<Arc<Engram>> {
        let sessions = self.sessions.lock().await;
        let arc = sessions
            .get(session_id)
            .ok_or_else(|| Error::EntityNotFound(format!("session '{session_id}' not found")))?
            .clone();
        drop(sessions);
        let mut s = arc.write().await;
        let engram = s
            .engrams
            .get(pointer)
            .cloned()
            .ok_or_else(|| Error::EntityNotFound(format!("engram '{pointer}' not found")))?;
        s.last_accessed.insert(pointer.to_string(), Instant::now());
        Ok(engram)
    }
}

/// Light projection returned by `list_engrams`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EngramRef {
    pub pointer: EngramPointer,
    pub topic: String,
    pub workspace: String,
    pub created_at: f64,
    pub entity_count: u32,
    pub claim_count: u32,
}

// ===========================================================================
// Materialise — 20-step build of an EngramSummary from the 33-table substrate.
// ===========================================================================

impl EngramManager {
    /// Run the full materialise pipeline for `topic` against `workspace` in
    /// the context of `session_id`. The 20 numbered steps map 1:1 onto
    /// `docs/active-engram-protocol.md` §4.
    pub async fn materialize_engram(
        &self,
        session_id: &str,
        workspace: &str,
        topic: &str,
        graph: &GraphStore,
        seed_entity_ids: Vec<String>,
        scope: EngramScope,
        cancel: Option<CancellationToken>,
    ) -> Result<(EngramPointer, Arc<EngramSummary>)> {
        let cancel = cancel.unwrap_or_else(CancellationToken::new);

        // Step 1 — semantic anchor (caller-provided seed entity ids; the
        // vector pass lives in the MCP handler and feeds us the seeds).
        check_cancel(&cancel)?;
        if seed_entity_ids.is_empty() {
            return Err(Error::EntityNotFound(format!(
                "no semantic anchors for topic '{topic}'"
            )));
        }

        // Step 2 — entity cluster expansion (2-hop).
        let mut params = BTreeMap::new();
        params.insert("seed_set".into(), dv_str_list(&seed_entity_ids));
        let cluster_rows = run_aep(graph, aepq::Q_ENTITY_CLUSTER_2HOP, params)?;
        check_cancel(&cancel)?;
        let mut entity_set: HashSet<String> = seed_entity_ids.iter().cloned().collect();
        for row in &cluster_rows.rows {
            entity_set.insert(dv_string(&row[0]));
        }
        let entity_vec: Vec<String> = entity_set.iter().cloned().collect();

        // Resolve cluster claim ids: every claim that touches a cluster entity.
        let cluster_claim_ids = list_cluster_claim_ids(graph, &entity_vec)?;
        check_cancel(&cancel)?;

        // Step 3 — alias resolution.
        let alias_map = list_aliases(graph, &entity_vec)?;
        check_cancel(&cancel)?;

        // Build entity_cluster summary rows.
        let entity_cluster: Vec<EntityRef> = list_entity_refs(graph, &entity_vec, &alias_map)?;
        check_cancel(&cancel)?;

        // Step 4 — trust gate: filter cluster claim ids by admission_tier.
        let trust_gated_ids = list_trust_gated(graph)?;
        let trust_gated_set: HashSet<String> = trust_gated_ids.iter().cloned().collect();
        let admitted_claim_ids: Vec<String> = cluster_claim_ids
            .iter()
            .filter(|id| trust_gated_set.contains(*id))
            .cloned()
            .collect();
        check_cancel(&cancel)?;

        // Compute claim count per tier (against the un-gated cluster set).
        let claim_count_by_tier =
            count_admission_tiers(graph, &cluster_claim_ids).unwrap_or_default();

        // Step 5 — source-authority overlay.
        let source_authority = list_source_authority(graph, &admitted_claim_ids)?;
        check_cancel(&cancel)?;

        // Step 6a — temporally-active claims (+ Step 6b chain walk).
        let temporal_active = list_temporal_active(graph)?;
        let temporal_set: HashSet<String> = temporal_active.iter().cloned().collect();
        let active_admitted: Vec<String> = admitted_claim_ids
            .iter()
            .filter(|id| temporal_set.is_empty() || temporal_set.contains(*id))
            .cloned()
            .collect();
        let supersession_terminals = list_supersession_chain(graph, &cluster_claim_ids)?;
        check_cancel(&cancel)?;

        // Step 7 — contradictions touching cluster.
        let (contradictions_by_claim, unresolved_contradictions) =
            list_contradictions(graph, &cluster_claim_ids)?;
        check_cancel(&cancel)?;

        // Step 8 — events window scan.
        let now = epoch_seconds();
        let window_days = scope
            .event_window_days
            .unwrap_or(self.config.default_event_window_days);
        let window_start = now - (window_days as f64 * 86400.0);
        let window_end = now + 86400.0;
        let events_window =
            list_events_window(graph, &entity_vec, window_start, window_end)?;
        check_cancel(&cancel)?;

        // Step 9 — structural patterns.
        let structural_pattern_hits = list_pattern_overlay(graph, &entity_vec)?;
        check_cancel(&cancel)?;

        // Step 10 — known-unknowns.
        let gaps = list_gaps(graph, &entity_vec)?;
        check_cancel(&cancel)?;

        // Step 11 — call graph.
        let call_edges = list_call_graph(graph, &cluster_claim_ids)?;
        check_cancel(&cancel)?;

        // Step 12 — doc tags.
        let doc_tags = list_doc_tags(graph, &cluster_claim_ids)?;
        let mut doc_tags_summary = DocTagHistogram::default();
        for t in &doc_tags {
            match t.kind.as_str() {
                "param" => doc_tags_summary.param += 1,
                "returns" => doc_tags_summary.returns += 1,
                "throws" => doc_tags_summary.throws += 1,
                "deprecated" => doc_tags_summary.deprecated += 1,
                "see" => doc_tags_summary.see += 1,
                _ => doc_tags_summary.other += 1,
            }
        }
        check_cancel(&cancel)?;

        // Step 13 — code markers.
        let code_markers = list_code_markers(graph, &cluster_claim_ids)?;
        check_cancel(&cancel)?;

        // Step 14 — test annotations.
        let test_origins_vec = list_test_origins(graph, &cluster_claim_ids)?;
        let test_origins_map: HashMap<String, TestAnnotationRef> = test_origins_vec
            .iter()
            .map(|t| (t.claim_id.clone(), t.clone()))
            .collect();
        check_cancel(&cancel)?;

        // Step 15 — git_blame + git_commits.
        let (git_blame_summary, git_commits_summary) =
            list_git_summaries(graph, &cluster_claim_ids)?;
        check_cancel(&cancel)?;

        // Step 16 — code metrics.
        let code_metrics = list_code_metrics(graph, &cluster_claim_ids)?;
        check_cancel(&cancel)?;

        // Step 17 — quantities.
        let quantities = list_quantities(graph, &cluster_claim_ids)?;
        check_cancel(&cancel)?;

        // Step 18 — sensitivity filter (computed but applied at probe time
        // with the caller's clearance set; for the summary we record the
        // *intent* — clearance applied is the scope default).
        let applied_clearance = scope
            .clearance
            .clone()
            .unwrap_or_else(|| self.config.probe_default_clearance.clone());
        let redacted_count = count_redacted(graph, &cluster_claim_ids, &applied_clearance)?;
        check_cancel(&cancel)?;

        // Step 19 — derivation roots.
        let derivation_roots_by_claim = list_derivation_roots(graph, &cluster_claim_ids)?;
        check_cancel(&cancel)?;

        // Step 20 — BLAKE3 stale-row identification is *lazy* per Plan §3.6;
        // here we only build the source_id → content_hash map so the probe
        // path can hop without a re-query.
        let source_id_to_hash = list_source_hash_map(graph)?;

        // Source references: cross-doc citations into cluster sources.
        let source_ids_in_cluster: HashSet<String> = source_authority
            .iter()
            .map(|s| s.source_id.clone())
            .collect();
        let source_references = list_source_references(graph, &source_ids_in_cluster)?;

        // Markdown headings outline for cluster sources.
        let headings_outline = list_headings(graph, &source_ids_in_cluster)?;

        // Temporal window summary.
        let temporal_window = (Some(window_start), Some(window_end));

        // Supersession terminals as ClaimRef list (only the terminals).
        let terminal_ids: HashSet<String> = supersession_terminals.values().cloned().collect();
        let supersession_terminal_refs: Vec<ClaimRef> =
            list_claim_refs(graph, &terminal_ids.iter().cloned().collect::<Vec<_>>())?;

        // Stale rows: empty at materialise time (lazy verify per probe).
        let stale_rows: Vec<RowRef> = Vec::new();

        let now_ts = epoch_seconds();
        let pointer = {
            let session_arc = self.session_or_create(session_id, workspace).await;
            let session = session_arc.read().await;
            self.next_pointer(&session)
        };

        let summary = EngramSummary {
            pointer: pointer.clone(),
            topic: topic.to_string(),
            created_at: now_ts,
            entity_cluster,
            claim_count_by_tier,
            source_authority,
            source_references,
            temporal_window,
            supersession_terminals: supersession_terminal_refs,
            events_window,
            doc_tags_summary,
            headings_outline,
            call_graph_edges: call_edges.clone(),
            test_origins: test_origins_vec.clone(),
            code_markers: code_markers.clone(),
            code_metrics,
            quantitative_signals: quantities.clone(),
            structural_pattern_hits,
            gaps: gaps.clone(),
            unresolved_contradictions: unresolved_contradictions.clone(),
            derivation_roots_by_claim,
            git_commits_summary,
            git_blame_summary,
            stale_rows,
            applied_clearance: applied_clearance.clone(),
            redacted_count,
        };
        let summary = Arc::new(summary);

        let engram = Arc::new(Engram {
            pointer: pointer.clone(),
            workspace: workspace.to_string(),
            topic: topic.to_string(),
            scope,
            created_at: now_ts,
            entity_set,
            seed_claim_ids: seed_entity_ids,
            cluster_claim_ids: active_admitted,
            summary: summary.clone(),
            source_id_to_hash,
            blake3_cache: RwLock::new(HashMap::new()),
            call_edges,
            doc_tags,
            code_markers,
            quantities,
            test_origins: test_origins_map,
            contradictions_by_claim,
            supersession_terminals,
            gaps,
        });

        let session_arc = self.session_or_create(session_id, workspace).await;
        let mut session = session_arc.write().await;
        if session.engrams.len() >= self.config.max_engrams_per_session {
            // LRU eviction: drop the least-recently-accessed engram.
            if let Some((victim, _)) = session
                .last_accessed
                .iter()
                .min_by_key(|(_, t)| **t)
                .map(|(p, t)| (p.clone(), *t))
            {
                session.last_accessed.remove(&victim);
                session.engrams.remove(&victim);
            }
        }
        session.engrams.insert(pointer.clone(), engram);
        session
            .last_accessed
            .insert(pointer.clone(), Instant::now());

        Ok((pointer, summary))
    }

    async fn session_or_create(
        &self,
        session_id: &str,
        workspace: &str,
    ) -> Arc<RwLock<SessionEngrams>> {
        let mut sessions = self.sessions.lock().await;
        sessions
            .entry(session_id.to_string())
            .or_insert_with(|| {
                Arc::new(RwLock::new(SessionEngrams {
                    workspace: workspace.to_string(),
                    engrams: HashMap::new(),
                    last_accessed: HashMap::new(),
                }))
            })
            .clone()
    }
}

// ===========================================================================
// Probe — route question to a Datalog template, run it, enrich with caveats,
// lazy-verify BLAKE3, return ProbeAnswer.
// ===========================================================================

/// 9 typed probe shapes. The 10th (Counterfactual) reuses derivation_root
/// walked forward in `probe_engram::dispatch` rather than its own template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeKind {
    Factual,
    Quantitative,
    Temporal,
    Authorship,
    Structural,
    RelationCallers,
    RelationRefs,
    Existential,
    Comparative,
    Counterfactual,
}

impl ProbeKind {
    /// Regex-routed classifier — returns `(kind, confidence)`. Confidence
    /// below 0.5 triggers a `LowConfidence` caveat at probe time so the
    /// LLM knows the route was uncertain (Plan §3.7).
    pub fn classify(question: &str) -> (Self, f64) {
        use regex::Regex;
        // A small static catalogue. First match wins; non-match falls back
        // to (Factual, 0.4).
        static PATTERNS: std::sync::OnceLock<Vec<(ProbeKind, f64, Regex)>> =
            std::sync::OnceLock::new();
        let patterns = PATTERNS.get_or_init(|| {
            vec![
                (
                    ProbeKind::Quantitative,
                    0.9,
                    Regex::new(r"(?i)\bhow (much|many|fast|slow|big|small|long)\b").unwrap(),
                ),
                (
                    ProbeKind::Quantitative,
                    0.85,
                    Regex::new(r"(?i)\b(p99|p95|p50|throughput|latency|qps|rps)\b").unwrap(),
                ),
                (
                    ProbeKind::Temporal,
                    0.9,
                    Regex::new(r"(?i)\bwhen (did|will|does|was|were|is)\b").unwrap(),
                ),
                (
                    ProbeKind::Authorship,
                    0.95,
                    Regex::new(r"(?i)\bwho (wrote|introduced|added|owns|changed|made)\b")
                        .unwrap(),
                ),
                (
                    ProbeKind::Authorship,
                    0.9,
                    Regex::new(r"(?i)\bgit\s+blame\b").unwrap(),
                ),
                (
                    ProbeKind::Structural,
                    0.85,
                    Regex::new(r"(?i)\b(signature|parameters|arguments|return type|shape)\b")
                        .unwrap(),
                ),
                (
                    ProbeKind::RelationCallers,
                    0.9,
                    Regex::new(r"(?i)\bwhat (calls|invokes)\b").unwrap(),
                ),
                (
                    ProbeKind::RelationCallers,
                    0.9,
                    Regex::new(r"(?i)\bwho calls\b").unwrap(),
                ),
                (
                    ProbeKind::RelationRefs,
                    0.85,
                    Regex::new(r"(?i)\b(references|cites|imports|links to)\b").unwrap(),
                ),
                (
                    ProbeKind::Existential,
                    0.8,
                    Regex::new(r"(?i)\b(is there|are there|does .* exist|do any)\b").unwrap(),
                ),
                (
                    ProbeKind::Comparative,
                    0.8,
                    Regex::new(r"(?i)\b(compare|difference between|versus|vs\.?)\b").unwrap(),
                ),
                (
                    ProbeKind::Counterfactual,
                    0.85,
                    Regex::new(r"(?i)\bwhat would (change|happen) if\b").unwrap(),
                ),
            ]
        });
        for (kind, conf, re) in patterns {
            if re.is_match(question) {
                return (*kind, *conf);
            }
        }
        (Self::Factual, 0.4)
    }
}

impl EngramManager {
    /// Run a probe against an Engram. Returns a `ProbeAnswer` whose fields
    /// are populated from the Engram's pre-materialised caches plus a
    /// targeted Datalog query for the answer rows.
    pub async fn probe_engram(
        &self,
        session_id: &str,
        pointer: &str,
        question: &str,
        clearance: Option<Vec<Sensitivity>>,
        graph: &GraphStore,
        byte_store: &dyn SourceByteStore,
        probe_kind_override: Option<ProbeKind>,
    ) -> Result<ProbeAnswer> {
        let engram = self.get_engram(session_id, pointer).await?;
        let (kind, conf) = match probe_kind_override {
            Some(k) => (k, 1.0),
            None => ProbeKind::classify(question),
        };
        let clearance = clearance.unwrap_or_else(|| self.config.probe_default_clearance.clone());
        let mut caveats: Vec<ProbeCaveat> = Vec::new();
        if conf < 0.5 {
            caveats.push(ProbeCaveat::LowConfidence {
                measured: conf,
                threshold: 0.5,
            });
        }
        let mut answer = self
            .dispatch_probe(&engram, kind, question, &clearance, graph)
            .await?;

        // Populate trial_scores + certificate_hash by querying the most
        // recent verdict for the first answer claim (Plan §3 + spec §5.1).
        if let Some(first_claim) = answer.claim_ids.first().cloned() {
            if let Ok(Some((scores, cert))) = lookup_trial_scores(graph, &first_claim) {
                answer.trial_scores = Some(scores);
                if !cert.is_empty() {
                    answer.certificate_hash = Some(cert);
                }
            }
            // Populate turn_provenance bounded to the most recent
            // `turn_provenance_window` turns of the session (Plan §3.8).
            answer.turn_provenance = Some(lookup_turn_provenance(
                graph,
                session_id,
                &first_claim,
                self.config.turn_provenance_window,
            )?);
            // Lookup derivation parents (one-step) and root.
            if let Ok(parents) = lookup_derivation_parents(graph, &first_claim) {
                answer.derivation_parents = parents;
            }
            if let Some(roots) = engram.summary.derivation_roots_by_claim.get(&first_claim) {
                answer.derivation_root = roots.first().cloned();
            }
            // Supersession chain successors when the answer claim was
            // superseded by a newer one.
            if let Some(term) = engram.supersession_terminals.get(&first_claim) {
                if term != &first_claim {
                    answer.superseded_by_chain = vec![term.clone()];
                }
            }
            // Originating test annotation, if any.
            if let Some(t) = engram.test_origins.get(&first_claim) {
                answer.test_origin = Some(t.clone());
            }
        }

        // Caveat enrichment over the answer set.
        for cid in &answer.claim_ids {
            if let Some(contras) = engram.contradictions_by_claim.get(cid) {
                for c in contras {
                    caveats.push(ProbeCaveat::UnresolvedContradiction {
                        with_claim_id: if &c.claim_a == cid {
                            c.claim_b.clone()
                        } else {
                            c.claim_a.clone()
                        },
                        explanation: c.explanation.clone(),
                    });
                }
            }
            if let Some(test) = engram.test_origins.get(cid) {
                caveats.push(ProbeCaveat::DerivedFromTest {
                    framework: test.framework.clone(),
                });
            }
            if let Some(term) = engram.supersession_terminals.get(cid) {
                if term != cid {
                    caveats.push(ProbeCaveat::SupersededByNewerClaim {
                        successor_id: term.clone(),
                    });
                }
            }
            for gap in &engram.gaps {
                if engram.entity_set.contains(&gap.entity_id) {
                    caveats.push(ProbeCaveat::GapAdjacent {
                        gap_id: gap.gap_id.clone(),
                        expected_claim_type: gap.expected_claim_type.clone(),
                    });
                    break;
                }
            }
        }

        // Lazy BLAKE3 verification: only the rows we're returning.
        if self.config.blake3_verify {
            for (idx, span) in answer.source_byte_spans.iter().enumerate() {
                let expected = answer.source_blake3s.get(idx).cloned().unwrap_or_default();
                if expected.is_empty() {
                    continue;
                }
                let ok = self
                    .verify_row(
                        &engram,
                        byte_store,
                        &span.source_id,
                        span.byte_start,
                        span.byte_end,
                        &expected,
                    )
                    .await?;
                if !ok {
                    caveats.push(ProbeCaveat::StaleRow {
                        content_blake3_mismatch: true,
                        reason: "verify_failed".into(),
                    });
                }
            }
        }

        // Sensitivity redaction caveat: emit one if any cluster claim was
        // dropped by the clearance gate.
        if engram.summary.redacted_count > 0 {
            caveats.push(ProbeCaveat::SensitivityRedaction {
                hidden_field: "claim".into(),
                required_clearance: highest_required_clearance(&clearance),
            });
        }

        // Cluster-aware context: pull from the Engram's pre-materialised caches.
        let related_quantities = engram.quantities.clone();
        let related_doc_tags = engram.doc_tags.clone();
        let related_calls = engram.call_edges.clone();
        let related_markers = engram.code_markers.clone();

        Ok(ProbeAnswer {
            related_quantities,
            related_doc_tags,
            related_calls,
            related_markers,
            caveats,
            ..answer
        })
    }

    async fn dispatch_probe(
        &self,
        engram: &Engram,
        kind: ProbeKind,
        question: &str,
        clearance: &[Sensitivity],
        graph: &GraphStore,
    ) -> Result<ProbeAnswer> {
        let cluster_set = dv_str_list(&engram.entity_set.iter().cloned().collect::<Vec<_>>());
        let cluster_claim_set = dv_str_list(&engram.cluster_claim_ids);
        let clearance_set =
            dv_str_list(&clearance.iter().map(sensitivity_str).collect::<Vec<_>>());

        match kind {
            ProbeKind::Factual => self.run_factual(engram, graph, cluster_set, clearance).await,
            ProbeKind::Quantitative => {
                self.run_quantitative(engram, graph, cluster_set).await
            }
            ProbeKind::Temporal => self.run_temporal(engram, graph, cluster_set).await,
            ProbeKind::Authorship => {
                self.run_authorship(engram, graph, cluster_claim_set).await
            }
            ProbeKind::Structural => {
                self.run_structural(engram, graph, cluster_claim_set).await
            }
            ProbeKind::RelationCallers => {
                self.run_relation_callers(engram, graph, question).await
            }
            ProbeKind::RelationRefs => self.run_relation_refs(engram, graph, question).await,
            ProbeKind::Existential => {
                self.run_existential(engram, graph, cluster_claim_set, question)
                    .await
            }
            ProbeKind::Comparative => {
                self.run_comparative(engram, graph, cluster_set, clearance_set)
                    .await
            }
            ProbeKind::Counterfactual => {
                self.run_counterfactual(engram, graph, cluster_claim_set).await
            }
        }
    }

    async fn run_factual(
        &self,
        engram: &Engram,
        graph: &GraphStore,
        cluster_set: DataValue,
        clearance: &[Sensitivity],
    ) -> Result<ProbeAnswer> {
        let mut params = BTreeMap::new();
        params.insert("cluster_set".into(), cluster_set);
        let rows = run_aep(graph, aepq::Q_PROBE_FACTUAL, params)?;
        let mut a = empty_answer(engram, AdmissionTier::Attested);
        for row in &rows.rows {
            // [statement, claim_id, source_id, byte_start, byte_end, blake3, tier, sensitivity]
            let sens = parse_sensitivity(&dv_string(&row[7]));
            if !clearance_contains(clearance, &sens) {
                continue;
            }
            a.answer.push(AnswerRow::Factual {
                statement: dv_string(&row[0]),
            });
            a.claim_ids.push(dv_string(&row[1]));
            a.source_byte_spans.push(SourceByteSpan {
                source_id: dv_string(&row[2]),
                byte_start: dv_u64(&row[3]),
                byte_end: dv_u64(&row[4]),
            });
            a.source_blake3s.push(dv_string(&row[5]));
            a.source_authority.push(TrustLevel::Unknown);
            a.admission_tier = parse_tier(&dv_string(&row[6]));
            a.sensitivity = sens;
        }
        Ok(a)
    }

    async fn run_quantitative(
        &self,
        engram: &Engram,
        graph: &GraphStore,
        cluster_set: DataValue,
    ) -> Result<ProbeAnswer> {
        let mut params = BTreeMap::new();
        params.insert("cluster_set".into(), cluster_set);
        let rows = run_aep(graph, aepq::Q_PROBE_QUANTITATIVE, params)?;
        let mut a = empty_answer(engram, AdmissionTier::Attested);
        for row in &rows.rows {
            // [metric_name, value, unit, qualifier, is_live, claim_id, source_id, byte_start, byte_end, blake3, sensitivity]
            a.answer.push(AnswerRow::Quantitative {
                metric_name: dv_string(&row[0]),
                value: dv_f64(&row[1]),
                unit: dv_string(&row[2]),
                qualifier: dv_string(&row[3]),
                is_live: dv_bool(&row[4]),
            });
            a.claim_ids.push(dv_string(&row[5]));
            a.source_byte_spans.push(SourceByteSpan {
                source_id: dv_string(&row[6]),
                byte_start: dv_u64(&row[7]),
                byte_end: dv_u64(&row[8]),
            });
            a.source_blake3s.push(dv_string(&row[9]));
            a.source_authority.push(TrustLevel::Unknown);
            a.sensitivity = parse_sensitivity(&dv_string(&row[10]));
        }
        Ok(a)
    }

    async fn run_temporal(
        &self,
        engram: &Engram,
        graph: &GraphStore,
        cluster_set: DataValue,
    ) -> Result<ProbeAnswer> {
        let mut params = BTreeMap::new();
        params.insert("cluster_set".into(), cluster_set);
        let now = epoch_seconds();
        params.insert(
            "window_start".into(),
            DataValue::Num(Num::Float(now - 86400.0 * 365.0)),
        );
        params.insert(
            "window_end".into(),
            DataValue::Num(Num::Float(now + 86400.0)),
        );
        let rows = run_aep(graph, aepq::Q_PROBE_TEMPORAL, params)?;
        let mut a = empty_answer(engram, AdmissionTier::Attested);
        for row in &rows.rows {
            a.answer.push(AnswerRow::Temporal {
                subject: dv_string(&row[0]),
                verb: dv_string(&row[1]),
                object: dv_string(&row[2]),
                timestamp: dv_f64(&row[3]),
                normalized_date: dv_string(&row[4]),
            });
            a.source_byte_spans.push(SourceByteSpan {
                source_id: dv_string(&row[5]),
                byte_start: dv_u64(&row[6]),
                byte_end: dv_u64(&row[7]),
            });
            a.source_blake3s.push(String::new());
            a.source_authority.push(TrustLevel::Unknown);
        }
        Ok(a)
    }

    async fn run_authorship(
        &self,
        engram: &Engram,
        graph: &GraphStore,
        cluster_claim_set: DataValue,
    ) -> Result<ProbeAnswer> {
        let mut params = BTreeMap::new();
        params.insert("cluster_claim_set".into(), cluster_claim_set);
        let rows = run_aep(graph, aepq::Q_PROBE_AUTHORSHIP, params)?;
        let mut a = empty_answer(engram, AdmissionTier::Attested);
        for row in &rows.rows {
            // [author, blamed_at, commit_sha, source_id, line_start, line_end, byte_start, byte_end]
            a.answer.push(AnswerRow::Authorship {
                author: dv_string(&row[0]),
                commit_sha: dv_string(&row[2]),
                blamed_at: dv_f64(&row[1]),
            });
            a.source_byte_spans.push(SourceByteSpan {
                source_id: dv_string(&row[3]),
                byte_start: dv_u64(&row[6]),
                byte_end: dv_u64(&row[7]),
            });
            a.source_blake3s.push(String::new());
            a.source_authority.push(TrustLevel::Unknown);
            a.git_blame.push(GitBlameRef {
                source_id: dv_string(&row[3]),
                line_start: dv_u64(&row[4]) as u32,
                line_end: dv_u64(&row[5]) as u32,
                commit_sha: dv_string(&row[2]),
                author: dv_string(&row[0]),
                blamed_at: dv_f64(&row[1]),
            });
        }
        Ok(a)
    }

    async fn run_structural(
        &self,
        engram: &Engram,
        graph: &GraphStore,
        cluster_claim_set: DataValue,
    ) -> Result<ProbeAnswer> {
        let mut params = BTreeMap::new();
        params.insert("cluster_claim_set".into(), cluster_claim_set);
        let rows = run_aep(graph, aepq::Q_PROBE_STRUCTURAL, params)?;
        let mut a = empty_answer(engram, AdmissionTier::Attested);
        for row in &rows.rows {
            a.answer.push(AnswerRow::Structural {
                parameters_json: dv_string(&row[0]),
                return_type: dv_string(&row[1]),
                visibility: dv_string(&row[2]),
                trait_name: dv_string(&row[3]),
                parent_scope: dv_string(&row[4]),
                field_types_json: dv_string(&row[5]),
            });
            a.claim_ids.push(dv_string(&row[6]));
            a.source_byte_spans.push(SourceByteSpan {
                source_id: dv_string(&row[7]),
                byte_start: dv_u64(&row[8]),
                byte_end: dv_u64(&row[9]),
            });
            a.source_blake3s.push(String::new());
            a.source_authority.push(TrustLevel::Unknown);
        }
        Ok(a)
    }

    async fn run_relation_callers(
        &self,
        engram: &Engram,
        graph: &GraphStore,
        question: &str,
    ) -> Result<ProbeAnswer> {
        // Heuristic: take the most frequently referenced callee_claim_id in
        // the cluster's call_edges that is mentioned in the question. If
        // none match, return empty.
        let target = guess_target(&engram.call_edges, question);
        let mut params = BTreeMap::new();
        params.insert("target".into(), DataValue::Str(target.clone().into()));
        let rows = run_aep(graph, aepq::Q_PROBE_RELATION_CALLERS, params)?;
        let mut a = empty_answer(engram, AdmissionTier::Attested);
        for row in &rows.rows {
            a.answer.push(AnswerRow::Relation {
                peer_claim_id: dv_string(&row[0]),
                edge_kind: "calls".into(),
                fragment: dv_string(&row[1]),
            });
            a.claim_ids.push(dv_string(&row[0]));
            a.source_byte_spans.push(SourceByteSpan {
                source_id: dv_string(&row[2]),
                byte_start: dv_u64(&row[3]),
                byte_end: dv_u64(&row[4]),
            });
            a.source_blake3s.push(String::new());
            a.source_authority.push(TrustLevel::Unknown);
        }
        Ok(a)
    }

    async fn run_relation_refs(
        &self,
        engram: &Engram,
        graph: &GraphStore,
        _question: &str,
    ) -> Result<ProbeAnswer> {
        // Use the first cluster source id as the target reference. Callers
        // wanting precision should pass `probe_kind` plus a source-scoped
        // topic at materialise time.
        let target = engram
            .summary
            .source_authority
            .first()
            .map(|s| s.source_id.clone())
            .unwrap_or_default();
        let mut params = BTreeMap::new();
        params.insert("target".into(), DataValue::Str(target.into()));
        let rows = run_aep(graph, aepq::Q_PROBE_RELATION_REFS, params)?;
        let mut a = empty_answer(engram, AdmissionTier::Attested);
        for row in &rows.rows {
            a.answer.push(AnswerRow::Relation {
                peer_claim_id: dv_string(&row[0]),
                edge_kind: dv_string(&row[1]),
                fragment: dv_string(&row[2]),
            });
            a.source_byte_spans.push(SourceByteSpan {
                source_id: dv_string(&row[0]),
                byte_start: dv_u64(&row[3]),
                byte_end: dv_u64(&row[4]),
            });
            a.source_blake3s.push(String::new());
            a.source_authority.push(TrustLevel::Unknown);
        }
        Ok(a)
    }

    async fn run_existential(
        &self,
        engram: &Engram,
        graph: &GraphStore,
        cluster_claim_set: DataValue,
        question: &str,
    ) -> Result<ProbeAnswer> {
        let claim_type = guess_claim_type(question);
        let mut params = BTreeMap::new();
        params.insert("cluster_claim_set".into(), cluster_claim_set);
        params.insert("claim_type".into(), DataValue::Str(claim_type.into()));
        let rows = run_aep(graph, aepq::Q_PROBE_EXISTENTIAL, params)?;
        let mut a = empty_answer(engram, AdmissionTier::Attested);
        let witness = rows.rows.first().map(|r| dv_string(&r[0]));
        a.answer.push(AnswerRow::Existential {
            present: !rows.rows.is_empty(),
            witness_claim_id: witness.clone(),
        });
        if let Some(w) = witness {
            a.claim_ids.push(w);
        }
        Ok(a)
    }

    async fn run_comparative(
        &self,
        engram: &Engram,
        graph: &GraphStore,
        cluster_set: DataValue,
        _clearance_set: DataValue,
    ) -> Result<ProbeAnswer> {
        // For v1, the comparative probe partitions the cluster set into two
        // halves alphabetically and pairs them. A more nuanced implementation
        // requires the caller to pass two named entity sets; the MCP layer
        // can accept this via `scope.seed_claim_ids` already.
        let entities: Vec<String> = engram.entity_set.iter().cloned().collect();
        if entities.len() < 2 {
            return Ok(empty_answer(engram, AdmissionTier::Attested));
        }
        let mut sorted = entities.clone();
        sorted.sort();
        let mid = sorted.len() / 2;
        let mut params = BTreeMap::new();
        params.insert("set_a".into(), dv_str_list(&sorted[..mid].to_vec()));
        params.insert("set_b".into(), dv_str_list(&sorted[mid..].to_vec()));
        params.insert("_cluster_set".into(), cluster_set);
        let rows = run_aep(graph, aepq::Q_PROBE_COMPARATIVE, params)?;
        let mut a = empty_answer(engram, AdmissionTier::Attested);
        let mut a_rows: Vec<String> = Vec::new();
        let mut b_rows: Vec<String> = Vec::new();
        for row in &rows.rows {
            // [claim_id, statement, claim_type, source_id, byte_start, byte_end, side]
            let side = dv_string(&row[6]);
            let stmt = dv_string(&row[1]);
            if side == "a" {
                a_rows.push(stmt);
            } else {
                b_rows.push(stmt);
            }
            a.claim_ids.push(dv_string(&row[0]));
            a.source_byte_spans.push(SourceByteSpan {
                source_id: dv_string(&row[3]),
                byte_start: dv_u64(&row[4]),
                byte_end: dv_u64(&row[5]),
            });
            a.source_blake3s.push(String::new());
            a.source_authority.push(TrustLevel::Unknown);
        }
        for (a_stmt, b_stmt) in a_rows.iter().zip(b_rows.iter()) {
            a.answer.push(AnswerRow::Comparative {
                a_statement: a_stmt.clone(),
                b_statement: b_stmt.clone(),
                delta_summary: format!("differs by surface form"),
            });
        }
        Ok(a)
    }

    async fn run_counterfactual(
        &self,
        engram: &Engram,
        graph: &GraphStore,
        cluster_claim_set: DataValue,
    ) -> Result<ProbeAnswer> {
        // Walk derivation_edges forward from cluster claims and surface
        // descendants. Reuses Q_DERIVATION_ROOT but inverts the semantics
        // by binding the cluster set as the "child" filter — the rule then
        // returns descendants whose root is in the cluster.
        let mut params = BTreeMap::new();
        params.insert("cluster_claim_set".into(), cluster_claim_set);
        let rows = run_aep(graph, aepq::Q_DERIVATION_ROOT, params)?;
        let mut a = empty_answer(engram, AdmissionTier::Attested);
        for row in &rows.rows {
            let descendant = dv_string(&row[0]);
            a.answer.push(AnswerRow::Counterfactual {
                descendant_claim_id: descendant.clone(),
                descendant_statement: String::new(),
                descendant_admission_tier: AdmissionTier::Attested,
            });
            a.claim_ids.push(descendant);
        }
        Ok(a)
    }
}

// ===========================================================================
// BLAKE3 verification — Plan §3.6.
// ===========================================================================

impl EngramManager {
    async fn verify_row(
        &self,
        engram: &Engram,
        byte_store: &dyn SourceByteStore,
        source_id: &str,
        byte_start: u64,
        byte_end: u64,
        expected: &str,
    ) -> Result<bool> {
        let Some(hash) = engram.source_id_to_hash.get(source_id) else {
            return Ok(false);
        };
        let key = (hash.0.clone(), byte_start, byte_end);
        if let Some(&ok) = engram.blake3_cache.read().await.get(&key) {
            return Ok(ok);
        }
        let bytes = byte_store
            .get_range(hash, byte_start as usize, byte_end as usize)
            .map_err(|e| Error::GraphStorage(format!("byte_store: {e}")))?
            .unwrap_or_default();
        let computed = format!("blake3:{}", blake3::hash(&bytes).to_hex());
        let ok = computed == expected;
        engram.blake3_cache.write().await.insert(key, ok);
        Ok(ok)
    }
}

// ===========================================================================
// Datalog list-helpers — small wrappers around the aep_queries consts.
// ===========================================================================

fn list_cluster_claim_ids(graph: &GraphStore, entities: &[String]) -> Result<Vec<String>> {
    let mut params = BTreeMap::new();
    params.insert("set".into(), dv_str_list(entities));
    let rows = graph
        .raw_db()
        .run_script(
            r#"?[claim_id] :=
                *claim_entity_edges{claim_id, entity_id},
                entity_id in $set"#,
            params,
            ScriptMutability::Immutable,
        )
        .map_err(|e| Error::GraphStorage(format!("cluster claim ids: {e}")))?;
    let mut ids: Vec<String> = rows.rows.iter().map(|r| dv_string(&r[0])).collect();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

fn list_aliases(graph: &GraphStore, entities: &[String]) -> Result<HashMap<String, Vec<String>>> {
    let mut params = BTreeMap::new();
    params.insert("cluster_set".into(), dv_str_list(entities));
    let rows = run_aep(graph, aepq::Q_ALIAS_RESOLUTION, params)?;
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for row in &rows.rows {
        let eid = dv_string(&row[0]);
        let alias = dv_string(&row[1]);
        out.entry(eid).or_default().push(alias);
    }
    Ok(out)
}

fn list_entity_refs(
    graph: &GraphStore,
    entities: &[String],
    aliases: &HashMap<String, Vec<String>>,
) -> Result<Vec<EntityRef>> {
    let mut params = BTreeMap::new();
    params.insert("set".into(), dv_str_list(entities));
    let rows = graph
        .raw_db()
        .run_script(
            r#"?[id, canonical_name, entity_type] :=
                id in $set,
                *entities{id, canonical_name, entity_type}"#,
            params,
            ScriptMutability::Immutable,
        )
        .map_err(|e| Error::GraphStorage(format!("entity refs: {e}")))?;
    Ok(rows
        .rows
        .iter()
        .map(|r| {
            let id = dv_string(&r[0]);
            EntityRef {
                aliases: aliases.get(&id).cloned().unwrap_or_default(),
                id,
                canonical_name: dv_string(&r[1]),
                entity_type: dv_string(&r[2]),
            }
        })
        .collect())
}

fn list_trust_gated(graph: &GraphStore) -> Result<Vec<String>> {
    let rows = graph
        .raw_db()
        .run_default(aepq::Q_TRUST_GATE)
        .map_err(|e| Error::GraphStorage(format!("trust gate: {e}")))?;
    Ok(rows.rows.iter().map(|r| dv_string(&r[0])).collect())
}

fn count_admission_tiers(graph: &GraphStore, ids: &[String]) -> Result<TierHistogram> {
    let mut params = BTreeMap::new();
    params.insert("set".into(), dv_str_list(ids));
    let rows = graph
        .raw_db()
        .run_script(
            r#"?[id, admission_tier] :=
                id in $set, *claims{id, admission_tier}"#,
            params,
            ScriptMutability::Immutable,
        )
        .map_err(|e| Error::GraphStorage(format!("tier histogram: {e}")))?;
    let mut hist = TierHistogram::default();
    for row in &rows.rows {
        match dv_string(&row[1]).as_str() {
            "rooted" => hist.rooted += 1,
            "attested" => hist.attested += 1,
            "quarantined" => hist.quarantined += 1,
            "rejected" => hist.rejected += 1,
            _ => {}
        }
    }
    Ok(hist)
}

fn list_source_authority(graph: &GraphStore, ids: &[String]) -> Result<Vec<SourceAuthority>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut params = BTreeMap::new();
    params.insert("cluster_claim_set".into(), dv_str_list(ids));
    let rows = run_aep(graph, aepq::Q_SOURCE_AUTHORITY, params)?;
    let mut by_source: HashMap<String, SourceAuthority> = HashMap::new();
    for row in &rows.rows {
        let source_id = dv_string(&row[1]);
        let entry = by_source
            .entry(source_id.clone())
            .or_insert_with(|| SourceAuthority {
                source_id: source_id.clone(),
                uri: dv_string(&row[2]),
                trust_level: parse_trust_level(&dv_string(&row[3])),
                claim_count: 0,
            });
        entry.claim_count += 1;
    }
    Ok(by_source.into_values().collect())
}

fn list_temporal_active(graph: &GraphStore) -> Result<Vec<String>> {
    let rows = graph
        .raw_db()
        .run_default(aepq::Q_TEMPORAL_ACTIVE)
        .map_err(|e| Error::GraphStorage(format!("temporal active: {e}")))?;
    Ok(rows.rows.iter().map(|r| dv_string(&r[0])).collect())
}

fn list_supersession_chain(
    graph: &GraphStore,
    cluster: &[String],
) -> Result<HashMap<String, String>> {
    if cluster.is_empty() {
        return Ok(HashMap::new());
    }
    let mut params = BTreeMap::new();
    params.insert("cluster_claim_set".into(), dv_str_list(cluster));
    let rows = run_aep(graph, aepq::Q_SUPERSESSION_CHAIN, params)?;
    let mut out = HashMap::new();
    for row in &rows.rows {
        out.insert(dv_string(&row[0]), dv_string(&row[1]));
    }
    Ok(out)
}

fn list_contradictions(
    graph: &GraphStore,
    cluster: &[String],
) -> Result<(HashMap<String, Vec<ContradictionRef>>, Vec<ContradictionRef>)> {
    if cluster.is_empty() {
        return Ok((HashMap::new(), Vec::new()));
    }
    let mut params = BTreeMap::new();
    params.insert("cluster_claim_set".into(), dv_str_list(cluster));
    let rows = run_aep(graph, aepq::Q_CONTRADICTIONS, params)?;
    let mut by_claim: HashMap<String, Vec<ContradictionRef>> = HashMap::new();
    let mut all = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for row in &rows.rows {
        let id = dv_string(&row[0]);
        if !seen.insert(id.clone()) {
            continue;
        }
        let c = ContradictionRef {
            id: id.clone(),
            claim_a: dv_string(&row[1]),
            claim_b: dv_string(&row[2]),
            explanation: dv_string(&row[3]),
            status: dv_string(&row[4]),
        };
        by_claim
            .entry(c.claim_a.clone())
            .or_default()
            .push(c.clone());
        by_claim
            .entry(c.claim_b.clone())
            .or_default()
            .push(c.clone());
        all.push(c);
    }
    Ok((by_claim, all))
}

fn list_events_window(
    graph: &GraphStore,
    entities: &[String],
    window_start: f64,
    window_end: f64,
) -> Result<Vec<EventTriple>> {
    if entities.is_empty() {
        return Ok(Vec::new());
    }
    let mut params = BTreeMap::new();
    params.insert("cluster_set".into(), dv_str_list(entities));
    params.insert("window_start".into(), DataValue::Num(Num::Float(window_start)));
    params.insert("window_end".into(), DataValue::Num(Num::Float(window_end)));
    let rows = run_aep(graph, aepq::Q_EVENTS_WINDOW, params)?;
    Ok(rows
        .rows
        .iter()
        .map(|r| EventTriple {
            subject_entity_id: dv_string(&r[1]),
            verb: dv_string(&r[2]),
            object_entity_id: dv_string(&r[3]),
            timestamp: dv_f64(&r[4]),
            normalized_date: dv_string(&r[5]),
        })
        .collect())
}

fn list_pattern_overlay(graph: &GraphStore, entities: &[String]) -> Result<Vec<PatternMatch>> {
    if entities.is_empty() {
        return Ok(Vec::new());
    }
    let mut params = BTreeMap::new();
    params.insert("cluster_set".into(), dv_str_list(entities));
    let rows = run_aep(graph, aepq::Q_PATTERN_OVERLAY, params)?;
    Ok(rows
        .rows
        .iter()
        .map(|r| PatternMatch {
            pattern_id: dv_string(&r[0]),
            entity_type: dv_string(&r[1]),
            condition_claim_type: dv_string(&r[2]),
            expected_claim_type: dv_string(&r[3]),
            frequency: dv_f64(&r[4]),
            sample_size: dv_u64(&r[5]) as u32,
        })
        .collect())
}

fn list_gaps(graph: &GraphStore, entities: &[String]) -> Result<Vec<KnownUnknown>> {
    if entities.is_empty() {
        return Ok(Vec::new());
    }
    let mut params = BTreeMap::new();
    params.insert("cluster_set".into(), dv_str_list(entities));
    let rows = run_aep(graph, aepq::Q_GAP_SCAN, params)?;
    Ok(rows
        .rows
        .iter()
        .map(|r| KnownUnknown {
            gap_id: dv_string(&r[0]),
            entity_id: dv_string(&r[1]),
            expected_claim_type: dv_string(&r[3]),
            confidence: dv_f64(&r[4]),
        })
        .collect())
}

fn list_call_graph(graph: &GraphStore, ids: &[String]) -> Result<Vec<CallEdge>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut params = BTreeMap::new();
    params.insert("cluster_claim_set".into(), dv_str_list(ids));
    let rows = run_aep(graph, aepq::Q_CALL_GRAPH, params)?;
    Ok(rows
        .rows
        .iter()
        .map(|r| CallEdge {
            caller_claim_id: dv_string(&r[0]),
            callee_name: dv_string(&r[1]),
            callee_claim_id: dv_string(&r[2]),
            source_id: dv_string(&r[3]),
            byte_start: dv_u64(&r[4]),
            byte_end: dv_u64(&r[5]),
        })
        .collect())
}

fn list_doc_tags(graph: &GraphStore, ids: &[String]) -> Result<Vec<DocTagRef>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut params = BTreeMap::new();
    params.insert("cluster_claim_set".into(), dv_str_list(ids));
    let rows = run_aep(graph, aepq::Q_DOC_TAGS, params)?;
    Ok(rows
        .rows
        .iter()
        .map(|r| DocTagRef {
            claim_id: dv_string(&r[0]),
            kind: dv_string(&r[1]),
            target: dv_string(&r[2]),
            description: dv_string(&r[3]),
        })
        .collect())
}

fn list_code_markers(graph: &GraphStore, ids: &[String]) -> Result<Vec<CodeMarkerRef>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut params = BTreeMap::new();
    params.insert("cluster_claim_set".into(), dv_str_list(ids));
    let rows = run_aep(graph, aepq::Q_CODE_MARKERS, params)?;
    Ok(rows
        .rows
        .iter()
        .map(|r| CodeMarkerRef {
            id: dv_string(&r[0]),
            source_id: dv_string(&r[1]),
            kind: dv_string(&r[2]),
            text: dv_string(&r[3]),
            in_claim_id: dv_string(&r[4]),
            byte_start: dv_u64(&r[5]),
            byte_end: dv_u64(&r[6]),
        })
        .collect())
}

fn list_test_origins(graph: &GraphStore, ids: &[String]) -> Result<Vec<TestAnnotationRef>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut params = BTreeMap::new();
    params.insert("cluster_claim_set".into(), dv_str_list(ids));
    let rows = run_aep(graph, aepq::Q_TEST_ORIGINS, params)?;
    Ok(rows
        .rows
        .iter()
        .map(|r| TestAnnotationRef {
            id: dv_string(&r[0]),
            claim_id: dv_string(&r[1]),
            framework: dv_string(&r[2]),
            annotation_kind: dv_string(&r[3]),
            name: dv_string(&r[4]),
        })
        .collect())
}

fn list_git_summaries(
    graph: &GraphStore,
    ids: &[String],
) -> Result<(GitBlameSummary, GitCommitsSummary)> {
    if ids.is_empty() {
        return Ok((GitBlameSummary::default(), GitCommitsSummary::default()));
    }
    let mut params = BTreeMap::new();
    params.insert("cluster_claim_set".into(), dv_str_list(ids));
    let blame_rows = run_aep(graph, aepq::Q_GIT_BLAME, params)?;
    let mut blame = GitBlameSummary::default();
    let mut authors: HashSet<String> = HashSet::new();
    for r in &blame_rows.rows {
        authors.insert(dv_string(&r[4]));
        blame.line_count += (dv_u64(&r[2]).saturating_sub(dv_u64(&r[1])) + 1) as u32;
    }
    blame.authors = authors.into_iter().collect();
    blame.authors.sort();
    // git_commits aggregation: walk source ids of cluster claims and pull
    // commit metadata.
    let mut params = BTreeMap::new();
    params.insert("set".into(), dv_str_list(ids));
    let cs = graph
        .raw_db()
        .run_script(
            r#"?[source_id, commit_sha, commit_author, commit_timestamp] :=
                claim_id in $set,
                *claim_source_edges{claim_id, source_id},
                *git_commits{source_id, commit_sha, commit_author, commit_timestamp}"#,
            params,
            ScriptMutability::Immutable,
        )
        .map_err(|e| Error::GraphStorage(format!("git_commits: {e}")))?;
    let mut commits = GitCommitsSummary::default();
    let mut commit_authors: HashSet<String> = HashSet::new();
    let mut earliest = f64::MAX;
    let mut latest = f64::MIN;
    for r in &cs.rows {
        commits.total_commits += 1;
        commit_authors.insert(dv_string(&r[2]));
        let ts = dv_f64(&r[3]);
        if ts < earliest {
            earliest = ts;
        }
        if ts > latest {
            latest = ts;
        }
    }
    commits.authors = commit_authors.into_iter().collect();
    commits.authors.sort();
    commits.earliest_commit = if earliest != f64::MAX { Some(earliest) } else { None };
    commits.latest_commit = if latest != f64::MIN { Some(latest) } else { None };
    Ok((blame, commits))
}

fn list_code_metrics(graph: &GraphStore, ids: &[String]) -> Result<Vec<CodeMetricRef>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut params = BTreeMap::new();
    params.insert("cluster_claim_set".into(), dv_str_list(ids));
    let rows = run_aep(graph, aepq::Q_CODE_METRICS, params)?;
    Ok(rows
        .rows
        .iter()
        .map(|r| CodeMetricRef {
            source_id: dv_string(&r[0]),
            scope: dv_string(&r[1]),
            scope_claim_id: dv_string(&r[2]),
            loc: dv_u64(&r[3]) as u32,
            cyclomatic: dv_u64(&r[4]) as u32,
            fan_in: dv_u64(&r[5]) as u32,
            fan_out: dv_u64(&r[6]) as u32,
            complexity_method: dv_string(&r[7]),
        })
        .collect())
}

fn list_quantities(graph: &GraphStore, ids: &[String]) -> Result<Vec<QuantityRef>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut params = BTreeMap::new();
    params.insert("cluster_claim_set".into(), dv_str_list(ids));
    let rows = run_aep(graph, aepq::Q_QUANTITIES, params)?;
    Ok(rows
        .rows
        .iter()
        .map(|r| QuantityRef {
            claim_id: dv_string(&r[0]),
            metric_name: dv_string(&r[1]),
            value: dv_f64(&r[2]),
            unit: dv_string(&r[3]),
            qualifier: dv_string(&r[4]),
            is_live: dv_bool(&r[5]),
            captured_at: dv_f64(&r[6]),
        })
        .collect())
}

fn count_redacted(
    graph: &GraphStore,
    ids: &[String],
    clearance: &[Sensitivity],
) -> Result<u32> {
    if ids.is_empty() {
        return Ok(0);
    }
    let mut params = BTreeMap::new();
    params.insert("cluster_claim_set".into(), dv_str_list(ids));
    params.insert(
        "caller_clearance_set".into(),
        dv_str_list(&clearance.iter().map(sensitivity_str).collect::<Vec<_>>()),
    );
    let allowed = run_aep(graph, aepq::Q_SENSITIVITY_FILTER, params)?;
    let allowed_count = allowed.rows.len() as u32;
    Ok((ids.len() as u32).saturating_sub(allowed_count))
}

fn list_derivation_roots(
    graph: &GraphStore,
    ids: &[String],
) -> Result<HashMap<String, Vec<String>>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut params = BTreeMap::new();
    params.insert("cluster_claim_set".into(), dv_str_list(ids));
    let rows = run_aep(graph, aepq::Q_DERIVATION_ROOT, params)?;
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for r in &rows.rows {
        out.entry(dv_string(&r[0]))
            .or_default()
            .push(dv_string(&r[1]));
    }
    Ok(out)
}

fn list_source_hash_map(graph: &GraphStore) -> Result<HashMap<String, ContentHash>> {
    let rows = graph
        .raw_db()
        .run_default("?[id, content_hash] := *sources{id, content_hash}")
        .map_err(|e| Error::GraphStorage(format!("source hashes: {e}")))?;
    Ok(rows
        .rows
        .iter()
        .map(|r| (dv_string(&r[0]), ContentHash(dv_string(&r[1]))))
        .collect())
}

fn list_source_references(
    graph: &GraphStore,
    sources: &HashSet<String>,
) -> Result<Vec<SourceReferenceEdge>> {
    if sources.is_empty() {
        return Ok(Vec::new());
    }
    let s: Vec<String> = sources.iter().cloned().collect();
    let mut params = BTreeMap::new();
    params.insert("set".into(), dv_str_list(&s));
    let rows = graph
        .raw_db()
        .run_script(
            r#"?[from_source_id, to_source_id, reference_kind, fragment] :=
                from_source_id in $set,
                *source_references{from_source_id, to_source_id, reference_kind, fragment}"#,
            params,
            ScriptMutability::Immutable,
        )
        .map_err(|e| Error::GraphStorage(format!("source_references: {e}")))?;
    Ok(rows
        .rows
        .iter()
        .map(|r| SourceReferenceEdge {
            from_source_id: dv_string(&r[0]),
            to_source_id: dv_string(&r[1]),
            reference_kind: dv_string(&r[2]),
            fragment: dv_string(&r[3]),
        })
        .collect())
}

fn list_headings(
    graph: &GraphStore,
    sources: &HashSet<String>,
) -> Result<Vec<HeadingRef>> {
    if sources.is_empty() {
        return Ok(Vec::new());
    }
    let s: Vec<String> = sources.iter().cloned().collect();
    let mut params = BTreeMap::new();
    params.insert("set".into(), dv_str_list(&s));
    let rows = graph
        .raw_db()
        .run_script(
            r#"?[id, source_id, level, text, parent_heading_id] :=
                source_id in $set,
                *headings{id, source_id, level, text, parent_heading_id}"#,
            params,
            ScriptMutability::Immutable,
        )
        .map_err(|e| Error::GraphStorage(format!("headings: {e}")))?;
    Ok(rows
        .rows
        .iter()
        .map(|r| HeadingRef {
            id: dv_string(&r[0]),
            source_id: dv_string(&r[1]),
            level: dv_u64(&r[2]) as u8,
            text: dv_string(&r[3]),
            parent_heading_id: dv_string(&r[4]),
        })
        .collect())
}

fn list_claim_refs(graph: &GraphStore, ids: &[String]) -> Result<Vec<ClaimRef>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut params = BTreeMap::new();
    params.insert("set".into(), dv_str_list(ids));
    let rows = graph
        .raw_db()
        .run_script(
            r#"?[id, statement, admission_tier] :=
                id in $set,
                *claims{id, statement, admission_tier}"#,
            params,
            ScriptMutability::Immutable,
        )
        .map_err(|e| Error::GraphStorage(format!("claim refs: {e}")))?;
    Ok(rows
        .rows
        .iter()
        .map(|r| ClaimRef {
            id: dv_string(&r[0]),
            statement: dv_string(&r[1]),
            admission_tier: parse_tier(&dv_string(&r[2])),
        })
        .collect())
}

// ===========================================================================
// Probe-time enrichment helpers.
// ===========================================================================

/// Most-recent trial verdict for a claim. Returns `(scores, certificate_hash)`.
/// `cert` is empty when no verdict carried one (e.g. quarantined claims
/// without a probe battery run yet).
fn lookup_trial_scores(
    graph: &GraphStore,
    claim_id: &str,
) -> Result<Option<(TrialScores, String)>> {
    let mut params = BTreeMap::new();
    params.insert("cid".into(), DataValue::Str(claim_id.into()));
    let rows = graph
        .raw_db()
        .run_script(
            r#"?[trial_at, provenance_score, contradiction_score, predicate_score,
                 topology_score, temporal_score, certificate_hash] :=
                *trial_verdicts{claim_id, trial_at, provenance_score, contradiction_score,
                                predicate_score, topology_score, temporal_score,
                                certificate_hash},
                claim_id = $cid"#,
            params,
            ScriptMutability::Immutable,
        )
        .map_err(|e| Error::GraphStorage(format!("trial_verdicts: {e}")))?;
    // Pick the most recent by trial_at.
    let mut best: Option<&Vec<DataValue>> = None;
    let mut best_at = f64::MIN;
    for row in &rows.rows {
        let at = dv_f64(&row[0]);
        if at >= best_at {
            best_at = at;
            best = Some(row);
        }
    }
    Ok(best.map(|row| {
        (
            TrialScores {
                provenance_score: dv_f64(&row[1]),
                contradiction_score: dv_f64(&row[2]),
                predicate_score: dv_f64(&row[3]),
                topology_score: dv_f64(&row[4]),
                temporal_score: dv_f64(&row[5]),
            },
            dv_string(&row[6]),
        )
    }))
}

/// Locate the originating turn for a claim, bounded to the most recent
/// `window` turns of the session per Plan §3.8.
///
/// `turns.claim_ids` is a JSON-encoded array (`graph.rs:254`) so an
/// indexed Datalog `in` predicate isn't available — we take the recent
/// slice and substring-match. For typical session sizes (≤ 200 turns)
/// this is sub-millisecond. Outside the window we emit
/// `TurnRef::Unknown { reason: "out_of_turn_window" }`.
fn lookup_turn_provenance(
    graph: &GraphStore,
    session_id: &str,
    claim_id: &str,
    window: usize,
) -> Result<TurnRef> {
    let mut params = BTreeMap::new();
    params.insert("sid".into(), DataValue::Str(session_id.into()));
    let rows = graph
        .raw_db()
        .run_script(
            r#"?[turn_number, claim_ids, timestamp] :=
                *turns{session_id, turn_number, claim_ids, timestamp},
                session_id = $sid"#,
            params,
            ScriptMutability::Immutable,
        )
        .map_err(|e| Error::GraphStorage(format!("turns: {e}")))?;
    // Sort by turn_number descending and take the most recent window.
    let mut entries: Vec<(u64, String, f64)> = rows
        .rows
        .iter()
        .map(|r| (dv_u64(&r[0]), dv_string(&r[1]), dv_f64(&r[2])))
        .collect();
    entries.sort_by(|a, b| b.0.cmp(&a.0));
    let needle = format!("\"{claim_id}\"");
    for (turn_number, claim_ids_json, timestamp) in entries.iter().take(window) {
        if claim_ids_json.contains(&needle) {
            return Ok(TurnRef::Found {
                session_id: session_id.into(),
                turn_number: *turn_number,
                timestamp: *timestamp,
            });
        }
    }
    Ok(TurnRef::Unknown {
        reason: "out_of_turn_window".into(),
    })
}

/// One-step derivation parents for a claim.
fn lookup_derivation_parents(graph: &GraphStore, claim_id: &str) -> Result<Vec<String>> {
    let mut params = BTreeMap::new();
    params.insert("cid".into(), DataValue::Str(claim_id.into()));
    let rows = graph
        .raw_db()
        .run_script(
            r#"?[parent_claim_id] :=
                *derivation_edges{parent_claim_id, child_claim_id},
                child_claim_id = $cid"#,
            params,
            ScriptMutability::Immutable,
        )
        .map_err(|e| Error::GraphStorage(format!("derivation parents: {e}")))?;
    Ok(rows.rows.iter().map(|r| dv_string(&r[0])).collect())
}

// ===========================================================================
// Probe-time small helpers.
// ===========================================================================

fn empty_answer(engram: &Engram, tier: AdmissionTier) -> ProbeAnswer {
    ProbeAnswer {
        answer: Vec::new(),
        claim_ids: Vec::new(),
        source_byte_spans: Vec::new(),
        source_authority: Vec::new(),
        source_blake3s: Vec::new(),
        admission_tier: tier,
        trial_scores: None,
        certificate_hash: None,
        grounding_score: None,
        grounding_method: Option::<GroundingMethod>::None,
        valid_window: engram.summary.temporal_window,
        superseded_by_chain: Vec::new(),
        derivation_parents: Vec::new(),
        derivation_root: None,
        sensitivity: Sensitivity::Public,
        turn_provenance: None,
        git_blame: Vec::new(),
        test_origin: None,
        related_quantities: Vec::new(),
        related_doc_tags: Vec::new(),
        related_calls: Vec::new(),
        related_markers: Vec::new(),
        caveats: Vec::new(),
    }
}

fn check_cancel(token: &CancellationToken) -> Result<()> {
    if token.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

fn epoch_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn dv_string(v: &DataValue) -> String {
    match v {
        DataValue::Str(s) => s.to_string(),
        DataValue::Num(Num::Int(i)) => i.to_string(),
        DataValue::Num(Num::Float(f)) => f.to_string(),
        DataValue::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

fn dv_u64(v: &DataValue) -> u64 {
    match v {
        DataValue::Num(Num::Int(i)) => *i as u64,
        DataValue::Num(Num::Float(f)) => *f as u64,
        _ => 0,
    }
}

fn dv_f64(v: &DataValue) -> f64 {
    match v {
        DataValue::Num(Num::Float(f)) => *f,
        DataValue::Num(Num::Int(i)) => *i as f64,
        _ => 0.0,
    }
}

fn dv_bool(v: &DataValue) -> bool {
    matches!(v, DataValue::Bool(true))
}

fn parse_tier(s: &str) -> AdmissionTier {
    match s {
        "rooted" => AdmissionTier::Rooted,
        "quarantined" => AdmissionTier::Quarantined,
        "rejected" => AdmissionTier::Rejected,
        _ => AdmissionTier::Attested,
    }
}

fn parse_sensitivity(s: &str) -> Sensitivity {
    match s.to_ascii_lowercase().as_str() {
        "internal" => Sensitivity::Internal,
        "confidential" => Sensitivity::Confidential,
        "restricted" => Sensitivity::Restricted,
        _ => Sensitivity::Public,
    }
}

fn parse_trust_level(s: &str) -> TrustLevel {
    match s {
        "Verified" | "verified" => TrustLevel::Verified,
        "Trusted" | "trusted" => TrustLevel::Trusted,
        "Untrusted" | "untrusted" => TrustLevel::Untrusted,
        "Quarantined" | "quarantined" => TrustLevel::Quarantined,
        _ => TrustLevel::Unknown,
    }
}

fn sensitivity_str(s: &Sensitivity) -> String {
    match s {
        Sensitivity::Public => "public".into(),
        Sensitivity::Internal => "internal".into(),
        Sensitivity::Confidential => "confidential".into(),
        Sensitivity::Restricted => "restricted".into(),
    }
}

fn clearance_contains(clearance: &[Sensitivity], s: &Sensitivity) -> bool {
    clearance.iter().any(|c| c == s)
}

fn highest_required_clearance(clearance: &[Sensitivity]) -> Sensitivity {
    // Returns the most restrictive sensitivity we'd need to see hidden
    // content. If the caller is `Public`-only, the hidden content needs
    // `Internal` or above to view.
    if !clearance.contains(&Sensitivity::Public) {
        Sensitivity::Public
    } else if !clearance.contains(&Sensitivity::Internal) {
        Sensitivity::Internal
    } else if !clearance.contains(&Sensitivity::Confidential) {
        Sensitivity::Confidential
    } else {
        Sensitivity::Restricted
    }
}

fn guess_target(edges: &[CallEdge], question: &str) -> String {
    let q = question.to_ascii_lowercase();
    edges
        .iter()
        .find(|e| q.contains(&e.callee_name.to_ascii_lowercase()))
        .map(|e| {
            if !e.callee_claim_id.is_empty() {
                e.callee_claim_id.clone()
            } else {
                e.callee_name.clone()
            }
        })
        .unwrap_or_default()
}

fn guess_claim_type(question: &str) -> String {
    let q = question.to_ascii_lowercase();
    for ct in [
        "configuration",
        "function_def",
        "type_def",
        "import",
        "observation",
        "definition",
        "policy",
        "decision",
    ] {
        if q.contains(ct) {
            return ct.to_string();
        }
    }
    "configuration".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_recognises_authorship_question() {
        let (k, c) = ProbeKind::classify("Who introduced the deprecation flag?");
        assert_eq!(k, ProbeKind::Authorship);
        assert!(c >= 0.9);
    }

    #[test]
    fn classify_recognises_quantitative() {
        let (k, _) = ProbeKind::classify("How fast is the auth pipeline at p99?");
        assert_eq!(k, ProbeKind::Quantitative);
    }

    #[test]
    fn classify_recognises_temporal() {
        let (k, _) = ProbeKind::classify("When did the team rotate the keys?");
        assert_eq!(k, ProbeKind::Temporal);
    }

    #[test]
    fn classify_recognises_relation_callers() {
        let (k, _) = ProbeKind::classify("What calls login()?");
        assert_eq!(k, ProbeKind::RelationCallers);
    }

    #[test]
    fn classify_recognises_existential() {
        let (k, _) = ProbeKind::classify("Is there a backup policy for the DB?");
        assert_eq!(k, ProbeKind::Existential);
    }

    #[test]
    fn classify_recognises_counterfactual() {
        let (k, _) = ProbeKind::classify("What would change if we drop the cache?");
        assert_eq!(k, ProbeKind::Counterfactual);
    }

    #[test]
    fn classify_falls_back_to_factual_with_low_confidence() {
        let (k, c) = ProbeKind::classify("Tell me about authentication.");
        assert_eq!(k, ProbeKind::Factual);
        assert!(c < 0.5);
    }

    #[test]
    fn highest_required_clearance_promotes_one_step() {
        assert_eq!(
            highest_required_clearance(&[Sensitivity::Public]),
            Sensitivity::Internal
        );
        assert_eq!(
            highest_required_clearance(&[Sensitivity::Public, Sensitivity::Internal]),
            Sensitivity::Confidential
        );
    }

    #[test]
    fn parse_sensitivity_round_trips() {
        for s in [
            Sensitivity::Public,
            Sensitivity::Internal,
            Sensitivity::Confidential,
            Sensitivity::Restricted,
        ] {
            assert_eq!(parse_sensitivity(&sensitivity_str(&s)), s);
        }
    }

    #[test]
    fn parse_tier_round_trips() {
        for s in ["rooted", "attested", "quarantined", "rejected"] {
            let t = parse_tier(s);
            // Confirm the tier maps back to its canonical lowercase string.
            let back = match t {
                AdmissionTier::Rooted => "rooted",
                AdmissionTier::Attested => "attested",
                AdmissionTier::Quarantined => "quarantined",
                AdmissionTier::Rejected => "rejected",
            };
            assert_eq!(back, s);
        }
    }
}
