//! Desktop-side install-manifest registration.  Mirrors what
//! `install.sh` does for the CLI path, but resolves the bundled
//! sidecar binary via Tauri's resource API.
//!
//! Idempotent — safe to call on every desktop launch.

use std::path::PathBuf;

use tauri::{AppHandle, Manager, Runtime};
use thinkingroot_core::install_manifest::{BinaryEntry, BinaryId, InstallManifest};

/// Resolve the absolute path of the bundled sidecar binary at
/// runtime.  Returns `None` in `pnpm tauri dev` mode where the
/// bundled binary is absent (per tauri.conf.json `externalBin` —
/// the bundle is produced only at release time).
pub fn resolve_bundled_binary_path<R: Runtime>(app: &AppHandle<R>) -> Option<PathBuf> {
    let resource_dir = app.path().resource_dir().ok()?;
    let triple = current_target_triple();
    let candidate = resource_dir
        .join("binaries")
        .join(format!("thinkingroot-agent-runtime-{triple}"));
    if candidate.exists() {
        Some(candidate)
    } else {
        None
    }
}

/// Best-known target triple at compile time.  Tauri's `externalBin`
/// suffixes binaries with `-<target_triple>` so we resolve it the
/// same way Tauri does.
fn current_target_triple() -> &'static str {
    if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_arch = "x86_64", target_os = "macos")) {
        "x86_64-apple-darwin"
    } else if cfg!(all(target_arch = "aarch64", target_os = "linux")) {
        "aarch64-unknown-linux-gnu"
    } else if cfg!(all(target_arch = "x86_64", target_os = "linux")) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_arch = "x86_64", target_os = "windows")) {
        "x86_64-pc-windows-msvc"
    } else {
        // Fallback — unknown target. Manifest entry won't be
        // written and the desktop continues using runtime
        // binary-resolution (PATH lookup).  Honest: don't
        // fabricate a triple we can't ship for.
        "unknown"
    }
}

/// Compute BLAKE3 of a file by streaming through `blake3::Hasher`.
fn blake3_of(path: &std::path::Path) -> Result<String, std::io::Error> {
    let mut file = std::fs::File::open(path)?;
    let mut h = blake3::Hasher::new();
    std::io::copy(&mut file, &mut h)?;
    Ok(h.finalize().to_hex().to_string())
}

/// Idempotently register the bundled binary in the install manifest.
/// Safe to call on every desktop launch — `register_or_update`
/// replaces in place by id.
///
/// Failures are logged and swallowed — manifest registration is
/// best-effort.  A missing bundled binary in dev mode, a permission
/// error on the config dir, or a tempfile-persist failure should
/// NOT crash the desktop.  Slice F's doctor surface will surface
/// the resulting missing/stale entry to the user.
pub fn register_desktop_bundle<R: Runtime>(app: &AppHandle<R>) {
    let Some(binary_path) = resolve_bundled_binary_path(app) else {
        tracing::info!(
            "no bundled sidecar binary on disk; skipping install manifest registration \
             (dev mode or missing bundle)"
        );
        return;
    };

    let checksum = match blake3_of(&binary_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                ?binary_path,
                error = %e,
                "blake3 of bundled binary failed; skipping manifest registration"
            );
            return;
        }
    };

    let entry = BinaryEntry {
        id: BinaryId::DesktopBundle,
        path: binary_path,
        version: env!("CARGO_PKG_VERSION").to_string(),
        installed_at: chrono::Utc::now(),
        checksum_blake3: checksum,
    };

    if let Err(e) = InstallManifest::register_or_update(entry) {
        tracing::warn!(error = %e, "failed to register desktop bundle in install manifest");
    } else {
        tracing::info!("registered desktop bundle in install manifest");
    }
}
