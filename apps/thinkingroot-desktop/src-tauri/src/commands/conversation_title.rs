//! AI-generated conversation titles for the sidebar.
//!
//! After the first user/assistant exchange, the desktop asks the
//! configured workspace LLM (via the sidecar `/ask` route, no agent
//! tools) for a short session name. Failures are silent — the interim
//! title from the first user line stays in place.

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use crate::commands::conversations::{
    Conversation, ConversationSummary, conv_dir, lookup_workspace, read_conversation,
    upsert_index, write_conversation,
};
use crate::commands::sidecar_client::SidecarClient;

const MIN_MESSAGES_FOR_AI_TITLE: usize = 2;
const TITLE_MAX_CHARS: usize = 48;
const EXCERPT_MAX_CHARS: usize = 400;

#[derive(Debug, Deserialize)]
pub struct ConversationTitleArgs {
    pub workspace: String,
    pub conversation_id: String,
}

#[derive(Debug, Serialize)]
struct AskTitleRequest {
    question: String,
    #[serde(default)]
    session_scope: Vec<String>,
    #[serde(default)]
    use_agent: bool,
}

#[derive(Debug, Deserialize)]
struct AskTitleResponse {
    answer: String,
}

/// Generate a short sidebar title once the conversation has at least
/// one exchange. Returns `None` when skipped (already titled, too few
/// messages, LLM unavailable, or empty model output).
#[tauri::command]
pub async fn conversations_generate_title(
    app: AppHandle,
    args: ConversationTitleArgs,
) -> Result<Option<ConversationSummary>, String> {
    let entry = lookup_workspace(&args.workspace)?;
    let dir = conv_dir(&entry.path);
    let mut conv = read_conversation(&dir, &args.conversation_id)?;

    if !should_generate_title(&conv) {
        return Ok(None);
    }

    let prompt = build_title_prompt(&conv);
    let title = match fetch_title_from_sidecar(&app, &args.workspace, &prompt).await {
        Some(t) => t,
        None => return Ok(None),
    };

    conv.summary.title = title;
    conv.summary.title_ai_generated = true;
    conv.summary.updated_at = chrono::Utc::now();
    write_conversation(&dir, &conv)?;
    let summary = conv.summary.clone();
    upsert_index(&dir, summary.clone())?;
    let _ = app.emit("conversations-changed", true);
    Ok(Some(summary))
}

fn should_generate_title(conv: &Conversation) -> bool {
    if conv.summary.title_user_customized || conv.summary.title_ai_generated {
        return false;
    }
    if conv.messages.len() < MIN_MESSAGES_FOR_AI_TITLE {
        return false;
    }
    let has_user = conv.messages.iter().any(|m| m.role == "user");
    let has_assistant = conv.messages.iter().any(|m| m.role == "assistant");
    has_user && has_assistant
}

fn build_title_prompt(conv: &Conversation) -> String {
    let user = conv
        .messages
        .iter()
        .find(|m| m.role == "user")
        .map(|m| excerpt(&m.content))
        .unwrap_or_default();
    let assistant = conv
        .messages
        .iter()
        .find(|m| m.role == "assistant")
        .map(|m| excerpt(&m.content))
        .unwrap_or_default();

    format!(
        "You label chat sessions for a sidebar. Given this exchange, reply with ONLY a short title \
         (3–6 words, sentence case, no quotes, no trailing punctuation).\n\n\
         User: {user}\n\nAssistant: {assistant}\n\nTitle:"
    )
}

fn excerpt(text: &str) -> String {
    let flat: String = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if flat.chars().count() <= EXCERPT_MAX_CHARS {
        flat
    } else {
        let mut t: String = flat.chars().take(EXCERPT_MAX_CHARS).collect();
        t.push('…');
        t
    }
}

async fn fetch_title_from_sidecar(
    app: &AppHandle,
    workspace: &str,
    prompt: &str,
) -> Option<String> {
    let sc = SidecarClient::ensure_workspace(app, workspace).await.ok()?;
    let path = format!("/api/v1/ws/{}/ask", workspace);
    let body = AskTitleRequest {
        question: prompt.to_string(),
        session_scope: Vec::new(),
        use_agent: false,
    };
    let resp: AskTitleResponse = sc.post(&path, &body).await.ok()?;
    sanitize_title(&resp.answer)
}

pub(crate) fn sanitize_title(raw: &str) -> Option<String> {
    let mut t = raw.trim().to_string();
    if t.is_empty() {
        return None;
    }
    // Model sometimes echoes "Title: foo" despite instructions.
    if let Some(rest) = t.strip_prefix("Title:") {
        t = rest.trim().to_string();
    }
    if (t.starts_with('"') && t.ends_with('"')) || (t.starts_with('\'') && t.ends_with('\'')) {
        t = t[1..t.len().saturating_sub(1)].trim().to_string();
    }
    t = t.lines().next().unwrap_or(&t).trim().to_string();
    if t.is_empty() {
        return None;
    }
    if t.chars().count() > TITLE_MAX_CHARS {
        let mut short: String = t.chars().take(TITLE_MAX_CHARS).collect();
        short = short.trim_end().to_string();
        if !short.ends_with('…') {
            short.push('…');
        }
        t = short;
    }
    Some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_quotes_and_prefix() {
        assert_eq!(
            sanitize_title("Title: \"Compile speed fix\"").as_deref(),
            Some("Compile speed fix")
        );
    }

    #[test]
    fn sanitize_truncates_long_output() {
        let long = "a".repeat(80);
        let out = sanitize_title(&long).unwrap();
        assert!(out.chars().count() <= TITLE_MAX_CHARS + 1);
    }
}
