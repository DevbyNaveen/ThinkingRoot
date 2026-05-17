//! Clean-room reimplementation. Inspired by openhuman/memory/store/agentmemory/
//! (GPL-3.0 reference, NOT lifted). The wire format itself — JSON shapes
//! + endpoint paths — is a public protocol, not copyrightable; same
//! shape as implementing HTTP from a spec.
//!
//! Phase E.4 (2026-05-17) — wire types for the agentmemory REST
//! protocol. Decoupled from `handlers.rs` so external Rust SDKs
//! (e.g. a future `agentmemory-client` crate) can depend on just
//! the types without pulling in the server.

use serde::{Deserialize, Serialize};

/// POST `/agentmemory/remember` request body.
#[derive(Debug, Clone, Deserialize)]
pub struct RememberRequest {
    /// Project name → ThinkingRoot workspace name.
    pub project: String,
    pub title: String,
    pub content: String,
    /// Memory type — free-form. Mapped onto our `ClaimType` if it
    /// matches a known value; otherwise stored as the catch-all
    /// `"fact"` type.
    #[serde(rename = "type", default = "default_type")]
    pub kind: String,
    /// Free-form concept tags. Used as entity links via the
    /// existing entity resolver.
    #[serde(default)]
    pub concepts: Vec<String>,
    /// Session ids the memory was learned in. Stored on the
    /// `Turn` substrate so cross-session retrieval works.
    #[serde(rename = "sessionIds", default)]
    pub session_ids: Vec<String>,
}

fn default_type() -> String {
    "fact".to_string()
}

/// POST `/agentmemory/remember` response.
#[derive(Debug, Clone, Serialize)]
pub struct RememberResponse {
    /// Opaque memory id — equals the underlying claim id.
    /// Clients pass this back to `/forget`.
    pub id: String,
}

/// POST `/agentmemory/smart-search` request body.
#[derive(Debug, Clone, Deserialize)]
pub struct SmartSearchRequest {
    pub query: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Optional project scope. When omitted, the daemon's default
    /// workspace is used.
    #[serde(default)]
    pub project: Option<String>,
}

fn default_limit() -> usize {
    20
}

/// POST `/agentmemory/smart-search` response.
#[derive(Debug, Clone, Serialize)]
pub struct SmartSearchResponse {
    pub results: Vec<MemoryHit>,
}

/// One ranked memory hit. Field names track the agentmemory wire
/// spec exactly — adding or renaming fields is a wire break.
#[derive(Debug, Clone, Serialize)]
pub struct MemoryHit {
    pub id: String,
    pub project: String,
    pub title: String,
    pub content: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub concepts: Vec<String>,
    #[serde(rename = "sessionIds")]
    pub session_ids: Vec<String>,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    pub score: f64,
}

/// GET `/agentmemory/memories` response.
#[derive(Debug, Clone, Serialize)]
pub struct MemoriesResponse {
    pub memories: Vec<MemoryHit>,
}

/// POST `/agentmemory/forget` request body.
#[derive(Debug, Clone, Deserialize)]
pub struct ForgetRequest {
    pub id: String,
}

/// POST `/agentmemory/forget` response.
#[derive(Debug, Clone, Serialize)]
pub struct ForgetResponse {
    pub forgotten: bool,
}

/// GET `/agentmemory/livez` response.
#[derive(Debug, Clone, Serialize)]
pub struct LivezResponse {
    pub ok: bool,
    pub version: String,
}

/// GET `/agentmemory/projects` response.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectsResponse {
    pub projects: Vec<ProjectInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectInfo {
    pub name: String,
    pub count: usize,
    #[serde(rename = "lastUpdated")]
    pub last_updated: String,
}
