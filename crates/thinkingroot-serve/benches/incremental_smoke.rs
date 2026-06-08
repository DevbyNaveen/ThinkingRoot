//! Incremental compile smoke benchmark — I-W7 sub-second p95 gate.
//!
//! Asserts that, on a 100-source structural-only workspace, an incremental
//! compile after a 1-line edit completes in <1000ms p95 across 5 trials.
//!
//! No LLM provider is configured (no config.toml written) so extraction runs
//! purely structurally — markdown headings, code signatures, and code links.
//! The fixture is reproducible from the markdown content alone; no network
//! access is required.
//!
//! Run with:
//!   `cargo bench -p thinkingroot-serve --bench incremental_smoke`
//!
//! Exits with status 1 (fail) if p95 exceeds the gate or any trial returned
//! an error.  Prints per-trial timings + p50/p95/max so the log makes any
//! regression diagnosable.

use std::path::PathBuf;
use std::time::Instant;

use tempfile::tempdir;
use thinkingroot_serve::pipeline::{run_pipeline_with_options, PipelineOptions, PipelineResult};
use tokio_util::sync::CancellationToken;

const N_SOURCES: usize = 100;
const N_TRIALS: usize = 5;
const P95_GATE_MS: u128 = 1000;

#[tokio::main]
async fn main() {
    match run_benchmark().await {
        Ok((mut timings, p95)) => {
            timings.sort();
            let p50 = timings[timings.len() / 2];
            let max = *timings.iter().max().unwrap();
            println!(
                "[bench] incremental smoke (n={N_SOURCES} sources, {N_TRIALS} trials): \
                 p50={p50}ms p95={p95}ms max={max}ms timings={timings:?}"
            );
            if p95 > P95_GATE_MS {
                eprintln!(
                    "[bench] FAIL: p95 ({p95}ms) exceeded gate ({P95_GATE_MS}ms)"
                );
                std::process::exit(1);
            }
            println!("[bench] PASS: p95 within {P95_GATE_MS}ms gate");
        }
        Err(e) => {
            eprintln!("[bench] FAIL: benchmark errored: {e}");
            std::process::exit(1);
        }
    }
}

async fn compile_once(root: &PathBuf) -> anyhow::Result<PipelineResult> {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let result = run_pipeline_with_options(
        root,
        None,
        Some(tx),
        PipelineOptions {
            cancel: CancellationToken::new(),
            no_rooting: true,
            skip_byte_audit: true,
            no_incremental: false,
            emit_summaries: false,
        },
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(result)
}

async fn run_benchmark() -> anyhow::Result<(Vec<u128>, u128)> {
    let tmp = tempdir()?;
    let root: PathBuf = tmp.path().to_path_buf();

    // Write a workspace config that forces structural-only extraction mode.
    // `structural_plus_llm = false` disables the "structural tier PLUS LLM"
    // dual-path: when a chunk is classified Tier 0 (Structural) and produces
    // results, it skips the LLM stage entirely.  This prevents the benchmark
    // from making real LLM API calls even on developer machines that have a
    // global Azure / OpenAI config in ~/Library/Application Support/thinkingroot/.
    //
    // NOTE: even with a real LLM configured globally, the benchmark fixture
    // produces ONLY Tier-0 chunks (Heading + link-bearing Prose), so
    // `llm_work` would be empty regardless.  The explicit config is defence-in-
    // depth: a future change to the router that re-classifies a chunk to LLM
    // tier would not silently make this benchmark hit the network.
    let tr_dir = root.join(".thinkingroot");
    std::fs::create_dir_all(&tr_dir)?;
    // Write a workspace config that forces structural-only extraction mode.
    //
    // `[llm]` section with empty `default_provider` makes `LlmConfig::is_configured()`
    // return false → `LlmClient::new()` returns `MissingConfig` → `Extractor::new`
    // catches the error and falls back to `LlmClient::new_structural_only()`.
    // This prevents real LLM calls even on developer machines with a global Azure/
    // OpenAI config, because `merge_with_global` only inherits the global LLM section
    // when the workspace TOML contains NO `[llm]` key at all.
    //
    // `structural_plus_llm = false` (in the separate `[extraction]` section) is not
    // written here because `ExtractionConfig.max_chunk_tokens` has no serde default —
    // a partial `[extraction]` TOML table fails to parse.  The structural-only
    // fallback path bypasses `structural_plus_llm` entirely (there is no LLM to call).
    std::fs::write(
        tr_dir.join("config.toml"),
        "[llm]\ndefault_provider = \"\"\nextraction_model = \"\"\ncompilation_model = \"\"\nmax_concurrent_requests = 5\nrequest_timeout_secs = 120\n",
    )?;

    // Fixture: N markdown files designed to produce exclusively structural
    // extractions (zero LLM calls).  Every chunk either resolves to Heading
    // (always structural) or Prose-with-links (structural via the link-bearing
    // prose path in the tier router).  Plain prose and code blocks are
    // intentionally omitted so the benchmark measures pipeline overhead, not
    // LLM round-trip latency.
    for i in 0..N_SOURCES {
        let path = root.join(format!("doc_{i:03}.md"));
        // All prose lines carry at least one Markdown link so the tier router
        // classifies them as Structural (link-bearing Prose path).
        let content = format!(
            "# Document {i}\n\n\
             Overview of [document {i}](./doc_{i:03}.md) and its [neighbour](./doc_{next:03}.md).\n\n\
             ## References\n\n\
             - See [previous document](./doc_{prev:03}.md) for context.\n\
             - See [next document](./doc_{next:03}.md) for continuation.\n\n\
             ## Summary\n\n\
             This document links to [doc {prev}](./doc_{prev:03}.md) and [doc {next}](./doc_{next:03}.md).\n",
            next = (i + 1) % N_SOURCES,
            prev = if i == 0 { N_SOURCES - 1 } else { i - 1 },
        );
        std::fs::write(&path, &content)?;
    }

    // 1. Initial full compile — warms the graph + fingerprints.
    compile_once(&root).await?;

    // 2. N_TRIALS incremental compiles, each preceded by a 1-line edit on a
    //    different file.  Each trial edits exactly 1 file and measures how long
    //    the subsequent incremental compile takes.
    let mut timings: Vec<u128> = Vec::with_capacity(N_TRIALS);
    for trial in 0..N_TRIALS {
        // Edit a distinct file per trial (using a prime stride to avoid repeats
        // across the 100-file corpus for up to 100 trials).
        let file_idx = (trial * 17) % N_SOURCES;
        let target_file = root.join(format!("doc_{file_idx:03}.md"));
        let mut content = std::fs::read_to_string(&target_file)?;
        // Append a new heading + link-bearing prose line so the edit changes
        // the content hash while keeping all chunks in the Structural tier
        // (Heading + link-bearing Prose = no LLM calls).  This exercises the
        // Phase 1 → Phase 3 → Phase 4 → Phase 6.7 pipeline with exactly 1
        // truly-changed source.
        content.push_str(&format!(
            "\n## Trial {trial}\n\nAdded by [benchmark trial {trial}](./trial_{trial:03}.md).\n"
        ));
        std::fs::write(&target_file, &content)?;

        let start = Instant::now();
        let result = compile_once(&root).await?;
        let elapsed_ms = start.elapsed().as_millis();
        timings.push(elapsed_ms);

        // Print per-phase timings for diagnosability.
        eprintln!(
            "[bench] trial {trial} ({elapsed_ms}ms) — phase_timings: {:?}",
            result.incremental_summary.phase_timings
        );

        // Invariant: exactly 1 source was truly changed per trial.
        // If the pipeline reports more, the source-granular filter or the
        // fingerprint store is not working correctly.
        anyhow::ensure!(
            result.incremental_summary.sources_truly_changed == 1,
            "trial {trial} (file doc_{file_idx:03}.md): expected 1 truly-changed source, \
             got {} — source-granular filter or fingerprint store may be broken",
            result.incremental_summary.sources_truly_changed
        );

        tracing::debug!(
            trial,
            file_idx,
            elapsed_ms,
            sources_truly_changed = result.incremental_summary.sources_truly_changed,
            "incremental smoke trial complete"
        );
    }

    timings.sort();
    let p95_idx = (timings.len() * 95 / 100).min(timings.len().saturating_sub(1));
    let p95 = timings[p95_idx];
    Ok((timings, p95))
}
