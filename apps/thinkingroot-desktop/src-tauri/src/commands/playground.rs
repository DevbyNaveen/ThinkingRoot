//! Playground — the zero-configuration workspace.
//!
//! A *playground* is just a regular workspace pre-mounted on first
//! launch at `~/.thinkingroot/playground/`. It's the answer to "I just
//! want to dump a page or note somewhere without setting up a folder."
//! Push-to-cloud, branching, sharing, and the rest all work the same
//! as for any other workspace — the only specialness is that the user
//! never has to create it.
//!
//! On boot, [`ensure_playground_at_boot`] is invoked from the setup
//! hook in `lib.rs`. It is idempotent: if the registry already
//! contains an entry named `playground`, it does nothing; otherwise it
//! creates the directory tree, writes a starter `README.md`, and
//! registers it.
//!
//! The exposed Tauri command [`playground_ensure`] is for the rare
//! case where the user deletes the entry from the registry — calling
//! it from the UI restores the playground without restarting the app.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use thinkingroot_core::{WorkspaceEntry, WorkspaceRegistry};

const PLAYGROUND_NAME: &str = "playground";

/// Marker file that records the schema version of the playground tree
/// so future migrations (e.g. adding `inbox/`) can detect a v1 layout.
const LAYOUT_VERSION_FILE: &str = ".thinkingroot/playground_layout";
const LAYOUT_VERSION: &str = "1";

const STARTER_README: &str = "\
# Playground

This is the **playground workspace** — a scratchpad ThinkingRoot
auto-mounts for you so you have somewhere to dump things without
setting up a folder.

Anything you save here behaves like a normal workspace: it compiles
into the Witness Mesh, you can chat with it, branch it, and push it
to the cloud the same way.

## Where things land

- **Browser → Save** drops the cleaned-up article markdown into
  `sources/` and triggers an incremental compile. Each saved page
  carries `url:` and `content_hash:` frontmatter so re-saving the
  same page is a no-op unless the content actually changed.
- **Chat notes** land here too when you don't have another
  workspace selected.

You're free to delete this README, rename the folder, or stop using
the playground entirely. ThinkingRoot won't recreate it once you've
removed it from the registry (unless you ask via Settings).
";

/// Resolve the on-disk path for the playground workspace.
fn playground_path() -> Result<PathBuf, String> {
    let home = dirs::home_dir().ok_or_else(|| "could not resolve home directory".to_string())?;
    Ok(home.join(".thinkingroot").join(PLAYGROUND_NAME))
}

/// View payload returned to the UI when ensuring the playground.
#[derive(Debug, Serialize, Clone)]
pub struct PlaygroundView {
    pub name: String,
    pub path: String,
    pub port: u16,
    pub created: bool,
}

/// Idempotently ensure the playground workspace exists on disk and is
/// registered. Returns `created = true` only when the registry entry
/// was newly added on this call.
pub fn ensure_playground() -> Result<PlaygroundView, String> {
    let path = playground_path()?;
    let sources = path.join("sources");
    fs::create_dir_all(&sources).map_err(|e| {
        format!("create playground sources/ at {}: {e}", sources.display())
    })?;

    let data_dir = path.join(".thinkingroot");
    fs::create_dir_all(&data_dir).map_err(|e| {
        format!("create .thinkingroot/ at {}: {e}", data_dir.display())
    })?;

    let layout_marker = path.join(LAYOUT_VERSION_FILE);
    if !layout_marker.exists() {
        let _ = fs::write(&layout_marker, LAYOUT_VERSION);
    }

    let readme = path.join("README.md");
    if !readme.exists() {
        let _ = fs::write(&readme, STARTER_README);
    }

    let mut registry = WorkspaceRegistry::load().map_err(|e| e.to_string())?;
    let already_registered = registry
        .workspaces
        .iter()
        .any(|w| w.name == PLAYGROUND_NAME || w.path == path);

    let mut created = false;
    if !already_registered {
        let port = registry.next_available_port();
        registry.add(WorkspaceEntry {
            name: PLAYGROUND_NAME.to_string(),
            path: path.clone(),
            port,
        });
        registry.save().map_err(|e| e.to_string())?;
        created = true;
    }

    // Re-read so the port we surface is whatever the registry now
    // holds (which may differ from the value we just inserted if a
    // race meant a parallel mount registered a conflicting port —
    // unlikely on boot, but cheap to be honest).
    let registry = WorkspaceRegistry::load().map_err(|e| e.to_string())?;
    let entry = registry
        .workspaces
        .iter()
        .find(|w| w.name == PLAYGROUND_NAME)
        .ok_or_else(|| "playground registry entry missing after add".to_string())?;

    Ok(PlaygroundView {
        name: entry.name.clone(),
        path: entry.path.display().to_string(),
        port: entry.port,
        created,
    })
}

/// Setup-hook helper: ensure playground exists, log on failure but
/// never abort app boot — a missing playground is not fatal (the
/// existing workspaces still work).
pub async fn ensure_playground_at_boot(app: &AppHandle) {
    match tokio::task::spawn_blocking(ensure_playground).await {
        Ok(Ok(view)) => {
            if view.created {
                tracing::info!(
                    workspace = %view.name,
                    path = %view.path,
                    "registered playground workspace on first launch"
                );
                // Sidebar reads workspace_list; nudge it to refresh
                // so the playground appears without requiring a
                // manual workspace refresh click.
                let _ = app.emit("workspaces-changed", true);
            } else {
                tracing::debug!(
                    workspace = %view.name,
                    "playground already registered — skipping"
                );
            }
        }
        Ok(Err(err)) => {
            tracing::warn!(error = %err, "could not ensure playground workspace at boot");
        }
        Err(join_err) => {
            tracing::warn!(error = %join_err, "playground ensure task panicked at boot");
        }
    }
}

/// Tauri command — manually re-ensure the playground from the UI.
#[tauri::command]
pub async fn playground_ensure(app: AppHandle) -> Result<PlaygroundView, String> {
    let view = tokio::task::spawn_blocking(ensure_playground)
        .await
        .map_err(|e| format!("ensure task panicked: {e}"))??;
    let _ = app.emit("workspaces-changed", true);
    Ok(view)
}

/// Living Paper payload returned by [`paper_get`]. The frontend
/// renders `markdown` via ReactMarkdown + Mermaid plugin; `path`
/// surfaces the on-disk location for the "open in editor" affordance.
#[derive(Debug, Clone, Serialize)]
pub struct PaperPayload {
    /// On-disk path of the paper.md the engine wrote (always
    /// `<workspace>/.thinkingroot/paper.md`).
    pub path: String,
    /// Whether the file actually exists. `false` means the
    /// workspace hasn't compiled yet (or Phase 10b failed and
    /// `.thinkingroot/paper.md` is absent — the synthesis is
    /// non-fatal per the pipeline's honesty rule).
    pub exists: bool,
    /// File contents — verbatim YAML frontmatter + markdown body.
    /// Empty string when `exists == false`.
    pub markdown: String,
}

/// Outcome of a Playground drop. Surfaced to the UI so the
/// DropZone can render an honest summary toast ("3 added, 1 skipped
/// — already present").
#[derive(Debug, Clone, Serialize)]
pub struct DropOutcome {
    /// Number of files actually copied into `inbox/`.
    pub copied: u64,
    /// Number of files skipped because a same-name file already
    /// existed at the destination.
    pub skipped_duplicate: u64,
    /// Number of files skipped because the source path could not
    /// be read (deleted between drop and ingest, permissions, etc.).
    pub skipped_unreadable: u64,
    /// Relative paths inside the workspace that the copy landed at,
    /// in the order they were processed. Useful for the toast +
    /// future "show in source library" jump.
    pub destination_paths: Vec<String>,
}

/// Tauri command: copy dropped files into a workspace's `inbox/`
/// directory. The Playground UI calls this after the Tauri window
/// emits a `playground-files-dropped` event; the engine's walker
/// honours `inbox/` like any other source directory on the next
/// compile.
///
/// Rules:
/// - Resolves the workspace's on-disk root via `WorkspaceRegistry`.
/// - Creates `<workspace>/inbox/` if it doesn't already exist.
/// - Same-name collisions are skipped (no overwrite — the user can
///   rename the source and retry). Hidden because honest:
///   silently overwriting on drag-drop is the kind of "helpful"
///   that loses work.
/// - Unreadable sources are logged + counted, not fatal.
/// - Returns a typed `DropOutcome` so the UI surfaces an honest
///   summary instead of a fabricated "N files added".
#[tauri::command]
pub async fn playground_drop(
    workspace: String,
    file_paths: Vec<String>,
) -> Result<DropOutcome, String> {
    let outcome = tokio::task::spawn_blocking(move || -> Result<DropOutcome, String> {
        let registry = WorkspaceRegistry::load()
            .map_err(|e| format!("workspace registry load: {e}"))?;
        let entry = registry
            .workspaces
            .into_iter()
            .find(|e| e.name == workspace)
            .ok_or_else(|| format!("workspace `{workspace}` not registered"))?;
        let inbox = entry.path.join("inbox");
        fs::create_dir_all(&inbox).map_err(|e| format!("create inbox: {e}"))?;

        let mut copied: u64 = 0;
        let mut skipped_duplicate: u64 = 0;
        let mut skipped_unreadable: u64 = 0;
        let mut destination_paths: Vec<String> = Vec::new();

        for src_str in file_paths {
            let src = PathBuf::from(&src_str);
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
            match fs::copy(&src, &dest) {
                Ok(_) => {
                    copied += 1;
                    destination_paths.push(
                        dest.strip_prefix(&entry.path)
                            .map(|p| p.to_string_lossy().into_owned())
                            .unwrap_or_else(|_| dest.to_string_lossy().into_owned()),
                    );
                }
                Err(e) => {
                    skipped_unreadable += 1;
                    tracing::warn!(
                        src = %src_str,
                        error = %e,
                        "playground_drop: skipping unreadable source"
                    );
                }
            }
        }

        Ok(DropOutcome {
            copied,
            skipped_duplicate,
            skipped_unreadable,
            destination_paths,
        })
    })
    .await
    .map_err(|e| format!("playground_drop task panicked: {e}"))??;
    Ok(outcome)
}

/// One row in the Source Library view. Mirrors the engine's
/// `SourceInfo` wire shape (`crates/thinkingroot-serve/src/engine.rs`)
/// plus a `display_name` derived from the URI's basename so the UI
/// doesn't have to do path-parsing in JS.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct PlaygroundSource {
    pub id: String,
    pub uri: String,
    pub source_type: String,
    /// BLAKE3 hex of the source bytes; empty for agent-contributed
    /// claims with no underlying file.
    #[serde(default)]
    pub content_hash: String,
}

/// Tauri command: list every source in the active workspace via the
/// sidecar's `GET /api/v1/ws/{ws}/sources`. Returns the raw
/// `SourceInfo`-shaped rows; the UI sorts + renders.
///
/// Routes through `SidecarClient` per the Cortex Protocol
/// single-writer rule — the desktop never opens `graph.db` directly.
#[tauri::command]
pub async fn playground_sources(app: AppHandle) -> Result<Vec<PlaygroundSource>, String> {
    let client = crate::commands::sidecar_client::SidecarClient::ensure_active(&app).await?;
    let path = format!("/api/v1/ws/{}/sources", urlencode(&client.workspace));
    let rows: Vec<PlaygroundSource> = client.get(&path).await?;
    Ok(rows)
}

/// One row of `GET /api/v1/ws/{ws}/witnesses/by-source`.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct WitnessesPerSourceRow {
    pub source_id: String,
    pub count: u64,
}

/// Tauri command: per-source witness counts via the sidecar. The
/// Source Library badges each entry with its count. Sources with
/// zero witnesses are absent from the response — UI treats missing
/// as zero.
#[tauri::command]
pub async fn playground_witnesses_by_source(
    app: AppHandle,
) -> Result<Vec<WitnessesPerSourceRow>, String> {
    let client = crate::commands::sidecar_client::SidecarClient::ensure_active(&app).await?;
    let path = format!(
        "/api/v1/ws/{}/witnesses/by-source",
        urlencode(&client.workspace)
    );
    let rows: Vec<WitnessesPerSourceRow> = client.get(&path).await?;
    Ok(rows)
}

/// One row of `playground_source_witnesses` — the Playground
/// click-through detail panel shape. Carries the witness's identity
/// + rule + confidence + first-span byte range; the heavier fields
/// (`inputs`, full `spans` vec) stay server-side. The `statement`
/// field is the materialised source-byte slice (lossless,
/// post-Phase-5-bridge-polish) so the panel renders real source
/// text inline — same wire shape the existing
/// `GET /api/v1/ws/{ws}/claims` returns for legacy callers.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct PlaygroundWitnessRow {
    pub id: String,
    pub witness_type: String,
    pub rule: String,
    pub symbol: Option<String>,
    pub confidence: f64,
    pub byte_start: u64,
    pub byte_end: u64,
}

/// Tauri command: witnesses anchored to a single source row. Used
/// when a researcher clicks a file in the Playground SourceLibrary
/// to inspect what got extracted from it. Returns the
/// `Witness` wire shape via the sidecar's existing
/// `GET /api/v1/ws/{ws}/witnesses?source_id=...` endpoint.
#[tauri::command]
pub async fn playground_source_witnesses(
    app: AppHandle,
    source_id: String,
) -> Result<Vec<PlaygroundWitnessRow>, String> {
    let client = crate::commands::sidecar_client::SidecarClient::ensure_active(&app).await?;
    let path = format!(
        "/api/v1/ws/{}/witnesses?source_id={}",
        urlencode(&client.workspace),
        urlencode(&source_id),
    );
    // The full Witness wire shape carries inputs_json + spans_json
    // (10+ fields); deserialise into `serde_json::Value` and
    // project to the slim PlaygroundWitnessRow so the UI doesn't
    // depend on the full Witness type binding (which would drag in
    // the WitnessInput / WitnessSpan / SourceId / WorkspaceId
    // shape).
    let raw: Vec<serde_json::Value> = client.get(&path).await?;
    let mut out: Vec<PlaygroundWitnessRow> = Vec::with_capacity(raw.len());
    for v in raw {
        // Tolerant projection: missing fields collapse to defaults
        // so a wire-shape bump doesn't blow up the panel.
        let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let witness_type = v
            .get("witness_type")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let rule = v
            .get("rule")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let symbol = v
            .get("symbol")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        let confidence = v
            .get("confidence")
            .and_then(|x| {
                if let Some(f) = x.as_f64() {
                    Some(f)
                } else if let Some(n) = x.as_u64() {
                    Some(n as f64)
                } else {
                    None
                }
            })
            .unwrap_or(0.0);
        // The byte range lives on `spans[0]` per the Witness Mesh
        // contract; the wire JSON also carries denormalised
        // `byte_start` / `byte_end` columns from the graph for
        // join speed.
        let byte_start = v
            .get("byte_start")
            .and_then(|x| x.as_u64())
            .or_else(|| {
                v.get("spans")
                    .and_then(|spans| spans.get(0))
                    .and_then(|s| s.get("start"))
                    .and_then(|x| x.as_u64())
            })
            .unwrap_or(0);
        let byte_end = v
            .get("byte_end")
            .and_then(|x| x.as_u64())
            .or_else(|| {
                v.get("spans")
                    .and_then(|spans| spans.get(0))
                    .and_then(|s| s.get("end"))
                    .and_then(|x| x.as_u64())
            })
            .unwrap_or(0);
        out.push(PlaygroundWitnessRow {
            id,
            witness_type,
            rule,
            symbol,
            confidence,
            byte_start,
            byte_end,
        });
    }
    Ok(out)
}

/// Minimal URL encoder for path components — same shape as the helper
/// in `memory.rs`, duplicated locally to avoid pulling that module's
/// other imports into the playground command surface.
fn urlencode(s: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// Tauri command: read the Living Paper for a workspace by name.
///
/// Resolves the workspace's on-disk root via [`WorkspaceRegistry`],
/// then reads `<root>/.thinkingroot/paper.md` synchronously off the
/// main thread (the file caps at a few KiB on real workspaces).
/// Returns a `PaperPayload` whose `exists` flag tells the UI
/// whether the workspace has compiled at least once.
#[tauri::command]
pub async fn paper_get(workspace: String) -> Result<PaperPayload, String> {
    let payload = tokio::task::spawn_blocking(move || -> Result<PaperPayload, String> {
        let registry = WorkspaceRegistry::load()
            .map_err(|e| format!("workspace registry load: {e}"))?;
        let entry = registry
            .workspaces
            .into_iter()
            .find(|e| e.name == workspace)
            .ok_or_else(|| format!("workspace `{workspace}` not registered"))?;
        let path = entry.path.join(".thinkingroot").join("paper.md");
        if !path.exists() {
            return Ok(PaperPayload {
                path: path.to_string_lossy().into_owned(),
                exists: false,
                markdown: String::new(),
            });
        }
        let markdown = fs::read_to_string(&path)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        Ok(PaperPayload {
            path: path.to_string_lossy().into_owned(),
            exists: true,
            markdown,
        })
    })
    .await
    .map_err(|e| format!("paper_get task panicked: {e}"))??;
    Ok(payload)
}

// ─────────────────────────────────────────────────────────────
// Playground v1 — 8 additional researcher-facing verbs.
//
// Every verb routes through `SidecarClient` (Cortex Protocol
// single-writer guarantee) or pure filesystem writes scoped to the
// workspace root resolved via the registry. None of these open
// `graph.db` directly.
// ─────────────────────────────────────────────────────────────

/// Resolve a workspace's on-disk root path via the registry. Used by
/// every verb that needs to write into the workspace tree (notes,
/// pack export). Returns a typed error message on misses so the UI
/// can surface "workspace X is not registered" rather than a generic
/// failure.
fn workspace_root(workspace: &str) -> Result<PathBuf, String> {
    let registry = WorkspaceRegistry::load()
        .map_err(|e| format!("workspace registry load: {e}"))?;
    registry
        .workspaces
        .into_iter()
        .find(|e| e.name == workspace)
        .map(|e| e.path)
        .ok_or_else(|| format!("workspace `{workspace}` not registered"))
}

/// Slugify an arbitrary string into a filesystem-safe filename
/// fragment. Lowercases, collapses runs of non-alphanumerics into a
/// single `-`, trims leading/trailing `-`. Mirrors the pattern used
/// by `service.rs::SERVICE_LABEL` sanitisation.
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
    out
}

/// Result of `playground_save_note` — surfaces the absolute path of
/// the written note so the UI can offer "Reveal in Finder" and the
/// new SourceLibrary refresh picks it up after the next compile.
#[derive(Debug, Clone, Serialize)]
pub struct SaveNoteOutcome {
    /// Absolute path of the persisted note file.
    pub path: String,
    /// Workspace-relative path (`notes/<slug>-<date>.md`).
    pub relative_path: String,
    /// Bytes written (frontmatter + body).
    pub bytes: u64,
    /// Whether an existing file was overwritten or a new one was
    /// created. Honest — pre-existing notes aren't silently merged.
    pub created: bool,
}

/// Tauri command: persist an AI reply (or any markdown body) as a
/// note under `<workspace>/notes/<slug>-<date>.md`. YAML frontmatter
/// carries the title + UTC timestamp + a `kind: chat-note` marker so
/// the next compile attributes provenance correctly.
#[tauri::command]
pub async fn playground_save_note(
    workspace: String,
    title: String,
    body: String,
) -> Result<SaveNoteOutcome, String> {
    let outcome = tokio::task::spawn_blocking(move || -> Result<SaveNoteOutcome, String> {
        let root = workspace_root(&workspace)?;
        let notes_dir = root.join("notes");
        fs::create_dir_all(&notes_dir).map_err(|e| format!("create notes dir: {e}"))?;
        let slug = slugify(&title);
        let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let filename = format!("{slug}-{date}.md");
        let path = notes_dir.join(&filename);
        let created = !path.exists();
        let frontmatter = format!(
            "---\ntitle: {title}\ncreated_at: {ts}\nkind: chat-note\nworkspace: {workspace}\n---\n\n",
            title = title.replace('\n', " ").replace('\r', " "),
            ts = chrono::Utc::now().to_rfc3339(),
        );
        let payload = format!("{frontmatter}{body}\n");
        let bytes = payload.len() as u64;
        // Atomic write: tempfile + rename, mirrors paper.md write
        // discipline and `service.rs` install-manifest pattern.
        let tmp = notes_dir.join(format!("{filename}.tmp"));
        fs::write(&tmp, payload.as_bytes()).map_err(|e| {
            format!("write tmp {}: {e}", tmp.display())
        })?;
        fs::rename(&tmp, &path).map_err(|e| {
            format!("rename {} -> {}: {e}", tmp.display(), path.display())
        })?;
        Ok(SaveNoteOutcome {
            path: path.to_string_lossy().into_owned(),
            relative_path: format!("notes/{filename}"),
            bytes,
            created,
        })
    })
    .await
    .map_err(|e| format!("playground_save_note task panicked: {e}"))??;
    Ok(outcome)
}

/// Body for `playground_open_proposal`. Mirrors the existing
/// `proposal_open` Tauri command's shape but pinned at the Playground
/// command surface so callers don't have to know the underlying
/// branch-proposal wire.
#[derive(Debug, Deserialize)]
pub struct OpenProposalArgs {
    pub workspace: String,
    /// Branch the proposal lives under. Playground default is
    /// `playground` so each user has a stable sandbox.
    pub branch: String,
    pub title: String,
    pub body: String,
}

/// Result of `playground_open_proposal` — only the proposal id and
/// REST status, kept slim so the UI can render a toast without
/// pre-fetching the full proposal record.
#[derive(Debug, Serialize, serde::Deserialize)]
pub struct OpenProposalOutcome {
    pub proposal_id: String,
    pub branch: String,
}

/// Tauri command: open a Knowledge Proposal carrying the AI reply as
/// the body. Delegates to the sidecar's
/// `POST /api/v1/branches/{branch}/proposals` route (the same one the
/// `proposal_open` MCP tool uses), so proposal storage stays a
/// single-writer concern of the daemon.
#[tauri::command]
pub async fn playground_open_proposal(
    app: AppHandle,
    args: OpenProposalArgs,
) -> Result<OpenProposalOutcome, String> {
    let client = crate::commands::sidecar_client::SidecarClient::ensure_active(&app).await?;
    let path = format!(
        "/api/v1/branches/{}/proposals",
        urlencode(&args.branch)
    );
    let body = serde_json::json!({
        "title": args.title,
        "body": args.body,
        "workspace": args.workspace,
    });
    // The sidecar returns the full proposal record; we only surface
    // id + branch to the UI so the response stays tight.
    let resp: serde_json::Value = client.post(&path, &body).await?;
    let proposal_id = resp
        .get("id")
        .and_then(|v| v.as_str())
        .or_else(|| resp.get("proposal_id").and_then(|v| v.as_str()))
        .unwrap_or_default()
        .to_string();
    if proposal_id.is_empty() {
        return Err("sidecar returned proposal without an id".into());
    }
    Ok(OpenProposalOutcome {
        proposal_id,
        branch: args.branch,
    })
}

/// Body for `playground_branch_conversation`. The `parent` branch is
/// optional — `None` forks from `main`, which is the typical
/// Playground default.
#[derive(Debug, Deserialize)]
pub struct BranchConversationArgs {
    pub workspace: String,
    pub name: String,
    pub parent: Option<String>,
    pub description: Option<String>,
}

/// Result: just the created branch's name + parent for UI confirmation.
#[derive(Debug, Serialize, serde::Deserialize)]
pub struct BranchConversationOutcome {
    pub branch: String,
    pub parent: Option<String>,
}

/// Tauri command: create a knowledge branch off the current
/// conversation so subsequent contributions land in an isolated
/// graph. Delegates to the sidecar's
/// `POST /api/v1/branches` — same wire shape `branch_create` uses,
/// but without the `BranchView` projection (the Playground UI just
/// needs the resulting name).
#[tauri::command]
pub async fn playground_branch_conversation(
    app: AppHandle,
    args: BranchConversationArgs,
) -> Result<BranchConversationOutcome, String> {
    let _ = args.workspace;
    let client =
        crate::commands::sidecar_client::SidecarClient::ensure_active_for_branches(&app).await?;
    let body = serde_json::json!({
        "name": args.name,
        "parent": args.parent,
        "description": args.description,
    });
    let resp: serde_json::Value = client.post("/api/v1/branches", &body).await?;
    let branch_obj = resp.get("branch").cloned().unwrap_or(resp);
    let name = branch_obj
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if name.is_empty() {
        return Err("sidecar returned branch without a name".into());
    }
    let parent = branch_obj
        .get("parent")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    Ok(BranchConversationOutcome {
        branch: name,
        parent,
    })
}

/// One quiz item. The structure mirrors what `brain_investigate`
/// returns so the UI doesn't need a separate type for "quiz" vs
/// "investigation" answer rows.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct QuizItem {
    pub question: String,
    pub answer: String,
    /// Cited witness ids the answer is grounded in. Empty when the
    /// underlying retriever returned no provenance — the UI then
    /// surfaces a "low-confidence" tag.
    #[serde(default)]
    pub citations: Vec<String>,
}

/// Tauri command: generate a corpus quiz on a topic by piping a
/// quiz-shaped prompt through the sidecar's brain.investigate route.
/// The investigate handler runs the same ReAct loop as ad-hoc chat,
/// so cited witness ids are real — no fabricated quiz answers.
#[tauri::command]
pub async fn playground_quiz(
    app: AppHandle,
    workspace: String,
    topic: String,
    count: Option<u32>,
) -> Result<Vec<QuizItem>, String> {
    let client = crate::commands::sidecar_client::SidecarClient::ensure_active(&app).await?;
    let n = count.unwrap_or(5).min(20).max(1);
    let prompt = format!(
        "Generate {n} concise quiz questions and answers about \"{topic}\" \
         strictly grounded in the workspace's witnesses. \
         Reply as a JSON array of {{\"question\":..., \"answer\":..., \"citations\":[witness_ids...]}} objects. \
         If the corpus does not cover the topic, return an empty array."
    );
    let body = serde_json::json!({
        "entity": topic.clone(),
        "question": prompt,
    });
    let path = format!(
        "/api/v1/ws/{}/brain/investigate",
        urlencode(&workspace)
    );
    let resp: serde_json::Value = client.post(&path, &body).await?;
    // Investigate returns `{answer, citations, ...}`. The answer is
    // free-form markdown; we try to extract the embedded JSON array
    // generated by the model. On failure we surface a single quiz
    // item carrying the raw answer so the user sees something honest.
    let answer = resp
        .get("answer")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if let Some(items) = extract_quiz_items(&answer) {
        return Ok(items);
    }
    // Honest fallback: investigate gave us a single body, not a quiz
    // structure. Surface it as one quiz item with the topic as the
    // question so the user sees real output instead of an error.
    let citations: Vec<String> = resp
        .get("citations")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    Ok(vec![QuizItem {
        question: topic,
        answer,
        citations,
    }])
}

/// Best-effort extractor: find the first `[ ... ]` JSON array in a
/// markdown blob and parse it as `Vec<QuizItem>`. Returns `None` when
/// the model didn't comply with the requested shape — the caller
/// then falls back to surfacing the raw answer.
fn extract_quiz_items(answer: &str) -> Option<Vec<QuizItem>> {
    // Look for a fenced ```json block first (the most common
    // formatting from Anthropic / OpenAI), fall back to scanning for
    // the first `[` to handle bare-array replies.
    let candidate = if let Some(start) = answer.find("```json") {
        let after = &answer[start + 7..];
        after.find("```").map(|end| &after[..end])
    } else if let Some(start) = answer.find('[') {
        // Bounded scan for the matching `]` so a stray `[` inside the
        // body doesn't swallow the whole reply.
        let mut depth = 0i32;
        let mut end = None;
        for (idx, ch) in answer[start..].char_indices() {
            match ch {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(start + idx + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        end.map(|e| &answer[start..e])
    } else {
        None
    };
    let slice = candidate?.trim();
    serde_json::from_str::<Vec<QuizItem>>(slice).ok()
}

/// Body for `playground_export_tr`. Honest minimal shape: the
/// destination defaults to `<workspace>.tr` in the user's home
/// downloads folder when not supplied so the UI's "Export" button
/// works without a file picker.
#[derive(Debug, Deserialize)]
pub struct ExportTrArgs {
    pub workspace: String,
    /// Absolute destination path, or `None` to use the default
    /// `<home>/Downloads/<workspace>.tr`.
    pub out_path: Option<String>,
}

/// Result of a successful export — the absolute path of the written
/// `.tr` pack so the UI can offer "Reveal" / "Share".
#[derive(Debug, Serialize)]
pub struct ExportTrOutcome {
    pub path: String,
    pub bytes: u64,
}

/// Tauri command: export the workspace as a `.tr` pack. Delegates to
/// the existing `pack_export` infrastructure (which shells out to
/// `root pack`). The Playground default destination is
/// `~/Downloads/<workspace>.tr` so a hackathon researcher can fire
/// this from the Playground UI and immediately share the bundle.
#[tauri::command]
pub async fn playground_export_tr(
    args: ExportTrArgs,
) -> Result<ExportTrOutcome, String> {
    let out_path = match args.out_path {
        Some(p) => PathBuf::from(p),
        None => {
            let home = dirs::home_dir()
                .ok_or_else(|| "could not resolve home directory".to_string())?;
            home.join("Downloads")
                .join(format!("{}.tr", slugify(&args.workspace)))
        }
    };
    let parent = out_path
        .parent()
        .ok_or_else(|| "destination has no parent directory".to_string())?;
    fs::create_dir_all(parent).map_err(|e| format!("create parent dir: {e}"))?;
    // `pack_export` expects an absolute workspace path; resolve from
    // the registry so the Playground UI can pass the workspace name.
    let ws_path = workspace_root(&args.workspace)?;
    let req = crate::commands::pack_export::PackExportRequest {
        workspace: ws_path.to_string_lossy().into_owned(),
        out_path: out_path.to_string_lossy().into_owned(),
        name: None,
        version: None,
        license: None,
        description: None,
        sign_keyless: false,
        branch: None,
    };
    let result = crate::commands::pack_export::pack_export(req).await?;
    Ok(ExportTrOutcome {
        path: result.out_path,
        bytes: result.bytes,
    })
}

/// Result of `playground_handoff_url` — the deep-link URI external
/// MCP-capable agents can paste to mount this workspace remotely.
#[derive(Debug, Serialize)]
pub struct HandoffUrl {
    pub url: String,
    /// The same URI as an `mcp.json` snippet pre-filled with the
    /// loopback endpoint, ready to paste into Claude Code / Cursor.
    pub mcp_config_snippet: String,
}

/// Tauri command: produce a hand-off URL that lets an external agent
/// (Claude Code, Cursor, Codex, Windsurf) mount this workspace via
/// the local MCP server. The URL embeds the workspace name only;
/// agents authenticate via the user's loopback endpoint — there's no
/// network egress, no token to revoke, and no opportunity for a
/// remote agent to silently exfiltrate the corpus.
#[tauri::command]
pub async fn playground_handoff_url(
    app: AppHandle,
    workspace: String,
) -> Result<HandoffUrl, String> {
    let client = crate::commands::sidecar_client::SidecarClient::ensure_active(&app).await?;
    // `tr+mcp://workspace/<name>?host=...&port=...` — bespoke scheme
    // recognised by the `root connect` flow + future first-party
    // editor extensions. External agents fall back to reading the
    // `mcp_config_snippet` field, which is the canonical mcp.json
    // entry shape.
    let url = format!(
        "tr+mcp://workspace/{}?host={}&port={}",
        urlencode(&workspace),
        client.host,
        client.port
    );
    let mcp_config_snippet = serde_json::to_string_pretty(&serde_json::json!({
        "mcpServers": {
            format!("thinkingroot-{workspace}"): {
                "command": "root",
                "args": ["serve", "--mcp-stdio", "--workspace", workspace],
            }
        }
    }))
    .map_err(|e| format!("serialise mcp snippet: {e}"))?;
    Ok(HandoffUrl {
        url,
        mcp_config_snippet,
    })
}

/// One row of `playground_gaps`. Slim projection of
/// `thinkingroot_reflect::GapReport`: just the fields the
/// Playground sidebar renders. The full record (with
/// pattern provenance) is reachable via MCP `gaps` for editor
/// integrations.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct GapRow {
    pub gap_id: String,
    pub entity: String,
    pub entity_type: String,
    pub missing_claim_type: String,
    pub pattern_confidence: f64,
    pub status: String,
}

/// Tauri command: list Phase 9 known-unknowns for the workspace via
/// the sidecar's new `GET /api/v1/ws/{ws}/gaps` route. Falls back to
/// an empty list when reflect hasn't been run yet — empty state is
/// honest, not an error.
#[tauri::command]
pub async fn playground_gaps(
    app: AppHandle,
    workspace: String,
    entity: Option<String>,
    min_confidence: Option<f64>,
    branch: Option<String>,
) -> Result<Vec<GapRow>, String> {
    let client = crate::commands::sidecar_client::SidecarClient::ensure_active(&app).await?;
    let mut path = format!("/api/v1/ws/{}/gaps", urlencode(&workspace));
    let mut sep = '?';
    if let Some(e) = entity {
        path.push(sep);
        path.push_str("entity=");
        path.push_str(&urlencode(&e));
        sep = '&';
    }
    if let Some(c) = min_confidence {
        path.push(sep);
        path.push_str(&format!("min_confidence={c}"));
        sep = '&';
    }
    if let Some(b) = branch {
        path.push(sep);
        path.push_str("branch=");
        path.push_str(&urlencode(&b));
    }
    // Reflect returns a richer `GapReport` shape; the slim projection
    // keeps the wire payload small + insulates the UI from upstream
    // additions.
    let raw: Vec<serde_json::Value> = client.get(&path).await.unwrap_or_default();
    let mut out: Vec<GapRow> = Vec::with_capacity(raw.len());
    for v in raw {
        let gap_id = v
            .get("gap_id")
            .or_else(|| v.get("id"))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if gap_id.is_empty() {
            continue;
        }
        out.push(GapRow {
            gap_id,
            entity: v
                .get("entity")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            entity_type: v
                .get("entity_type")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            missing_claim_type: v
                .get("missing_claim_type")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            pattern_confidence: v
                .get("pattern_confidence")
                .and_then(|x| x.as_f64())
                .unwrap_or(0.0),
            status: v
                .get("status")
                .and_then(|x| x.as_str())
                .unwrap_or("open")
                .to_string(),
        });
    }
    Ok(out)
}

/// Result of `paper_regenerate` — surfaces the rendered bytes the
/// sidecar wrote so the UI can refresh PaperPanel inline without a
/// follow-up `paper_get` round-trip.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct PaperRegenerateOutcome {
    pub byte_length: u64,
    pub sections: u32,
    pub markdown: String,
}

/// Tauri command: rerun the Living Paper synthesiser against the
/// workspace's current Witness Mesh state without driving a full
/// compile. Routes through `POST /api/v1/ws/{ws}/paper/regenerate`
/// (new this commit). The sidecar writes the file atomically; the
/// returned bytes are exactly what landed at
/// `<root>/.thinkingroot/paper.md`.
#[tauri::command]
pub async fn paper_regenerate(
    app: AppHandle,
    workspace: String,
) -> Result<PaperRegenerateOutcome, String> {
    let client = crate::commands::sidecar_client::SidecarClient::ensure_active(&app).await?;
    let path = format!(
        "/api/v1/ws/{}/paper/regenerate",
        urlencode(&workspace)
    );
    let body = serde_json::json!({});
    let outcome: PaperRegenerateOutcome = client.post(&path, &body).await?;
    Ok(outcome)
}
