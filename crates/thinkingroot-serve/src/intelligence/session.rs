use std::collections::{HashMap, HashSet};
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

/// Per-session state for an MCP agent connection.
///
/// Tracks what knowledge has been delivered so subsequent responses
/// are semantic diffs — the agent only sees new information. This is
/// the "working memory" layer on top of the persistent graph.
#[derive(Debug, Clone)]
pub struct SessionContext {
    pub id: String,
    pub workspace: String,
    /// Canonical entity names explored this session (ordered by recency).
    pub active_entities: Vec<String>,
    /// Claim IDs already delivered — used to filter duplicate content.
    pub delivered_claim_ids: HashSet<String>,
    /// Entity the agent is currently focused on (set by the `focus` tool).
    pub focus_entity: Option<String>,
    /// Branch the agent has checked out (set by `checkout_branch` tool).
    /// When set, `contribute` writes to this branch instead of main.
    pub active_branch: Option<String>,
    /// Remaining token budget for the current tool call.
    pub token_budget: usize,
    created_at: Instant,
    last_active: Instant,
}

impl SessionContext {
    pub fn new(id: impl Into<String>, workspace: impl Into<String>) -> Self {
        let now = Instant::now();
        Self {
            id: id.into(),
            workspace: workspace.into(),
            active_entities: Vec::new(),
            delivered_claim_ids: HashSet::new(),
            focus_entity: None,
            active_branch: None,
            token_budget: DEFAULT_TOKEN_BUDGET,
            created_at: now,
            last_active: now,
        }
    }

    /// Mark claim IDs as delivered — they will be filtered from future responses.
    pub fn mark_delivered(&mut self, claim_ids: &[String]) {
        self.last_active = Instant::now();
        for id in claim_ids {
            self.delivered_claim_ids.insert(id.clone());
        }
    }

    /// Set the active branch for this session (contribute writes here instead of main).
    pub fn set_branch(&mut self, branch: String) {
        self.last_active = Instant::now();
        self.active_branch = Some(branch);
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
    fn deduct_tokens_saturates_at_zero() {
        let mut s = SessionContext::new("sess-1", "my-ws");
        s.deduct_tokens(DEFAULT_TOKEN_BUDGET + 1000);
        assert_eq!(s.token_budget, 0);
    }
}
