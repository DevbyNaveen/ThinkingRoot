//! Unified project activity bus. Every subsystem publishes a typed
//! `ActivityEvent` here; the REST layer fans it to `/activity/stream`
//! (SSE) and appends it to a durable `activity.jsonl` on the workspace
//! volume. Hue is bound to the subsystem (`ActivityClass`) so the
//! Console terminal reads at a glance.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Subsystem class → drives the Console's color shade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityClass {
    Connection,
    Ingest,
    Retrieval,
    Function,
    Branch,
    Error,
}

/// One activity event. `detail` carries the raw payload (scores, ids,
/// counts, latency) so the Console can expand a row without a second
/// fetch. `kind` is a dotted string (e.g. `mcp.connected`, `recall`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityEvent {
    pub id: String,
    pub ts: DateTime<Utc>,
    pub ws: String,
    pub class: ActivityClass,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    pub summary: String,
    #[serde(default)]
    pub detail: serde_json::Value,
}

impl ActivityEvent {
    /// Construct with a generated id + `now()` timestamp.
    pub fn new(
        ws: impl Into<String>,
        class: ActivityClass,
        kind: impl Into<String>,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            ts: Utc::now(),
            ws: ws.into(),
            class,
            kind: kind.into(),
            session_id: None,
            principal: None,
            user_id: None,
            summary: summary.into(),
            detail: serde_json::Value::Null,
        }
    }

    pub fn with_session(mut self, session_id: Option<String>) -> Self {
        self.session_id = session_id;
        self
    }
    pub fn with_principal(mut self, principal: Option<String>) -> Self {
        self.principal = principal;
        self
    }
    pub fn with_user(mut self, user_id: Option<String>) -> Self {
        self.user_id = user_id;
        self
    }
    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = detail;
        self
    }
}

/// Short label for the Console: "InAppAgent" | "claude-code (McpClient)" | "AgentMemory".
pub fn principal_label(p: &crate::mcp::telemetry::PrincipalKind) -> String {
    use crate::mcp::telemetry::PrincipalKind::*;
    match p {
        InAppAgent => "InAppAgent".into(),
        McpClient { user_agent } => format!("{user_agent} (McpClient)"),
        AgentMemory { user_agent, .. } => format!("{user_agent} (AgentMemory)"),
    }
}

/// Truncate a string for one-line log summaries (char-safe).
pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n).collect();
        out.push('…');
        out
    }
}

// ── Durable persistence (on the workspace volume, NOT config_dir) ────
//
// `mcp-sessions.jsonl` writes to `dirs::config_dir()` which inside the
// engine container is `/home/tr/.config` — lost on respawn. The activity
// log instead lives under the workspace root (the mounted volume), the
// same durable location `.thinkingroot-refs` uses, so history survives
// container restarts.

const ACT_DIR: &str = ".thinkingroot-activity";
const ACT_FILE: &str = "activity.jsonl";
const ACT_ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// Append one event as a JSON line under
/// `<workspace_root>/.thinkingroot-activity/activity.jsonl`.
pub fn append_event(workspace_root: &Path, ev: &ActivityEvent) -> std::io::Result<()> {
    let dir = workspace_root.join(ACT_DIR);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(ACT_FILE);
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > ACT_ROTATE_BYTES {
            let _ = std::fs::rename(&path, dir.join("activity.jsonl.1"));
        }
    }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    let mut line = serde_json::to_vec(ev)?;
    line.push(b'\n');
    f.write_all(&line)
}

/// Read the most-recent `limit` events (optionally those strictly before
/// `before` ts). Returns oldest→newest within the returned window.
pub fn read_recent(
    workspace_root: &Path,
    limit: usize,
    before: Option<DateTime<Utc>>,
) -> std::io::Result<Vec<ActivityEvent>> {
    let path = workspace_root.join(ACT_DIR).join(ACT_FILE);
    let data = match std::fs::read_to_string(&path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e),
    };
    let mut evs: Vec<ActivityEvent> = data
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<ActivityEvent>(l).ok())
        .filter(|e| before.map_or(true, |b| e.ts < b))
        .collect();
    let start = evs.len().saturating_sub(limit);
    Ok(evs.split_off(start))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_serializes_with_class_and_kind() {
        let ev = ActivityEvent::new("main", ActivityClass::Retrieval, "recall", "q -> 4 claims")
            .with_detail(serde_json::json!({ "claims": 4, "top_score": 0.82 }));
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["class"], "retrieval");
        assert_eq!(json["kind"], "recall");
        assert_eq!(json["detail"]["claims"], 4);
        // Optional fields omitted when None.
        assert!(json.get("session_id").is_none());
    }

    #[test]
    fn append_and_read_roundtrip() {
        let dir = std::env::temp_dir().join(format!("tr-act-{}", uuid::Uuid::new_v4()));
        let ev = ActivityEvent::new("main", ActivityClass::Branch, "branch.created", "x");
        append_event(&dir, &ev).unwrap();
        let back = read_recent(&dir, 10, None).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].kind, "branch.created");
        std::fs::remove_dir_all(&dir).ok();
    }
}
