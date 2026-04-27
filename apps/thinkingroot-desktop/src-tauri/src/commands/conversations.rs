//! Conversation persistence — per-workspace JSON store.
//!
//! Each workspace owns a `.thinkingroot/conversations/` directory:
//!
//! ```text
//! <workspace>/.thinkingroot/conversations/
//!   index.json                — array of summaries (id, title, ts)
//!   <conversation-id>.json    — full transcript
//! ```
//!
//! We deliberately do not link sqlite here: `thinkingroot-graph`
//! already consumes the global `links="sqlite3"` slot via cozo, and a
//! second linker entry would refuse to build. The JSON layout is also
//! easier to inspect, sync, and back up than a binary DB.
//!
//! All writes are atomic (write to `*.tmp`, then `rename`). Reads
//! tolerate a missing index by treating it as empty.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thinkingroot_core::WorkspaceRegistry;
use uuid::Uuid;

/// One conversation summary as the sidebar shows it.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ConversationSummary {
    pub id: String,
    pub workspace: String,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub message_count: usize,
}

/// One persisted message. Mirrors the UI's `ChatMessage` shape closely
/// enough that the front-end can render either source directly.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ConversationMessage {
    pub id: String,
    pub role: String,
    pub content: String,
    pub model: Option<String>,
    pub created_at: DateTime<Utc>,
    /// Provenance claim ids referenced when the engine returned this turn.
    #[serde(default)]
    pub claims_used: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Conversation {
    pub summary: ConversationSummary,
    pub messages: Vec<ConversationMessage>,
}

// ─── List ────────────────────────────────────────────────────────────

#[tauri::command]
pub fn conversations_list(workspace: Option<String>) -> Result<Vec<ConversationSummary>, String> {
    let mut all: Vec<ConversationSummary> = Vec::new();
    for ws in resolve_targets(workspace.as_deref())? {
        let dir = conv_dir(&ws.path);
        let idx = read_index(&dir).unwrap_or_default();
        all.extend(idx);
    }
    all.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(all)
}

// ─── Create ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ConversationCreateArgs {
    pub workspace: String,
    pub title: Option<String>,
}

#[tauri::command]
pub fn conversations_create(
    args: ConversationCreateArgs,
) -> Result<ConversationSummary, String> {
    let entry = lookup_workspace(&args.workspace)?;
    let dir = conv_dir(&entry.path);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create dir: {e}"))?;

    let now = Utc::now();
    let summary = ConversationSummary {
        id: Uuid::new_v4().to_string(),
        workspace: entry.name.clone(),
        title: args
            .title
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("New conversation")
            .to_string(),
        created_at: now,
        updated_at: now,
        message_count: 0,
    };

    let conv = Conversation {
        summary: summary.clone(),
        messages: Vec::new(),
    };
    write_conversation(&dir, &conv)?;
    upsert_index(&dir, summary.clone())?;
    Ok(summary)
}

// ─── Get ─────────────────────────────────────────────────────────────

#[tauri::command]
pub fn conversations_get(workspace: String, id: String) -> Result<Conversation, String> {
    let entry = lookup_workspace(&workspace)?;
    let dir = conv_dir(&entry.path);
    read_conversation(&dir, &id)
}

// ─── Append message ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct AppendMessageArgs {
    pub workspace: String,
    pub conversation_id: String,
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub claims_used: Vec<String>,
}

#[tauri::command]
pub fn conversations_append_message(
    args: AppendMessageArgs,
) -> Result<ConversationMessage, String> {
    let entry = lookup_workspace(&args.workspace)?;
    let dir = conv_dir(&entry.path);
    let mut conv = read_conversation(&dir, &args.conversation_id)?;

    let now = Utc::now();
    let msg = ConversationMessage {
        id: Uuid::new_v4().to_string(),
        role: args.role,
        content: args.content,
        model: args.model,
        created_at: now,
        claims_used: args.claims_used,
    };
    conv.messages.push(msg.clone());

    // First user message becomes the auto-title — the same heuristic
    // every chat product uses for unnamed sessions.
    if conv.summary.title == "New conversation" && msg.role == "user" {
        conv.summary.title = derive_title(&msg.content);
    }
    conv.summary.updated_at = now;
    conv.summary.message_count = conv.messages.len();

    write_conversation(&dir, &conv)?;
    upsert_index(&dir, conv.summary.clone())?;
    Ok(msg)
}

// ─── Delete ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ConversationDeleteArgs {
    pub workspace: String,
    pub id: String,
}

#[tauri::command]
pub fn conversations_delete(args: ConversationDeleteArgs) -> Result<bool, String> {
    let entry = lookup_workspace(&args.workspace)?;
    let dir = conv_dir(&entry.path);
    let path = dir.join(format!("{}.json", args.id));
    let removed = path.exists();
    if removed {
        std::fs::remove_file(&path).map_err(|e| format!("remove: {e}"))?;
    }
    let mut idx = read_index(&dir).unwrap_or_default();
    idx.retain(|s| s.id != args.id);
    write_index(&dir, &idx)?;
    Ok(removed)
}

// ─── Rename ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ConversationRenameArgs {
    pub workspace: String,
    pub id: String,
    pub title: String,
}

#[tauri::command]
pub fn conversations_rename(args: ConversationRenameArgs) -> Result<ConversationSummary, String> {
    let entry = lookup_workspace(&args.workspace)?;
    let dir = conv_dir(&entry.path);
    let mut conv = read_conversation(&dir, &args.id)?;
    let trimmed = args.title.trim();
    if trimmed.is_empty() {
        return Err("title cannot be empty".to_string());
    }
    conv.summary.title = trimmed.to_string();
    conv.summary.updated_at = Utc::now();
    write_conversation(&dir, &conv)?;
    upsert_index(&dir, conv.summary.clone())?;
    Ok(conv.summary)
}

// ─── Internals ───────────────────────────────────────────────────────

fn conv_dir(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".thinkingroot").join("conversations")
}

fn lookup_workspace(name: &str) -> Result<thinkingroot_core::WorkspaceEntry, String> {
    let registry = WorkspaceRegistry::load().map_err(|e| e.to_string())?;
    registry
        .workspaces
        .iter()
        .find(|w| w.name == name)
        .cloned()
        .ok_or_else(|| format!("workspace `{name}` not found"))
}

fn resolve_targets(
    explicit: Option<&str>,
) -> Result<Vec<thinkingroot_core::WorkspaceEntry>, String> {
    let registry = WorkspaceRegistry::load().map_err(|e| e.to_string())?;
    if let Some(name) = explicit {
        Ok(registry
            .workspaces
            .iter()
            .filter(|w| w.name == name)
            .cloned()
            .collect())
    } else {
        Ok(registry.workspaces.clone())
    }
}

fn read_index(dir: &Path) -> Option<Vec<ConversationSummary>> {
    let path = dir.join("index.json");
    let raw = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_index(dir: &Path, idx: &[ConversationSummary]) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("create dir: {e}"))?;
    let path = dir.join("index.json");
    let body = serde_json::to_string_pretty(idx).map_err(|e| format!("encode: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body).map_err(|e| format!("write: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("rename: {e}"))
}

fn upsert_index(dir: &Path, summary: ConversationSummary) -> Result<(), String> {
    let mut idx = read_index(dir).unwrap_or_default();
    if let Some(existing) = idx.iter_mut().find(|s| s.id == summary.id) {
        *existing = summary;
    } else {
        idx.push(summary);
    }
    idx.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    write_index(dir, &idx)
}

fn read_conversation(dir: &Path, id: &str) -> Result<Conversation, String> {
    if !is_safe_id(id) {
        return Err("invalid conversation id".to_string());
    }
    let path = dir.join(format!("{id}.json"));
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("read: {e}"))?;
    serde_json::from_str(&raw).map_err(|e| format!("decode: {e}"))
}

fn write_conversation(dir: &Path, conv: &Conversation) -> Result<(), String> {
    if !is_safe_id(&conv.summary.id) {
        return Err("invalid conversation id".to_string());
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("create dir: {e}"))?;
    let path = dir.join(format!("{}.json", conv.summary.id));
    let body = serde_json::to_string_pretty(conv).map_err(|e| format!("encode: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body).map_err(|e| format!("write: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("rename: {e}"))
}

fn derive_title(content: &str) -> String {
    let line = content.lines().next().unwrap_or(content).trim();
    let mut t = line.chars().take(60).collect::<String>();
    if line.chars().count() > 60 {
        t.push('…');
    }
    if t.is_empty() { "Untitled".to_string() } else { t }
}

fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}
