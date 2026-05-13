//! Slice 3 integration tests — Phase 10 README synthesis hook in the
//! compile pipeline.
//!
//! Pattern mirrors `incremental_summary_test.rs`: structural-only
//! compile (no LLM, no rooting, byte-audit skipped) so tests run
//! offline. Each test runs a fresh `run_pipeline_with_options` against
//! a tempdir workspace and asserts the README outputs.

use std::fs;
use std::path::PathBuf;

use tempfile::tempdir;
use thinkingroot_llm::readme::{BEGIN_MARKER, END_MARKER};
use thinkingroot_serve::pipeline::{PipelineOptions, run_pipeline_with_options};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn make_workspace(root: &PathBuf, files: &[(&str, &str)]) {
    let tr_dir = root.join(".thinkingroot");
    fs::create_dir_all(&tr_dir).unwrap();
    for (name, content) in files {
        let path = root.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }
}

async fn compile(root: &PathBuf) -> thinkingroot_core::Result<()> {
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
    .map(|_| ())
}

#[tokio::test(flavor = "multi_thread")]
async fn compile_writes_canonical_readme() {
    // Greenfield workspace: only the canonical .thinkingroot/README.md
    // is auto-written. The root README.md is opt-in — see the
    // "compile_appends_to_existing_root_readme" test for the
    // user-creates-a-file path. Auto-creating a root README would
    // make the parser pick it up as a source on the next compile,
    // creating a substrate feedback loop.
    let tmp = tempdir().unwrap();
    let root: PathBuf = tmp.path().to_path_buf();
    make_workspace(
        &root,
        &[(
            "src/lib.rs",
            "/// Hello world\npub fn add(a: i32, b: i32) -> i32 { a + b }\n",
        )],
    );

    compile(&root).await.expect("compile must succeed");

    let canonical = root.join(".thinkingroot/README.md");
    assert!(canonical.exists(), "canonical README missing");
    let canonical_text = fs::read_to_string(&canonical).unwrap();
    assert!(canonical_text.contains("## Overview"), "missing Overview");
    assert!(
        canonical_text.contains("## Sources"),
        "missing Sources section"
    );
    assert!(
        canonical_text.contains("## How to use"),
        "missing How to use",
    );

    // Root README must NOT be auto-created — auto-creation is opt-in.
    assert!(
        !root.join("README.md").exists(),
        "root README must not be auto-created on greenfield compile"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn compile_appends_to_existing_root_readme() {
    // User opts in by creating a (possibly minimal) README.md at root.
    // The pipeline appends the auto-block on the next compile.
    let tmp = tempdir().unwrap();
    let root: PathBuf = tmp.path().to_path_buf();
    make_workspace(
        &root,
        &[
            ("README.md", "# My Project\n\nHand-written intro.\n"),
            (
                "src/lib.rs",
                "/// Hello world\npub fn add(a: i32, b: i32) -> i32 { a + b }\n",
            ),
        ],
    );

    compile(&root).await.expect("compile must succeed");

    let root_text = fs::read_to_string(root.join("README.md")).unwrap();
    assert!(
        root_text.starts_with("# My Project\n\nHand-written intro.\n"),
        "user prefix must be byte-preserved"
    );
    assert!(root_text.contains(BEGIN_MARKER));
    assert!(root_text.contains(END_MARKER));
    assert!(
        root_text.contains("## Overview"),
        "auto-block must be appended"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn compile_preserves_user_content_outside_markers() {
    let tmp = tempdir().unwrap();
    let root: PathBuf = tmp.path().to_path_buf();
    let user_root_readme = format!(
        "# My Curated Project\n\nHand-written intro that I own.\n\n{BEGIN_MARKER}\n_old auto content_\n{END_MARKER}\n\n## License\n\nMIT — hand-written tail.\n",
    );
    make_workspace(
        &root,
        &[
            ("README.md", user_root_readme.as_str()),
            ("src/lib.rs", "pub fn add(a: i32, b: i32) -> i32 { a + b }\n"),
        ],
    );

    compile(&root).await.expect("compile must succeed");

    let merged = fs::read_to_string(root.join("README.md")).unwrap();
    assert!(
        merged.starts_with("# My Curated Project\n\nHand-written intro that I own.\n\n"),
        "user prefix must be byte-preserved, got start:\n{}",
        &merged[..merged.len().min(200)],
    );
    assert!(
        merged.ends_with("\n\n## License\n\nMIT — hand-written tail.\n"),
        "user tail must be byte-preserved, got end:\n{}",
        &merged[merged.len().saturating_sub(200)..],
    );
    assert!(
        !merged.contains("_old auto content_"),
        "stale auto-block content must be replaced"
    );
    assert_eq!(
        merged.matches(BEGIN_MARKER).count(),
        1,
        "exactly one BEGIN marker must remain"
    );
    assert_eq!(
        merged.matches(END_MARKER).count(),
        1,
        "exactly one END marker must remain"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn compile_skips_root_readme_when_markers_malformed() {
    let tmp = tempdir().unwrap();
    let root: PathBuf = tmp.path().to_path_buf();
    // Unbalanced: only BEGIN, no END.
    let malformed = format!("# Project\n\nIntro.\n\n{BEGIN_MARKER}\nincomplete\n");
    make_workspace(
        &root,
        &[
            ("README.md", malformed.as_str()),
            ("src/lib.rs", "pub fn add(a: i32, b: i32) -> i32 { a + b }\n"),
        ],
    );

    compile(&root).await.expect("compile must succeed");

    let after = fs::read_to_string(root.join("README.md")).unwrap();
    assert_eq!(
        after, malformed,
        "malformed-marker root README must be left byte-untouched"
    );
    // Canonical README should still be written — only the root path is skipped.
    assert!(root.join(".thinkingroot/README.md").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn pack_authority_chain_picks_root_readme_over_canonical() {
    // User opts in to a root README; the pack authority chain picks it
    // over the canonical view.
    let tmp = tempdir().unwrap();
    let root: PathBuf = tmp.path().to_path_buf();
    make_workspace(
        &root,
        &[
            ("README.md", "# Curated\n\nHand-written.\n"),
            ("src/lib.rs", "pub fn add(a: i32, b: i32) -> i32 { a + b }\n"),
        ],
    );

    compile(&root).await.expect("compile must succeed");

    let root_readme = fs::read_to_string(root.join("README.md")).unwrap();
    let canonical = fs::read_to_string(root.join(".thinkingroot/README.md")).unwrap();
    assert!(root_readme.contains(BEGIN_MARKER));
    assert!(!canonical.contains(BEGIN_MARKER), "canonical view has no markers");

    // Replicate exactly what `pack_cmd::build_manifest_v3` does: take
    // the root README first, fall back to canonical, filter empty.
    let chosen = fs::read_to_string(root.join("README.md"))
        .ok()
        .or_else(|| fs::read_to_string(root.join(".thinkingroot/README.md")).ok())
        .filter(|s| !s.is_empty())
        .expect("at least one README must exist post-compile");
    assert_eq!(chosen, root_readme, "root README must win over canonical");
}

#[tokio::test(flavor = "multi_thread")]
async fn pack_authority_chain_falls_back_to_canonical_when_root_missing() {
    // User never created a root README — the pack authority chain
    // falls back to .thinkingroot/README.md (the canonical view).
    let tmp = tempdir().unwrap();
    let root: PathBuf = tmp.path().to_path_buf();
    make_workspace(
        &root,
        &[("src/lib.rs", "pub fn add(a: i32, b: i32) -> i32 { a + b }\n")],
    );

    compile(&root).await.expect("compile must succeed");

    // Greenfield workspace: only the canonical view was written.
    assert!(!root.join("README.md").exists());

    let chosen = fs::read_to_string(root.join("README.md"))
        .ok()
        .or_else(|| fs::read_to_string(root.join(".thinkingroot/README.md")).ok())
        .filter(|s| !s.is_empty())
        .expect("canonical fallback must still exist");
    assert!(
        chosen.contains("## Overview"),
        "fallback must be the canonical README, got:\n{chosen}"
    );
}
