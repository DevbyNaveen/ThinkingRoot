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
