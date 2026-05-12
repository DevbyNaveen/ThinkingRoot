//! Save Page — turn the captive browser's current page into a source
//! file in a workspace and kick off an incremental compile.
//!
//! Flow:
//!
//! 1. UI calls [`browser_save_page`] with `view_id` (captive tab) and
//!    `workspace` (registered workspace name to save into; falls back
//!    to the playground when callers can't decide).
//! 2. We mint a random `request_id`, register a one-shot sender in
//!    [`AppState.pending_extracts`], and inject a bundle of
//!    `Readability.js` + `Turndown.js` + our `extract.js` orchestrator
//!    into the captive webview via `webview.eval()`. The
//!    orchestrator computes clean article markdown and calls back
//!    into [`browser_extract_callback`] via Tauri's IPC bridge
//!    (`window.__TAURI_INTERNALS__.invoke`). That bridge bypasses the
//!    captive page's CSP because it's a WebKit script-message-handler,
//!    not a network call — which is the only reason we can extract
//!    from sites like google.com whose `connect-src` would block a
//!    plain `fetch()`.
//! 3. We await the channel with a generous 20-second timeout (most
//!    extractions complete in <300 ms; long timeout protects against
//!    slow JS-heavy pages whose DOM is still settling).
//! 4. Hash-dedup: we BLAKE3 the extracted markdown and look for an
//!    existing `sources/*.md` file in the target workspace whose
//!    frontmatter `url:` matches. Three outcomes:
//!      - identical hash → no-op; return `AlreadySaved`.
//!      - different hash → write `<slug>-v2.md` (or v3…) AND stamp
//!        the old file with `superseded_by:` frontmatter so future
//!        scans skip it.
//!      - no existing match → write `<slug>-<yyyy-mm-dd>.md`.
//! 5. Spawn `workspace_compile` in the background so the new file
//!    flows through the Witness Mesh pipeline. Progress events ride
//!    the existing `workspace_compile_progress` channel — the UI
//!    listens for those and updates its toast.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
use thinkingroot_core::WorkspaceRegistry;
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::commands::workspaces::{workspace_compile, WorkspaceCompileArgs};
use crate::state::AppState;

const READABILITY_JS: &str = include_str!("../../resources/js/readability.min.js");
const TURNDOWN_JS: &str = include_str!("../../resources/js/turndown.js");
const EXTRACT_JS: &str = include_str!("../../resources/js/extract.js");

const EXTRACTION_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Deserialize)]
pub struct BrowserSavePageArgs {
    pub view_id: String,
    pub workspace: String,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum SaveStatus {
    /// Fresh write — no prior file matched this URL.
    Saved,
    /// A prior file existed for this URL with byte-identical markdown;
    /// we did not write a duplicate file or trigger a recompile.
    AlreadySaved,
    /// A prior file existed for this URL but its markdown has changed.
    /// We wrote a new `-v{N+1}` file and stamped the old file with
    /// `superseded_by:` so it stops showing up as "current" in scans.
    Updated,
}

#[derive(Debug, Serialize)]
pub struct BrowserSavePageResult {
    pub status: SaveStatus,
    /// Absolute path of the file we just wrote (or the existing file
    /// in the `AlreadySaved` case so the UI can offer "Open file").
    pub path: String,
    pub slug: String,
    pub title: String,
    pub url: String,
    pub workspace: String,
    /// BLAKE3 hex of the extracted markdown bytes. Surfaced so the UI
    /// can show it in the toast for "saved · `<short hash>`".
    pub content_hash: String,
    /// `Some(prior_path)` in the `Updated` case so the UI can offer
    /// "View previous version" without having to scan the directory
    /// itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prior_path: Option<String>,
}

/// Payload posted back from the captive webview via Tauri IPC. The
/// orchestrator either delivers a successful extraction OR an `error`
/// describing why it gave up — never both, but defensively we treat
/// `error: Some(...)` as authoritative.
#[derive(Debug, Clone, Deserialize)]
pub struct ExtractCallbackPayload {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub markdown: Option<String>,
    #[serde(default)]
    pub byline: Option<String>,
    #[serde(default)]
    pub site_name: Option<String>,
    #[serde(default)]
    pub excerpt: Option<String>,
    #[serde(default)]
    pub length: Option<u64>,
    #[serde(default)]
    pub error: Option<String>,
}

/// Captive-webview callback. Routes the payload to the pending oneshot
/// channel for the matching `request_id`. If no sender is registered
/// (timed out, double-call), the payload is dropped silently — the
/// caller's timeout path already handled the failure.
#[tauri::command]
pub async fn browser_extract_callback(
    app: AppHandle,
    request_id: String,
    payload: ExtractCallbackPayload,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let sender = state.pending_extracts.lock().await.remove(&request_id);
    if let Some(tx) = sender {
        // Receiver may have been dropped (timeout) — that's fine.
        let _ = tx.send(payload);
    } else {
        tracing::debug!(
            request_id = %request_id,
            "browser_extract_callback received with no pending receiver — likely a duplicate or post-timeout delivery"
        );
    }
    Ok(())
}

#[tauri::command]
pub async fn browser_save_page(
    app: AppHandle,
    args: BrowserSavePageArgs,
) -> Result<BrowserSavePageResult, String> {
    let state = app.state::<AppState>();

    // 1. Resolve the captive webview.
    let session = {
        let map = state.browsers.read().await;
        map.get(&args.view_id)
            .cloned()
            .ok_or_else(|| format!("no browser session `{}`", args.view_id))?
    };

    // 2. Resolve the target workspace's path.
    let registry = WorkspaceRegistry::load().map_err(|e| e.to_string())?;
    let entry = registry
        .workspaces
        .iter()
        .find(|w| w.name == args.workspace)
        .cloned()
        .ok_or_else(|| {
            format!(
                "workspace `{}` not registered — pass a name from workspace_list or use `playground`",
                args.workspace
            )
        })?;
    let workspace_name = entry.name.clone();
    let workspace_path = entry.path.clone();
    drop(registry);

    // 3. Register the pending channel and inject the extractor.
    let request_id = Uuid::new_v4().to_string();
    let (tx, rx) = oneshot::channel::<ExtractCallbackPayload>();
    state
        .pending_extracts
        .lock()
        .await
        .insert(request_id.clone(), tx);

    let js_request_id = serde_json::to_string(&request_id)
        .map_err(|e| format!("encode request id: {e}"))?;
    let bundled = format!(
        "(function(){{\n\
         try {{\n\
         {readability}\n\
         }} catch(_e){{ /* readability evaluated; ignore re-eval errors */ }}\n\
         try {{\n\
         {turndown}\n\
         }} catch(_e){{ /* turndown evaluated; ignore re-eval errors */ }}\n\
         window.__TR_EXTRACT_REQ_ID = {req_id};\n\
         {extract}\n\
         }})();",
        readability = READABILITY_JS,
        turndown = TURNDOWN_JS,
        req_id = js_request_id,
        extract = EXTRACT_JS,
    );
    if let Err(e) = session.webview.eval(&bundled) {
        // Clean up the pending entry so we don't leak senders.
        state.pending_extracts.lock().await.remove(&request_id);
        return Err(format!("inject extraction script: {e}"));
    }

    // 4. Await the callback with a timeout.
    let payload = match tokio::time::timeout(EXTRACTION_TIMEOUT, rx).await {
        Ok(Ok(p)) => p,
        Ok(Err(_recv_err)) => {
            return Err("extraction channel closed before payload arrived".to_string());
        }
        Err(_elapsed) => {
            // Drop any stale sender we may have left behind.
            state.pending_extracts.lock().await.remove(&request_id);
            return Err(format!(
                "Save timed out after {}s — the page may still be loading, try again",
                EXTRACTION_TIMEOUT.as_secs()
            ));
        }
    };

    if let Some(err) = payload.error.as_deref() {
        return Err(format!("Extraction failed: {err}"));
    }

    let title = payload
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| "Untitled".to_string());
    let url = payload
        .url
        .clone()
        .filter(|u| !u.trim().is_empty())
        .ok_or_else(|| "Extraction returned no URL — refusing to save without provenance".to_string())?;
    let markdown = payload
        .markdown
        .clone()
        .filter(|m| !m.trim().is_empty())
        .ok_or_else(|| "Extraction returned empty markdown — nothing to save".to_string())?;

    // 5. Compute content hash + work out the destination filename.
    let content_hash = blake3::hash(markdown.as_bytes()).to_hex().to_string();
    let sources_dir = workspace_path.join("sources");
    std::fs::create_dir_all(&sources_dir)
        .map_err(|e| format!("create sources/ at {}: {e}", sources_dir.display()))?;

    let slug = slugify(&title);
    let today = Utc::now().format("%Y-%m-%d").to_string();

    let existing = find_existing_for_url(&sources_dir, &url)
        .map_err(|e| format!("scan existing files: {e}"))?;

    let (status, save_name, prior_to_supersede) = match existing {
        Some(ExistingFile {
            path,
            content_hash: prior_hash,
            base_slug,
        }) if prior_hash == content_hash => {
            return Ok(BrowserSavePageResult {
                status: SaveStatus::AlreadySaved,
                path: path.to_string_lossy().into_owned(),
                slug,
                title,
                url,
                workspace: workspace_name,
                content_hash,
                prior_path: None,
            });
        }
        Some(ExistingFile {
            path, base_slug, ..
        }) => {
            let v = next_version_suffix(&sources_dir, &base_slug);
            let new_name = format!("{base_slug}-v{v}.md");
            (SaveStatus::Updated, new_name, Some(path))
        }
        None => {
            let name = format!("{slug}-{today}.md");
            (SaveStatus::Saved, name, None)
        }
    };

    let save_path = sources_dir.join(&save_name);
    let content = render_markdown_with_frontmatter(
        &title,
        &url,
        &content_hash,
        payload.byline.as_deref(),
        payload.site_name.as_deref(),
        payload.excerpt.as_deref(),
        payload.length,
        &markdown,
    );
    std::fs::write(&save_path, &content)
        .map_err(|e| format!("write {}: {e}", save_path.display()))?;

    let prior_path_str = if let Some(prior) = &prior_to_supersede {
        // Stamp the old file with `superseded_by:` so future
        // `find_existing_for_url` scans skip it. Best-effort —
        // failure to update the old file does not cancel the save.
        if let Ok(old_content) = std::fs::read_to_string(prior) {
            let updated = inject_frontmatter_kv(&old_content, "superseded_by", &save_name);
            if updated != old_content {
                let _ = std::fs::write(prior, updated);
            }
        }
        Some(prior.to_string_lossy().into_owned())
    } else {
        None
    };

    // 6. Kick off compile in the background. We don't await — the UI
    //    listens to the existing `workspace_compile_progress` event
    //    stream and updates its toast as Witnesses count up.
    let app_clone = app.clone();
    let ws_for_compile = workspace_name.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = workspace_compile(
            app_clone,
            WorkspaceCompileArgs {
                target: ws_for_compile,
                branch: None,
            },
        )
        .await
        {
            tracing::warn!(
                error = %e,
                "post-save compile dispatch failed — file is saved but Witness Mesh is stale"
            );
        }
    });

    Ok(BrowserSavePageResult {
        status,
        path: save_path.to_string_lossy().into_owned(),
        slug,
        title,
        url,
        workspace: workspace_name,
        content_hash,
        prior_path: prior_path_str,
    })
}

// ── Filename + frontmatter helpers ─────────────────────────────────

/// URL-safe slug from a free-form title. ASCII-lowercased, non-alnum
/// runs collapsed to single `-`, capped at 60 chars, never empty.
fn slugify(title: &str) -> String {
    let lower = title.to_lowercase();
    let mut s = String::with_capacity(lower.len());
    let mut last_dash = true;
    for c in lower.chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c);
            last_dash = false;
        } else if !last_dash {
            s.push('-');
            last_dash = true;
        }
    }
    while s.ends_with('-') {
        s.pop();
    }
    if s.is_empty() {
        s.push_str("page");
    }
    if s.len() > 60 {
        s.truncate(60);
        while s.ends_with('-') {
            s.pop();
        }
    }
    s
}

struct ExistingFile {
    path: PathBuf,
    content_hash: String,
    /// Base slug WITHOUT any trailing `-v{N}` version suffix — the
    /// canonical stem used to derive the next version filename.
    base_slug: String,
}

/// Scan `sources/` for a `*.md` whose frontmatter `url:` matches and
/// which has not been superseded. Returns the first match (filesystem
/// order; for a registry-of-one this is deterministic enough — if
/// duplicates ever exist the user can resolve manually).
fn find_existing_for_url(
    sources_dir: &Path,
    target_url: &str,
) -> Result<Option<ExistingFile>, String> {
    let entries = match std::fs::read_dir(sources_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("read_dir: {e}")),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some(fm) = parse_frontmatter(&content) else {
            continue;
        };
        if fm.contains_key("superseded_by") {
            continue;
        }
        let Some(file_url) = fm.get("url") else {
            continue;
        };
        if file_url == target_url {
            let content_hash = fm.get("content_hash").cloned().unwrap_or_default();
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("page")
                .to_string();
            let base_slug = strip_version_or_date_suffix(&stem);
            return Ok(Some(ExistingFile {
                path,
                content_hash,
                base_slug,
            }));
        }
    }
    Ok(None)
}

/// Minimal YAML frontmatter parser. Recognises only `key: value`
/// lines between two `---` delimiters at the start of the file.
/// Quotes are stripped if present. Multi-line values not supported
/// (we never write any).
fn parse_frontmatter(content: &str) -> Option<HashMap<String, String>> {
    let trimmed = content.trim_start_matches(['\u{feff}', '\n', '\r']);
    let rest = trimmed.strip_prefix("---")?;
    let rest = rest.trim_start_matches('\r').strip_prefix('\n')?;
    let end = rest.find("\n---")?;
    let body = &rest[..end];
    let mut map = HashMap::new();
    for line in body.lines() {
        let Some(colon) = line.find(':') else {
            continue;
        };
        let key = line[..colon].trim();
        let value = line[colon + 1..]
            .trim()
            .trim_matches('"')
            .trim_matches('\'');
        if !key.is_empty() {
            map.insert(key.to_string(), value.to_string());
        }
    }
    Some(map)
}

/// `"my-article-2026-05-12"` → `"my-article"`
/// `"my-article-v3"` → `"my-article"`
fn strip_version_or_date_suffix(stem: &str) -> String {
    // -vN suffix (one or two digits typical, defensively support more)
    if let Some(idx) = stem.rfind("-v") {
        let tail = &stem[idx + 2..];
        if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
            return stem[..idx].to_string();
        }
    }
    // Trailing -YYYY-MM-DD (always 10 chars wide)
    if stem.len() >= 11 {
        let split = stem.len() - 10;
        let (head, date_part) = stem.split_at(split);
        if let Some(head_stripped) = head.strip_suffix('-')
            && looks_like_iso_date(date_part)
        {
            return head_stripped.to_string();
        }
    }
    stem.to_string()
}

fn looks_like_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b[..4].iter().all(|c| c.is_ascii_digit())
        && b[5..7].iter().all(|c| c.is_ascii_digit())
        && b[8..10].iter().all(|c| c.is_ascii_digit())
}

/// Highest existing `<base_slug>-v{N}.md` in the directory, plus one.
/// Treats the un-versioned and dated filenames as implicit v1.
fn next_version_suffix(sources_dir: &Path, base_slug: &str) -> u32 {
    let mut highest = 1u32;
    let Ok(entries) = std::fs::read_dir(sources_dir) else {
        return 2;
    };
    for entry in entries.flatten() {
        let stem = entry
            .path()
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string());
        let Some(stem) = stem else { continue };
        let Some(rest) = stem.strip_prefix(base_slug) else {
            continue;
        };
        if let Some(v_part) = rest.strip_prefix("-v")
            && let Ok(n) = v_part.parse::<u32>()
            && n > highest
        {
            highest = n;
        }
    }
    highest + 1
}

fn render_markdown_with_frontmatter(
    title: &str,
    url: &str,
    content_hash: &str,
    byline: Option<&str>,
    site_name: Option<&str>,
    excerpt: Option<&str>,
    length: Option<u64>,
    markdown: &str,
) -> String {
    let captured_at = Utc::now().to_rfc3339();
    let mut s = String::with_capacity(markdown.len() + 512);
    s.push_str("---\n");
    s.push_str(&format!("title: {}\n", yaml_quote(title)));
    s.push_str(&format!("url: {}\n", yaml_quote(url)));
    s.push_str(&format!("captured_at: {captured_at}\n"));
    s.push_str(&format!("content_hash: {content_hash}\n"));
    if let Some(byline) = byline.filter(|s| !s.trim().is_empty()) {
        s.push_str(&format!("byline: {}\n", yaml_quote(byline)));
    }
    if let Some(site) = site_name.filter(|s| !s.trim().is_empty()) {
        s.push_str(&format!("site_name: {}\n", yaml_quote(site)));
    }
    if let Some(ex) = excerpt.filter(|s| !s.trim().is_empty()) {
        s.push_str(&format!("excerpt: {}\n", yaml_quote(ex)));
    }
    if let Some(len) = length {
        s.push_str(&format!("length: {len}\n"));
    }
    s.push_str("---\n\n");
    s.push_str(markdown.trim_end());
    s.push('\n');
    s
}

/// Quote a string as a YAML scalar safely. We always emit
/// double-quoted form to dodge ambiguity around special leading
/// chars (`#`, `-`, `:`, `?`, `*`, `&`, …). Newlines collapsed to
/// `\n` so the line stays on a single YAML line.
fn yaml_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Insert a `key: value` line into an existing file's frontmatter.
/// If the file has no frontmatter, returns the original unchanged.
/// If the key already exists, returns the original unchanged
/// (idempotent — we never overwrite an existing `superseded_by:`).
fn inject_frontmatter_kv(content: &str, key: &str, value: &str) -> String {
    let trimmed = content.trim_start_matches(['\u{feff}']);
    let Some(rest_after_first_marker) = trimmed.strip_prefix("---\n") else {
        return content.to_string();
    };
    // Check whether the key already exists in the block.
    if let Some(end_idx) = rest_after_first_marker.find("\n---") {
        let block = &rest_after_first_marker[..end_idx];
        let needle = format!("{key}:");
        if block.lines().any(|l| l.trim_start().starts_with(&needle)) {
            return content.to_string();
        }
    } else {
        return content.to_string();
    }
    let insert = format!("{key}: {}\n", yaml_quote(value));
    let prefix_len = trimmed.as_ptr() as usize - content.as_ptr() as usize;
    let head_end = prefix_len + "---\n".len();
    let mut out = String::with_capacity(content.len() + insert.len());
    out.push_str(&content[..head_end]);
    out.push_str(&insert);
    out.push_str(&content[head_end..]);
    out
}

// ── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn slugify_strips_punctuation_and_caps_length() {
        assert_eq!(slugify("Hello, World!"), "hello-world");
        assert_eq!(slugify("  ---   "), "page");
        let long = slugify(&"ab".repeat(40));
        assert!(long.len() <= 60);
    }

    #[test]
    fn strip_version_or_date_suffix_handles_both_forms() {
        assert_eq!(
            strip_version_or_date_suffix("my-article-2026-05-12"),
            "my-article"
        );
        assert_eq!(strip_version_or_date_suffix("my-article-v3"), "my-article");
        assert_eq!(strip_version_or_date_suffix("my-article"), "my-article");
    }

    #[test]
    fn parse_frontmatter_extracts_quoted_and_unquoted_values() {
        let content =
            "---\ntitle: \"A page\"\nurl: https://example.com/x\ncontent_hash: deadbeef\n---\n\nbody";
        let fm = parse_frontmatter(content).expect("frontmatter parses");
        assert_eq!(fm.get("title").map(String::as_str), Some("A page"));
        assert_eq!(
            fm.get("url").map(String::as_str),
            Some("https://example.com/x")
        );
        assert_eq!(fm.get("content_hash").map(String::as_str), Some("deadbeef"));
    }

    #[test]
    fn find_existing_for_url_returns_match_and_skips_superseded() {
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("a-2026-05-10.md");
        let b = dir.path().join("b-2026-05-11.md");
        std::fs::write(
            &a,
            "---\nurl: https://example.com/x\ncontent_hash: aaaa\nsuperseded_by: a-v2.md\n---\n\nbody",
        )
        .unwrap();
        std::fs::write(
            &b,
            "---\nurl: https://example.com/x\ncontent_hash: bbbb\n---\n\nbody",
        )
        .unwrap();
        let hit = find_existing_for_url(dir.path(), "https://example.com/x")
            .unwrap()
            .expect("should match the non-superseded entry");
        assert_eq!(hit.content_hash, "bbbb");
        assert_eq!(hit.base_slug, "b");
    }

    #[test]
    fn inject_frontmatter_kv_is_idempotent() {
        let original = "---\ntitle: x\nurl: y\n---\n\nbody";
        let once = inject_frontmatter_kv(original, "superseded_by", "next.md");
        assert!(once.contains("superseded_by: \"next.md\""));
        let twice = inject_frontmatter_kv(&once, "superseded_by", "other.md");
        assert_eq!(once, twice, "second call must be a no-op");
    }
}
