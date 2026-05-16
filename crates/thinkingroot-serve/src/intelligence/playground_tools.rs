// crates/thinkingroot-serve/src/intelligence/playground_tools.rs
//
// Playground action verbs exposed to the AI — Phase α of the Cognition
// Commits design doc (`docs/2026-05-15-cognition-commits-design.md`).
//
// Closes the gap where the human-facing Playground had 23 Tauri commands
// the AI could not invoke. Each verb here:
//
//   * Has a pure `_in_path` helper taking a workspace root `&Path` —
//     unit-testable without a mounted `QueryEngine`.
//   * Has a thin `_impl` wrapper that resolves the workspace path via
//     `QueryEngine::workspace_root_path`. Both MCP dispatch
//     (`mcp/tools.rs`) and the in-app `ToolRegistry`
//     (`intelligence/builtin_tools.rs`) call the wrapper — single
//     source of truth for the side-effecting logic.
//   * Is registered as a write tool — both surfaces route through their
//     respective approval gates before dispatch.
//   * Returns an honest `Outcome` with explicit counts / `created` flags
//     instead of silent success — CLAUDE.md honesty rules apply.
//
// Verbs in this commit (Phase α):
//   - `save_note`         — atomic markdown write to `notes/<slug>-<date>.md`
//   - `regenerate_paper`  — re-synthesize Living Paper without full compile
//   - `ingest_path`       — copy files from a host path into `inbox/`
//
// Deferred to next session: `organize_files`, `trash_files`, `export_tr`,
// `list_directory`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{Value, json};
use thinkingroot_llm::llm::Tool;
use tokio::sync::RwLock;

use crate::engine::QueryEngine;
use crate::intelligence::tools::{ToolHandler, ToolHandlerResult};

// ─────────────────────────────────────────────────────────────────
// save_note
// ─────────────────────────────────────────────────────────────────

/// Outcome of [`save_note_impl`]. `created` is `false` when an
/// identically-named note already existed and we refused to overwrite —
/// surfaced honestly so callers can decide whether to retry with a
/// different title rather than discover the no-op via filesystem stat.
#[derive(Debug, Clone, Serialize)]
pub struct SaveNoteOutcome {
    /// Absolute on-disk path of the (possibly pre-existing) file.
    pub path: String,
    /// Workspace-relative path, forward-slash separated.
    pub relative_path: String,
    /// Bytes written. Zero when `created == false`.
    pub bytes: u64,
    /// `true` when this call created the file, `false` when an
    /// identically-named file already existed.
    pub created: bool,
}

/// Write a note into `<workspace_root>/notes/<slug>-<YYYY-MM-DD>.md`.
/// Pure-FS variant suitable for unit tests — does not consult any
/// `QueryEngine`. Most callers want [`save_note_impl`].
///
/// Atomic write via tempfile + rename. Refuses to overwrite an
/// existing note; the workspace's compile path honours `notes/` like
/// any source directory so a same-day re-save with a different body
/// would re-extract conflicting witnesses without honest provenance.
pub async fn save_note_in_path(
    workspace_root: PathBuf,
    workspace_name: String,
    title: String,
    body: String,
) -> Result<SaveNoteOutcome, String> {
    if title.trim().is_empty() {
        return Err("save_note: title must not be empty".to_string());
    }
    if body.trim().is_empty() {
        return Err("save_note: body must not be empty".to_string());
    }
    tokio::task::spawn_blocking(move || -> Result<SaveNoteOutcome, String> {
        let notes_dir = workspace_root.join("notes");
        std::fs::create_dir_all(&notes_dir)
            .map_err(|e| format!("save_note: create notes dir: {e}"))?;
        let slug = slugify(&title);
        let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let filename = format!("{slug}-{date}.md");
        let path = notes_dir.join(&filename);
        if path.exists() {
            return Ok(SaveNoteOutcome {
                path: path.to_string_lossy().into_owned(),
                relative_path: format!("notes/{filename}"),
                bytes: 0,
                created: false,
            });
        }
        let frontmatter = format!(
            "---\ntitle: {title_yaml}\ncreated_at: {ts}\nkind: chat-note\nworkspace: {workspace_name}\n---\n\n",
            title_yaml = yaml_quote(&title),
            ts = chrono::Utc::now().to_rfc3339(),
        );
        let payload = format!("{frontmatter}{body}\n");
        let bytes = payload.len() as u64;
        let tmp = notes_dir.join(format!("{filename}.tmp"));
        std::fs::write(&tmp, payload.as_bytes())
            .map_err(|e| format!("save_note: write tmp {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path).map_err(|e| {
            // Best-effort tmp cleanup on rename failure so we don't
            // leave half-written stragglers in notes/.
            let _ = std::fs::remove_file(&tmp);
            format!(
                "save_note: rename {} -> {}: {e}",
                tmp.display(),
                path.display()
            )
        })?;
        Ok(SaveNoteOutcome {
            path: path.to_string_lossy().into_owned(),
            relative_path: format!("notes/{filename}"),
            bytes,
            created: true,
        })
    })
    .await
    .map_err(|e| format!("save_note: task panicked: {e}"))?
}

/// Resolve the workspace root from the engine, then delegate to
/// [`save_note_in_path`]. Surface for MCP + in-app callers.
pub async fn save_note_impl(
    engine: &QueryEngine,
    workspace: &str,
    title: &str,
    body: &str,
) -> Result<SaveNoteOutcome, String> {
    let root = engine
        .workspace_root_path(workspace)
        .ok_or_else(|| format!("save_note: workspace `{workspace}` not mounted"))?;
    save_note_in_path(
        root,
        workspace.to_string(),
        title.to_string(),
        body.to_string(),
    )
    .await
}

/// Lowercases, collapses runs of non-alphanumerics into `-`, trims
/// trailing dashes, caps at 60 chars. Empty input collapses to "note".
/// Mirrors the slug rules in the Tauri `playground.rs` and
/// `browser_save.rs` commands so notes the AI writes interleave
/// cleanly with notes the user writes via Playground.
fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_dash = true;
    for c in input.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("note");
    }
    if out.len() > 60 {
        out.truncate(60);
        while out.ends_with('-') {
            out.pop();
        }
    }
    out
}

/// Double-quoted YAML scalar with `\"`, `\\`, `\n`, `\r`, `\t` escapes
/// — dodges special-leading-char ambiguity (`#`, `-`, `:`, `?`, `*`,
/// `&`). Single-line by construction.
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

// ─────────────────────────────────────────────────────────────────
// regenerate_paper
// ─────────────────────────────────────────────────────────────────

/// Outcome of [`regenerate_paper_impl`]. Slim projection of
/// `thinkingroot_paper::PaperOutput` so the wire payload stays small
/// (the full markdown is recoverable from disk via
/// `<root>/.thinkingroot/paper.md` — the agent rarely needs the body
/// inline and including it would bloat tool-result tokens).
#[derive(Debug, Clone, Serialize)]
pub struct RegeneratePaperOutcome {
    /// Absolute path of the regenerated paper.
    pub path: String,
    /// Total bytes written.
    pub byte_length: u64,
    /// Section count in the rendered paper (deterministic skeleton +
    /// AI narrative combined). Sourced from
    /// `PaperOutput.frontmatter.sections.len()` — matches the REST
    /// `paper_regenerate_handler` response shape.
    pub sections: u32,
}

/// Re-synthesize the Living Paper for a workspace without driving a
/// full compile. Delegates to [`QueryEngine::regenerate_paper`] which
/// owns the LLM-vs-deterministic branch and the atomic write to disk.
pub async fn regenerate_paper_impl(
    engine: &QueryEngine,
    workspace: &str,
) -> Result<RegeneratePaperOutcome, String> {
    let root = engine
        .workspace_root_path(workspace)
        .ok_or_else(|| format!("regenerate_paper: workspace `{workspace}` not mounted"))?;
    let out = engine
        .regenerate_paper(workspace)
        .await
        .map_err(|e| format!("regenerate_paper: {e}"))?;
    Ok(RegeneratePaperOutcome {
        path: root
            .join(".thinkingroot")
            .join("paper.md")
            .to_string_lossy()
            .into_owned(),
        byte_length: out.byte_length as u64,
        sections: out.frontmatter.sections.len() as u32,
    })
}

// ─────────────────────────────────────────────────────────────────
// ingest_path
// ─────────────────────────────────────────────────────────────────

/// Outcome of [`ingest_path_impl`]. Honest counts so callers can
/// surface a precise summary ("3 copied, 1 skipped — already in inbox/")
/// rather than a fabricated "N files ingested". `destination_paths`
/// records workspace-relative paths in copy order so a follow-up
/// `compile` call (or the Source Library refresh) has the exact set to
/// scan.
#[derive(Debug, Clone, Serialize)]
pub struct IngestPathOutcome {
    pub copied: u64,
    pub skipped_duplicate: u64,
    pub skipped_unreadable: u64,
    pub destination_paths: Vec<String>,
}

/// Pure-FS variant of [`ingest_path_impl`] suitable for unit tests.
/// Copies files from `source_path` into `<workspace_root>/inbox/`. See
/// [`ingest_path_impl`] for the full contract.
pub async fn ingest_path_in_path(
    workspace_root: PathBuf,
    source_path: String,
) -> Result<IngestPathOutcome, String> {
    let source = PathBuf::from(&source_path);
    if !source.is_absolute() {
        return Err(format!(
            "ingest_path: `{source_path}` must be an absolute path"
        ));
    }
    if !source.exists() {
        return Err(format!(
            "ingest_path: source `{source_path}` does not exist"
        ));
    }

    tokio::task::spawn_blocking(move || -> Result<IngestPathOutcome, String> {
        let inbox = workspace_root.join("inbox");
        std::fs::create_dir_all(&inbox)
            .map_err(|e| format!("ingest_path: create inbox: {e}"))?;

        let files: Vec<PathBuf> = if source.is_file() {
            vec![source.clone()]
        } else if source.is_dir() {
            let mut out: Vec<PathBuf> = Vec::new();
            let read = std::fs::read_dir(&source)
                .map_err(|e| format!("ingest_path: read_dir `{}`: {e}", source.display()))?;
            for entry in read.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                // Top-level only; skip hidden files (dot-prefix) and
                // symlinks (would expand the surface area unhonestly).
                if name_str.starts_with('.') {
                    continue;
                }
                let meta = match entry.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if !meta.is_file() || meta.file_type().is_symlink() {
                    continue;
                }
                out.push(entry.path());
            }
            out
        } else {
            return Err(format!(
                "ingest_path: source `{}` is neither a file nor a directory",
                source.display()
            ));
        };

        let mut copied = 0u64;
        let mut skipped_duplicate = 0u64;
        let mut skipped_unreadable = 0u64;
        let mut destination_paths: Vec<String> = Vec::new();

        for src in files {
            let filename = match src.file_name() {
                Some(n) => n.to_owned(),
                None => {
                    skipped_unreadable += 1;
                    continue;
                }
            };
            let dest = inbox.join(&filename);
            if dest.exists() {
                skipped_duplicate += 1;
                continue;
            }
            match std::fs::copy(&src, &dest) {
                Ok(_) => {
                    copied += 1;
                    destination_paths
                        .push(format!("inbox/{}", filename.to_string_lossy()));
                }
                Err(e) => {
                    skipped_unreadable += 1;
                    tracing::warn!(
                        src = %src.display(),
                        error = %e,
                        "ingest_path: skipping unreadable source"
                    );
                }
            }
        }

        Ok(IngestPathOutcome {
            copied,
            skipped_duplicate,
            skipped_unreadable,
            destination_paths,
        })
    })
    .await
    .map_err(|e| format!("ingest_path: task panicked: {e}"))?
}

/// Copy files from an absolute host path into the workspace's `inbox/`
/// directory. Does NOT trigger a compile — the agent's next turn
/// invokes the existing `compile` MCP tool when it wants the Witness
/// Mesh refreshed. Decomposing the verbs lets the agent decide whether
/// to ingest-then-think or ingest-then-compile-then-think.
///
/// Rules:
///   - `source_path` must be an absolute path that exists.
///   - When `source_path` is a single file, copies that file. When it
///     is a directory, copies every non-hidden, non-symlink regular
///     file at the TOP LEVEL only (no recursion).
///   - Same-name collisions are skipped (no overwrite).
///   - Unreadable sources are logged + counted, not fatal.
pub async fn ingest_path_impl(
    engine: &QueryEngine,
    workspace: &str,
    source_path: &str,
) -> Result<IngestPathOutcome, String> {
    let root = engine
        .workspace_root_path(workspace)
        .ok_or_else(|| format!("ingest_path: workspace `{workspace}` not mounted"))?;
    ingest_path_in_path(root, source_path.to_string()).await
}

// ─────────────────────────────────────────────────────────────────
// ToolHandler wrappers (in-app agent surface)
// ─────────────────────────────────────────────────────────────────

/// Shared context for all three Playground tool handlers. Carries the
/// engine + workspace name the in-app agent is operating in. The MCP
/// surface uses the impl helpers directly with `&QueryEngine` from
/// `handle_call` — no `PlaygroundToolContext` round-trip there.
#[derive(Clone)]
pub struct PlaygroundToolContext {
    pub engine: Arc<RwLock<QueryEngine>>,
    pub workspace: String,
}

pub struct SaveNoteTool {
    ctx: PlaygroundToolContext,
}

impl SaveNoteTool {
    pub fn new(ctx: PlaygroundToolContext) -> Self {
        Self { ctx }
    }
    pub fn spec() -> Tool {
        Tool::new(
            "save_note",
            "Save a markdown body as a note under <workspace>/notes/<slug>-<date>.md with YAML frontmatter (title, created_at, kind=chat-note). Use this to persist a synthesised reply, a meeting summary, or any AI-authored markdown into the workspace so the next compile picks it up as a source. Refuses to overwrite an existing same-day note — surfaces `created: false` so you can retry with a different title.",
            json!({
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "Short human-readable title. Becomes the slug + YAML frontmatter title. Non-alphanumeric chars collapse to '-'."
                    },
                    "body": {
                        "type": "string",
                        "description": "Markdown body. Citation chips ([[witness:<id>]]) are honoured by the next compile pass."
                    }
                },
                "required": ["title", "body"]
            }),
        )
    }
}

#[async_trait]
impl ToolHandler for SaveNoteTool {
    async fn handle(&self, input: Value) -> ToolHandlerResult {
        let Some(title) = input.get("title").and_then(|v| v.as_str()) else {
            return ToolHandlerResult::error("save_note: missing required `title`");
        };
        let Some(body) = input.get("body").and_then(|v| v.as_str()) else {
            return ToolHandlerResult::error("save_note: missing required `body`");
        };
        let engine = self.ctx.engine.read().await;
        match save_note_impl(&engine, &self.ctx.workspace, title, body).await {
            Ok(outcome) => serialize_outcome("save_note", &outcome),
            Err(e) => ToolHandlerResult::error(e),
        }
    }
}

pub struct RegeneratePaperTool {
    ctx: PlaygroundToolContext,
}

impl RegeneratePaperTool {
    pub fn new(ctx: PlaygroundToolContext) -> Self {
        Self { ctx }
    }
    pub fn spec() -> Tool {
        Tool::new(
            "regenerate_paper",
            "Re-synthesize the Living Paper (`<workspace>/.thinkingroot/paper.md`) against the current Witness Mesh state. Cheap relative to `compile` — no parse/extract/ground passes, just the synthesizer + (when an LLM is configured) the AI-narrative sections. Use after the user adds a few notes or witnesses and wants the paper refreshed without paying the cost of a full compile.",
            json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        )
    }
}

#[async_trait]
impl ToolHandler for RegeneratePaperTool {
    async fn handle(&self, _input: Value) -> ToolHandlerResult {
        let engine = self.ctx.engine.read().await;
        match regenerate_paper_impl(&engine, &self.ctx.workspace).await {
            Ok(outcome) => serialize_outcome("regenerate_paper", &outcome),
            Err(e) => ToolHandlerResult::error(e),
        }
    }
}

pub struct IngestPathTool {
    ctx: PlaygroundToolContext,
}

impl IngestPathTool {
    pub fn new(ctx: PlaygroundToolContext) -> Self {
        Self { ctx }
    }
    pub fn spec() -> Tool {
        Tool::new(
            "ingest_path",
            "Copy files from an absolute host path into the workspace's `inbox/` directory. When `source_path` is a single file, copies it. When it is a directory, copies every non-hidden top-level regular file (no recursion). Does NOT trigger a compile — call the `compile` tool next when you want the Witness Mesh refreshed. Same-name files are skipped (no overwrite); honest counts surfaced as copied / skipped_duplicate / skipped_unreadable.",
            json!({
                "type": "object",
                "properties": {
                    "source_path": {
                        "type": "string",
                        "description": "Absolute path on the user's machine (e.g. /Users/alice/Desktop/papers/ or /tmp/notes.md). Relative paths are refused."
                    }
                },
                "required": ["source_path"]
            }),
        )
    }
}

#[async_trait]
impl ToolHandler for IngestPathTool {
    async fn handle(&self, input: Value) -> ToolHandlerResult {
        let Some(source_path) = input.get("source_path").and_then(|v| v.as_str()) else {
            return ToolHandlerResult::error("ingest_path: missing required `source_path`");
        };
        let engine = self.ctx.engine.read().await;
        match ingest_path_impl(&engine, &self.ctx.workspace, source_path).await {
            Ok(outcome) => serialize_outcome("ingest_path", &outcome),
            Err(e) => ToolHandlerResult::error(e),
        }
    }
}

/// Serialise a tool outcome to the agent-visible content string.
/// Pretty-printed JSON so the LLM sees structured fields without a
/// custom decoder; matches the `mcp_text_result` pattern on the MCP
/// surface so both adapters produce byte-equivalent results.
fn serialize_outcome<T: Serialize>(tool: &str, outcome: &T) -> ToolHandlerResult {
    match serde_json::to_string_pretty(outcome) {
        Ok(s) => ToolHandlerResult::ok(s),
        Err(e) => ToolHandlerResult::error(format!("{tool}: serialize outcome: {e}")),
    }
}

/// Parse an optional JSON array of 64-char lower-hex witness ids
/// into typed `WitnessId`s. Used by the `commit_cognition` MCP + in-app
/// handlers to project `witnesses_added` / `citations` payloads with a
/// single error path. Empty / null / absent → `Vec::new()`; non-array
/// or invalid entries → typed `Err(String)`.
pub fn parse_witness_ids(
    v: Option<&Value>,
) -> std::result::Result<Vec<thinkingroot_core::types::WitnessId>, String> {
    use thinkingroot_core::types::WitnessId;
    match v {
        None => Ok(Vec::new()),
        Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(arr)) => {
            let mut out: Vec<WitnessId> = Vec::with_capacity(arr.len());
            for item in arr {
                let s = item
                    .as_str()
                    .ok_or_else(|| "every entry must be a 64-char hex string".to_string())?;
                let id = WitnessId::from_hex(s)
                    .map_err(|e| format!("invalid witness id `{s}`: {e}"))?;
                out.push(id);
            }
            Ok(out)
        }
        _ => Err("must be an array of hex strings or omitted".to_string()),
    }
}

/// Validate that an absolute host path is safe enough to read.
/// Currently a placeholder that defers to `ingest_path_in_path`'s own
/// is-absolute / exists checks — exposed as a separate function so a
/// future allow-list / approval-policy can hook here without changing
/// every call site. NOT yet load-bearing in α.
pub fn validate_host_path(p: &Path) -> Result<(), String> {
    if !p.is_absolute() {
        return Err(format!("path `{}` must be absolute", p.display()));
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────
// Shared safe-path helper (workspace-scoped)
// ─────────────────────────────────────────────────────────────────

/// Resolve a workspace-relative `rel` against `root`, refusing every
/// path that would escape via `..`, absolute components, or device
/// prefixes. Canonicalises both root and candidate; verifies
/// `starts_with`. For not-yet-existing destinations (create-folder,
/// move-destination) validates the parent canonicalises within root.
///
/// Mirrors the discipline of `playground_fs::safe_path_within` so the
/// Tauri-side and AI-side path validation agree byte-for-byte.
fn safe_path_within(root: &Path, rel: &str) -> Result<PathBuf, String> {
    use std::path::Component;
    if rel.is_empty() {
        return root
            .canonicalize()
            .map_err(|e| format!("canonicalize workspace root: {e}"));
    }
    let candidate = Path::new(rel);
    for component in candidate.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => return Err(format!("invalid path component in `{rel}`")),
        }
    }
    let joined = root.join(candidate);
    let canonical_root = root
        .canonicalize()
        .map_err(|e| format!("canonicalize workspace root: {e}"))?;
    match joined.canonicalize() {
        Ok(canon) => {
            if !canon.starts_with(&canonical_root) {
                return Err(format!(
                    "resolved path `{}` escapes workspace root",
                    canon.display()
                ));
            }
            Ok(canon)
        }
        Err(_) => {
            // Pending destination — verify parent canonicalises within.
            let parent = joined
                .parent()
                .ok_or_else(|| format!("path `{rel}` has no parent"))?;
            let canon_parent = parent
                .canonicalize()
                .map_err(|e| format!("canonicalize parent `{}`: {e}", parent.display()))?;
            if !canon_parent.starts_with(&canonical_root) {
                return Err(format!(
                    "parent of `{rel}` escapes workspace root: {}",
                    canon_parent.display()
                ));
            }
            let leaf = joined
                .file_name()
                .ok_or_else(|| format!("path `{rel}` has no leaf component"))?;
            Ok(canon_parent.join(leaf))
        }
    }
}

/// Variant of `safe_path_within` for pending destinations that may
/// nest one or more directories deeper than any existing ancestor —
/// e.g. moving `inbox/foo.md` to `sources/2026/foo.md` where neither
/// `sources/` nor `sources/2026/` exists yet. Walks up the joined
/// candidate's ancestors until it finds an existing one, then
/// canonicalises and verifies that ancestor is inside the root. The
/// caller is responsible for `create_dir_all`-ing the missing
/// intermediates before the actual `fs::rename`.
///
/// Used only by `organize_files_in_path` — the strict
/// `safe_path_within` is the default everywhere else so a typo can't
/// accidentally create an arbitrarily-deep tree.
fn safe_pending_dest_within(root: &Path, rel: &str) -> Result<PathBuf, String> {
    use std::path::Component;
    if rel.is_empty() {
        return Err("destination must not be empty".to_string());
    }
    let candidate = Path::new(rel);
    for component in candidate.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => return Err(format!("invalid path component in `{rel}`")),
        }
    }
    let joined = root.join(candidate);
    let canonical_root = root
        .canonicalize()
        .map_err(|e| format!("canonicalize workspace root: {e}"))?;

    let mut ancestor: &Path = joined.as_path();
    loop {
        if ancestor.exists() {
            break;
        }
        match ancestor.parent() {
            Some(p) => ancestor = p,
            None => return Err(format!("path `{rel}` has no existing ancestor")),
        }
    }
    let canon_ancestor = ancestor
        .canonicalize()
        .map_err(|e| format!("canonicalize ancestor `{}`: {e}", ancestor.display()))?;
    if !canon_ancestor.starts_with(&canonical_root) {
        return Err(format!(
            "path `{rel}` escapes workspace root (ancestor: {})",
            canon_ancestor.display()
        ));
    }
    Ok(joined)
}

/// Compute a workspace-relative path for an absolute path under root,
/// using forward slashes regardless of OS so frontend `split("/")`
/// parsing stays portable. Falls back to the absolute path string when
/// the input isn't a strict descendant of root.
fn rel_to_workspace(absolute: &Path, root: &Path) -> String {
    use std::path::Component;
    let canon_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    match absolute.strip_prefix(&canon_root) {
        Ok(rel) => rel
            .components()
            .map(|c| match c {
                Component::Normal(s) => s.to_string_lossy().into_owned(),
                _ => String::new(),
            })
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("/"),
        Err(_) => absolute.to_string_lossy().into_owned(),
    }
}

// ─────────────────────────────────────────────────────────────────
// list_directory
// ─────────────────────────────────────────────────────────────────

/// One row in a directory listing. Slim projection of
/// `playground_fs::PlaygroundDirEntry` for the AI surface: drops the
/// extension-classification `kind` because the AI rarely needs an icon
/// taxonomy and including it bloats tool-result tokens for large dirs.
#[derive(Debug, Clone, Serialize)]
pub struct ListedEntry {
    pub name: String,
    pub rel_path: String,
    pub is_dir: bool,
    pub size_bytes: u64,
}

/// Outcome of [`list_directory_impl`]. `parent_rel_path` is `None`
/// when the listing target is the workspace root, else it carries the
/// rel-path one level up so the AI can navigate breadcrumb-style.
#[derive(Debug, Clone, Serialize)]
pub struct ListDirectoryOutcome {
    pub rel_path: String,
    pub parent_rel_path: Option<String>,
    pub entries: Vec<ListedEntry>,
}

/// List the immediate children of `<workspace_root>/<rel_path>`.
/// Hides dotfiles (names starting with `.`) at every level so the AI
/// never accidentally enumerates engine-managed state like
/// `.thinkingroot/` or VCS metadata. Folders sort first, then files,
/// alphabetical within each group — matches Finder/VS Code intuition.
pub async fn list_directory_in_path(
    workspace_root: PathBuf,
    rel_path: String,
) -> Result<ListDirectoryOutcome, String> {
    use std::path::Component;
    tokio::task::spawn_blocking(move || -> Result<ListDirectoryOutcome, String> {
        let canonical_root = workspace_root
            .canonicalize()
            .map_err(|e| format!("list_directory: canonicalize root: {e}"))?;
        let target = if rel_path.is_empty() {
            canonical_root.clone()
        } else {
            safe_path_within(&workspace_root, &rel_path)?
        };
        if !target.is_dir() {
            return Err(format!(
                "list_directory: `{}` is not a directory",
                target.display()
            ));
        }
        let read = std::fs::read_dir(&target)
            .map_err(|e| format!("list_directory: read_dir `{}`: {e}", target.display()))?;
        let mut entries: Vec<ListedEntry> = Vec::new();
        for entry in read.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let path = entry.path();
            let is_dir = meta.is_dir();
            let size_bytes = if is_dir { 0 } else { meta.len() };
            let rel = rel_to_workspace(&path, &canonical_root);
            entries.push(ListedEntry {
                name,
                rel_path: rel,
                is_dir,
                size_bytes,
            });
        }
        entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        });
        let parent_rel_path = if rel_path.is_empty() {
            None
        } else {
            let p = Path::new(&rel_path)
                .parent()
                .map(|p| {
                    p.components()
                        .map(|c| match c {
                            Component::Normal(s) => s.to_string_lossy().into_owned(),
                            _ => String::new(),
                        })
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join("/")
                })
                .unwrap_or_default();
            Some(p)
        };
        Ok(ListDirectoryOutcome {
            rel_path,
            parent_rel_path,
            entries,
        })
    })
    .await
    .map_err(|e| format!("list_directory: task panicked: {e}"))?
}

/// Resolve the workspace and delegate to [`list_directory_in_path`].
pub async fn list_directory_impl(
    engine: &QueryEngine,
    workspace: &str,
    rel_path: &str,
) -> Result<ListDirectoryOutcome, String> {
    let root = engine
        .workspace_root_path(workspace)
        .ok_or_else(|| format!("list_directory: workspace `{workspace}` not mounted"))?;
    list_directory_in_path(root, rel_path.to_string()).await
}

// ─────────────────────────────────────────────────────────────────
// organize_files
// ─────────────────────────────────────────────────────────────────

/// One move operation in a batch `organize_files` call. `from` and `to`
/// are both workspace-relative paths; both must canonically resolve
/// inside the workspace root or the op is `skipped_invalid`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct OrganizeOp {
    pub from: String,
    pub to: String,
}

/// One row in [`OrganizeOutcome::moves`]. Records what actually moved
/// (for the AI's next-turn reasoning) and how the destination resolved.
#[derive(Debug, Clone, Serialize)]
pub struct OrganizeMove {
    pub from: String,
    pub to: String,
}

/// Outcome of [`organize_files_impl`]. Counts mirror
/// `PlaygroundMoveOutcome` — honest skips so the agent can decide
/// whether to retry, rename, or surface the conflict to the user.
#[derive(Debug, Clone, Serialize)]
pub struct OrganizeOutcome {
    pub moved: u64,
    pub skipped_conflict: u64,
    pub skipped_invalid: u64,
    pub moves: Vec<OrganizeMove>,
}

/// Atomically rename/move each op's `from` to `to` inside the
/// workspace. Conflicts (target exists, source missing, path escapes
/// root, would-move-folder-into-itself) are skipped with honest
/// counts. Uses `fs::rename` which is atomic on the same filesystem;
/// cross-filesystem moves return `skipped_invalid` rather than fall
/// back to copy-then-delete (the agent should explicitly call
/// `ingest_path` + `trash_files` if cross-FS is the intent).
pub async fn organize_files_in_path(
    workspace_root: PathBuf,
    ops: Vec<OrganizeOp>,
) -> Result<OrganizeOutcome, String> {
    tokio::task::spawn_blocking(move || -> Result<OrganizeOutcome, String> {
        let canonical_root = workspace_root
            .canonicalize()
            .map_err(|e| format!("organize_files: canonicalize root: {e}"))?;
        let mut moved = 0u64;
        let mut skipped_conflict = 0u64;
        let mut skipped_invalid = 0u64;
        let mut moves: Vec<OrganizeMove> = Vec::new();

        for op in ops {
            if op.from.is_empty() || op.to.is_empty() {
                skipped_invalid += 1;
                continue;
            }
            let from = match safe_path_within(&workspace_root, &op.from) {
                Ok(p) => p,
                Err(_) => {
                    skipped_invalid += 1;
                    continue;
                }
            };
            if !from.exists() {
                skipped_invalid += 1;
                continue;
            }
            let to = match safe_pending_dest_within(&workspace_root, &op.to) {
                Ok(p) => p,
                Err(_) => {
                    skipped_invalid += 1;
                    continue;
                }
            };
            if to.exists() {
                skipped_conflict += 1;
                continue;
            }
            // Refuse moving a folder into itself.
            if from.is_dir() && to.starts_with(&from) {
                skipped_invalid += 1;
                continue;
            }
            // Create any missing intermediate directories. The lenient
            // `safe_pending_dest_within` validated that the deepest
            // existing ancestor is inside root, so this create_dir_all
            // cannot escape.
            if let Some(parent) = to.parent()
                && !parent.exists()
            {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    tracing::warn!(
                        parent = %parent.display(),
                        error = %e,
                        "organize_files: failed to create parent dir"
                    );
                    skipped_invalid += 1;
                    continue;
                }
            }
            match std::fs::rename(&from, &to) {
                Ok(()) => {
                    moved += 1;
                    moves.push(OrganizeMove {
                        from: rel_to_workspace(&from, &canonical_root),
                        to: rel_to_workspace(&to, &canonical_root),
                    });
                }
                Err(_) => {
                    skipped_invalid += 1;
                }
            }
        }

        Ok(OrganizeOutcome {
            moved,
            skipped_conflict,
            skipped_invalid,
            moves,
        })
    })
    .await
    .map_err(|e| format!("organize_files: task panicked: {e}"))?
}

/// Resolve workspace and delegate to [`organize_files_in_path`].
pub async fn organize_files_impl(
    engine: &QueryEngine,
    workspace: &str,
    ops: Vec<OrganizeOp>,
) -> Result<OrganizeOutcome, String> {
    let root = engine
        .workspace_root_path(workspace)
        .ok_or_else(|| format!("organize_files: workspace `{workspace}` not mounted"))?;
    organize_files_in_path(root, ops).await
}

// ─────────────────────────────────────────────────────────────────
// trash_files
// ─────────────────────────────────────────────────────────────────

/// Trash directory relative to the workspace root.
const TRASH_REL: &str = ".thinkingroot/trash";

/// Outcome of [`trash_files_impl`]. `trash_paths` records the
/// `.thinkingroot/trash/<ts>-<name>` paths so a follow-up restore call
/// has the exact entries to invert.
#[derive(Debug, Clone, Serialize)]
pub struct TrashOutcome {
    pub trashed: u64,
    pub skipped: u64,
    pub trash_paths: Vec<String>,
}

/// Move each rel_path to `<workspace>/.thinkingroot/trash/<unix-ts>-<name>`.
/// `.thinkingroot/` is walker-ignored by the compile pipeline so
/// trashed items never re-extract as witnesses. Refuses to trash
/// anything inside `.thinkingroot/` itself (would let an agent
/// silently delete engine state — exactly the kind of "helpful"
/// that loses work).
pub async fn trash_files_in_path(
    workspace_root: PathBuf,
    rel_paths: Vec<String>,
) -> Result<TrashOutcome, String> {
    use std::time::{SystemTime, UNIX_EPOCH};
    tokio::task::spawn_blocking(move || -> Result<TrashOutcome, String> {
        let canonical_root = workspace_root
            .canonicalize()
            .map_err(|e| format!("trash_files: canonicalize root: {e}"))?;
        let trash_dir = canonical_root.join(TRASH_REL);
        std::fs::create_dir_all(&trash_dir)
            .map_err(|e| format!("trash_files: create trash dir: {e}"))?;
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let mut trashed = 0u64;
        let mut skipped = 0u64;
        let mut trash_paths: Vec<String> = Vec::new();

        for rel in rel_paths {
            if rel.starts_with(".thinkingroot") {
                skipped += 1;
                continue;
            }
            let source = match safe_path_within(&workspace_root, &rel) {
                Ok(p) => p,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            if !source.exists() {
                skipped += 1;
                continue;
            }
            let leaf = match source.file_name().and_then(|n| n.to_str()) {
                Some(s) => s.to_string(),
                None => {
                    skipped += 1;
                    continue;
                }
            };
            let trash_name = format!("{ts}-{leaf}");
            let dest = trash_dir.join(&trash_name);
            match std::fs::rename(&source, &dest) {
                Ok(()) => {
                    trashed += 1;
                    trash_paths.push(format!("{TRASH_REL}/{trash_name}"));
                }
                Err(_) => {
                    skipped += 1;
                }
            }
        }

        Ok(TrashOutcome {
            trashed,
            skipped,
            trash_paths,
        })
    })
    .await
    .map_err(|e| format!("trash_files: task panicked: {e}"))?
}

/// Resolve workspace and delegate to [`trash_files_in_path`].
pub async fn trash_files_impl(
    engine: &QueryEngine,
    workspace: &str,
    rel_paths: Vec<String>,
) -> Result<TrashOutcome, String> {
    let root = engine
        .workspace_root_path(workspace)
        .ok_or_else(|| format!("trash_files: workspace `{workspace}` not mounted"))?;
    trash_files_in_path(root, rel_paths).await
}

// ─────────────────────────────────────────────────────────────────
// ToolHandler wrappers for the 3 tail verbs
// ─────────────────────────────────────────────────────────────────

pub struct ListDirectoryTool {
    ctx: PlaygroundToolContext,
}

impl ListDirectoryTool {
    pub fn new(ctx: PlaygroundToolContext) -> Self {
        Self { ctx }
    }
    pub fn spec() -> Tool {
        Tool::new(
            "list_directory",
            "List the immediate children of a workspace-relative directory. Hidden dotfiles are filtered. Folders sort first, then files, alphabetical within each group. Use as a substrate-aware `ls` when you need to know what's under `notes/`, `sources/`, `inbox/` etc. before deciding what to organize / trash / ingest. Returns `{ rel_path, parent_rel_path, entries: [{ name, rel_path, is_dir, size_bytes }] }`.",
            json!({
                "type": "object",
                "properties": {
                    "rel_path": {
                        "type": "string",
                        "description": "Directory to list, workspace-relative. Empty string lists the workspace root. Forward slashes regardless of OS."
                    }
                },
                "required": []
            }),
        )
    }
}

#[async_trait]
impl ToolHandler for ListDirectoryTool {
    async fn handle(&self, input: Value) -> ToolHandlerResult {
        let rel_path = input
            .get("rel_path")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let engine = self.ctx.engine.read().await;
        match list_directory_impl(&engine, &self.ctx.workspace, rel_path).await {
            Ok(outcome) => serialize_outcome("list_directory", &outcome),
            Err(e) => ToolHandlerResult::error(e),
        }
    }
}

pub struct OrganizeFilesTool {
    ctx: PlaygroundToolContext,
}

impl OrganizeFilesTool {
    pub fn new(ctx: PlaygroundToolContext) -> Self {
        Self { ctx }
    }
    pub fn spec() -> Tool {
        Tool::new(
            "organize_files",
            "Batch rename/move files within a workspace. Each op `{from, to}` is workspace-relative; both paths are validated to stay inside the workspace root. Atomic `rename(2)` per op — conflicts (target exists, source missing, path escape) are skipped with honest counts. Use to reorganize ingested files (e.g. `inbox/foo.pdf` → `sources/2026/foo.pdf`) after the user dumps a folder. Cross-filesystem moves are not attempted; the agent should call `ingest_path` + `trash_files` for that.",
            json!({
                "type": "object",
                "properties": {
                    "ops": {
                        "type": "array",
                        "description": "Move operations. Order is preserved; each op is applied in turn so a later op can target a path that a previous op just produced.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "from": { "type": "string", "description": "Existing source path, workspace-relative." },
                                "to":   { "type": "string", "description": "Destination path, workspace-relative. Parent directories are created as needed." }
                            },
                            "required": ["from", "to"]
                        }
                    }
                },
                "required": ["ops"]
            }),
        )
    }
}

#[async_trait]
impl ToolHandler for OrganizeFilesTool {
    async fn handle(&self, input: Value) -> ToolHandlerResult {
        let ops_raw = match input.get("ops").and_then(|v| v.as_array()) {
            Some(arr) => arr.clone(),
            None => {
                return ToolHandlerResult::error("organize_files: missing required `ops` (array)");
            }
        };
        let mut ops: Vec<OrganizeOp> = Vec::with_capacity(ops_raw.len());
        for v in ops_raw {
            match serde_json::from_value::<OrganizeOp>(v) {
                Ok(op) => ops.push(op),
                Err(e) => {
                    return ToolHandlerResult::error(format!(
                        "organize_files: invalid op shape: {e}"
                    ));
                }
            }
        }
        let engine = self.ctx.engine.read().await;
        match organize_files_impl(&engine, &self.ctx.workspace, ops).await {
            Ok(outcome) => serialize_outcome("organize_files", &outcome),
            Err(e) => ToolHandlerResult::error(e),
        }
    }
}

pub struct TrashFilesTool {
    ctx: PlaygroundToolContext,
}

impl TrashFilesTool {
    pub fn new(ctx: PlaygroundToolContext) -> Self {
        Self { ctx }
    }
    pub fn spec() -> Tool {
        Tool::new(
            "trash_files",
            "Move files into `<workspace>/.thinkingroot/trash/<unix-ts>-<name>`. Reversible by manually moving back; the next compile won't re-extract trashed items because `.thinkingroot/` is walker-ignored. Refuses to trash anything inside `.thinkingroot/` itself. Use when the user says \"delete that note\" or \"clean up the inbox\" — never let the agent invoke `std::fs::remove` directly.",
            json!({
                "type": "object",
                "properties": {
                    "rel_paths": {
                        "type": "array",
                        "description": "Workspace-relative paths to trash. Each must canonically resolve inside the workspace root.",
                        "items": { "type": "string" }
                    }
                },
                "required": ["rel_paths"]
            }),
        )
    }
}

#[async_trait]
impl ToolHandler for TrashFilesTool {
    async fn handle(&self, input: Value) -> ToolHandlerResult {
        let rel_paths: Vec<String> = match input.get("rel_paths").and_then(|v| v.as_array()) {
            Some(arr) => arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            None => {
                return ToolHandlerResult::error(
                    "trash_files: missing required `rel_paths` (array of strings)",
                );
            }
        };
        if rel_paths.is_empty() {
            return ToolHandlerResult::error("trash_files: `rel_paths` must not be empty");
        }
        let engine = self.ctx.engine.read().await;
        match trash_files_impl(&engine, &self.ctx.workspace, rel_paths).await {
            Ok(outcome) => serialize_outcome("trash_files", &outcome),
            Err(e) => ToolHandlerResult::error(e),
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// Phase β.1 — Cognition Commits (in-app handlers)
// ─────────────────────────────────────────────────────────────────

pub struct CommitCognitionTool {
    ctx: PlaygroundToolContext,
}

impl CommitCognitionTool {
    pub fn new(ctx: PlaygroundToolContext) -> Self {
        Self { ctx }
    }
    pub fn spec() -> Tool {
        Tool::new(
            "commit_cognition",
            "Record one cognition event against a workspace branch as a content-addressed commit. The commit id is BLAKE3-derived; same inputs always produce the same id. Cited / added witnesses MUST exist in the workspace; fabricated references are rejected. Use this once per agent turn that produced an observable cognitive change. The chat history IS the commit DAG.",
            json!({
                "type": "object",
                "properties": {
                    "branch":          { "type": "string", "description": "Branch this commit belongs to." },
                    "parent_id":       { "type": "string", "description": "64-char hex parent commit id. Omit for genesis commit on a branch." },
                    "author_kind":     { "type": "string", "enum": ["user", "agent"] },
                    "author_id":       { "type": "string", "description": "User id or agent principal." },
                    "author_model":    { "type": "string", "description": "Model name (kind=agent only)." },
                    "prompt":          { "type": "string" },
                    "reasoning":       { "type": "string" },
                    "witnesses_added": { "type": "array", "items": { "type": "string" } },
                    "citations":       { "type": "array", "items": { "type": "string" } },
                    "gaps_surfaced":   { "type": "array", "items": { "type": "string" } }
                },
                "required": ["branch", "author_kind", "author_id"]
            }),
        )
    }
}

#[async_trait]
impl ToolHandler for CommitCognitionTool {
    async fn handle(&self, input: Value) -> ToolHandlerResult {
        use thinkingroot_core::types::{CognitionCommit, CommitAuthor, CommitId};

        let branch = match input.get("branch").and_then(|v| v.as_str()) {
            Some(b) if !b.is_empty() => b.to_string(),
            _ => return ToolHandlerResult::error("commit_cognition: missing required `branch`"),
        };
        let prompt = input
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let reasoning = input
            .get("reasoning")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let parent = match input.get("parent_id").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => match CommitId::from_hex(s) {
                Ok(p) => Some(p),
                Err(e) => {
                    return ToolHandlerResult::error(format!(
                        "commit_cognition: invalid parent_id `{s}`: {e}"
                    ));
                }
            },
            _ => None,
        };

        let author = match (
            input.get("author_kind").and_then(|v| v.as_str()),
            input.get("author_id").and_then(|v| v.as_str()),
        ) {
            (Some("user"), Some(uid)) => CommitAuthor::User {
                id: uid.to_string(),
            },
            (Some("agent"), Some(principal)) => {
                let model = input
                    .get("author_model")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                CommitAuthor::Agent {
                    model,
                    principal: principal.to_string(),
                }
            }
            _ => {
                return ToolHandlerResult::error(
                    "commit_cognition: `author_kind` must be 'user' or 'agent' and `author_id` is required",
                );
            }
        };

        let witnesses_added = match parse_witness_ids(input.get("witnesses_added")) {
            Ok(v) => v,
            Err(e) => {
                return ToolHandlerResult::error(format!(
                    "commit_cognition: witnesses_added: {e}"
                ));
            }
        };
        let citations = match parse_witness_ids(input.get("citations")) {
            Ok(v) => v,
            Err(e) => {
                return ToolHandlerResult::error(format!("commit_cognition: citations: {e}"));
            }
        };
        let gaps_surfaced: Vec<String> = input
            .get("gaps_surfaced")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let commit = CognitionCommit::new(
            parent,
            branch,
            author,
            prompt,
            reasoning,
            witnesses_added,
            citations,
            gaps_surfaced,
            chrono::Utc::now(),
        );
        let engine = self.ctx.engine.read().await;
        match engine.commit_cognition(&self.ctx.workspace, &commit).await {
            Ok(()) => serialize_outcome("commit_cognition", &commit),
            Err(e) => ToolHandlerResult::error(format!("commit_cognition: {e}")),
        }
    }
}

pub struct ListCommitsTool {
    ctx: PlaygroundToolContext,
}

impl ListCommitsTool {
    pub fn new(ctx: PlaygroundToolContext) -> Self {
        Self { ctx }
    }
    pub fn spec() -> Tool {
        Tool::new(
            "list_commits",
            "List cognition commits on a branch, newest first. Returns the full CognitionCommit shape (id, parent, branch, author, prompt, reasoning, witnesses_added, citations, gaps_surfaced, created_at). Read-only — safe to call at any time.",
            json!({
                "type": "object",
                "properties": {
                    "branch": { "type": "string", "description": "Branch to list. Defaults to `main` if omitted." },
                    "limit":  { "type": "integer", "minimum": 1, "description": "Max commits to return. Omit for all." }
                },
                "required": []
            }),
        )
    }
}

#[async_trait]
impl ToolHandler for ListCommitsTool {
    async fn handle(&self, input: Value) -> ToolHandlerResult {
        let branch = input
            .get("branch")
            .and_then(|v| v.as_str())
            .unwrap_or("main")
            .to_string();
        let limit = input
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize);
        let engine = self.ctx.engine.read().await;
        match engine
            .list_cognition_commits(&self.ctx.workspace, &branch, limit)
            .await
        {
            Ok(commits) => serialize_outcome("list_commits", &commits),
            Err(e) => ToolHandlerResult::error(format!("list_commits: {e}")),
        }
    }
}

/// Phase γ.1 — `merge_cognition` in-app tool.
///
/// Wraps `engine.compute_merge_plan` so the desktop's in-app agent
/// can request a deterministic merge plan between two cognition-commit
/// branches. Pure read; ApprovalGate routes it as non-write so the
/// agent can call it without user confirmation.
pub struct MergeCognitionTool {
    ctx: PlaygroundToolContext,
}

impl MergeCognitionTool {
    pub fn new(ctx: PlaygroundToolContext) -> Self {
        Self { ctx }
    }
    pub fn spec() -> Tool {
        Tool::new(
            "merge_cognition",
            "Compute a deterministic merge plan between two cognition-commit branches. Returns conflict_kind (identical / left_ahead / right_ahead / diverged / no_common_history), the LCA when present, the commits unique to each side, and the partitioned witness-id sets each side cited or added since the LCA. Pure read — no commit recorded. Use this before proposing a merge so the synthesis is grounded in the real divergence rather than guessed prose.",
            json!({
                "type": "object",
                "properties": {
                    "left_branch":  { "type": "string", "description": "Branch treated as the 'left' side (typically the destination, e.g. `main`)." },
                    "right_branch": { "type": "string", "description": "Branch treated as the 'right' side (typically the topic / candidate being merged in)." }
                },
                "required": ["left_branch", "right_branch"]
            }),
        )
    }
}

#[async_trait]
impl ToolHandler for MergeCognitionTool {
    async fn handle(&self, input: Value) -> ToolHandlerResult {
        let left_branch = match input.get("left_branch").and_then(|v| v.as_str()) {
            Some(b) if !b.is_empty() => b.to_string(),
            _ => {
                return ToolHandlerResult::error(
                    "merge_cognition: missing required `left_branch`".to_string(),
                );
            }
        };
        let right_branch = match input.get("right_branch").and_then(|v| v.as_str()) {
            Some(b) if !b.is_empty() => b.to_string(),
            _ => {
                return ToolHandlerResult::error(
                    "merge_cognition: missing required `right_branch`".to_string(),
                );
            }
        };
        let engine = self.ctx.engine.read().await;
        match engine
            .compute_merge_plan(&self.ctx.workspace, &left_branch, &right_branch)
            .await
        {
            Ok(plan) => serialize_outcome("merge_cognition", &plan),
            Err(e) => ToolHandlerResult::error(format!("merge_cognition: {e}")),
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Hello, World!"), "hello-world");
        assert_eq!(slugify(""), "note");
        assert_eq!(slugify("   "), "note");
        assert_eq!(slugify("Scaling Law Survey"), "scaling-law-survey");
        let long = slugify(&"ab".repeat(40));
        assert!(long.len() <= 60);
        assert!(!long.ends_with('-'));
    }

    #[test]
    fn yaml_quote_escapes_specials() {
        assert_eq!(yaml_quote("plain"), "\"plain\"");
        assert_eq!(yaml_quote("with \"quotes\""), "\"with \\\"quotes\\\"\"");
        assert_eq!(yaml_quote("line\nbreak"), "\"line\\nbreak\"");
        assert_eq!(yaml_quote("tab\there"), "\"tab\\there\"");
    }

    #[test]
    fn validate_host_path_rejects_relative() {
        assert!(validate_host_path(Path::new("relative/path.md")).is_err());
        assert!(validate_host_path(Path::new("/absolute/path.md")).is_ok());
    }

    #[tokio::test]
    async fn save_note_writes_to_notes_dir_with_frontmatter() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let outcome = save_note_in_path(
            root.clone(),
            "ws".to_string(),
            "Hello World".to_string(),
            "body text".to_string(),
        )
        .await
        .expect("save_note ok");
        assert!(outcome.created);
        assert!(outcome.bytes > 0);
        assert!(outcome.relative_path.starts_with("notes/hello-world-"));
        assert!(outcome.relative_path.ends_with(".md"));

        let on_disk = std::fs::read_to_string(&outcome.path).unwrap();
        assert!(on_disk.starts_with("---\n"));
        assert!(on_disk.contains("title: \"Hello World\""));
        assert!(on_disk.contains("kind: chat-note"));
        assert!(on_disk.contains("workspace: ws"));
        assert!(on_disk.ends_with("body text\n"));
    }

    #[tokio::test]
    async fn save_note_refuses_to_overwrite_same_day_note() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let first = save_note_in_path(
            root.clone(),
            "ws".to_string(),
            "Same Title".to_string(),
            "first body".to_string(),
        )
        .await
        .expect("first ok");
        assert!(first.created);

        let second = save_note_in_path(
            root.clone(),
            "ws".to_string(),
            "Same Title".to_string(),
            "second body".to_string(),
        )
        .await
        .expect("second ok");
        assert!(!second.created, "second call must not overwrite");
        assert_eq!(second.bytes, 0);
        assert_eq!(second.path, first.path);

        // On-disk content is the FIRST body, never the second.
        let on_disk = std::fs::read_to_string(&first.path).unwrap();
        assert!(on_disk.contains("first body"));
        assert!(!on_disk.contains("second body"));
    }

    #[tokio::test]
    async fn save_note_rejects_empty_inputs() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let err = save_note_in_path(
            root.clone(),
            "ws".to_string(),
            "".to_string(),
            "body".to_string(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("title"));
        let err = save_note_in_path(
            root.clone(),
            "ws".to_string(),
            "title".to_string(),
            "".to_string(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("body"));
    }

    #[tokio::test]
    async fn save_note_impl_rejects_unmounted_workspace() {
        let engine = QueryEngine::new();
        let err = save_note_impl(&engine, "missing", "Title", "body")
            .await
            .unwrap_err();
        assert!(err.contains("not mounted"));
    }

    #[tokio::test]
    async fn ingest_path_copies_directory_top_level_only() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let src_dir = tmp.path().join("src_papers");
        std::fs::create_dir_all(&src_dir).unwrap();
        write(&src_dir.join("a.pdf"), "fake pdf bytes a");
        write(&src_dir.join("b.md"), "# b");
        write(&src_dir.join(".hidden.md"), "hidden — must be skipped");
        std::fs::create_dir_all(src_dir.join("sub")).unwrap();
        write(&src_dir.join("sub").join("c.md"), "subdir — must be skipped");

        let outcome = ingest_path_in_path(
            root.clone(),
            src_dir.to_string_lossy().into_owned(),
        )
        .await
        .expect("ingest ok");
        assert_eq!(outcome.copied, 2, "two top-level non-hidden files");
        assert_eq!(outcome.skipped_duplicate, 0);
        assert_eq!(outcome.skipped_unreadable, 0);
        assert_eq!(outcome.destination_paths.len(), 2);

        // Hidden and subdirectory files must NOT have been copied.
        let inbox = root.join("inbox");
        assert!(inbox.join("a.pdf").exists());
        assert!(inbox.join("b.md").exists());
        assert!(!inbox.join(".hidden.md").exists());
        assert!(!inbox.join("c.md").exists());
    }

    #[tokio::test]
    async fn ingest_path_skips_duplicates() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let src = tmp.path().join("note.md");
        write(&src, "first");
        let first =
            ingest_path_in_path(root.clone(), src.to_string_lossy().into_owned())
                .await
                .unwrap();
        assert_eq!(first.copied, 1);
        assert_eq!(first.skipped_duplicate, 0);

        // Second call sees the destination already present.
        let second =
            ingest_path_in_path(root.clone(), src.to_string_lossy().into_owned())
                .await
                .unwrap();
        assert_eq!(second.copied, 0);
        assert_eq!(second.skipped_duplicate, 1);
    }

    #[tokio::test]
    async fn ingest_path_rejects_relative_or_missing() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let err = ingest_path_in_path(root.clone(), "relative/path.md".to_string())
            .await
            .unwrap_err();
        assert!(err.contains("absolute"));

        let err =
            ingest_path_in_path(root.clone(), "/nope/does/not/exist/xyz".to_string())
                .await
                .unwrap_err();
        assert!(err.contains("does not exist"));
    }

    #[tokio::test]
    async fn ingest_path_impl_rejects_unmounted_workspace() {
        let engine = QueryEngine::new();
        let err = ingest_path_impl(&engine, "missing", "/tmp/anything")
            .await
            .unwrap_err();
        assert!(err.contains("not mounted"));
    }

    #[tokio::test]
    async fn regenerate_paper_impl_rejects_unmounted_workspace() {
        let engine = QueryEngine::new();
        let err = regenerate_paper_impl(&engine, "missing")
            .await
            .unwrap_err();
        assert!(err.contains("not mounted"));
    }

    #[test]
    fn save_note_tool_spec_requires_title_and_body() {
        let spec = SaveNoteTool::spec();
        assert_eq!(spec.name, "save_note");
        let required = spec.input_schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "title"));
        assert!(required.iter().any(|v| v == "body"));
    }

    #[test]
    fn regenerate_paper_tool_spec_has_no_required_fields() {
        let spec = RegeneratePaperTool::spec();
        assert_eq!(spec.name, "regenerate_paper");
        let required = spec.input_schema.get("required");
        if let Some(r) = required {
            assert!(r.as_array().unwrap().is_empty());
        }
    }

    #[test]
    fn ingest_path_tool_spec_requires_source_path() {
        let spec = IngestPathTool::spec();
        assert_eq!(spec.name, "ingest_path");
        let required = spec.input_schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "source_path"));
    }

    // ── safe_path_within / rel_to_workspace ──────────────────────

    #[test]
    fn safe_path_rejects_parent_escape() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let err = safe_path_within(&root, "../etc/passwd").unwrap_err();
        assert!(err.contains("invalid path component"));
    }

    #[test]
    fn safe_path_rejects_absolute() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let err = safe_path_within(&root, "/etc/passwd").unwrap_err();
        assert!(err.contains("invalid path component"));
    }

    #[test]
    fn safe_path_accepts_nested_existing() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let nested = root.join("inbox").join("sub");
        std::fs::create_dir_all(&nested).unwrap();
        let resolved = safe_path_within(&root, "inbox/sub").unwrap();
        assert_eq!(
            resolved.canonicalize().unwrap(),
            nested.canonicalize().unwrap()
        );
    }

    #[test]
    fn safe_path_accepts_pending_destination() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(root.join("inbox")).unwrap();
        let resolved = safe_path_within(&root, "inbox/new-folder").unwrap();
        assert!(resolved.ends_with("inbox/new-folder"));
    }

    #[test]
    fn rel_to_workspace_uses_forward_slashes() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let nested = root.join("inbox").join("sub").join("file.txt");
        std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
        std::fs::write(&nested, b"hi").unwrap();
        let canonical = root.canonicalize().unwrap();
        let rel = rel_to_workspace(&nested.canonicalize().unwrap(), &canonical);
        assert_eq!(rel, "inbox/sub/file.txt");
    }

    // ── list_directory ───────────────────────────────────────────

    #[tokio::test]
    async fn list_directory_root_hides_dotfiles_and_sorts_folders_first() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(root.join("zsubdir")).unwrap();
        std::fs::create_dir_all(root.join("amid")).unwrap();
        write(&root.join("alpha.md"), "a");
        write(&root.join("beta.md"), "b");
        write(&root.join(".hidden"), "hidden");
        std::fs::create_dir_all(root.join(".thinkingroot")).unwrap();

        let outcome =
            list_directory_in_path(root.clone(), String::new()).await.unwrap();
        assert!(outcome.parent_rel_path.is_none());
        assert_eq!(outcome.entries.len(), 4, "two dirs + two visible files");
        // Folders first, then files. Within each group, alphabetical.
        assert_eq!(outcome.entries[0].name, "amid");
        assert!(outcome.entries[0].is_dir);
        assert_eq!(outcome.entries[1].name, "zsubdir");
        assert!(outcome.entries[1].is_dir);
        assert_eq!(outcome.entries[2].name, "alpha.md");
        assert!(!outcome.entries[2].is_dir);
        assert_eq!(outcome.entries[3].name, "beta.md");
    }

    #[tokio::test]
    async fn list_directory_subdir_includes_parent_rel_path() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(root.join("a").join("b")).unwrap();
        write(&root.join("a").join("b").join("note.md"), "x");
        let outcome = list_directory_in_path(root.clone(), "a/b".to_string())
            .await
            .unwrap();
        assert_eq!(outcome.parent_rel_path.as_deref(), Some("a"));
        assert_eq!(outcome.entries.len(), 1);
        assert_eq!(outcome.entries[0].name, "note.md");
    }

    #[tokio::test]
    async fn list_directory_rejects_escape() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let err = list_directory_in_path(root.clone(), "../escape".to_string())
            .await
            .unwrap_err();
        assert!(err.contains("invalid path component"));
    }

    #[tokio::test]
    async fn list_directory_rejects_non_directory_target() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        write(&root.join("a.md"), "x");
        let err = list_directory_in_path(root.clone(), "a.md".to_string())
            .await
            .unwrap_err();
        assert!(err.contains("not a directory"));
    }

    #[tokio::test]
    async fn list_directory_impl_rejects_unmounted_workspace() {
        let engine = QueryEngine::new();
        let err = list_directory_impl(&engine, "missing", "")
            .await
            .unwrap_err();
        assert!(err.contains("not mounted"));
    }

    // ── organize_files ───────────────────────────────────────────

    #[tokio::test]
    async fn organize_files_moves_files_within_workspace() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(root.join("inbox")).unwrap();
        std::fs::create_dir_all(root.join("sources")).unwrap();
        write(&root.join("inbox").join("a.md"), "a");
        write(&root.join("inbox").join("b.md"), "b");

        let ops = vec![
            OrganizeOp {
                from: "inbox/a.md".to_string(),
                to: "sources/a.md".to_string(),
            },
            OrganizeOp {
                from: "inbox/b.md".to_string(),
                to: "sources/2026/b.md".to_string(),
            },
        ];
        let outcome = organize_files_in_path(root.clone(), ops).await.unwrap();
        assert_eq!(outcome.moved, 2);
        assert_eq!(outcome.skipped_conflict, 0);
        assert_eq!(outcome.skipped_invalid, 0);
        assert!(root.join("sources").join("a.md").exists());
        assert!(root.join("sources").join("2026").join("b.md").exists());
        assert!(!root.join("inbox").join("a.md").exists());
    }

    #[tokio::test]
    async fn organize_files_skips_conflicts_and_escapes() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        write(&root.join("a.md"), "a");
        write(&root.join("b.md"), "b"); // already exists at destination

        let ops = vec![
            // Conflict — b.md exists.
            OrganizeOp {
                from: "a.md".to_string(),
                to: "b.md".to_string(),
            },
            // Escape — `..` rejected by safe_path_within.
            OrganizeOp {
                from: "a.md".to_string(),
                to: "../escape.md".to_string(),
            },
            // Missing source.
            OrganizeOp {
                from: "nope.md".to_string(),
                to: "renamed.md".to_string(),
            },
        ];
        let outcome = organize_files_in_path(root.clone(), ops).await.unwrap();
        assert_eq!(outcome.moved, 0);
        assert_eq!(outcome.skipped_conflict, 1);
        assert_eq!(outcome.skipped_invalid, 2);
    }

    #[tokio::test]
    async fn organize_files_refuses_folder_into_self() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(root.join("dir").join("inner")).unwrap();
        let ops = vec![OrganizeOp {
            from: "dir".to_string(),
            to: "dir/inner/dir".to_string(),
        }];
        let outcome = organize_files_in_path(root.clone(), ops).await.unwrap();
        assert_eq!(outcome.moved, 0);
        assert_eq!(outcome.skipped_invalid, 1);
    }

    #[test]
    fn organize_files_tool_spec_requires_ops() {
        let spec = OrganizeFilesTool::spec();
        assert_eq!(spec.name, "organize_files");
        let required = spec.input_schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "ops"));
    }

    // ── trash_files ──────────────────────────────────────────────

    #[tokio::test]
    async fn trash_files_moves_to_dot_thinkingroot_trash() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        write(&root.join("a.md"), "a");
        write(&root.join("b.md"), "b");

        let outcome =
            trash_files_in_path(root.clone(), vec!["a.md".into(), "b.md".into()])
                .await
                .unwrap();
        assert_eq!(outcome.trashed, 2);
        assert_eq!(outcome.skipped, 0);
        assert_eq!(outcome.trash_paths.len(), 2);
        for p in &outcome.trash_paths {
            assert!(p.starts_with(".thinkingroot/trash/"));
            assert!(p.contains('-')); // <ts>-<name>
        }
        assert!(!root.join("a.md").exists());
        assert!(!root.join("b.md").exists());
        assert!(root.join(".thinkingroot").join("trash").exists());
    }

    #[tokio::test]
    async fn trash_files_skips_dot_thinkingroot_paths() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(root.join(".thinkingroot")).unwrap();
        write(&root.join(".thinkingroot").join("secret.db"), "x");
        let outcome = trash_files_in_path(
            root.clone(),
            vec![".thinkingroot/secret.db".into()],
        )
        .await
        .unwrap();
        assert_eq!(outcome.trashed, 0);
        assert_eq!(outcome.skipped, 1);
        // File is still present in .thinkingroot/ — agent cannot
        // silently delete engine state.
        assert!(root.join(".thinkingroot").join("secret.db").exists());
    }

    #[tokio::test]
    async fn trash_files_skips_missing_paths() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let outcome =
            trash_files_in_path(root.clone(), vec!["nope.md".into()]).await.unwrap();
        assert_eq!(outcome.trashed, 0);
        assert_eq!(outcome.skipped, 1);
    }

    #[test]
    fn trash_files_tool_spec_requires_rel_paths() {
        let spec = TrashFilesTool::spec();
        assert_eq!(spec.name, "trash_files");
        let required = spec.input_schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "rel_paths"));
    }

    #[test]
    fn list_directory_tool_spec_has_no_required_fields() {
        let spec = ListDirectoryTool::spec();
        assert_eq!(spec.name, "list_directory");
        let required = spec.input_schema.get("required");
        if let Some(r) = required {
            assert!(r.as_array().unwrap().is_empty());
        }
    }
}
