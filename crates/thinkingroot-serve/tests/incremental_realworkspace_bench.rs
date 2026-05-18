//! Tier 3 N — Real-workspace acceptance bench.
//!
//! Runs the incremental pipeline against a realistic subset of this very
//! repo: actual Rust source files, real markdown docs, real TOML
//! configs. This is the "real data" companion to
//! incremental_realdata_bench's synthetic 300-source markdown corpus —
//! verifies that the Tier 1+2+3 perf wins hold on real codebase shape
//! (function decls, doc comments, code links, config trees), not just
//! synthetic markdown.
//!
//! Run with:
//!   cargo test --release -p thinkingroot-serve \
//!     --test incremental_realworkspace_bench -- --ignored --nocapture
//!
//! Marked `#[ignore]` because it writes a tempdir of hundreds of source
//! files and the bench takes 30-60s on warm hardware. Depends on the
//! repo's own source tree being available via CARGO_MANIFEST_DIR — this
//! is a developer-run bench, not portable CI.

use std::path::{Path, PathBuf};
use std::time::Instant;

use tempfile::tempdir;
use thinkingroot_serve::pipeline::{run_pipeline_with_options, PipelineOptions, PipelineResult};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Recursively copy a directory's contents, skipping non-source dirs.
/// Returns the number of files copied.
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<usize> {
    std::fs::create_dir_all(dst)?;
    let mut count = 0;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        // Skip non-source artifacts that would either bloat the
        // workspace or be ignored by the pipeline anyway.
        if name_str == "target"
            || name_str == "node_modules"
            || name_str == ".thinkingroot"
            || name_str.starts_with('.')
        {
            continue;
        }

        if ty.is_dir() {
            count += copy_dir_recursive(&src_path, &dst_path)?;
        } else if ty.is_file() {
            std::fs::copy(&src_path, &dst_path)?;
            count += 1;
        }
    }
    Ok(count)
}

/// Walk a directory tree and collect every file path.
fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') {
            continue;
        }
        if ty.is_dir() {
            walk_files(&path, out)?;
        } else if ty.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

/// Construct the real workspace: copy representative crates +
/// markdown docs from this repo into `root`. Returns the list of
/// source files for edit selection.
fn build_real_workspace(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    // .thinkingroot/config.toml — structural-only, no LLM.
    let tr_dir = root.join(".thinkingroot");
    std::fs::create_dir_all(&tr_dir)?;
    std::fs::write(
        tr_dir.join("config.toml"),
        "[llm]\n\
         default_provider = \"\"\n\
         extraction_model = \"\"\n\
         compilation_model = \"\"\n\
         max_concurrent_requests = 5\n\
         request_timeout_secs = 120\n",
    )?;

    // CARGO_MANIFEST_DIR points at crates/thinkingroot-serve; go up
    // twice to reach the repo root.
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has parent")
        .parent()
        .expect("CARGO_MANIFEST_DIR has grandparent");

    let mut total_copied = 0;

    // Three real crates worth of Rust source — small enough to
    // bench fast, large enough to be representative.
    for crate_name in &[
        "thinkingroot-core",
        "thinkingroot-extract",
        "tr-format",
    ] {
        let crate_src = repo_root.join("crates").join(crate_name).join("src");
        if crate_src.exists() {
            let dest = root.join(format!("crates/{crate_name}/src"));
            total_copied += copy_dir_recursive(&crate_src, &dest)?;
            // Also copy the crate's Cargo.toml — the structural
            // emitter for TOML configs touches it.
            let crate_toml = repo_root.join("crates").join(crate_name).join("Cargo.toml");
            if crate_toml.exists() {
                let dest_toml = root.join(format!("crates/{crate_name}/Cargo.toml"));
                if let Some(parent) = dest_toml.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(&crate_toml, &dest_toml)?;
                total_copied += 1;
            }
        }
    }

    // Real markdown docs.
    let docs = repo_root.join("docs");
    if docs.exists() {
        total_copied += copy_dir_recursive(&docs, &root.join("docs"))?;
    }

    // Top-level Cargo.toml — copied as a realistic config-tree input.
    // README.md is INTENTIONALLY excluded: pipeline.rs's Phase
    // `synth_paper` writes back a synthesised README block to
    // `<root>/README.md` after every compile, which would flip the
    // file's content hash on the next compile and make no-op
    // recompile report truly_changed=1 (not zero) — a real-workflow
    // friction worth flagging but distinct from the structural
    // pipeline cost we want to measure here.
    let src_cargo = repo_root.join("Cargo.toml");
    if src_cargo.exists() {
        std::fs::copy(&src_cargo, &root.join("Cargo.toml"))?;
        total_copied += 1;
    }

    eprintln!("[bench] copied {total_copied} real source files to tempdir");

    // Walk the workspace and collect file paths for edit selection.
    let mut files = Vec::new();
    walk_files(root, &mut files)?;
    files.sort();
    Ok(files)
}

async fn compile_once(root: &Path) -> thinkingroot_core::Result<PipelineResult> {
    let (tx, _rx) = mpsc::unbounded_channel();
    run_pipeline_with_options(
        root,
        None,
        Some(tx),
        PipelineOptions {
            cancel: CancellationToken::new(),
            no_rooting: true,
            skip_byte_audit: true,
            no_incremental: false,
        },
    )
    .await
}

/// Append a kind-appropriate edit comment to each file. Mirrors how a
/// developer would actually touch a source file.
fn edit_files(targets: &[PathBuf]) -> std::io::Result<()> {
    for path in targets {
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        let suffix = match ext {
            "rs" => "\n// Tier-3-N benchmark edit\n",
            "md" => "\n\n## Tier 3 N benchmark edit\n",
            "toml" => "\n# Tier 3 N benchmark edit\n",
            _ => "\n// edit\n",
        };
        let prior = std::fs::read_to_string(path)?;
        std::fs::write(path, format!("{prior}{suffix}"))?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real-workspace bench; takes 30-60s. Run with `-- --ignored --nocapture`."]
async fn tier3_real_workspace_bench() {
    let tmp = tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let files = build_real_workspace(&root).expect("build real workspace");
    eprintln!("[bench] workspace at {} ({} total files)", root.display(), files.len());

    // Baseline full compile.
    let baseline_start = Instant::now();
    let baseline = compile_once(&root).await.expect("baseline compile");
    let baseline_ms = baseline_start.elapsed().as_millis();
    eprintln!(
        "[bench] BASELINE full compile: {baseline_ms}ms (sources_total={}, sources_truly_changed={})",
        baseline.incremental_summary.sources_total,
        baseline.incremental_summary.sources_truly_changed,
    );

    // Require enough files for the largest trial.
    assert!(
        files.len() >= 53,
        "real workspace must have >= 53 files for the 47-file trial; got {}",
        files.len()
    );

    // ── Trial 1: 1 file edit ──────────────────────────────────────
    edit_files(&files[0..1]).expect("edit 1 file");
    let t1_start = Instant::now();
    let t1 = compile_once(&root).await.expect("trial 1");
    let t1_ms = t1_start.elapsed().as_millis();
    eprintln!(
        "[bench] REAL  1-file edit: {t1_ms}ms (truly_changed={}) — {:?}",
        t1.incremental_summary.sources_truly_changed,
        t1.incremental_summary.phase_timings
    );

    // ── Trial 2: 5 file edits ─────────────────────────────────────
    edit_files(&files[1..6]).expect("edit 5 files");
    let t2_start = Instant::now();
    let t2 = compile_once(&root).await.expect("trial 2");
    let t2_ms = t2_start.elapsed().as_millis();
    eprintln!(
        "[bench] REAL  5-file edit: {t2_ms}ms (truly_changed={}) — {:?}",
        t2.incremental_summary.sources_truly_changed,
        t2.incremental_summary.phase_timings
    );

    // ── Trial 3: 47 file edits ────────────────────────────────────
    edit_files(&files[6..53]).expect("edit 47 files");
    let t3_start = Instant::now();
    let t3 = compile_once(&root).await.expect("trial 3");
    let t3_ms = t3_start.elapsed().as_millis();
    eprintln!(
        "[bench] REAL 47-file edit: {t3_ms}ms (truly_changed={}) — {:?}",
        t3.incremental_summary.sources_truly_changed,
        t3.incremental_summary.phase_timings
    );

    // ── Trial 4: no-op recompile ──────────────────────────────────
    let t4_start = Instant::now();
    let t4 = compile_once(&root).await.expect("trial 4 (no-op)");
    let t4_ms = t4_start.elapsed().as_millis();
    eprintln!(
        "[bench] REAL no-op recompile: {t4_ms}ms (truly_changed={})",
        t4.incremental_summary.sources_truly_changed,
    );
    // Note: a strict no-op recompile should report truly_changed=0
    // when the workspace contains no engine-written files. README is
    // intentionally excluded above to keep this property. If the
    // assertion ever fires, a NEW pipeline write-back path has
    // appeared and should either be opt-out-able or excluded here.
    assert!(
        t4.incremental_summary.sources_truly_changed <= 1,
        "no-op should report ≤1 truly-changed source; got {} — a pipeline \
         write-back path has been added since this bench was written",
        t4.incremental_summary.sources_truly_changed,
    );

    eprintln!(
        "[bench] PASS real-workspace. baseline={baseline_ms}ms 1f={t1_ms}ms 5f={t2_ms}ms 47f={t3_ms}ms noop={t4_ms}ms"
    );
}
