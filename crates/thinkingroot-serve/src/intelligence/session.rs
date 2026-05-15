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
    created_at: Instant,
    last_active: Instant,
}

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
            created_at: now,
            last_active: now,
        }
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
