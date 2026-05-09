//! Memory commands.
//!
//! Stream A — these were previously implemented by mounting a second
//! in-process [`thinkingroot_serve::engine::QueryEngine`] inside the
//! desktop process. That violated the Cortex Protocol invariant that
//! the `root serve` daemon is the single owner of `graph.db` and
//! caused the silent-corruption class the protocol was built to
//! prevent. They now route every read through the sidecar's HTTP
//! REST surface.
//!
//! | Command       | Daemon route                                        |
//! |---------------|-----------------------------------------------------|
//! | `memory_list` | `GET /api/v1/ws/{ws}/claims?claim_type=…`           |
//! | `brain_load`  | `GET /claims` + `/entities` + `/relations` + `/claims/rooted` |
//!
//! `brain_load` issues four sequential GETs in `tokio::try_join!` so
//! a single Brain mount pays the round-trip cost in parallel rather
//! than serially.

use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use crate::commands::sidecar_client::SidecarClient;

#[derive(Debug, Serialize, Clone)]
pub struct ClaimRow {
    pub id: String,
    pub tier: String,
    pub confidence: f64,
    pub statement: String,
    pub source: String,
    pub claim_type: String,
}

/// Wire shape returned by `GET /claims`. Matches
/// `thinkingroot_serve::engine::ClaimInfo`.
#[derive(Debug, Deserialize)]
struct ClaimInfoWire {
    id: String,
    statement: String,
    confidence: f64,
    claim_type: String,
    source_uri: String,
}

#[derive(Debug, Deserialize)]
struct EntityInfoWire {
    name: String,
    entity_type: String,
    claim_count: usize,
}

#[derive(Debug, Serialize, Clone)]
pub struct EntityRow {
    pub name: String,
    pub entity_type: String,
    pub claim_count: usize,
}

#[derive(Debug, Serialize, Clone)]
pub struct RelationEdge {
    pub source: String,
    pub target: String,
    pub relation_type: String,
    pub strength: f64,
}

/// Wire shape returned by `GET /relations` — mirrors the JSON body the
/// rest.rs handler emits (lines 909-918): `{from, to, relation_type, strength}`.
#[derive(Debug, Deserialize)]
struct RelationWire {
    from: String,
    to: String,
    relation_type: String,
    strength: f64,
}

/// Combined payload for the Brain view — one round trip populates
/// both the d3 graph and the virtualized claim table.
#[derive(Debug, Serialize, Clone)]
pub struct BrainSnapshot {
    pub claims: Vec<ClaimRow>,
    pub entities: Vec<EntityRow>,
    pub relations: Vec<RelationEdge>,
    pub rooted_ids: Vec<String>,
}

#[tauri::command]
pub async fn memory_list(app: AppHandle, filter: Option<String>) -> Result<Vec<ClaimRow>, String> {
    let client = SidecarClient::ensure_active(&app).await?;
    let path = match filter {
        Some(f) if !f.trim().is_empty() => format!(
            "/api/v1/ws/{}/claims?claim_type={}",
            urlencode(&client.workspace),
            urlencode(&f),
        ),
        _ => format!("/api/v1/ws/{}/claims", urlencode(&client.workspace)),
    };
    let claims: Vec<ClaimInfoWire> = client.get(&path).await?;
    Ok(claims
        .into_iter()
        .map(|c| ClaimRow {
            id: c.id,
            tier: tier_for(c.confidence, false),
            confidence: c.confidence,
            statement: c.statement,
            source: c.source_uri,
            claim_type: c.claim_type,
        })
        .collect())
}

#[tauri::command]
pub async fn brain_load(app: AppHandle) -> Result<BrainSnapshot, String> {
    let client = SidecarClient::ensure_active(&app).await?;
    let ws = urlencode(&client.workspace);

    // Issue all four reads in parallel — the d3 graph + claim table
    // need every shape before the user sees anything.
    let claims_path = format!("/api/v1/ws/{ws}/claims");
    let entities_path = format!("/api/v1/ws/{ws}/entities");
    let relations_path = format!("/api/v1/ws/{ws}/relations");
    let rooted_path = format!("/api/v1/ws/{ws}/claims/rooted");

    let (claims, entities, relations, rooted) = tokio::try_join!(
        client.get::<Vec<ClaimInfoWire>>(&claims_path),
        client.get::<Vec<EntityInfoWire>>(&entities_path),
        client.get::<Vec<RelationWire>>(&relations_path),
        client.get::<Vec<ClaimInfoWire>>(&rooted_path),
    )?;

    let rooted_ids: Vec<String> = rooted.iter().map(|c| c.id.clone()).collect();
    let rooted_set: std::collections::HashSet<&str> =
        rooted_ids.iter().map(String::as_str).collect();

    let claim_rows: Vec<ClaimRow> = claims
        .into_iter()
        .map(|c| {
            let is_rooted = rooted_set.contains(c.id.as_str());
            ClaimRow {
                id: c.id,
                tier: tier_for(c.confidence, is_rooted),
                confidence: c.confidence,
                statement: c.statement,
                source: c.source_uri,
                claim_type: c.claim_type,
            }
        })
        .collect();

    Ok(BrainSnapshot {
        claims: claim_rows,
        entities: entities
            .into_iter()
            .map(|e| EntityRow {
                name: e.name,
                entity_type: e.entity_type,
                claim_count: e.claim_count,
            })
            .collect(),
        relations: relations
            .into_iter()
            .map(|r| RelationEdge {
                source: r.from,
                target: r.to,
                relation_type: r.relation_type,
                strength: r.strength,
            })
            .collect(),
        rooted_ids,
    })
}

fn tier_for(confidence: f64, is_rooted: bool) -> String {
    if is_rooted {
        "rooted".to_string()
    } else if confidence >= 0.7 {
        "attested".to_string()
    } else {
        "unknown".to_string()
    }
}

fn urlencode(s: &str) -> String {
    // Workspace names + claim_type values are kept tight in this codebase
    // (alphanumeric + underscore + hyphen). Use a minimal percent-encoder
    // so we don't add a percent_encoding workspace dep just for two
    // commands.  Anything outside the safe set gets %xx-encoded.
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}
