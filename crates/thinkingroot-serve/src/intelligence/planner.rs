use super::session::SessionContext;

/// The inferred intent of an agent's query to the knowledge graph.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryIntent {
    /// Deep-dive into a specific entity: full context + claims + relations.
    FullContext,
    /// Find what depends on / calls a given entity (impact analysis).
    ReverseDeps,
    /// Explore immediate neighbours of an entity (1-hop graph view).
    Neighborhood,
    /// Semantic search then focus on the best matching entity.
    SearchAndFocus,
    /// High-level workspace overview — counts, top entities, recent decisions.
    WorkspaceBrief,
}

/// A single retrieval step to execute.
#[derive(Debug, Clone)]
pub enum PlanStep {
    GetEntityContext(String),
    FindReverseDeps(String),
    GetNeighborhood(String),
    Search(String, usize),
    GetWorkspaceSummary,
}

/// Complete retrieval plan for one agent request.
#[derive(Debug, Clone)]
pub struct QueryPlan {
    pub intent: QueryIntent,
    pub steps: Vec<PlanStep>,
    /// Primary entity name extracted from the query, if determinable.
    pub entity_hint: Option<String>,
}

/// Heuristic query planner: classifies agent intent and builds a retrieval plan
/// without any LLM call. Executes in ~1 µs. No I/O.
///
/// Intent classification uses keyword matching — the same pattern used in
/// production by GraphRAG-style retrievers before graph traversal.
pub fn plan_query(query: &str, session: &SessionContext) -> QueryPlan {
    let q = query.to_lowercase();

    // ── Workspace overview ───────────────────────────────────────
    if q.contains("overview")
        || q.contains("brief")
        || q.contains("summary")
        || q.contains("workspace")
        || q.contains("what do you know")
        || q.contains("what's in")
        || q.contains("whats in")
        || q.contains("status")
    {
        return QueryPlan {
            intent: QueryIntent::WorkspaceBrief,
            steps: vec![PlanStep::GetWorkspaceSummary],
            entity_hint: None,
        };
    }

    // ── Reverse dependency / impact analysis ─────────────────────
    if q.contains("depends on")
        || q.contains("who calls")
        || q.contains("callers of")
        || q.contains("who uses")
        || q.contains("who imports")
        || q.contains("reverse")
        || q.contains("impact")
        || q.contains("what calls")
    {
        let entity = extract_entity_name(query, &session.focus_entity);
        return QueryPlan {
            intent: QueryIntent::ReverseDeps,
            steps: vec![PlanStep::FindReverseDeps(
                entity.clone().unwrap_or_default(),
            )],
            entity_hint: entity,
        };
    }

    // ── Neighbourhood / graph topology ───────────────────────────
    if q.contains("neighbor")
        || q.contains("neighbour")
        || q.contains("connected to")
        || q.contains("related to")
        || q.contains("graph view")
        || q.contains("topology")
        || q.contains("what is around")
    {
        let entity = extract_entity_name(query, &session.focus_entity);
        return QueryPlan {
            intent: QueryIntent::Neighborhood,
            steps: vec![PlanStep::GetNeighborhood(
                entity.clone().unwrap_or_default(),
            )],
            entity_hint: entity,
        };
    }

    // ── Entity deep-dive: CamelCase, quoted name, or session focus ──
    let entity = extract_entity_name(query, &session.focus_entity);
    if let Some(ref name) = entity {
        if !name.is_empty() {
            return QueryPlan {
                intent: QueryIntent::FullContext,
                steps: vec![PlanStep::GetEntityContext(name.clone())],
                entity_hint: entity,
            };
        }
    }

    // ── Default: semantic search ─────────────────────────────────
    QueryPlan {
        intent: QueryIntent::SearchAndFocus,
        steps: vec![PlanStep::Search(query.to_string(), 10)],
        entity_hint: None,
    }
}

/// Extract a probable entity name from a natural-language query.
///
/// Priority order:
/// 1. Quoted string: `"AuthService"` → `AuthService`
/// 2. PascalCase / UPPER_CASE word: `AuthService` or `AUTH_SERVICE`
/// 3. snake_case identifier: `auth_service`
/// 4. Session focus entity (fallback — entity agent last investigated)
fn extract_entity_name(query: &str, focus: &Option<String>) -> Option<String> {
    // 1. Quoted name.
    if let Some(start) = query.find('"') {
        if let Some(end) = query[start + 1..].find('"') {
            let name = &query[start + 1..start + 1 + end];
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }

    // 2. PascalCase word (first char uppercase, length ≥ 3).
    for word in query.split_whitespace() {
        let word = word.trim_matches(|c: char| !c.is_alphanumeric());
        if word.len() >= 3 && word.chars().next().is_some_and(|c| c.is_uppercase()) {
            return Some(word.to_string());
        }
    }

    // 3. snake_case identifier.
    for word in query.split_whitespace() {
        let word = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
        if word.contains('_') && word.len() >= 3 {
            return Some(word.to_string());
        }
    }

    // 4. Session focus entity.
    focus.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intelligence::session::SessionContext;

    fn empty_session() -> SessionContext {
        SessionContext::new("test-session", "test-ws")
    }

    #[test]
    fn plans_workspace_brief_for_overview() {
        let plan = plan_query("give me a brief overview", &empty_session());
        assert_eq!(plan.intent, QueryIntent::WorkspaceBrief);
        assert!(matches!(plan.steps[0], PlanStep::GetWorkspaceSummary));
    }

    #[test]
    fn plans_full_context_for_pascal_case_entity() {
        let plan = plan_query("tell me about AuthService", &empty_session());
        assert_eq!(plan.intent, QueryIntent::FullContext);
        assert_eq!(plan.entity_hint.as_deref(), Some("AuthService"));
    }

    #[test]
    fn plans_full_context_for_quoted_entity() {
        let plan = plan_query(r#"what is "QueryEngine"?"#, &empty_session());
        assert_eq!(plan.intent, QueryIntent::FullContext);
        assert_eq!(plan.entity_hint.as_deref(), Some("QueryEngine"));
    }

    #[test]
    fn plans_reverse_deps_for_who_calls() {
        let plan = plan_query("who calls PostgreSQL?", &empty_session());
        assert_eq!(plan.intent, QueryIntent::ReverseDeps);
    }

    #[test]
    fn plans_reverse_deps_for_impact() {
        let plan = plan_query(
            "what is the impact of changing AuthService?",
            &empty_session(),
        );
        assert_eq!(plan.intent, QueryIntent::ReverseDeps);
    }

    #[test]
    fn plans_neighborhood() {
        let plan = plan_query("show me what is connected to GraphStore", &empty_session());
        assert_eq!(plan.intent, QueryIntent::Neighborhood);
    }

    #[test]
    fn plans_search_as_fallback() {
        let plan = plan_query("how does authentication work end to end?", &empty_session());
        assert_eq!(plan.intent, QueryIntent::SearchAndFocus);
    }

    #[test]
    fn falls_back_to_focus_entity_when_no_name_in_query() {
        let mut session = empty_session();
        session.set_focus("GraphStore".to_string());
        let plan = plan_query("show me reverse dependencies", &session);
        assert_eq!(plan.entity_hint.as_deref(), Some("GraphStore"));
    }

    #[test]
    fn extract_snake_case_entity() {
        let plan = plan_query("tell me about graph_store", &empty_session());
        assert_eq!(plan.intent, QueryIntent::FullContext);
        assert_eq!(plan.entity_hint.as_deref(), Some("graph_store"));
    }
}
