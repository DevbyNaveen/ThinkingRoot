//! `.tr` file-association handler.
//!
//! When the OS routes a `.tr` open intent to ThinkingRoot Desktop —
//! whether from a Finder double-click, a Windows Explorer Open With,
//! or a webview drag-drop — the front-end fires
//! [`install_tr_file`] with the absolute path. The command opens the
//! pack, renders a preview via [`tr_render`], runs trust verification
//! via [`tr_verify`], and returns the combined [`InstallPreview`]
//! payload to the UI.
//!
//! Verification policy: local files use the same default the CLI's
//! `--local-only` path applies (`allow_unsigned: true`,
//! `require_min_tier: T0`). The user can refuse to confirm the
//! install regardless of the verdict, so the preview always returns
//! a [`Verdict`] — including `Unsigned` / `Revoked` / `Tampered` —
//! and lets the sheet render the appropriate badge.

use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use tr_format::read_v3_pack;
use tr_revocation::{CacheConfig, RevocationCache};
use tr_verify::{V3Verdict, verify_v3_pack_with_revocation};

const BUILTIN_DEFAULT_REGISTRY: &str = "https://thinkingroot.dev";

/// Combined response for the install sheet. Mirrors the shape of
/// [`tr_render::RenderedPreview`] plus the trust verdict and a few
/// manifest essentials the UI displays before extracting markdown.
#[derive(Debug, Serialize, Clone)]
pub struct InstallPreview {
    pub path: String,
    pub name: String,
    pub version: String,
    pub license: Option<String>,
    pub markdown: String,
    pub manifest_table: String,
    pub source_count: u64,
    pub claim_count: u64,
    pub source_archive_bytes: u64,
    pub verdict: V3Verdict,
}

/// Read the `.tr` at `path`, render its preview, and run trust
/// verification. Errors stringify as user-facing messages (Tauri
/// commands serialize errors as `String`).
#[tauri::command]
pub async fn install_tr_file(path: String) -> Result<InstallPreview, String> {
    let path_buf = PathBuf::from(&path);
    if !path_buf.is_file() {
        return Err(format!("not a file: {}", path_buf.display()));
    }

    let bytes = std::fs::read(&path_buf).map_err(|e| format!("read {}: {e}", path_buf.display()))?;
    let pack = read_v3_pack(&bytes).map_err(|e| format!("parse .tr: {e}"))?;

    let rendered = tr_render::render_preview(&pack)
        .map_err(|e| format!("render preview: {e}"))?;

    let verdict = run_verifier(&pack)
        .await
        .map_err(|e| format!("verify: {e}"))?;

    Ok(InstallPreview {
        path: path_buf.display().to_string(),
        name: pack.manifest.name.clone(),
        version: pack.manifest.version.to_string(),
        license: pack.manifest.license.clone(),
        markdown: rendered.markdown,
        manifest_table: rendered.manifest_table,
        source_count: rendered.source_count,
        claim_count: rendered.claim_count,
        source_archive_bytes: rendered.source_archive_bytes,
        verdict,
    })
}

async fn run_verifier(pack: &tr_format::V3Pack) -> anyhow::Result<V3Verdict> {
    let registry_url = url::Url::parse(BUILTIN_DEFAULT_REGISTRY)?;
    let cache_dir = tr_revocation::default_cache_dir()
        .ok_or_else(|| anyhow::anyhow!("no platform cache dir available"))?;
    let cache = Arc::new(RevocationCache::new(CacheConfig::defaults_for(
        registry_url,
        cache_dir,
    )));

    Ok(verify_v3_pack_with_revocation(pack, &cache).await)
}
