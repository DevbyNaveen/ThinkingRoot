use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

/// In-memory session store shared across all MCP transports.
pub type SessionStore = Arc<Mutex<HashMap<String, SessionContext>>>;

/// Create a new empty session store.
pub fn new_session_store() -> SessionStore {
    Arc::new(Mutex::new(HashMap::new()))
}

const DEFAULT_TOKEN_BUDGET: usize = 4_000;
const SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Soft cap on the per-session `delivered_claim_ids` deduplication
/// set.  A long-lived MCP session that runs hundreds of `investigate`
/// calls would otherwise grow this set without bound — a 100k-claim
/// graph repeatedly delivered claim-by-claim accumulates ~50 MiB of
/// `String` headers in the session-store mutex critical section,
/// which is shared across every concurrent session.
///
/// 50_000 is the upper bound where dedup still adds value: beyond
/// that the session has likely "seen" the entire workspace and the
/// agent's queries are returning stale repeats anyway.  When the cap
/// is reached we evict the oldest insertions FIFO so recent claims
/// continue to dedup correctly.
const MAX_DELIVERED_CLAIMS: usize = 50_000;

/// Per-session state for an MCP agent connection.
///
/// Tracks what knowledge has been delivered so subsequent responses
/// are semantic diffs — the agent only sees new information. This is
/// the "working memory" layer on top of the persistent graph.
#[derive(Debug, Clone)]
pub struct SessionContext {
    pub id: String,
    pub workspace: String,
    /// Optional user identity associated with this session.
    pub owner: Option<String>,
    /// Canonical entity names explored this session (ordered by recency).
    pub active_entities: Vec<String>,
    /// Claim IDs already delivered — used to filter duplicate content.
    pub delivered_claim_ids: HashSet<String>,
    /// FIFO insertion order for `delivered_claim_ids` so the soft cap
    /// can evict the oldest entries.  Kept as a side-channel so the
    /// `HashSet` retains O(1) `contains()` for the hot is_new_claim
    /// check.
    delivered_order: VecDeque<String>,
    /// Entity the agent is currently focused on (set by the `focus` tool).
    pub focus_entity: Option<String>,
    /// Branch the agent has checked out (set by `checkout_branch` tool).
    /// When set, `contribute` writes to this branch instead of main.
    pub active_branch: Option<String>,
    /// Remaining token budget for the current tool call.
    pub token_budget: usize,
    /// Number of contribute calls made in this session (used as turn number for turn calendar).
    pub turn_count: u64,
    /// Number of *chat* turns completed in this session — incremented
    /// once per agent run that emits a terminal `Done` event. Distinct
    /// from `turn_count` (which counts `contribute` write calls) so the
    /// Observer's `ChatTurn.turn_number` is a meaningful ordinal even
    /// for read-only sessions that never write claims.
    pub chat_turn_count: u64,
    /// Phase B.1 (2026-05-17): the user's first message in this
    /// session. Drives topic-branch titling — when
    /// `maintenance::cleanup_once` auto-merges the stream branch
    /// into a `topic/*` Feature branch, the first user message is
    /// propagated onto the topic branch's `description` so the UI
    /// surfaces a meaningful human-readable title without paying
    /// for an LLM summarisation pass.
    ///
    /// Set exactly once per session via
    /// [`SessionContext::set_first_user_message_if_unset`]; subsequent
    /// turns leave it unchanged. Persisted on the active stream
    /// branch's `description` field at first-message receipt so it
    /// survives session eviction.
    pub first_user_message: Option<String>,
    /// Ship 3D (2026-05-20) — Reflexion pattern. After every chat turn
    /// the verifier produces a critique; storing it here lets the next
    /// turn's `<previous_verify>` reminder block bias the LLM toward
    /// self-correction when the prior answer scored weak grounding.
    /// `None` on session entry; populated by the post-Done verify
    /// hook in `rest.rs::agent_stream_response`.
    pub last_verify_verdict: Option<String>,
    /// Ship 3D — citations from the prior turn that resolved against
    /// the substrate.
    pub last_verify_citations_verified: Option<u32>,
    /// Ship 3D — citations from the prior turn that DID NOT resolve.
    /// A non-zero value here is the precise signal the next turn must
    /// surface.
    pub last_verify_citations_unverified: Option<u32>,
    /// Ship 3D — one-line human-readable critique reason from the
    /// prior turn (verdict-specific).
    pub last_verify_reason: Option<String>,
    /// Ship 3E (2026-05-20) — last retrieval query that returned thin
    /// evidence. Used by the `<search_was_shallow>` reminder to warn
    /// the next turn against re-running the same shallow query.
    pub last_search_query: Option<String>,
    /// Ship 3E — hit count from the last retrieval; the threshold
    /// `SHALLOW_RETRIEVAL_THRESHOLD` determines whether the bus emits
    /// the warning block.
    pub last_search_hits: Option<u32>,
    /// C6 (2026-05-22) — `clientInfo` field from the MCP
    /// `initialize` request. When present, this identifies the AI
    /// tool driving the session (e.g., "claude-code", "cursor",
    /// "codex") and lets `session_actor` map known AI clients to
    /// `Principal::Agent` instead of `Principal::User`. None over
    /// REST chat + over MCP transports that didn't receive a
    /// `clientInfo` block on initialize (preserves pre-C6
    /// behaviour for tests).
    pub client_info: Option<ClientInfo>,
    /// M4 — the query-INDEPENDENT capsule frame (system prompt + workspace
    /// brief + routed tools) warmed for this session's active branch +
    /// prompt. Reused across turns so per-turn capsule work collapses to
    /// just retrieval. Invalidated on a contribute to this session (the
    /// brief/tools can change) and rebuilt lazily. `brief_json` keeps this
    /// struct decoupled from the engine-layer `WorkspaceSummary` type.
    pub warm_frame: Option<WarmFrame>,
    created_at: Instant,
    last_active: Instant,
}

/// M4 — the cached, query-independent part of a [`crate::engine::CompiledCapsule`]
/// held on a session so a live streaming-branch turn only pays for
/// retrieval. `brief_json` is the serialized `WorkspaceSummary` (stored
/// as a string to avoid coupling the session layer to the engine layer).
#[derive(Debug, Clone)]
pub struct WarmFrame {
    pub branch: Option<String>,
    pub prompt_name: String,
    pub prompt_version: i64,
    pub system: String,
    pub brief_json: String,
    pub tools: Vec<String>,
}

/// MCP spec `clientInfo` object (per JSON-RPC `initialize` params).
/// Carries the AI client's name and version so the daemon can
/// attribute writes correctly (Claude Code vs Cursor vs Codex) and
/// surface cross-tool awareness in the reminder bus.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ClientInfo {
    pub name: String,
    #[serde(default)]
    pub version: String,
}

impl ClientInfo {
    /// Return true if `self.name` matches one of the canonical AI
    /// client identifiers we recognise. `session_actor` uses this
    /// to decide between `Principal::Agent` (known AI client) and
    /// `Principal::User` (human-driven CLI, unknown client, or
    /// absent clientInfo).
    ///
    /// Match is case-insensitive on the name to tolerate small
    /// vendor capitalisation differences across MCP clients.
    pub fn is_known_ai_client(&self) -> bool {
        is_known_ai_client_name(&self.name)
    }
}

/// Canonical list of AI clients we recognise — kept in one place so
/// the mapping stays consistent across `ClientInfo::is_known_ai_client`
/// and any future MCP server-side audit / billing / quota logic.
///
/// Adding a client here changes its attribution from
/// `Principal::User` to `Principal::Agent`. Don't add a vendor
/// name without verifying their MCP client actually sends a
/// recognisable `clientInfo.name` (verified for Claude Desktop,
/// Claude Code, Cursor as of 2026-05).
pub const KNOWN_AI_CLIENT_NAMES: &[&str] = &[
    "claude-code",
    "claude-desktop",
    "cursor",
    "windsurf",
    "continue",
    "codex",
    "gemini-cli",
    "zed",
    "root-cli",
];

/// Case-insensitive name match against [`KNOWN_AI_CLIENT_NAMES`].
pub fn is_known_ai_client_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    KNOWN_AI_CLIENT_NAMES
        .iter()
        .any(|known| *known == lower.as_str())
}

/// Threshold below which a retrieval call counts as "shallow" — the
/// bus emits the `search_was_shallow` reminder when `last_search_hits`
/// is at or below this. Chosen empirically: ≤2 hits is the danger
/// zone where the model will re-run the same query and burn iterations
/// without gaining new evidence.
pub const SHALLOW_RETRIEVAL_THRESHOLD: u32 = 2;

impl SessionContext {
    pub fn new(id: impl Into<String>, workspace: impl Into<String>) -> Self {
        let now = Instant::now();
        Self {
            id: id.into(),
            workspace: workspace.into(),
            owner: None,
            active_entities: Vec::new(),
            delivered_claim_ids: HashSet::new(),
            delivered_order: VecDeque::new(),
            focus_entity: None,
            active_branch: None,
            token_budget: DEFAULT_TOKEN_BUDGET,
            turn_count: 0,
            chat_turn_count: 0,
            first_user_message: None,
            last_verify_verdict: None,
            last_verify_citations_verified: None,
            last_verify_citations_unverified: None,
            last_verify_reason: None,
            last_search_query: None,
            last_search_hits: None,
            client_info: None,
            warm_frame: None,
            created_at: now,
            last_active: now,
        }
    }

    /// M4 — drop the warm capsule frame. Called on a contribute to this
    /// session so the next capsule rebuilds the frame against fresh
    /// brief/tools instead of serving a stale orientation.
    pub fn invalidate_warm_frame(&mut self) {
        self.last_active = Instant::now();
        self.warm_frame = None;
    }

    /// Ship 3D (2026-05-20) — record the prior turn's verifier verdict
    /// so the next turn's `<previous_verify>` reminder block can read
    /// it. Idempotent within a turn; overwritten on every new verify.
    /// Touching `last_active` so eviction policy treats verifier feedback
    /// as session liveness.
    pub fn record_verify_critique(
        &mut self,
        verdict: impl Into<String>,
        citations_verified: u32,
        citations_unverified: u32,
        reason: impl Into<String>,
    ) {
        self.last_active = Instant::now();
        self.last_verify_verdict = Some(verdict.into());
        self.last_verify_citations_verified = Some(citations_verified);
        self.last_verify_citations_unverified = Some(citations_unverified);
        self.last_verify_reason = Some(reason.into());
    }

    /// Ship 3E (2026-05-20) — record the most recent retrieval call's
    /// query + hit count so the `<search_was_shallow>` reminder fires
    /// next turn when evidence was thin. Hits at or below
    /// `SHALLOW_RETRIEVAL_THRESHOLD` are the trigger condition.
    pub fn record_search_outcome(&mut self, query: impl Into<String>, hits: u32) {
        self.last_active = Instant::now();
        self.last_search_query = Some(query.into());
        self.last_search_hits = Some(hits);
    }

    /// Phase B.1 (2026-05-17): record the user's first message in
    /// this session if not already set. Returns `Some(stored_msg)`
    /// when the call actually set the field (caller should persist
    /// onto the stream branch description); `None` when a prior turn
    /// already set it (idempotent no-op).
    ///
    /// Trims whitespace and rejects empty input (treats as no-op).
    /// Caps the stored message at 256 chars to keep `branches.toml`
    /// bounded — the description is a human-readable title, not a
    /// full transcript copy.
    pub fn set_first_user_message_if_unset(&mut self, msg: &str) -> Option<String> {
        self.last_active = Instant::now();
        if self.first_user_message.is_some() {
            return None;
        }
        let trimmed = msg.trim();
        if trimmed.is_empty() {
            return None;
        }
        const MAX_LEN: usize = 256;
        let stored: String = if trimmed.chars().count() > MAX_LEN {
            // Take first MAX_LEN chars (not bytes — avoid splitting a
            // UTF-8 codepoint) and append an ellipsis marker. Char
            // boundaries are O(MAX_LEN) which is fine for a one-shot
            // per session.
            let mut s: String = trimmed.chars().take(MAX_LEN).collect();
            s.push('…');
            s
        } else {
            trimmed.to_string()
        };
        self.first_user_message = Some(stored.clone());
        Some(stored)
    }

    /// Allocate the next chat-turn ordinal. Bumps `chat_turn_count`
    /// and returns the new value, so the first turn is `1`, the
    /// second is `2`, etc. — matches the human-readable `Turn N`
    /// scheme the Observer's `condense_window` emits into the
    /// staged observation text.
    pub fn next_chat_turn(&mut self) -> u64 {
        self.last_active = Instant::now();
        self.chat_turn_count = self.chat_turn_count.saturating_add(1);
        self.chat_turn_count
    }

    /// Mark claim IDs as delivered — they will be filtered from future responses.
    ///
    /// Bounded at `MAX_DELIVERED_CLAIMS` (50 k entries) via FIFO
    /// eviction.  Pre-fix a long-running session could accumulate the
    /// entire workspace's claim ids in this set — for a 200 k-claim
    /// graph the session-store mutex held ~10 MiB *per session*,
    /// multiplied by however many concurrent agents.
    pub fn mark_delivered(&mut self, claim_ids: &[String]) {
        self.last_active = Instant::now();
        for id in claim_ids {
            // Skip duplicates so the FIFO order tracks unique
            // insertions, not insertion attempts.
            if self.delivered_claim_ids.insert(id.clone()) {
                self.delivered_order.push_back(id.clone());
            }
        }
        // Evict oldest entries when over the cap.  The hot path
        // (`is_new_claim`) still hits the HashSet for O(1) lookup;
        // only this writer path pays the cap-maintenance cost.
        while self.delivered_claim_ids.len() > MAX_DELIVERED_CLAIMS {
            if let Some(oldest) = self.delivered_order.pop_front() {
                self.delivered_claim_ids.remove(&oldest);
            } else {
                break;
            }
        }
    }

    /// Set the active branch for this session (contribute writes here instead of main).
    pub fn set_branch(&mut self, branch: String) {
        self.last_active = Instant::now();
        self.active_branch = Some(branch);
    }

    /// Associate a human owner with this session.
    pub fn set_owner(&mut self, owner: String) {
        self.last_active = Instant::now();
        self.owner = Some(owner);
    }

    /// Clear the active branch (revert contribute back to main).
    pub fn clear_branch(&mut self) {
        self.last_active = Instant::now();
        self.active_branch = None;
    }

    /// Set the focal entity, recording it in the active entity list.
    pub fn set_focus(&mut self, entity: String) {
        self.last_active = Instant::now();
        self.record_entity(entity.clone());
        self.focus_entity = Some(entity);
    }

    /// Record that an entity was explored this session.
    pub fn record_entity(&mut self, entity: String) {
        self.last_active = Instant::now();
        if !self.active_entities.contains(&entity) {
            self.active_entities.push(entity);
        }
    }

    /// Returns true if this claim has NOT been delivered to this agent yet.
    pub fn is_new_claim(&self, id: &str) -> bool {
        !self.delivered_claim_ids.contains(id)
    }

    /// Returns true if the session has been idle longer than SESSION_TTL (24 h).
    pub fn is_expired(&self) -> bool {
        self.last_active.elapsed() > SESSION_TTL
    }

    /// Deduct tokens from the per-call budget.
    pub fn deduct_tokens(&mut self, count: usize) {
        self.token_budget = self.token_budget.saturating_sub(count);
    }

    /// Reset the token budget (called at the start of each tool invocation).
    pub fn reset_budget(&mut self) {
        self.token_budget = DEFAULT_TOKEN_BUDGET;
    }

    /// Number of claims delivered to this agent so far.
    pub fn delivered_count(&self) -> usize {
        self.delivered_claim_ids.len()
    }

    /// Wall-clock age of this session in seconds.
    pub fn age_secs(&self) -> u64 {
        self.created_at.elapsed().as_secs()
    }

    /// Seconds since the last activity touched this session. Used by the
    /// stream-cleanup task to decide whether an in-memory session is still
    /// holding its `stream/*` branch alive.
    pub fn idle_secs(&self) -> u64 {
        self.last_active.elapsed().as_secs()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_session_starts_empty() {
        let s = SessionContext::new("sess-1", "my-ws");
        assert_eq!(s.active_entities.len(), 0);
        assert_eq!(s.delivered_claim_ids.len(), 0);
        assert!(s.focus_entity.is_none());
        assert_eq!(s.token_budget, DEFAULT_TOKEN_BUDGET);
    }

    #[test]
    fn mark_delivered_filters_claims() {
        let mut s = SessionContext::new("sess-1", "my-ws");
        s.mark_delivered(&["c1".to_string(), "c2".to_string()]);
        assert!(!s.is_new_claim("c1"));
        assert!(!s.is_new_claim("c2"));
        assert!(s.is_new_claim("c3"));
    }

    #[test]
    fn set_focus_records_entity() {
        let mut s = SessionContext::new("sess-1", "my-ws");
        s.set_focus("AuthService".to_string());
        assert_eq!(s.focus_entity.as_deref(), Some("AuthService"));
        assert!(s.active_entities.contains(&"AuthService".to_string()));
    }

    #[test]
    fn record_entity_deduplicates() {
        let mut s = SessionContext::new("sess-1", "my-ws");
        s.record_entity("AuthService".to_string());
        s.record_entity("AuthService".to_string());
        assert_eq!(s.active_entities.len(), 1);
    }

    #[test]
    fn session_not_expired_immediately() {
        let s = SessionContext::new("sess-1", "my-ws");
        assert!(!s.is_expired());
    }

    #[test]
    fn next_chat_turn_starts_at_one_and_increments() {
        let mut s = SessionContext::new("sess-1", "my-ws");
        assert_eq!(s.chat_turn_count, 0);
        assert_eq!(s.next_chat_turn(), 1);
        assert_eq!(s.next_chat_turn(), 2);
        assert_eq!(s.chat_turn_count, 2);
    }

    #[test]
    fn next_chat_turn_independent_of_turn_count() {
        let mut s = SessionContext::new("sess-1", "my-ws");
        s.turn_count = 7; // simulate prior contribute calls
        assert_eq!(s.next_chat_turn(), 1, "chat turn ordinal is its own counter");
        assert_eq!(s.turn_count, 7, "contribute counter is untouched");
    }

    #[test]
    fn deduct_tokens_saturates_at_zero() {
        let mut s = SessionContext::new("sess-1", "my-ws");
        s.deduct_tokens(DEFAULT_TOKEN_BUDGET + 1000);
        assert_eq!(s.token_budget, 0);
    }

    #[test]
    fn set_first_user_message_records_on_first_call() {
        let mut s = SessionContext::new("sess-1", "my-ws");
        assert!(s.first_user_message.is_none());
        let returned = s.set_first_user_message_if_unset("How does the auth system work?");
        assert_eq!(returned.as_deref(), Some("How does the auth system work?"));
        assert_eq!(
            s.first_user_message.as_deref(),
            Some("How does the auth system work?")
        );
    }

    #[test]
    fn set_first_user_message_is_idempotent() {
        let mut s = SessionContext::new("sess-1", "my-ws");
        assert!(s
            .set_first_user_message_if_unset("First message")
            .is_some());
        // Subsequent calls must NOT overwrite. Returning None signals
        // "already set" so the caller doesn't redundantly hit the
        // branch registry.
        let returned = s.set_first_user_message_if_unset("Second message");
        assert!(returned.is_none(), "must not overwrite an existing value");
        assert_eq!(
            s.first_user_message.as_deref(),
            Some("First message"),
            "stored value must be the first call's input"
        );
    }

    #[test]
    fn set_first_user_message_rejects_empty_and_whitespace() {
        let mut s = SessionContext::new("sess-1", "my-ws");
        assert!(s.set_first_user_message_if_unset("").is_none());
        assert!(s.first_user_message.is_none(), "empty input is a no-op");
        assert!(s.set_first_user_message_if_unset("   \t\n  ").is_none());
        assert!(
            s.first_user_message.is_none(),
            "whitespace-only input is a no-op"
        );
        // After two rejected attempts, a valid message still takes.
        let returned = s.set_first_user_message_if_unset("Real question?");
        assert_eq!(returned.as_deref(), Some("Real question?"));
    }

    #[test]
    fn set_first_user_message_truncates_long_input() {
        let mut s = SessionContext::new("sess-1", "my-ws");
        // 300-char string of all 'a' — exceeds the 256-char MAX_LEN cap.
        let long: String = "a".repeat(300);
        let returned = s
            .set_first_user_message_if_unset(&long)
            .expect("non-empty input must store something");
        // Truncated to 256 chars + the ellipsis marker.
        assert_eq!(
            returned.chars().count(),
            257,
            "truncated value = MAX_LEN chars + 1 ellipsis char"
        );
        assert!(
            returned.ends_with('…'),
            "truncation marker must be the trailing ellipsis"
        );
    }

    #[test]
    fn set_first_user_message_handles_utf8_codepoints() {
        // Regression guard: truncation MUST cut on char boundaries,
        // not byte boundaries — splitting a multi-byte UTF-8 char
        // would corrupt branches.toml on disk.
        let mut s = SessionContext::new("sess-1", "my-ws");
        // 200 multi-byte chars — well under the 256 char cap, but
        // would exceed many naive byte-based caps (each char is 2-3
        // bytes).
        let msg: String = "你".repeat(200);
        let returned = s
            .set_first_user_message_if_unset(&msg)
            .expect("non-empty input must store something");
        assert_eq!(
            returned.chars().count(),
            200,
            "200 chars are under the cap, no truncation"
        );
        assert!(
            std::str::from_utf8(returned.as_bytes()).is_ok(),
            "stored value must be valid UTF-8 — no codepoint split"
        );
    }

    #[test]
    fn delivered_claim_ids_stay_bounded() {
        // Regression: pre-fix this set grew without bound, exhausting
        // RAM on long-running MCP sessions in production.
        let mut s = SessionContext::new("sess-1", "my-ws");
        let n = MAX_DELIVERED_CLAIMS + 1_000;
        let ids: Vec<String> = (0..n).map(|i| format!("c{i}")).collect();
        s.mark_delivered(&ids);
        assert_eq!(
            s.delivered_claim_ids.len(),
            MAX_DELIVERED_CLAIMS,
            "set must not exceed the cap"
        );
        // Oldest should have been evicted.
        assert!(
            s.is_new_claim("c0"),
            "oldest entry must have been evicted by FIFO"
        );
        // Newest should still be present.
        assert!(
            !s.is_new_claim(&format!("c{}", n - 1)),
            "most-recent entry must still be tracked"
        );
    }
}
