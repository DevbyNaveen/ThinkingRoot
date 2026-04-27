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
use tr_format::{TrustTier, reader as tr_reader};
use tr_revocation::{CacheConfig, RevocationCache};
use tr_verify::{AuthorKeyStore, Verdict, Verifier, VerifierConfig};

const BUILTIN_DEFAULT_REGISTRY: &str = "https://thinkingroot.dev";

/// Combined response for the install sheet. Mirrors the shape of
/// [`tr_render::RenderedPreview`] plus the trust verdict and a few
/// manifest essentials the UI displays before extracting markdown.
#[derive(Debug, Serialize, Clone)]
pub struct InstallPreview {
    pub path: String,
    pub name: String,
    pub version: String,
    pub license: String,
    pub trust_tier: String,
    pub markdown: String,
    pub manifest_table: String,
    pub source_count: usize,
    pub entry_count: usize,
    pub payload_bytes: u64,
    pub verdict: Verdict,
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
    let pack = tr_reader::read_bytes(&bytes).map_err(|e| format!("parse .tr: {e}"))?;

    let rendered = tr_render::render_preview(&pack.manifest, &bytes)
        .map_err(|e| format!("render preview: {e}"))?;

    let verdict = run_verifier(&pack)
        .await
        .map_err(|e| format!("verify: {e}"))?;

    Ok(InstallPreview {
        path: path_buf.display().to_string(),
        name: pack.manifest.name.clone(),
        version: pack.manifest.version.to_string(),
        license: pack.manifest.license.clone(),
        trust_tier: trust_tier_str(pack.manifest.trust_tier),
        markdown: rendered.markdown,
        manifest_table: rendered.manifest_table,
        source_count: rendered.source_count,
        entry_count: rendered.entry_count,
        payload_bytes: rendered.payload_bytes,
        verdict,
    })
}

async fn run_verifier(pack: &tr_reader::Pack) -> anyhow::Result<Verdict> {
    let registry_url = url::Url::parse(BUILTIN_DEFAULT_REGISTRY)?;
    let cache_dir = tr_revocation::default_cache_dir()
        .ok_or_else(|| anyhow::anyhow!("no platform cache dir available"))?;
    let cache = Arc::new(RevocationCache::new(CacheConfig::defaults_for(
        registry_url,
        cache_dir,
    )));

    let verifier = Verifier::new(VerifierConfig {
        revocation: cache,
        author_keys: Arc::new(AuthorKeyStore::empty()),
        // Local files default to T0 — same policy the CLI's local-path
        // resolver uses. The sheet UI renders an explicit `Unsigned`
        // badge anyway so the user retains the final decision.
        require_min_tier: TrustTier::T0,
        allow_unsigned: true,
    });

    Ok(verifier.verify(pack).await?)
}

fn trust_tier_str(tier: TrustTier) -> String {
    match tier {
        TrustTier::T0 => "T0",
        TrustTier::T1 => "T1",
        TrustTier::T2 => "T2",
        TrustTier::T3 => "T3",
        TrustTier::T4 => "T4",
    }
    .to_string()
}
