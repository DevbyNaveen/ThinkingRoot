//! Slice 9 — desktop pack export.
//!
//! Wraps `root pack` as a sub-process so the desktop can drive the
//! same export flow the CLI ships, without re-implementing the
//! sigstore signing path or the v3 manifest builder.  The Tauri
//! binary is the same `root` binary the user runs from the terminal
//! (resolved via `$THINKINGROOT_ROOT_BINARY` first, then `$PATH`),
//! so behaviour is bit-identical between desktop and CLI.
//!
//! # Why a subprocess instead of an in-process call?
//!
//! `pack_cmd::run_pack` lives in the `thinkingroot-cli` crate and
//! pulls Sigstore-keyless signing through the `tr-sigstore/live`
//! feature, which transitively brings in `sigstore-rs`,
//! `openidconnect`, and a second `reqwest` config.  Linking that
//! straight into the Tauri binary would roughly double the desktop
//! build cost and force the desktop to ship the OIDC browser-redirect
//! listener.  The shipped `root` binary already carries that surface;
//! shelling out is the same code path the user trusts at the
//! terminal, with one less stack frame in between.
//!
//! # Honesty contract
//!
//! - The desktop never claims success without proof.  After the
//!   subprocess returns 0 we re-`stat` the output file and return its
//!   real byte count.  An empty pack is still surfaced as success;
//!   a missing one fails loudly.
//! - The `pack_hash` field is recomputed on the desktop side via
//!   `tr_format::read_v3_pack` — we do NOT trust the CLI's stdout
//!   line, since a corrupted pack could land on disk and the user
//!   would not notice until install time.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tr_format::read_v3_pack;

/// Form state submitted by the desktop UI.
#[derive(Debug, Clone, Deserialize)]
pub struct PackExportRequest {
    /// Absolute path to the workspace root (must contain
    /// `.thinkingroot/`).
    pub workspace: String,
    /// Absolute output path (chosen via the OS save dialog).
    pub out_path: String,
    /// `owner/slug` form. Optional — falls back to `Pack.toml`.
    pub name: Option<String>,
    /// SemVer version. Optional.
    pub version: Option<String>,
    /// SPDX license expression. Optional.
    pub license: Option<String>,
    /// One-line description. Optional.
    pub description: Option<String>,
    /// Drive Sigstore-keyless DSSE signing. Identical to the CLI's
    /// `--sign-keyless`; honors `$TR_OIDC_TOKEN` to skip the browser
    /// flow when the host environment has a usable JWT.
    #[serde(default)]
    pub sign_keyless: bool,
    /// Optional non-main branch to pack (T1.4).
    pub branch: Option<String>,
}

/// Returned to the UI on success.
#[derive(Debug, Clone, Serialize)]
pub struct PackExportResult {
    /// Absolute path of the written pack.
    pub out_path: String,
    /// Bytes on disk after a successful write.
    pub bytes: u64,
    /// `manifest.pack_hash` recomputed on the desktop side.  Empty
    /// string when the file failed to parse — the desktop surfaces
    /// the parse error in `warning` rather than returning an error
    /// (the bytes already landed on disk; the user can retry).
    pub pack_hash: String,
    /// Truthful trust tier — `T1` when signed, `T0` otherwise.
    pub trust_tier: String,
    /// Non-fatal warnings collected during the export (e.g. a manifest
    /// parse error after a successful write).  Empty in the common
    /// case.
    pub warnings: Vec<String>,
    /// stdout from `root pack` so the user can copy/paste the line if
    /// they want to script around it.
    pub stdout_log: String,
    /// stderr from `root pack` for diagnostics.
    pub stderr_log: String,
}

/// Lightweight estimator surfaced before the user commits.  Reads the
/// workspace's `Pack.toml` and the BLAKE3 ledger to give the user a
/// rough size + claim count without running a full pack.
#[derive(Debug, Clone, Serialize)]
pub struct PackEstimate {
    /// Whether the workspace appears compiled (`.thinkingroot/graph/`
    /// exists). When false the export will fail; the UI greys out the
    /// submit button.
    pub compiled: bool,
    /// Pack name resolved from `Pack.toml`. Empty when missing.
    pub name: String,
    /// Pack version resolved from `Pack.toml`. Empty when missing.
    pub version: String,
    /// SPDX license. None when missing.
    pub license: Option<String>,
    /// Pack description. None when missing.
    pub description: Option<String>,
    /// Approximate source byte count (from the byte-store directory).
    pub source_bytes: u64,
    /// Number of entries in the byte store. Lower bound on the source
    /// file count of the eventual pack.
    pub source_files: u64,
}

#[derive(Debug, Deserialize)]
struct PackTomlFile {
    pack: PackTomlInner,
}

#[derive(Debug, Deserialize)]
struct PackTomlInner {
    name: String,
    version: String,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

/// Locate the `root` binary.  Mirrors `mcp_local::resolve_root_binary`
/// so test stubs that override `THINKINGROOT_ROOT_BINARY` work the
/// same way for both surfaces.
fn resolve_root_binary() -> Option<String> {
    if let Ok(override_path) = std::env::var("THINKINGROOT_ROOT_BINARY")
        && !override_path.is_empty()
    {
        return Some(override_path);
    }
    let bin = if cfg!(windows) { "root.exe" } else { "root" };
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate.display().to_string());
        }
    }
    None
}

/// Estimate without writing.
#[tauri::command]
pub async fn pack_estimate(workspace: String) -> Result<PackEstimate, String> {
    let ws = PathBuf::from(&workspace);
    let engine = ws.join(".thinkingroot");
    let compiled = engine.join("graph").exists();

    let pack_meta: Option<PackTomlInner> = tokio::fs::read_to_string(ws.join("Pack.toml"))
        .await
        .ok()
        .and_then(|raw| toml::from_str::<PackTomlFile>(&raw).ok().map(|p| p.pack));

    let (source_bytes, source_files) = if compiled {
        scan_byte_store(&engine).await
    } else {
        (0, 0)
    };

    Ok(PackEstimate {
        compiled,
        name: pack_meta
            .as_ref()
            .map(|p| p.name.clone())
            .unwrap_or_default(),
        version: pack_meta
            .as_ref()
            .map(|p| p.version.clone())
            .unwrap_or_default(),
        license: pack_meta.as_ref().and_then(|p| p.license.clone()),
        description: pack_meta.as_ref().and_then(|p| p.description.clone()),
        source_bytes,
        source_files,
    })
}

/// Run the export.
#[tauri::command]
pub async fn pack_export(req: PackExportRequest) -> Result<PackExportResult, String> {
    let ws = PathBuf::from(&req.workspace);
    if !ws.join(".thinkingroot").exists() {
        return Err(format!(
            "no engine output at `{}/.thinkingroot/`; compile the workspace first",
            ws.display()
        ));
    }
    let bin = resolve_root_binary()
        .ok_or_else(|| "could not locate `root` binary in PATH".to_string())?;

    let mut cmd = Command::new(&bin);
    cmd.arg("pack");
    cmd.arg(&req.workspace);
    cmd.arg("--out").arg(&req.out_path);
    if let Some(n) = &req.name {
        cmd.arg("--name").arg(n);
    }
    if let Some(v) = &req.version {
        cmd.arg("--version").arg(v);
    }
    if let Some(l) = &req.license {
        cmd.arg("--license").arg(l);
    }
    if let Some(d) = &req.description {
        cmd.arg("--description").arg(d);
    }
    if let Some(b) = &req.branch {
        cmd.arg("--branch").arg(b);
    }
    if req.sign_keyless {
        cmd.arg("--sign-keyless");
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("spawn {bin} pack: {e}"))?;

    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    let stdout_task = tokio::spawn(async move {
        let mut buf = String::new();
        let mut reader = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            buf.push_str(&line);
            buf.push('\n');
        }
        buf
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = String::new();
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            buf.push_str(&line);
            buf.push('\n');
        }
        buf
    });

    let status = child
        .wait()
        .await
        .map_err(|e| format!("wait root pack: {e}"))?;

    let stdout_log = stdout_task.await.map_err(|e| e.to_string())?;
    let stderr_log = stderr_task.await.map_err(|e| e.to_string())?;

    if !status.success() {
        return Err(format!(
            "root pack failed (exit {}). stderr:\n{}",
            status.code().unwrap_or(-1),
            stderr_log
        ));
    }

    let out_path = PathBuf::from(&req.out_path);
    let bytes = tokio::fs::metadata(&out_path)
        .await
        .map_err(|e| format!("stat {}: {e}", out_path.display()))?
        .len();

    let mut warnings = Vec::new();
    let (pack_hash, trust_tier) = match tokio::fs::read(&out_path).await {
        Ok(buf) => match read_v3_pack(&buf) {
            Ok(pack) => {
                let hash = pack.manifest.pack_hash.clone();
                let tier = if pack.signature.is_some() { "T1" } else { "T0" };
                (hash, tier.to_string())
            }
            Err(e) => {
                warnings.push(format!("manifest parse: {e}"));
                (String::new(), "T0".to_string())
            }
        },
        Err(e) => {
            warnings.push(format!("read pack for hash recompute: {e}"));
            (String::new(), "T0".to_string())
        }
    };

    Ok(PackExportResult {
        out_path: out_path.display().to_string(),
        bytes,
        pack_hash,
        trust_tier,
        warnings,
        stdout_log,
        stderr_log,
    })
}

async fn scan_byte_store(engine: &Path) -> (u64, u64) {
    let store = engine.join("rooting").join("sources");
    let mut bytes = 0u64;
    let mut files = 0u64;
    let mut stack = vec![store];
    while let Some(dir) = stack.pop() {
        let mut rd = match tokio::fs::read_dir(&dir).await {
            Ok(r) => r,
            Err(_) => continue,
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let ft = match entry.file_type().await {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                if let Ok(meta) = entry.metadata().await {
                    bytes = bytes.saturating_add(meta.len());
                    files = files.saturating_add(1);
                }
            }
        }
    }
    (bytes, files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn estimate_reports_uncompiled_for_empty_workspace() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().to_string_lossy().to_string();
        let est = pack_estimate(ws).await.unwrap();
        assert!(!est.compiled);
        assert_eq!(est.name, "");
        assert_eq!(est.source_bytes, 0);
    }

    #[tokio::test]
    async fn estimate_reads_pack_toml() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join(".thinkingroot/graph")).unwrap();
        std::fs::write(
            ws.join("Pack.toml"),
            r#"[pack]
name = "acme/widgets"
version = "0.2.1"
license = "MIT"
description = "Widgets pack"
"#,
        )
        .unwrap();
        let est = pack_estimate(ws.display().to_string()).await.unwrap();
        assert!(est.compiled);
        assert_eq!(est.name, "acme/widgets");
        assert_eq!(est.version, "0.2.1");
        assert_eq!(est.license.as_deref(), Some("MIT"));
        assert_eq!(est.description.as_deref(), Some("Widgets pack"));
    }

    #[tokio::test]
    async fn export_rejects_uncompiled_workspace() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().to_string_lossy().to_string();
        let req = PackExportRequest {
            workspace: ws,
            out_path: tmp.path().join("out.tr").display().to_string(),
            name: None,
            version: None,
            license: None,
            description: None,
            sign_keyless: false,
            branch: None,
        };
        let err = pack_export(req).await.unwrap_err();
        assert!(err.contains("compile the workspace first"));
    }
}
