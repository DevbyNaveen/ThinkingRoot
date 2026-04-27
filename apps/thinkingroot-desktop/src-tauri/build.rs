//! Build-time helper that stages the OSS `root` binary as a Tauri
//! sidecar.
//!
//! Tauri's `externalBin` contract expects each binary to live next
//! to `tauri.conf.json` under `binaries/<name>-<target-triple>` so
//! the bundler can copy it into the resource directory. We pull
//! the prebuilt binary from the parent workspace's
//! `target/release/root` if it exists; otherwise we leave the
//! directory empty and emit a `cargo:warning=` so dev iterations
//! still succeed.
//!
//! For a release bundle, run
//!   `cargo build --release -p thinkingroot-cli`
//! from the parent workspace before `pnpm tauri build`.

use std::path::{Path, PathBuf};

fn main() {
    // Stage the sidecar BEFORE handing off to `tauri_build::build()`.
    // `tauri_build` validates every `externalBin` entry against the
    // expected `<name>-<triple>` path on disk, so the file must exist
    // by the time we call it. When the upstream binary is missing,
    // fall back to a placeholder copy of the host's `root` (or, last
    // resort, an empty file with a loud cargo warning) — the runtime
    // sidecar resolver tolerates an unusable binary by falling back
    // to PATH lookup.
    stage_sidecar_binary();

    tauri_build::build();
}

fn stage_sidecar_binary() {
    let triple = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    let exe_suffix = if cfg!(windows) { ".exe" } else { "" };

    let manifest_dir =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let binaries_dir = manifest_dir.join("binaries");
    if !binaries_dir.exists() {
        std::fs::create_dir_all(&binaries_dir).expect("create binaries/");
    }

    let staged = binaries_dir.join(format!("thinkingroot-agent-runtime-{triple}{exe_suffix}"));

    let workspace_root = manifest_dir
        .ancestors()
        .nth(3)
        .expect("workspace root above apps/thinkingroot-desktop/src-tauri/");
    let source = workspace_root
        .join("target")
        .join("release")
        .join(format!("root{exe_suffix}"));

    println!("cargo:rerun-if-changed={}", source.display());

    if !source.exists() {
        println!(
            "cargo:warning=root binary not found at {} — staging an empty placeholder. \
             Run `cargo build --release -p thinkingroot-cli` from {} for a working bundle.",
            source.display(),
            workspace_root.display(),
        );
        if !staged.exists() {
            // Tauri only requires the externalBin path to exist; we
            // emit a zero-byte placeholder so dev iterations work.
            // The runtime sidecar resolver detects this and falls
            // back to `$PATH` automatically.
            if let Err(err) = std::fs::write(&staged, b"") {
                println!(
                    "cargo:warning=failed to create placeholder {}: {err}",
                    staged.display()
                );
            }
        }
        return;
    }

    if let Err(err) = stage(&source, &staged) {
        println!(
            "cargo:warning=failed to stage sidecar binary {} → {}: {err}",
            source.display(),
            staged.display(),
        );
    }
}

fn stage(source: &Path, dest: &Path) -> std::io::Result<()> {
    if dest.exists() {
        let src_meta = std::fs::metadata(source)?;
        let dst_meta = std::fs::metadata(dest)?;
        if src_meta.modified()? <= dst_meta.modified()? {
            return Ok(());
        }
    }
    std::fs::copy(source, dest)?;
    Ok(())
}
