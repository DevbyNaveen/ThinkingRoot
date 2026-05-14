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

use serde::Serialize;
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
