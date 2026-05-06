//! Claims listing — `claims_list`, `claims_as_of`, `claims_rooted`.
//!
//! Mirrors the daemon's existing routes:
//!   - `GET /api/v1/ws/{ws}/claims?...` (list + filter)
//!   - `GET /api/v1/ws/{ws}/claims/as-of?as_of=...&branch=...` (T2.4)
//!   - `GET /api/v1/ws/{ws}/claims/rooted` (trust-rooted view)

use tauri::AppHandle;

use crate::commands::sidecar_client::SidecarClient;

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => out.push(ch),
            _ => {
                let mut buf = [0u8; 4];
                for b in ch.encode_utf8(&mut buf).bytes() {
                    out.push_str(&format!("%{b:02X}"));
                }
            }
        }
    }
    out
}

#[tauri::command]
pub async fn claims_list(
    app: AppHandle,
    claim_type: Option<String>,
    entity: Option<String>,
    min_confidence: Option<f64>,
    limit: Option<u32>,
    offset: Option<u32>,
) -> Result<serde_json::Value, String> {
    let sc = SidecarClient::ensure_active(&app).await?;
    let mut path = format!("/api/v1/ws/{}/claims", sc.workspace);
    let mut q: Vec<(String, String)> = Vec::new();
    if let Some(t) = claim_type {
        q.push(("type".into(), url_encode(&t)));
    }
    if let Some(e) = entity {
        q.push(("entity".into(), url_encode(&e)));
    }
    if let Some(c) = min_confidence {
        q.push(("min_confidence".into(), c.to_string()));
    }
    if let Some(l) = limit {
        q.push(("limit".into(), l.to_string()));
    }
    if let Some(o) = offset {
        q.push(("offset".into(), o.to_string()));
    }
    if !q.is_empty() {
        path.push('?');
        path.push_str(
            &q.into_iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("&"),
        );
    }
    sc.get::<serde_json::Value>(&path).await
}

#[tauri::command]
pub async fn claims_as_of(
    app: AppHandle,
    as_of: String,
    branch: Option<String>,
) -> Result<serde_json::Value, String> {
    let sc = SidecarClient::ensure_active(&app).await?;
    let mut path = format!(
        "/api/v1/ws/{}/claims/as-of?as_of={}",
        sc.workspace,
        url_encode(&as_of)
    );
    if let Some(b) = branch {
        path.push_str(&format!("&branch={}", url_encode(&b)));
    }
    sc.get::<serde_json::Value>(&path).await
}

#[tauri::command]
pub async fn claims_rooted(app: AppHandle) -> Result<serde_json::Value, String> {
    let sc = SidecarClient::ensure_active(&app).await?;
    let path = format!("/api/v1/ws/{}/claims/rooted", sc.workspace);
    sc.get::<serde_json::Value>(&path).await
}
