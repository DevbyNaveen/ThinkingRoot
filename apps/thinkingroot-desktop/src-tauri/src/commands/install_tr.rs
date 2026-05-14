//! `.tr` file-association handler.
//!
//! Wired against the v3 reader (`tr_format::read_v3_pack`) and
//! renderer (`tr_render::render_preview`) so the install sheet shows
//! the actual pack identity / counts / signature status instead of a
//! stub error.
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
    /// Number of Witness Mesh rows packed in the `.tr` file.
    /// Zero on `tr/3`/`tr/3.1` packs and `tr/3.2` packs without
    /// witnesses.
    pub witness_count: u64,
    /// First ~500 chars of the Living Paper body (YAML frontmatter
    /// stripped) when the pack ships `paper.md`. `None` otherwise.
    pub paper_preview_md: Option<String>,
    /// `true` iff the pack carries the Witness Mesh pair
    /// (`witnesses.cbor` + `rule_catalog.toml`) — used to badge a
    /// pack as "witness-grounded" in the install sheet.
    pub has_witness_mesh: bool,
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
        witness_count: preview.witness_count,
        paper_preview_md: preview.paper_preview_md,
        has_witness_mesh: preview.has_witness_mesh,
    })
}
