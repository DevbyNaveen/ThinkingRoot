//! `.tr` file-association handler — temporarily stubbed pending v3
//! migration (commit `d3ed699` migrated `tr-render` and the v1 wire
//! format was deleted in `a53c56a`; the install-sheet bindings here
//! still need rewiring against `read_v3_pack` + `V3Pack` + the new
//! `RenderedPreview` shape (`claim_count` / `source_archive_bytes`)).
//!
//! Returning a structured error keeps the command registered so the
//! UI's drag-drop / Open With path renders the "not yet wired"
//! sheet rather than panicking at IPC time.

use serde::Serialize;

#[derive(Debug, Serialize, Clone)]
pub struct InstallPreview {
    pub path: String,
    pub name: String,
    pub version: String,
    pub license: String,
    pub trust_tier: String,
    pub markdown: String,
    pub manifest_table: String,
    pub source_count: u64,
    pub claim_count: u64,
    pub source_archive_bytes: u64,
}

#[tauri::command]
pub async fn install_tr_file(path: String) -> Result<InstallPreview, String> {
    let _ = path;
    Err(
        "install_tr_file: pending v3 migration (tr_format::read_v3_pack + tr_render::RenderedPreview rewire)"
            .to_string(),
    )
}