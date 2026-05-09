//! Privacy dashboard backend.
//!
//! Stream A — these were previously implemented by mounting a second
//! in-process `QueryEngine` inside the desktop process. `privacy_forget`
//! in particular was a **graph-mutating write** racing against the
//! daemon's reads — exactly the silent-corruption class the Cortex
//! Protocol forbids. They now route through the sidecar's REST surface
//! so the daemon stays the single owner of `graph.db`.
//!
//! | Command           | Daemon route                                       |
//! |-------------------|----------------------------------------------------|
//! | `privacy_summary` | `GET /sources` + `/claims` + `/entities`           |
//! | `privacy_forget`  | `POST /api/v1/ws/{ws}/sources/forget`              |
//!
//! Forgetting a source is a graph mutation: the daemon's
//! `forget_source` removes the source row plus every claim/entity-edge/
//! vector/contradiction descendant, then atomically swaps the read
//! cache. There is no soft-delete tombstone — the row is gone.

use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use crate::commands::sidecar_client::SidecarClient;

#[derive(Debug, Serialize, Clone, Deserialize)]
pub struct PrivacySource {
    pub id: String,
    pub uri: String,
    pub source_type: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct PrivacySummary {
    pub workspace: String,
    pub sources: Vec<PrivacySource>,
    pub source_count: usize,
    pub claim_count: usize,
    pub entity_count: usize,
}

#[derive(Debug, Deserialize)]
struct ClaimWire {
    #[allow(dead_code)]
    id: String,
}

#[derive(Debug, Deserialize)]
struct EntityWire {
    #[allow(dead_code)]
    name: String,
}

#[derive(Debug, Deserialize)]
struct ForgetResponse {
    removed: usize,
}

/// Read counts + source list for the active workspace.
#[tauri::command]
pub async fn privacy_summary(app: AppHandle) -> Result<PrivacySummary, String> {
    let client = SidecarClient::ensure_active(&app).await?;
    let ws = urlencode(&client.workspace);

    let sources_path = format!("/api/v1/ws/{ws}/sources");
    let claims_path = format!("/api/v1/ws/{ws}/claims");
    let entities_path = format!("/api/v1/ws/{ws}/entities");

    let (sources, claims, entities) = tokio::try_join!(
        client.get::<Vec<PrivacySource>>(&sources_path),
        client.get::<Vec<ClaimWire>>(&claims_path),
        client.get::<Vec<EntityWire>>(&entities_path),
    )?;

    Ok(PrivacySummary {
        workspace: client.workspace.clone(),
        source_count: sources.len(),
        claim_count: claims.len(),
        entity_count: entities.len(),
        sources,
    })
}

/// Forget every claim/edge/vector descended from `source_uri`. Returns
/// the number of source rows removed (0 if no match).
#[tauri::command]
pub async fn privacy_forget(app: AppHandle, source_uri: String) -> Result<usize, String> {
    let client = SidecarClient::ensure_active(&app).await?;
    let path = format!("/api/v1/ws/{}/sources/forget", urlencode(&client.workspace),);
    let body = serde_json::json!({ "source_uri": source_uri });
    let resp: ForgetResponse = client.post(&path, &body).await?;
    Ok(resp.removed)
}

fn urlencode(s: &str) -> String {
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
