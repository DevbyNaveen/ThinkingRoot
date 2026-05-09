//! `.tr` file-association handler.
//!
//! Stream E — wired against the v3 reader (`tr_format::read_v3_pack`)
//! and renderer (`tr_render::render_preview`) so the install sheet
//! shows the actual pack identity / counts / signature status instead
//! of a stub error.
//!
//! The trust-tier classification is conservative: signed packs land
//! at `T1` (Sigstore Bundle present); unsigned packs land at `T0`. The
//! verifier (`tr-verify`) is NOT invoked here because preview is a
//! read-only inspector. `root install <pack>` is the load-bearing
//! verification gate.

use std::path::Path;

use serde::Serialize;
use tr_format::V3Pack;

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
    // Read the pack bytes off the main thread — large packs can take
    // seconds to slurp on slow disks and we don't want to stall the
    // Tauri IPC reactor.
    let bytes = tokio::task::spawn_blocking({
        let p = path.clone();
        move || std::fs::read(Path::new(&p))
    })
    .await
    .map_err(|e| format!("read task panicked: {e}"))?
    .map_err(|e| format!("read pack {path}: {e}"))?;

    let pack: V3Pack =
        tr_format::read_v3_pack(&bytes).map_err(|e| format!("parse v3 pack: {e}"))?;

    let preview = tr_render::render_preview(&pack).map_err(|e| format!("render preview: {e}"))?;

    let trust_tier = if pack.signature.is_some() {
        "T1".to_string() // Sigstore-signed; /install gates further verification
    } else {
        "T0".to_string() // unsigned; the user can still inspect but install will warn
    };

    Ok(InstallPreview {
        path,
        name: pack.manifest.name.clone(),
        version: pack.manifest.version.to_string(),
        license: pack
            .manifest
            .license
            .clone()
            .unwrap_or_else(|| "unspecified".to_string()),
        trust_tier,
        markdown: preview.markdown,
        manifest_table: preview.manifest_table,
        source_count: preview.source_count,
        claim_count: preview.claim_count,
        source_archive_bytes: preview.source_archive_bytes,
    })
}
