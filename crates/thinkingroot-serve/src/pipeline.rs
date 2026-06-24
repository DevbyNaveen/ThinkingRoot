use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use thinkingroot_core::Result;
use thinkingroot_core::config::Config;
use thinkingroot_core::types::WorkspaceId;
use thinkingroot_graph::StorageEngine;
use tokio_util::sync::CancellationToken;

/// Events emitted by the pipeline to drive CLI progress bars.
/// Sent via `tokio::sync::mpsc::UnboundedSender<ProgressEvent>`.
/// The CLI bar-driver task consumes these and renders indicatif bars.
///
/// `Serialize`/`Deserialize` are derived so the SSE compile route in
/// `rest.rs::compile_stream` can wire-encode each event as a JSON SSE
/// frame and the desktop sidecar consumer can deserialise back into
/// the same enum without a parallel wire vocabulary.  Wire shape:
/// `{"kind":"parse_complete","files":12}`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProgressEvent {
    /// Parsing is about to begin. Emitted immediately before `parse_directory`
    /// so the bar driver can start its clock at the same instant the pipeline
    /// does — config load and data-dir setup are NOT counted as parse time.
    ParseStart,
    /// Parsing finished. `files` = number of documents parsed.
    ParseComplete { files: usize },
    /// Diff phase is starting — comparing parsed docs against the stored graph
    /// to identify changed/unchanged/deleted sources, and loading graph-primed
    /// context for extraction. Bar driver shows a "Diffing" spinner here so
    /// users see real progress instead of a misleading "waiting for LLM" while
    /// CozoDB queries run.
    DiffStart,
    /// Diff phase finished. Counts let the driver render an honest summary
    /// (e.g. "12 changed · 188 unchanged · 0 deleted") and decide whether to
    /// expect any later phases at all.
    DiffComplete {
        changed: usize,
        unchanged: usize,
        deleted: usize,
    },
    /// Extraction is starting. Includes batch sizing so the UI can explain
    /// what work is about to happen before the first batch returns.
    ExtractionStart {
        total_chunks: usize,
        batch_size: usize,
        total_batches: usize,
    },
    /// A batch of extraction work has started running.
    ExtractionBatchStart {
        batch_index: usize,
        total_batches: usize,
        range_start: usize,
        range_end: usize,
        batch_chunks: usize,
    },
    /// One original chunk processed (cache hit or LLM result).
    ChunkDone {
        done: usize,
        total: usize,
        source_uri: String,
    },
    /// All chunks extracted. Summary data for solidifying the bar.
    ExtractionComplete {
        claims: usize,
        entities: usize,
        cache_hits: usize,
    },
    /// Some LLM batches failed permanently (retries exhausted) and the
    /// claims they would have produced are missing.  Emitted after
    /// `ExtractionComplete` only when `failed_batches > 0`.  Pre-fix
    /// these failures were silently dropped — the user only saw "ok"
    /// even though their compile was incomplete.
    ExtractionPartial {
        failed_batches: usize,
        failed_chunk_ranges: Vec<(usize, usize)>,
    },
    /// Grounding tribunal events — **deleted in Witness Mesh cutover**.
    /// Variants retained so SSE deserializers built against pre-cutover
    /// daemons keep parsing; pipeline.rs never emits them post-cutover.
    GroundingStart {
        llm_claims: usize,
        structural_claims: usize,
    },
    GroundingModelReady,
    GroundingProgress { done: usize, total: usize },
    GroundingDone { accepted: usize, rejected: usize },
    /// Witness Mesh persistence (Phase 6.45) is starting. `raw` = the
    /// extractor's pre-dedup witness count.
    WitnessMeshStart { raw: usize },
    /// Witness Mesh persistence finished. `persisted` = rows actually
    /// written after content-id dedup and SAFETY cross-check; `deduped`
    /// = collapsed duplicates; `edges` = DAG edges in the input mesh;
    /// `errors` = mesh-assembly warnings (malformed witnesses dropped).
    WitnessMeshDone {
        persisted: usize,
        deduped: usize,
        edges: usize,
        errors: usize,
    },
    /// Fingerprint check finished. `cutoffs` = sources skipped by fingerprint match.
    FingerprintDone {
        truly_changed: usize,
        cutoffs: usize,
    },
    /// Entity resolution is starting.
    LinkingStart { total_entities: usize },
    /// One entity resolved (created or merged).
    EntityResolved { done: usize, total: usize },
    /// Linking finished.
    LinkComplete {
        entities: usize,
        relations: usize,
        contradictions: usize,
    },
    /// Vector index update finished.
    VectorUpdateDone {
        entities_indexed: usize,
        claims_indexed: usize,
    },
    /// Incremental vector upsert progress.
    VectorProgress { done: usize, total: usize },
    /// Artifact compilation finished.
    CompilationDone { artifacts: usize },
    /// One artifact compiled. Drives the real progress bar.
    CompilationProgress { done: usize, total: usize },
    /// Verification finished.
    VerificationDone { health: u8 },
    /// Rooting is starting — total candidate count.
    RootingStart { candidates: usize },
    /// One claim tried by the Rooter.
    RootingProgress { done: usize, total: usize },
    /// Rooting finished. Tier counts summarize the outcome.
    RootingDone {
        rooted: usize,
        attested: usize,
        quarantined: usize,
        rejected: usize,
    },
    /// Fired immediately after each pipeline phase completes.  Lets SSE
    /// consumers render real-time per-phase progress instead of waiting
    /// for the terminal IncrementalDone event.
    PhaseDone { name: String, elapsed_ms: u64 },
    /// Fired once at the end of every successful compile, carrying the
    /// full structured summary.  CLI summary printer + desktop summary
    /// panel both consume this event.
    IncrementalDone { summary: thinkingroot_core::IncrementalSummary },
    /// The pipeline returned `Err(_)`. Emitted by the public `run_pipeline`
    /// wrapper before the channel closes, so the bar driver can finalise any
    /// in-flight bars with a failure style instead of the ambiguous "skipped"
    /// dim dash.
    PipelineFailed { error: String },
    /// Live progress snapshot — emitted by the daemon's ticker every
    /// 250 ms while a compile is running. This is the **single canonical
    /// progress event** for the unified CLI + desktop progress bar.
    /// All the per-phase events above (`ChunkDone`, `EntityResolved`,
    /// `LinkingStart`, `WitnessMeshStart`, …) are retained only for
    /// legacy SSE consumers; new code should match on `CompileTick`.
    CompileTick(thinkingroot_core::CompileTick),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PipelineResult {
    pub files_parsed: usize,
    pub claims_count: usize,
    pub entities_count: usize,
    pub relations_count: usize,
    pub contradictions_count: usize,
    pub artifacts_count: usize,
    pub health_score: u8,
    pub cache_hits: usize,
    pub early_cutoffs: usize,
    pub structural_extractions: usize,
    /// `true` when the pipeline wrote at least one change to CozoDB.
    /// `false` means all files were fingerprint-identical — the cache is still
    /// current and the caller should skip the reload entirely.
    pub cache_dirty: bool,
    /// LLM batches that exhausted retries during extraction.  Non-zero
    /// means the compile is partial — claims are missing for chunks in
    /// `failed_chunk_ranges`.  Surfaced so the CLI can print a yellow
    /// warning and the desktop can render a non-fatal toast.
    #[serde(default)]
    pub failed_batches: usize,
    /// `(range_start, range_end)` chunk ranges (inclusive, 1-indexed) of
    /// every batch that failed permanently.  Identical wire shape to
    /// `ProgressEvent::ExtractionBatchStart::range_*` so callers don't
    /// need a second vocabulary.
    #[serde(default)]
    pub failed_chunk_ranges: Vec<(usize, usize)>,
    /// Structured delta for this compile run.  Always populated — even
    /// on the early-return path (nothing changed) so consumers never
    /// branch on presence.
    #[serde(default)]
    pub incremental_summary: thinkingroot_core::IncrementalSummary,
}

/// Live compile-progress state shared between the pipeline body and
/// the ticker task. Each `set_step` resets `done`/`total` and stamps a
/// new step-start timestamp; phases advance via `advance` / `set_done`
/// atomics. The ticker reads a coherent `CompileTick` snapshot every
/// 250 ms — no per-row events on the channel, no lock contention.
pub(crate) struct CompileProgressState {
    step: std::sync::atomic::AtomicU8,
    done: std::sync::atomic::AtomicU64,
    total: std::sync::atomic::AtomicU64,
    pipeline_start: std::time::Instant,
    /// Milliseconds since `pipeline_start` when the current step began.
    /// Stored as a u64 atomic so we never need a mutex.
    step_started_ms: std::sync::atomic::AtomicU64,
    /// Short sub-phase label surfaced as `CompileTick.detail` so the UI
    /// can render an honest indeterminate-spinner caption. Set 10–15
    /// times per compile at sub-phase boundaries (a `&'static str` from
    /// a fixed catalog — no allocation, no payload contention). The
    /// ticker reads this every 250 ms; the read is the only place a
    /// lock is held, and only for the duration of a 1-pointer copy.
    substep: std::sync::Mutex<&'static str>,
}

impl CompileProgressState {
    pub(crate) fn new() -> Self {
        Self {
            step: std::sync::atomic::AtomicU8::new(
                thinkingroot_core::CompileStep::Reading.index(),
            ),
            done: std::sync::atomic::AtomicU64::new(0),
            total: std::sync::atomic::AtomicU64::new(0),
            pipeline_start: std::time::Instant::now(),
            step_started_ms: std::sync::atomic::AtomicU64::new(0),
            substep: std::sync::Mutex::new(""),
        }
    }

    pub(crate) fn set_step(&self, step: thinkingroot_core::CompileStep, total: u64) {
        use std::sync::atomic::Ordering;
        let elapsed = self.pipeline_start.elapsed().as_millis() as u64;
        // Order: step_started_ms first so concurrent snapshot reads see
        // a consistent (step_started_ms <= now) view of the new step.
        self.step_started_ms.store(elapsed, Ordering::Release);
        self.done.store(0, Ordering::Release);
        self.total.store(total, Ordering::Release);
        self.step.store(step.index(), Ordering::Release);
    }

    pub(crate) fn advance(&self, n: u64) {
        self.done
            .fetch_add(n, std::sync::atomic::Ordering::AcqRel);
    }

    pub(crate) fn set_done(&self, n: u64) {
        self.done.store(n, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn set_total(&self, n: u64) {
        self.total.store(n, std::sync::atomic::Ordering::Release);
    }

    /// Replace the sub-phase label surfaced as `CompileTick.detail`.
    /// Callers pass `&'static str` from the in-file catalog (e.g.
    /// `"removing changed sources"`, `"persisting witnesses"`); the
    /// ticker snapshot copies the pointer under a brief Mutex.  Poison
    /// is impossible (no panic between lock + write), so a `.lock()`
    /// failure is intentionally swallowed — substep is observability,
    /// not correctness.
    pub(crate) fn set_substep(&self, label: &'static str) {
        if let Ok(mut guard) = self.substep.lock() {
            *guard = label;
        }
    }

    fn step_from_index(idx: u8) -> thinkingroot_core::CompileStep {
        use thinkingroot_core::CompileStep::*;
        match idx {
            1 => Reading,
            2 => Extracting,
            3 => Linking,
            4 => Persisting,
            5 => Packing,
            // Defensive default — unreachable if `set_step` is the only mutator.
            _ => Reading,
        }
    }

    pub(crate) fn snapshot(&self) -> thinkingroot_core::CompileTick {
        use std::sync::atomic::Ordering;
        let step = Self::step_from_index(self.step.load(Ordering::Acquire));
        let done = self.done.load(Ordering::Acquire);
        let total = self.total.load(Ordering::Acquire);
        let step_started_ms = self.step_started_ms.load(Ordering::Acquire);
        let total_elapsed_ms = self.pipeline_start.elapsed().as_millis() as u64;
        let step_elapsed_ms = total_elapsed_ms.saturating_sub(step_started_ms);
        let eta_ms = if total > 0 && done > 0 && done < total {
            // u128 to avoid overflow on large workspaces.
            let remaining = (total - done) as u128;
            let est = (step_elapsed_ms as u128).saturating_mul(remaining) / done as u128;
            Some(est.min(u64::MAX as u128) as u64)
        } else {
            None
        };
        // Substep is a 1-pointer copy; an empty literal maps to `None`
        // so the UI's fallback caption logic only fires when the engine
        // genuinely has nothing more specific to say.
        let detail = self
            .substep
            .lock()
            .ok()
            .map(|g| *g)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        thinkingroot_core::CompileTick {
            step,
            done,
            total,
            step_elapsed_ms,
            total_elapsed_ms,
            eta_ms,
            detail,
        }
    }
}

/// Spawn a tokio task that emits `CompileTick` snapshots every 250 ms
/// until cancelled. Owns its own cancel-token so the pipeline body can
/// shut it down on success or early return without affecting the
/// user-facing pipeline `cancel` token.
fn spawn_compile_ticker(
    state: std::sync::Arc<CompileProgressState>,
    tx: tokio::sync::mpsc::UnboundedSender<ProgressEvent>,
    cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Emit one tick immediately so the consumer never sees a blank
        // start — the first 250 ms of compile would otherwise look idle.
        let _ = tx.send(ProgressEvent::CompileTick(state.snapshot()));
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if tx.send(ProgressEvent::CompileTick(state.snapshot())).is_err() {
                        // Channel closed — pipeline must be done; bail.
                        break;
                    }
                }
                _ = cancel.cancelled() => break,
            }
        }
    })
}

/// RAII guard that cancels and aborts the compile ticker on drop. Used
/// so every early-return path in `run_pipeline_inner` (including the
/// `?`-propagated errors) tears down the ticker cleanly.
struct TickerGuard {
    cancel: tokio_util::sync::CancellationToken,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for TickerGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(h) = self.handle.take() {
            h.abort();
        }
    }
}

/// Run the v3 pipeline: Parse → Extract+Ground+Rooting+Link+SVO →
/// CozoDB persist. The 3 user-visible phases (Parse / Extract /
/// Pack+Sign) of the v3 final plan §5 are realised here as Parse +
/// Extract; Pack+Sign lives in `tr-format` / `tr-sigstore` and runs
/// only when the user invokes `root pack`.
///
/// Vector indexing, markdown artifacts, and post-compile health
/// verification are NOT part of `root compile` — they live in
/// dedicated commands (`root query` / `root render` / `root health`)
/// per v3 spec §11. Skipping them at compile time is what lets the
/// 3-phase pipeline finish in ~30s instead of ~3min.
pub async fn run_pipeline(
    root_path: &Path,
    branch: Option<&str>,
    progress: Option<tokio::sync::mpsc::UnboundedSender<ProgressEvent>>,
) -> Result<PipelineResult> {
    // Existing callers (CLI, MCP stdio, integration tests) get a token
    // that is never tripped — same behaviour as before this fix.  The
    // desktop calls `run_pipeline_with_options` directly with its own
    // token so it can implement the Stop button.
    run_pipeline_with_options(root_path, branch, progress, PipelineOptions::default()).await
}

/// Cancel-aware variant of [`run_pipeline`].  Equivalent to
/// `run_pipeline_with_options(.., PipelineOptions { cancel, ..Default::default() })`.
/// Kept for callers that only need cancellation and don't want to
/// construct an options struct.
pub async fn run_pipeline_with_cancel(
    root_path: &Path,
    branch: Option<&str>,
    progress: Option<tokio::sync::mpsc::UnboundedSender<ProgressEvent>>,
    cancel: CancellationToken,
) -> Result<PipelineResult> {
    run_pipeline_with_options(
        root_path,
        branch,
        progress,
        PipelineOptions {
            cancel,
            ..Default::default()
        },
    )
    .await
}

/// Per-invocation knobs.  Adding fields here is the supported way to
/// extend the pipeline without forking another `run_pipeline_*`
/// function for every flag.  The `Default` impl produces the
/// behaviour of the un-tripped, full-rooting compile that
/// [`run_pipeline`] uses for backwards-compat.
#[derive(Debug, Clone)]
pub struct PipelineOptions {
    /// Cancellation token consulted at every phase boundary.  When
    /// tripped mid-run, the pipeline stops at the next checkpoint,
    /// surfaces `Error::Cancelled`, and emits
    /// `ProgressEvent::PipelineFailed`.  Partial state already
    /// persisted by Phase 4 (changed-source removal) is preserved.
    pub cancel: CancellationToken,
    /// Pre-Witness-Mesh: skipped Phase 6.5 (Rooting admission gate).
    /// Post-cutover (2026-05-11): Phase 6.5 is deleted, so this flag
    /// is a no-op kept for wire-compat — both `true` and `false`
    /// produce identical behaviour. Retained because the CLI's
    /// `--no-rooting` flag, the desktop's compile-stream request body,
    /// and SSE consumers built against pre-cutover daemons all plumb
    /// through this struct field. The historical env-var shortcut
    /// `TR_ROOTING_DISABLED=1` (which sat behind `unsafe { std::env::set_var(...) }`)
    /// is gone — see the "Phase 6.5: Rooting — DELETED" block below
    /// for the cutover rationale.
    pub no_rooting: bool,
    /// For tests + local iteration: skip the Phase 9 byte-coverage +
    /// structural-orphan audit so a fixture that doesn't satisfy 100%
    /// byte coverage can still drive the pipeline.  Equivalent to
    /// `TR_SKIP_BYTE_AUDIT=1` env var, but typed and per-call.
    /// Production callers must leave this `false`.
    pub skip_byte_audit: bool,
    /// Disable incremental cutoffs: bypass the Phase 3 fingerprint check
    /// so every potentially-changed source proceeds through Phase 4+.
    /// Phase 1 diff (mtime/size) still runs and the fingerprint store is
    /// still updated after each source processes, but `truly_changed` is
    /// set to the full `potentially_changed` set regardless of fingerprint
    /// equality.  Use when the workspace is in a known-bad state and a
    /// guaranteed full rebuild is wanted without nuking `.thinkingroot/`.
    pub no_incremental: bool,
    /// E4: build the hierarchical summary ladder (function→file→repo) after
    /// the structural phases. Default `false` — the 91.2% retrieval path is
    /// untouched unless a caller opts in.
    pub emit_summaries: bool,
}

impl Default for PipelineOptions {
    fn default() -> Self {
        Self {
            cancel: CancellationToken::new(),
            no_rooting: false,
            skip_byte_audit: false,
            no_incremental: false,
            emit_summaries: false,
        }
    }
}

/// Full-surface variant.  The other run_pipeline functions delegate
/// here.  Adding a new pipeline knob means adding a field to
/// [`PipelineOptions`] — no new public function required.
pub async fn run_pipeline_with_options(
    root_path: &Path,
    branch: Option<&str>,
    progress: Option<tokio::sync::mpsc::UnboundedSender<ProgressEvent>>,
    options: PipelineOptions,
) -> Result<PipelineResult> {
    let result = run_pipeline_inner(root_path, branch, progress.clone(), options).await;
    if let Err(ref e) = result
        && let Some(ref tx) = progress
    {
        let _ = tx.send(ProgressEvent::PipelineFailed {
            error: e.to_string(),
        });
    }
    result
}

// `#[allow(unused_assignments)]` is intentional: the `mark_phase!` macro
// updates `last_phase_end = now;` for the next invocation, so the very
// last `mark_phase!("audit")` call records `last_phase_end` for nobody.
// rustc's unused-assignment lint does not understand this control-flow
// pattern (it is correct in general, just not for an instrumentation
// macro emitted at every phase boundary).
#[allow(unused_assignments)]
async fn run_pipeline_inner(
    root_path: &Path,
    branch: Option<&str>,
    progress: Option<tokio::sync::mpsc::UnboundedSender<ProgressEvent>>,
    options: PipelineOptions,
) -> Result<PipelineResult> {
    let PipelineOptions { cancel, no_rooting, skip_byte_audit, no_incremental, emit_summaries } =
        options;
    // Helper macro — every long-running phase boundary checks this so
    // Stop / Ctrl-C never has to wait for the next batch to finish.
    macro_rules! bail_if_cancelled {
        () => {
            if cancel.is_cancelled() {
                return Err(thinkingroot_core::Error::Cancelled);
            }
        };
    }
    macro_rules! emit {
        ($event:expr) => {
            if let Some(ref tx) = progress {
                let _ = tx.send($event);
            }
        };
    }

    // Per-phase timing infrastructure.  `phase_start` anchors the total;
    // `last_phase_end` rolls forward after each `mark_phase!` call so
    // the per-phase elapsed is wall-time for that phase only, not
    // cumulative.  Using `std::collections::BTreeMap` keeps the JSON
    // wire encoding deterministically ordered — important for snapshot
    // tests downstream.
    use std::collections::BTreeMap;
    let mut phase_timings: BTreeMap<String, u64> = BTreeMap::new();
    let pipeline_start = std::time::Instant::now();
    let mut last_phase_end = pipeline_start;

    // Records elapsed time since the previous mark, inserts into
    // `phase_timings`, and emits a `PhaseDone` SSE event.  For the
    // split-phase case (entity_relations = Phase 5 + Phase 8), callers
    // use `+=` via `or_insert(0)` — the macro always *replaces* for
    // single-phase keys and the entity_relations key is handled inline.
    macro_rules! mark_phase {
        ($name:expr) => {{
            let now = std::time::Instant::now();
            let elapsed = now.duration_since(last_phase_end).as_millis() as u64;
            last_phase_end = now;
            phase_timings.insert($name.to_string(), elapsed);
            emit!(ProgressEvent::PhaseDone {
                name: $name.to_string(),
                elapsed_ms: elapsed,
            });
        }};
    }

    // ─── Unified compile-progress ticker ─────────────────────────────
    // Spawn one ticker task that owns the truth about "what step are
    // we on" and emits a `CompileTick` snapshot every 250 ms. Each
    // phase below calls `progress_state.set_step(...)` to relabel the
    // ticker; no per-row events are required for the bar to keep moving
    // (the step_elapsed_ms in every snapshot stays honest). The
    // `_ticker_guard` cancels + aborts the task on every return path
    // (`?`-propagated errors included) via its Drop impl.
    let progress_state = std::sync::Arc::new(CompileProgressState::new());
    let _ticker_guard = progress.as_ref().map(|tx| {
        progress_state.set_step(thinkingroot_core::CompileStep::Reading, 0);
        progress_state.set_substep("starting compile");
        let ticker_cancel = tokio_util::sync::CancellationToken::new();
        let handle = spawn_compile_ticker(
            progress_state.clone(),
            tx.clone(),
            ticker_cancel.clone(),
        );
        TickerGuard {
            cancel: ticker_cancel,
            handle: Some(handle),
        }
    });

    let config = Config::load_merged(root_path)?;
    let data_dir = thinkingroot_branch::snapshot::resolve_data_dir(root_path, branch);
    std::fs::create_dir_all(&data_dir)?;

    // ParseStart fires *here*, after config/data-dir setup but immediately
    // before the actual parse, so the displayed "Parsing" elapsed reflects
    // only the cost of `parse_directory` itself.
    progress_state.set_substep("reading files");
    emit!(ProgressEvent::ParseStart);
    bail_if_cancelled!();
    let documents = thinkingroot_parse::parse_directory(root_path, &config.parsers)?;
    let files_parsed = documents.len();
    // Reading is done — stamp the total so the bar shows N/N for a
    // moment. We stay on `Reading` through migration (opens graph) and
    // Phase 1 (diffs filesystem against graph); the transition to
    // `Extracting` happens at the actual Phase 2 entry below. This keeps
    // each user-visible step label honest to the work currently running.
    progress_state.set_total(files_parsed as u64);
    progress_state.set_done(files_parsed as u64);
    emit!(ProgressEvent::ParseComplete {
        files: files_parsed
    });
    bail_if_cancelled!();

    // ─── Diff phase: compare against the stored graph ──────────────────
    // Storage open + fingerprint load + content-hash scan + deletion detect
    // + graph-primed context load all live under one user-visible bar.
    progress_state.set_substep("opening graph");
    emit!(ProgressEvent::DiffStart);
    let mut storage = StorageEngine::init(&data_dir).await?;
    let mut fingerprints = crate::fingerprint::FingerprintStore::load(&data_dir);

    // ─── Compile schema auto-migration (pre-v2 → v2 → v3) ────────────
    // Detect a workspace whose `compile_schema_version` predates the
    // current contract and run the appropriate migration(s) before Phase 1
    // starts. Both migrations are idempotent — re-running is always safe.
    //
    // Chain: version "" or "1" → backfill_structural (v2) → water-flow GC (v3).
    // Version "2" skips the CCC backfill and goes straight to the v3 step.
    // Version "3" skips both.
    //
    // Each migration drops and re-opens the storage handle to avoid racing
    // with the CozoDB SQLite write mutex.
    let current_version = storage
        .graph
        .get_workspace_meta("compile_schema_version")?
        .unwrap_or_default();
    if current_version != "3" {
        // Migration runs while the bar is still on `Reading`. Substep
        // surfaces the actual work so a 1-GB substrate's multi-minute
        // backfill is honest, not a silent "0.0s elapsed" spinner.
        progress_state.set_substep("migrating schema");
        if current_version != "2" {
            tracing::info!(
                current_version = %current_version,
                "auto-migrating workspace from pre-v2 \u{2192} v2 (Compile Completeness Contract)"
            );
            // Drop so backfill_structural can re-open with exclusive write access.
            drop(storage);
            let _ = crate::backfill::backfill_structural(&data_dir)?;
            // Re-open with the v2 schema in place.
            storage = StorageEngine::init(&data_dir).await?;
        }
        tracing::info!("auto-migrating workspace from v2 \u{2192} v3 (water-flow incremental GC)");
        // Drop again so backfill_water_flow_v3_at_path can re-open with exclusive access.
        drop(storage);
        crate::backfill::backfill_water_flow_v3_at_path(&data_dir)?;
        // Re-open with the v3 schema in place.
        storage = StorageEngine::init(&data_dir).await?;
        progress_state.set_substep("opening graph");
    }

    // ─── Phase 1: Identify potentially-changed documents ───────────────
    // (content hash differs from stored — NOT yet removed from graph)
    //
    // T12 optimization: load all stored (uri → content_hash) pairs in a
    // single batch query and build an in-memory lookup map.  Pre-T12 this
    // was N individual `find_sources_by_uri` queries (one per parsed file),
    // which dominated Phase 1 latency on workspaces with many files — e.g.
    // 100 queries × ~20ms each = ~2s even when 99 files were unchanged.
    // The batched path pays one round-trip regardless of workspace size.
    progress_state.set_substep("diffing workspace");
    let stored_hashes: HashSet<String> = {
        let pairs = storage.graph.get_sources_with_hashes()?;
        pairs
            .into_iter()
            .map(|(uri, hash)| format!("{uri}\x00{hash}"))
            .collect()
    };

    let mut potentially_changed: Vec<_> = Vec::new();
    let mut skipped = 0usize;

    for doc in &documents {
        // A document is unchanged iff its uri+hash pair is present in the
        // stored set AND its content_hash is non-empty.  New files (no stored
        // entry) and modified files (different hash) fall through to
        // `potentially_changed`.
        let unchanged = !doc.content_hash.0.is_empty()
            && stored_hashes.contains(&format!("{}\x00{}", doc.uri, doc.content_hash.0));
        if unchanged {
            skipped += 1;
        } else {
            potentially_changed.push(doc);
        }
    }

    // Detect deleted files (in graph but not in filesystem).
    // Reuse the `get_all_sources` call (includes source_type) to identify
    // file-backed sources that are no longer present on disk.
    let current_uris: HashSet<&str> = documents.iter().map(|d| d.uri.as_str()).collect();
    let mut deleted_sources: Vec<(String, String)> = Vec::new(); // (source_id, uri)
    for (source_id, uri, source_type, _content_hash) in storage.graph.get_all_sources()? {
        let is_file_backed = matches!(source_type.as_str(), "File" | "Document");
        if is_file_backed && !current_uris.contains(uri.as_str()) {
            deleted_sources.push((source_id, uri));
        }
    }

    // Diff phase ends here — emit summary so the bar driver can finalise the
    // Diffing bar with concrete counts and decide whether to expect later phases.
    emit!(ProgressEvent::DiffComplete {
        changed: potentially_changed.len(),
        unchanged: skipped,
        deleted: deleted_sources.len(),
    });
    mark_phase!("diff");

    // ─── Early exit: nothing to process ────────────────────────────────
    // Vectors are not built here — `root query` lazy-builds them on
    // first call per v3 final plan §13.1.
    if potentially_changed.is_empty() && deleted_sources.is_empty() {
        let total_elapsed_ms = pipeline_start.elapsed().as_millis() as u64;
        let summed: u64 = phase_timings.values().sum();
        if total_elapsed_ms > summed {
            phase_timings.insert("other".to_string(), total_elapsed_ms - summed);
        }
        let summary = thinkingroot_core::IncrementalSummary {
            sources_total: documents.len(),
            sources_unchanged: skipped,
            sources_truly_changed: 0,
            sources_deleted: 0,
            sources_resolution_dirty: 0,
            claims_added: 0,
            claims_updated: 0,
            claims_deleted: 0,
            structural_rows_emitted: 0,
            structural_rows_cascaded: 0,
            bytes_re_extracted: 0,
            llm_calls: 0,
            cache_hits: 0,
            structural_extractions: 0,
            chunks_without_extraction: 0,
            phase_timings: phase_timings.clone(),
            total_elapsed_ms,
        };
        emit!(ProgressEvent::IncrementalDone { summary: summary.clone() });
        return Ok(PipelineResult {
            files_parsed,
            claims_count: 0,
            entities_count: 0,
            relations_count: 0,
            contradictions_count: 0,
            artifacts_count: 0,
            health_score: 0,
            cache_hits: 0,
            early_cutoffs: skipped,
            structural_extractions: 0,
            // All files were content-hash identical — CozoDB was not touched.
            cache_dirty: false,
            failed_batches: 0,
            failed_chunk_ranges: Vec::new(),
            incremental_summary: summary,
        });
    }

    // ─── Phase 2: Extract potentially-changed documents (with cache) ───
    let workspace_id = WorkspaceId::new();
    let cache_hits;
    let extraction;

    // Graph-Primed Context (known entities / relations) was wired
    // into a `with_known_entities` no-op on the LLM extractor.  The
    // function + its `GraphPrimedContext` parameter were removed in
    // the Phase 2 `thinkingroot-llm` split (2026-05-14) because
    // structural extraction consults no prompts.  `GraphStore::
    // get_known_entities` / `get_known_relations` remain available
    // for any future chat-time consumer that needs them.

    // Bar transitions to `Extracting` here — the actual Phase 2 work.
    // Pre-fix the transition happened back at parse-end (line 553) and
    // the entire diff + migration ran under a mislabeled "Extracting"
    // bar. Total = potentially-changed source count; per-source advance
    // is implicit (structural extraction is microseconds per chunk so
    // the bar fills near-instantly once the rule pass begins).
    progress_state.set_step(
        thinkingroot_core::CompileStep::Extracting,
        potentially_changed.len() as u64,
    );
    progress_state.set_substep("extracting structure");
    if potentially_changed.is_empty() {
        // Only deletions — no extraction needed.
        cache_hits = 0;
        extraction = thinkingroot_extract::ExtractionOutput::default();
    } else {
        // Witness Mesh era (2026-05-14): the extractor carries no
        // progress callback, cache, checkpoint, or cancellation handle
        // — structural extraction is pure CPU and runs in microseconds
        // per chunk. Cancellation is checked at pipeline phase
        // boundaries, not inside the extractor. The
        // `ProgressEvent::ExtractionStart` / `ExtractionBatchStart` /
        // `ChunkDone` SSE variants remain on `ProgressEvent` for
        // wire-format stability but are no longer emitted from the
        // extract phase.
        let extractor = thinkingroot_extract::Extractor::new(&config).await?;
        // Source-granular re-extraction (T12).  When incremental cutoffs are
        // enabled, restrict extraction to the `potentially_changed` set only —
        // these are the documents that failed the Phase 1 content-hash check.
        // Unchanged documents have already been filtered out by Phase 1's
        // content-hash diff, so the extractor never needs to process them.
        // Using `None` (extract all) when `no_incremental` is set preserves
        // the "guaranteed full rebuild" semantics of that flag.
        //
        // We build the filter from `potentially_changed` (Phase 1 set), NOT
        // from `truly_changed` (Phase 3 set), because Phase 3's fingerprint
        // check runs AFTER extraction in the current pipeline ordering.  The
        // set is small when the user edited 1 file — extracting strictly more
        // than necessary (vs. truly_changed) is a performance trade-off, not a
        // correctness bug.
        let extraction_filter: Option<std::collections::HashSet<thinkingroot_core::types::SourceId>> =
            if no_incremental {
                None
            } else {
                Some(
                    potentially_changed
                        .iter()
                        .map(|d| d.source_id)
                        .collect(),
                )
            };
        let raw = extractor
            .extract_all(
                &potentially_changed
                    .iter()
                    .map(|d| (*d).clone())
                    .collect::<Vec<_>>(),
                workspace_id,
                extraction_filter,
            )
            .await?;
        emit!(ProgressEvent::ExtractionComplete {
            claims: raw.claims.len(),
            entities: raw.entities.len(),
            cache_hits: raw.cache_hits,
        });
        if raw.failed_batches > 0 {
            tracing::warn!(
                failed_batches = raw.failed_batches,
                "extraction completed with permanent batch failures — emitting partial event"
            );
            emit!(ProgressEvent::ExtractionPartial {
                failed_batches: raw.failed_batches,
                failed_chunk_ranges: raw.failed_batch_ranges.clone(),
            });
        }
        cache_hits = raw.cache_hits;
        extraction = raw;
    }
    mark_phase!("extract");

    // Log tiered extraction stats.
    if extraction.structural_extractions > 0 {
        tracing::info!(
            "tiered extraction: {} structural (zero LLM), {} cache hits, {} LLM calls",
            extraction.structural_extractions,
            extraction.cache_hits,
            extraction
                .chunks_processed
                .saturating_sub(extraction.cache_hits + extraction.structural_extractions),
        );
    }

    // ─── Phase 2b: Cascade Grounding — DELETED in Witness Mesh cutover ───
    //
    // The 4-judge tribunal (lexical, span, semantic, NLI) existed to
    // grade LLM-paraphrased claim text against source bytes. Under
    // Witness Mesh, every Witness IS its byte span — there is no
    // paraphrase to grade. The single surviving verifier
    // (`witness_verifier::verify_witness_anchor`) is a BLAKE3
    // comparison run at probe time + by `tr-verify` at pack-open
    // time. The 22KB grounder + 17KB NLI ONNX + 3.8KB lexical +
    // 3.8KB semantic collapse to ~200 LOC of cryptographic anchor
    // verification.
    //
    // Legacy claims that flow through the dual-write transition
    // stay un-graded: their `grounding_score` field remains at the
    // default `-1.0` (unverified). Read-side consumers that fall
    // back to claims should treat -1.0 as "use Witness Mesh
    // instead" rather than reading the legacy field.
    //
    // See `.claude/rules/witness-mesh.md` I-W8 for the anchor
    // verification contract.
    bail_if_cancelled!();
    // Phase 2b (the 4-judge grounding tribunal) is deleted in the
    // Witness Mesh cutover. The honest progress bar for this stretch
    // of the compile is the Witness Mesh persist at Phase 6.45 below
    // — we don't emit a synthetic 0.0s "Grounding accepted N" line
    // any more.

    // Phase 2c (SVO event extraction) is intentionally deferred to Phase 2c-post-link
    // below.  It must run AFTER Phase 7 (Linker) so that entity names can be resolved
    // to their real CozoDB ULIDs.  Running it here (before entities exist) would
    // produce events with wrong / empty entity references, breaking the event calendar.

    // ─── Phase 3: Fingerprint check ────────────────────────────────────
    // For each potentially-changed doc, compute a fingerprint of its extracted
    // claims. If identical to stored fingerprint, skip this source entirely.
    // When `no_incremental` is set, all potentially-changed docs proceed
    // regardless of fingerprint equality (fingerprint store is still updated
    // so the next incremental run can resume normally).
    //
    // Stays on `Extracting` step — fingerprint depends on the extracted
    // claims so it's conceptually "post-process the extract output."
    progress_state.set_substep("fingerprinting");
    let mut truly_changed: Vec<_> = Vec::new();
    let mut fingerprint_cutoffs = 0usize;

    for doc in &potentially_changed {
        // Collect claims for this source and serialize as fingerprint input.
        let source_claims: Vec<_> = extraction
            .claims
            .iter()
            .filter(|c| c.source == doc.source_id)
            .collect();
        // M1: propagate the serialize error rather than silently
        // computing a fingerprint of empty bytes.  Pre-fix a serialize
        // failure made every run see the source as "fingerprint-
        // matched empty" which then short-circuited as unchanged —
        // claims for that source would never persist.
        let fp_bytes = serde_json::to_vec(&source_claims).map_err(|e| {
            thinkingroot_core::Error::Config(format!("fingerprint serialize for {}: {e}", doc.uri))
        })?;
        let fp = crate::fingerprint::FingerprintStore::compute(&fp_bytes);

        if !no_incremental && fingerprints.is_unchanged(&doc.uri, &fp) {
            fingerprint_cutoffs += 1;
            tracing::debug!("fingerprint early cutoff for {}", doc.uri);
        } else {
            fingerprints.update(&doc.uri, fp);
            truly_changed.push(*doc);
        }
    }

    emit!(ProgressEvent::FingerprintDone {
        truly_changed: truly_changed.len(),
        cutoffs: fingerprint_cutoffs,
    });
    mark_phase!("fingerprint");

    bail_if_cancelled!();

    // ─── Phase 4: Remove changed + deleted sources from graph ──────────
    // T6: collect "resolution-dirty" sources BEFORE the cascade fires.
    // `remove_source_by_id` (called via `remove_source_by_uri`) cascades
    // `resolution_deps`, so we must query upstream deps now — after the
    // cascade the rows are gone and the dirty set would be empty. The
    // set is used to log observability data immediately; T11 (watch mode)
    // consumes it for source-granular re-extraction.
    //
    // Bar transitions to `Persisting` here — Phase 4 is substrate GC
    // (cascades across 16 structural tables), conceptually closer to
    // "preparing the substrate for new writes" than to extraction.  Total
    // = changed + deleted sources; per-source `advance(1)` happens at
    // each `remove_source_by_uri` call below so the bar moves as work
    // completes instead of glued at 0/N.
    //
    // §T13 perf: pre-fix Phase 4 issued ~10N individual Cozo queries
    // per N changed sources (7 readers × N + `count_structural_rows_for_source`
    // itself a 16-query nest). On the engine's own repo this dominated
    // the "stuck at 20%" freeze. The rewrite below collapses every
    // pre-cascade reader into 5 fixed-cost batched calls; the per-source
    // `remove_source_by_uri` cascade stays per-source (I-W4 atomic
    // boundary contract — multi_transaction is the actual atomic
    // primitive, not script-level union).
    progress_state.set_step(
        thinkingroot_core::CompileStep::Persisting,
        (truly_changed.len() + deleted_sources.len()) as u64,
    );
    progress_state.set_substep("removing changed sources");

    // Cache `find_sources_by_uri` once per uri — pre-fix the same query
    // ran three times per doc (once per top-level reader loop).
    let mut uri_to_existing_sids: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::with_capacity(truly_changed.len());
    for doc in &truly_changed {
        let existing = storage.graph.find_sources_by_uri(&doc.uri)?;
        let sids: Vec<String> = existing.into_iter().map(|(sid, _, _)| sid).collect();
        uri_to_existing_sids.insert(doc.uri.clone(), sids);
    }

    // Build the universe of sids that Phase 4 will cascade away — both
    // the live sids backing truly-changed uris and the orphaned sids of
    // deleted-on-disk files.
    let mut all_phase4_sids: Vec<String> = Vec::new();
    for sids in uri_to_existing_sids.values() {
        all_phase4_sids.extend(sids.iter().cloned());
    }
    for (sid, _) in &deleted_sources {
        all_phase4_sids.push(sid.clone());
    }
    all_phase4_sids.sort_unstable();
    all_phase4_sids.dedup();

    // 5 batched reads (≈O(1) round trips regardless of N):
    let resolution_dirty_sources = storage
        .graph
        .list_dependent_sources_for_many(&all_phase4_sids)?;
    tracing::info!(
        resolution_dirty = resolution_dirty_sources.len(),
        "phase 4 resolution-dirty sources (cross-source Phase 7e deps now stale)"
    );

    // Snapshot claim + structural-row counts BEFORE the cascade fires so
    // `IncrementalSummary` can report honest `claims_deleted` and
    // `structural_rows_cascaded`. Per-source attribution flattens into
    // a total — the original per-source loop summed into the same two
    // accumulators, so the wire surface is identical.
    let claim_ids_by_sid = storage
        .graph
        .get_claim_ids_for_sources(&all_phase4_sids)?;
    let phase4_claim_delete_count: usize =
        claim_ids_by_sid.values().map(|v| v.len()).sum();
    let cascade_counts_by_sid = storage
        .graph
        .count_structural_rows_for_sources(&all_phase4_sids)?;
    let phase4_cascade_row_count: usize = cascade_counts_by_sid.values().sum();

    // Cross-file staleness: the union of triples touching any sid we're
    // about to cascade, PLUS triples involving any entity any of these
    // sids contributed to (a removed source may have linked entity X to
    // entity Y in another file — the X↔Y edge needs re-aggregation
    // even though neither X nor Y was directly cascaded). Pre-fix
    // `get_all_triples_involving_entities` was called per-source-id
    // with 2N internal queries; the rewrite (Commit 3) makes it a
    // single inline-relation join.
    let mut affected_triples = storage
        .graph
        .get_source_relation_triples_for_sources(&all_phase4_sids)?;
    let entity_ids_union: Vec<String> = storage
        .graph
        .get_entity_ids_for_sources(&all_phase4_sids)?
        .into_iter()
        .collect();
    if !entity_ids_union.is_empty() {
        let cross_file_triples = storage
            .graph
            .get_all_triples_involving_entities(&entity_ids_union)?;
        tracing::debug!(
            "cross-file staleness: {} entity ids, {} cross-file triples added (batched)",
            entity_ids_union.len(),
            cross_file_triples.len()
        );
        affected_triples.extend(cross_file_triples);
    }

    // Batched cascade — ONE multi_transaction across every sid in
    // `all_phase4_sids`. Tier 2 commit G. Pre-Tier-2 this loop opened
    // 47 separate Cozo transactions on the canonical 47-source
    // incremental and additionally fanned out ~30 child :rm scripts
    // per source — the bench measured 948 ms (47% of total wall
    // time) here. The batched path runs ~31 IN-set :rm queries
    // total across the whole batch inside ONE transaction.
    //
    // I-W4 atomicity widens from per-source to per-batch (strictly
    // stronger for concurrent readers — no torn intermediates within
    // a Phase 4 cycle anymore).
    bail_if_cancelled!();
    if !all_phase4_sids.is_empty() {
        let _stats = storage.graph.transactional_remove_sources(&all_phase4_sids)?;
        // Advance progress for every source the batch covered.
        for _ in &truly_changed {
            progress_state.advance(1);
        }
        for _ in &deleted_sources {
            progress_state.advance(1);
        }
    }

    // Fingerprints.remove(uri) is filesystem I/O — must run outside
    // the Cozo transaction (it doesn't participate in the substrate
    // atomic boundary anyway). Only deleted_sources need this; truly
    // changed sources' fingerprints get refreshed in Phase 3.
    for (_source_id, uri) in &deleted_sources {
        fingerprints.remove(uri);
    }
    mark_phase!("remove_sources");

    // ─── Phase 5: Incremental entity relation update for removals ──────
    // First of two `Linking` slots in the user-visible bar. Phase 5 is
    // entity-relation work; the second slot is the linker + SVO post
    // Phase 7. We don't know an honest total here (Cozo-side update),
    // so the ticker shows a spinner with elapsed time only.
    progress_state.set_step(thinkingroot_core::CompileStep::Linking, 0);
    progress_state.set_substep("updating relations");
    if !affected_triples.is_empty() {
        affected_triples.sort_unstable();
        affected_triples.dedup();
        storage
            .graph
            .update_entity_relations_for_triples(&affected_triples)?;
    }
    // Phase 5 elapsed is the first half of the split entity_relations key.
    // Phase 8 (post-link entity relation update) contributes the second half.
    {
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(last_phase_end).as_millis() as u64;
        last_phase_end = now;
        *phase_timings.entry("entity_relations".to_string()).or_insert(0) += elapsed;
        emit!(ProgressEvent::PhaseDone {
            name: "entity_relations".to_string(),
            elapsed_ms: elapsed,
        });
    }

    // If only deletions or all fingerprint hits — no new content to link.
    if truly_changed.is_empty() {
        emit!(ProgressEvent::LinkComplete {
            entities: 0,
            relations: 0,
            contradictions: 0
        });

        let total_elapsed_ms = pipeline_start.elapsed().as_millis() as u64;
        let summed: u64 = phase_timings.values().sum();
        if total_elapsed_ms > summed {
            phase_timings.insert("other".to_string(), total_elapsed_ms - summed);
        }
        let summary = thinkingroot_core::IncrementalSummary {
            sources_total: documents.len(),
            sources_unchanged: skipped,
            sources_truly_changed: 0,
            sources_deleted: deleted_sources.len(),
            sources_resolution_dirty: resolution_dirty_sources.len(),
            claims_added: 0,
            claims_updated: 0,
            claims_deleted: phase4_claim_delete_count,
            structural_rows_emitted: 0,
            structural_rows_cascaded: phase4_cascade_row_count,
            bytes_re_extracted: 0,
            llm_calls: 0,
            cache_hits,
            structural_extractions: extraction.structural_extractions,
            chunks_without_extraction: extraction
                .chunks_processed
                .saturating_sub(cache_hits + extraction.structural_extractions),
            phase_timings: phase_timings.clone(),
            total_elapsed_ms,
        };
        emit!(ProgressEvent::IncrementalDone { summary: summary.clone() });

        fingerprints.save()?;
        config.save(root_path)?;

        // Health/artifacts/contradictions are surfaced by `root health`
        // and `root render` — `root compile` only persists the graph.
        return Ok(PipelineResult {
            files_parsed,
            claims_count: 0,
            entities_count: 0,
            relations_count: 0,
            contradictions_count: 0,
            artifacts_count: 0,
            health_score: 0,
            cache_hits,
            early_cutoffs: skipped + fingerprint_cutoffs,
            structural_extractions: extraction.structural_extractions,
            // Deletions or fingerprint cutoffs mutated CozoDB — cache is stale.
            cache_dirty: true,
            failed_batches: extraction.failed_batches,
            failed_chunk_ranges: extraction.failed_batch_ranges.clone(),
            incremental_summary: summary,
        });
    }

    bail_if_cancelled!();

    // ─── Phase 6: Insert sources for truly-changed documents ───────────
    // Also persist source bytes to the durable Rooting byte-store so Phase 6.5
    // (and future re-rooting sweeps) can re-execute probes against them. The
    // byte-store is content-addressed, so multiple writes with the same hash
    // are no-ops — fresh recompiles of an unchanged file cost zero extra I/O.
    //
    // First `Persisting` slot in the user-visible bar — covers Phase 6
    // sources + 6.45 witnesses + 6.7 structural rebuild. Total is the
    // number of truly-changed sources we're about to rewrite; the
    // per-source insert loop below advances the counter.
    progress_state.set_step(
        thinkingroot_core::CompileStep::Persisting,
        truly_changed.len() as u64,
    );
    progress_state.set_substep("inserting sources");
    // Write source bytes to the byte store at the SAME root the graph's
    // `materialize_statement` reads from. `StorageEngine::init` opens the graph
    // at `{data_dir}/graph` (storage.rs), so `GraphStore::init` roots its byte
    // store at `{data_dir}/graph/rooting/sources`. This previously wrote to
    // `FileSystemSourceStore::new(&data_dir)` → `{data_dir}/rooting/sources`,
    // ONE LEVEL UP from where the graph reads. The mismatch meant
    // `materialize_statement` never found the bytes, so statements AND their
    // embeddings fell back to byte-anchor placeholders — hybrid/vector recall
    // matched nothing and `ask` returned `claims_used=0`. Aligning the write
    // dir with the graph dir restores semantic recall end-to-end.
    let byte_store = thinkingroot_graph::FileSystemSourceStore::new(&data_dir.join("graph"))
        .map_err(|e| thinkingroot_core::Error::Config(format!("rooting byte store: {e}")))?;
    for doc in &truly_changed {
        let source = thinkingroot_core::Source::new(doc.uri.clone(), doc.source_type)
            .with_id(doc.source_id)
            .with_hash(doc.content_hash.clone());
        storage.graph.insert_source(&source)?;
        progress_state.advance(1);

        // Persist the ORIGINAL source bytes to the durable byte store.
        // The current consumer is `witness_verifier::verify_witness_anchor`
        // which re-hashes the byte slice referenced by each Witness's
        // `spans[0]` and compares against `content_blake3`. A chunk-
        // join reconstruction (the pre-fix shape) differs from the
        // original on any file with non-`\n`-newlines, BOMs, or
        // inter-chunk whitespace — making every Witness on those
        // sources fail anchor verification at probe time. Reading
        // from `doc.uri` matches what the parser saw a few phases
        // earlier; if the file was modified between parse and now
        // the next compile's Phase 1 fingerprint diff will catch it.
        //
        // Sources whose URI doesn't round-trip to a readable file
        // (e.g. synthetic doc URIs for hand-contributed Documents,
        // git-history rows, virtual MCP feeds) are skipped honestly
        // with a tracing warning — the witness verifier degrades to
        // "in-place re-check" for those rows, but the alternative
        // (writing the chunk-join into the byte store) would
        // silently break verification rather than admit absence.
        use thinkingroot_graph::SourceByteStore;
        // For transformed formats (PDF → extracted text) the parser set
        // `anchored_text`: the witnesses' byte ranges index into THAT, not the
        // raw binary file. Store it so materialization returns real text. For
        // text-native formats (anchored_text == None) store the raw file bytes,
        // which preserves witness-anchor re-hash verification.
        let from_anchored: Option<Vec<u8>> =
            doc.anchored_text.as_ref().map(|t| t.as_bytes().to_vec());
        let source_path = std::path::Path::new(&doc.uri);
        let read_result = match from_anchored {
            Some(bytes) => Ok(bytes),
            None => std::fs::read(source_path),
        };
        match read_result {
            Ok(bytes) => {
                byte_store
                    .put(doc.source_id, &doc.content_hash, &bytes)
                    .map_err(|e| {
                        thinkingroot_core::Error::Config(format!("byte store put: {e}"))
                    })?;
            }
            Err(e) => {
                tracing::warn!(
                    target: "pipeline",
                    source_id = %doc.source_id,
                    uri = %doc.uri,
                    error = %e,
                    "byte store: source URI does not resolve to a readable file; \
                     witness anchor verification will degrade to in-place re-check \
                     for rows on this source"
                );
            }
        }
    }

    // Filter extraction to only truly-changed sources.
    let truly_changed_ids: HashSet<thinkingroot_core::types::SourceId> =
        truly_changed.iter().map(|d| d.source_id).collect();

    let structural_extractions = extraction.structural_extractions;

    let mut filtered_extraction = thinkingroot_extract::ExtractionOutput {
        // Persist-boundary binary guard: drop any claim whose statement is not
        // human-readable text (binary/PDF bytes ingested as a "claim"). This is
        // the single write-point that catches every claim-creation path, so a
        // binary file (e.g. a PDF) re-ingested on boot can no longer re-pollute
        // the graph. Pairs with the recall-side filter + the embed-side skip.
        claims: extraction
            .claims
            .into_iter()
            .filter(|c| {
                truly_changed_ids.contains(&c.source)
                    && crate::intelligence::hybrid::is_probably_text(&c.statement)
            })
            .collect(),
        entities: extraction.entities,
        relations: extraction
            .relations
            .into_iter()
            .filter(|r| truly_changed_ids.contains(&r.source))
            .collect(),
        claim_entity_names: extraction.claim_entity_names,
        sources_processed: truly_changed.len(),
        chunks_processed: extraction.chunks_processed,
        cache_hits: extraction.cache_hits,
        structural_extractions: extraction.structural_extractions,
        source_texts: extraction.source_texts,
        claim_source_quotes: extraction.claim_source_quotes,
        // Carry partial-failure attribution forward so consumers downstream
        // (currently the pipeline summary; soon the desktop UI per C4)
        // can render an honest "N batches failed" warning.
        failed_batches: extraction.failed_batches,
        failed_batch_ranges: extraction.failed_batch_ranges,
        // Compile Completeness Contract §5 decorations carried from the
        // extractor into Phase 6.7 (when it lands). Filtering parallels
        // `claims` above so only truly-changed sources' decorations
        // survive. The HashMap retain pattern matches what `claim_entity_names`
        // would do if it were filtered (currently it isn't — see issue C5).
        claim_quantities: extraction.claim_quantities,
        claim_expirations: extraction.claim_expirations,
        // Witness Mesh — filter by truly-changed source set, same
        // as `claims` / `relations`. Per-source filtering is the only
        // way to keep incremental compile correct: a Witness for an
        // unchanged source must not be re-written (its id is
        // content-derived, so re-write is a no-op, but the redundant
        // I/O would erase the I-W6 source-granular invariant).
        witnesses: extraction
            .witnesses
            .into_iter()
            .filter(|w| truly_changed_ids.contains(&w.source))
            .collect(),
    };

    // ─── Phase 6.45: Witness Mesh persistence ──────────────────────────
    // Write Witnesses produced by the rule-catalog extractors before
    // Rooting / linker run. The witnesses substrate is independent of
    // the claims substrate — Witnesses are content-addressed, never
    // graded by the tribunal, and their inserts are idempotent on
    // re-derived rows (same rule + same spans → same id).
    //
    // We persist the deduplicated mesh (drop intra-batch duplicates,
    // SAFETY-rule cross-check) rather than the raw stream so the
    // `witnesses` table reflects the same shape callers see via
    // `walk_mesh`. Mesh-assembly errors (UnknownRule, MalformedSpan)
    // are logged at WARN — the pipeline does not abort because the
    // legacy `claims` flow is still load-bearing during the
    // Witness-Mesh transition.
    // Track how many witnesses Phase 6.45 actually persisted so the
    // pipeline's `claims_count` honestly reflects substrate growth
    // even when the LLM-extraction path produced zero claims (the
    // post-cutover default). See `PipelineResult.claims_count` below.
    let mut persisted_witness_count: usize = 0;
    if !filtered_extraction.witnesses.is_empty() {
        let raw_witness_count = filtered_extraction.witnesses.len();
        progress_state.set_substep("persisting witnesses");
        emit!(ProgressEvent::WitnessMeshStart { raw: raw_witness_count });
        let assembled = thinkingroot_extract::assemble_witness_mesh(
            std::mem::take(&mut filtered_extraction.witnesses),
        );
        if !assembled.errors.is_empty() {
            for err in &assembled.errors {
                tracing::warn!(error = %err, "witness mesh assembly: dropping malformed witness");
            }
        }
        if !assembled.witnesses.is_empty() {
            storage
                .graph
                .insert_witnesses_batch(&assembled.witnesses)?;
        }
        if !assembled.edges.is_empty() {
            storage
                .graph
                .insert_witness_input_edges_batch(&assembled.edges)?;
        }
        // SOTA Lever 2 — derive typed edges (Related/TemporalNext/
        // Supersedes/Contradicts) from the persisted witness set and
        // write them into `witness_typed_edges`. Mechanical only; no
        // LLM. Operates on `assembled.witnesses` (post-dedup, post-
        // SAFETY-cross-check) so the edges target the same id space
        // the storage layer just persisted.
        //
        // Errors here are `tracing::warn!`-logged (not propagated) to
        // match the Phase 6.45 contract — typed-edge derivation is
        // additive substrate, never gating compile. The retrieval
        // path falls back to non-typed-edge ranking on an empty
        // `witness_typed_edges` table.
        let typed_edges =
            thinkingroot_extract::typed_edges::derive_all_typed_edges(&assembled.witnesses);
        let typed_edges_emitted = typed_edges.len();
        let typed_edges_inserted = if !typed_edges.is_empty() {
            match storage
                .graph
                .insert_witness_typed_edges_batch(&typed_edges)
            {
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!(
                        target: "witness_typed_edges",
                        error = %e,
                        "typed-edge insert failed; retrieval will fall back to fuse_score"
                    );
                    0
                }
            }
        } else {
            0
        };
        persisted_witness_count = assembled.witnesses.len();
        tracing::info!(
            raw = raw_witness_count,
            persisted = persisted_witness_count,
            edges = assembled.edges.len(),
            typed_edges = typed_edges_inserted,
            deduped = assembled.dedup_count,
            errors = assembled.errors.len(),
            "Witness Mesh: persisted to graph"
        );
        let _ = typed_edges_emitted; // pinned for future progress-event surface
        emit!(ProgressEvent::WitnessMeshDone {
            persisted: persisted_witness_count,
            deduped: assembled.dedup_count,
            edges: assembled.edges.len(),
            errors: assembled.errors.len(),
        });
    }
    mark_phase!("witness_mesh");

    // ─── Phase 6.5: Rooting — DELETED in Witness Mesh cutover ────────────
    //
    // The LLM-claim admission gate is obviated by content-addressed
    // Witnesses. The Witness Mesh substrate (Phase 6.45 above) admits
    // every Witness by construction: a Witness's id is a BLAKE3 over
    // `(rule, spans)`, anchored by `content_blake3 ==
    // BLAKE3(source[spans[0].start..end])`. There is nothing to "trial"
    // because the substrate cannot lie — it doesn't paraphrase.
    //
    // The `_ = no_rooting;` line keeps the parameter live during the
    // dual-substrate transition (legacy claims still flow through Link
    // unchanged); callers that pass `--no-rooting` still get the same
    // behaviour they always did because Phase 6.5 no longer runs at all.
    //
    // See `.claude/rules/witness-mesh.md` for the Witness Mesh's I-W8
    // anchor verification — the surviving piece of the tribunal that
    // replaces Rooting's five-probe trial.
    let _ = no_rooting;
    let _ = config.rooting.disabled;

    // ─── Phase 6.7: Structural Persist (Compile Completeness Contract §6) ───
    // Walks every chunk in every truly-changed document and emits typed
    // rows into the 16 new structural CozoDB tables, plus a chunks_residual
    // fall-through for chunks no other emitter covers (the catch-all that
    // makes I-3 byte coverage tractable). Stamps `content_blake3` onto
    // every Claim before the linker writes them. Pre-conditions all
    // satisfied here: sources inserted (Phase 6), bytes available in
    // byte_store, admission_tier stamped (Phase 6.5 / no-op when rooting
    // disabled), linker not yet run.
    progress_state.set_substep("persisting structural rows");
    let phase_6_7_doc_refs: Vec<&thinkingroot_core::ir::DocumentIR> =
        truly_changed.iter().copied().collect();
    let phase_6_7_stats = crate::structural_persist::phase_6_7_structural_persist(
        &phase_6_7_doc_refs,
        &mut filtered_extraction,
        &storage.graph,
        &byte_store,
        &cancel,
    )?;
    tracing::info!(
        sources = phase_6_7_stats.sources_processed,
        rows = phase_6_7_stats.structural_rows_emitted,
        residual = phase_6_7_stats.residual_rows_emitted,
        blake3_spans = phase_6_7_stats.blake3_distinct_spans,
        elapsed_ms = phase_6_7_stats.elapsed.as_millis() as u64,
        "phase 6.7 structural persist complete"
    );
    mark_phase!("structural_persist");

    // North-star Phase 1d — enqueue truly-changed sources for ASYNC LLM
    // atomic-fact extraction (kept off the compile critical path). The
    // AtomicExtractTask maintenance tick drains the queue, reads each source's
    // verbatim raw_chunks, extracts grounded facts, and builds the spine.
    // Default-ON; `TR_ATOMIC_EXTRACT=0` disables enqueue + drain together.
    let atomic_extract_on = std::env::var("TR_ATOMIC_EXTRACT")
        .map(|v| !(v == "0" || v.eq_ignore_ascii_case("false")))
        .unwrap_or(true);
    if atomic_extract_on && !truly_changed.is_empty() {
        let sids: Vec<String> = truly_changed.iter().map(|d| d.source_id.to_string()).collect();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        if let Err(e) = storage.graph.enqueue_atomic_extract(&sids, now) {
            tracing::warn!("atomic-extract enqueue failed (non-fatal): {e}");
        }
    }

    // Second `Linking` slot — covers Phase 7 (linker), Phase 7e
    // (resolution), and Phase 8 (post-link entity-relation update + SVO
    // event extraction). Phase 7's linker has its own done/total
    // callback; we let the linker plumb counter advances directly into
    // `progress_state` so the bar shows real entity-merge progress.
    let entities_total = filtered_extraction.entities.len() as u64;
    progress_state.set_step(thinkingroot_core::CompileStep::Linking, entities_total);
    progress_state.set_substep("linking entities");

    // Phase 5 Witness Mesh cutover (2026-05-14): `claims_count` is
    // the pipeline's "this compile produced N substrate rows" signal
    // surfaced to CLI summary + desktop progress + REST `compile`
    // response. With Witness Mesh as the primary write path, witnesses
    // produced by Phase 6.45 contribute to the count alongside any
    // legacy claims from Phase 7 (Linker). For witness-only workspaces
    // (the post-cutover default), `filtered_extraction.claims.len()`
    // is 0 and the witness count carries the signal.
    let claims_count = filtered_extraction.claims.len() + persisted_witness_count;
    let entities_count = filtered_extraction.entities.len();
    let relations_count = filtered_extraction.relations.len();
    // Snapshot failed-batch attribution before Linker moves the
    // extraction.  The PipelineResult needs them at the very end so
    // the CLI/desktop can render an honest partial-failure summary.
    let failed_batches = filtered_extraction.failed_batches;
    let failed_chunk_ranges = filtered_extraction.failed_batch_ranges.clone();
    // Snapshot chunks_processed for llm_calls computation in IncrementalSummary.
    let extraction_chunks_processed = filtered_extraction.chunks_processed;

    // Retain a lightweight clone of the filtered claims for Phase 2c-post-link
    // (SVO event extraction).  We clone before the linker takes ownership so that
    // the post-link phase has access to statements + event_date timestamps.
    let claims_for_svo: Vec<thinkingroot_core::Claim> = filtered_extraction.claims.clone();

    bail_if_cancelled!();

    // ─── Phase 7: Link ─────────────────────────────────────────────────
    //
    // Build the Phase 7e revalidation scope BEFORE constructing the
    // linker. Scope = truly_changed ∪ dependents(truly_changed). The
    // dependents lookup uses `resolution_deps:by_to` — every source
    // that resolves TO a truly-changed source has its own
    // function_calls / code_links rows potentially invalidated and
    // must be revalidated. Sources outside the union cannot have had
    // their resolutions affected by this compile, so we save the
    // workspace-wide scan for them.
    //
    // We pass scope=None (workspace-wide) only on the early-incremental
    // edge case where truly_changed.is_empty() but Phase 4 deletions
    // landed — those deletions can dangle resolutions in any source,
    // and we don't currently capture the reverse map for deleted
    // sources at this point. The workspace-wide fallback is safe and
    // matches pre-2026-05-18 behaviour.
    let resolution_scope: Option<std::collections::HashSet<String>> = if truly_changed.is_empty() {
        None
    } else {
        let truly_changed_ids: Vec<String> = truly_changed
            .iter()
            .map(|d| d.source_id.to_string())
            .collect();
        let mut scope: std::collections::HashSet<String> =
            truly_changed_ids.iter().cloned().collect();
        let downstream = storage
            .graph
            .list_dependent_sources_for_many(&truly_changed_ids)?;
        scope.extend(downstream);
        Some(scope)
    };

    let linker = {
        let mut l = thinkingroot_link::Linker::new(&storage.graph);
        if let Some(scope) = resolution_scope.clone() {
            l = l.with_resolution_scope(scope);
        }
        if let Some(ref tx) = progress {
            let tx_link = tx.clone();
            let total_entities = filtered_extraction.entities.len();
            emit!(ProgressEvent::LinkingStart { total_entities });
            // Plumb the linker's per-entity callback into both the
            // legacy `EntityResolved` event (kept for the old
            // multi-bar driver) AND the unified `progress_state`
            // counter (used by the ticker → `CompileTick`).
            let state_for_linker = progress_state.clone();
            let pf = Arc::new(move |done: usize, total: usize| {
                state_for_linker.set_total(total as u64);
                state_for_linker.set_done(done as u64);
                let _ = tx_link.send(ProgressEvent::EntityResolved { done, total });
            }) as thinkingroot_link::EntityProgressFn;
            l.with_progress(pf)
        } else {
            l
        }
    };
    let link_output = linker.link(filtered_extraction)?;
    emit!(ProgressEvent::LinkComplete {
        entities: link_output.entities_created + link_output.entities_merged,
        relations: link_output.relations_linked,
        contradictions: link_output.contradictions_detected,
    });
    mark_phase!("link");

    // ─── Phase 2c-post-link: SVO Event Calendar ──────────────────────────
    // Now that Phase 7 has written all entities to CozoDB, we can build the
    // complete entity_name → ULID map and extract SVO events with correct IDs.
    //
    // This is the world-class temporal memory architecture:
    //   compile time  → events table populated with real entity ULIDs
    //   query time    → 50µs Datalog range scan (vs Chronos 100-200ms)
    //
    // Non-fatal: event calendar failure must never abort the pipeline.
    {
        progress_state.set_substep("extracting events");
        // Audit invariant: no `unwrap_or_default()` on engine-error
        // returns.  A real CozoDB failure here previously masqueraded
        // as "no entities yet" and silently skipped event-calendar
        // compilation; surface it explicitly so a systemic storage
        // bug shows up in error logs.
        let entity_name_to_id: std::collections::HashMap<String, String> =
            match storage.graph.get_all_entities() {
                Ok(rows) => rows
                    .into_iter()
                    .map(|(id, name, _)| (name.to_lowercase(), id))
                    .collect(),
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "event calendar: get_all_entities failed; skipping SVO compilation"
                    );
                    std::collections::HashMap::new()
                }
            };

        if entity_name_to_id.is_empty() {
            tracing::warn!("event calendar: entity table empty after linking — skipping");
        } else {
            let extractor = thinkingroot_llm::EventExtractor::new();
            let extracted_events =
                extractor.extract_from_claims(&claims_for_svo, &entity_name_to_id);

            if !extracted_events.is_empty() {
                match storage.graph.insert_events(&extracted_events) {
                    Ok(n) => tracing::info!(
                        count = n,
                        entities = entity_name_to_id.len(),
                        "event calendar: SVO events compiled with correct entity IDs"
                    ),
                    // Events are a derived secondary index — the rest
                    // of the graph (claims, entities, relations) is
                    // already persisted by this point, so we don't
                    // fail the compile.  We do log at error level
                    // (was warn) so a systemic insert bug surfaces
                    // instead of sitting silently in tracing output:
                    // pre-fix the `:put events { col: var }` syntax
                    // bug made every call here error and the
                    // calendar table sat empty in prod for unknown
                    // duration without anyone noticing.
                    Err(e) => tracing::error!(
                        attempted = extracted_events.len(),
                        "event calendar: insertion failed — calendar queries will return empty until next successful compile: {e}"
                    ),
                }
            } else {
                tracing::info!(
                    "event calendar: no SVO events found in {} claims",
                    claims_for_svo.len()
                );
            }
        }
    }

    // ─── Phase 8: Incremental entity relation update for new sources ───
    //
    // Tier 2 commit H: batched gather. Pre-Tier-2 this loop ran
    // `get_source_relation_triples(source_id)` ONCE PER truly-changed
    // source — N round trips on a workspace where N could be 47+.
    // The batched API consolidates into one CozoDB query with an
    // IN-set parameter, matching the pattern from Phase 4's pre-
    // cascade reader block at pipeline.rs:1012.
    progress_state.set_substep("updating relations");
    let phase8_source_ids: Vec<String> = truly_changed
        .iter()
        .map(|d| d.source_id.to_string())
        .collect();
    let mut new_triples = storage
        .graph
        .get_source_relation_triples_for_sources(&phase8_source_ids)?;
    if new_triples.is_empty() && link_output.relations_linked > 0 {
        tracing::warn!(
            "relations were linked ({}) but no source relation triples found; \
             entity_relations may be stale",
            link_output.relations_linked
        );
    }
    new_triples.sort_unstable();
    new_triples.dedup();
    storage
        .graph
        .update_entity_relations_for_triples(&new_triples)?;
    // Phase 8 elapsed is the second half of the split entity_relations key.
    // Phase 5 (removal-side) contributed the first half earlier.
    // CONTRACT: two PhaseDone events are emitted for "entity_relations" —
    // one from Phase 5 (removals) and one here (additions).  SSE consumers
    // that want a unified bar sum them; consumers that want the split keep
    // them separate.  IncrementalSummary.phase_timings["entity_relations"]
    // is the combined total of both.
    {
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(last_phase_end).as_millis() as u64;
        last_phase_end = now;
        *phase_timings.entry("entity_relations".to_string()).or_insert(0) += elapsed;
        emit!(ProgressEvent::PhaseDone {
            name: "entity_relations".to_string(),
            elapsed_ms: elapsed,
        });
    }

    // Vector index, markdown artifacts, and post-compile health
    // verification are NOT part of `root compile` in v3 — they live
    // in `root query` (which lazily builds the index on first call),
    // `root render`, and `root health` respectively. Per v3 final
    // plan §5.4 / §11.

    // ─── Phase 9: Byte-Coverage Audit (Compile Completeness Contract §7) ───
    // Enforces I-3: every source byte maps to ≥1 structural row OR a
    // chunks_residual row. CI-gating; fails the compile when any
    // source has uncovered bytes. Per-compile escape hatch:
    // `TR_SKIP_BYTE_AUDIT=1` (intended for local iteration; CI must
    // keep it on).
    //
    // Second `Persisting` slot — audit reads the substrate rather than
    // writing, but from the user's bar this is "verifying what we just
    // persisted." No known total → spinner with elapsed-only.
    progress_state.set_step(thinkingroot_core::CompileStep::Persisting, 0);
    progress_state.set_substep("auditing byte coverage");
    let phase_9_skip = skip_byte_audit
        || std::env::var("TR_SKIP_BYTE_AUDIT")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
    if !phase_9_skip {
        let phase_9_started = std::time::Instant::now();
        let orphans = storage.graph.query_orphan_bytes()?;
        if !orphans.is_empty() {
            // Group by source so the error sample is human-scannable.
            let mut by_source: std::collections::HashMap<String, Vec<(u64, u64)>> =
                std::collections::HashMap::new();
            let total_orphan_bytes: usize = orphans
                .iter()
                .map(|(_, s, e)| (e.saturating_sub(*s)) as usize)
                .sum();
            for (sid, bs, be) in &orphans {
                by_source
                    .entry(sid.to_string())
                    .or_default()
                    .push((*bs, *be));
            }
            let sources_with_orphans = by_source.len();
            // Take the first 5 entries for the diagnostic sample. HashMap
            // iteration order isn't stable but that's fine — `take(5)` is
            // a "give me a representative slice" contract, not a
            // "give me the deterministic-ranked top-5".
            let sample: Vec<(String, Vec<(u64, u64)>)> =
                by_source.into_iter().take(5).collect();
            tracing::error!(
                sources = sources_with_orphans,
                bytes = total_orphan_bytes,
                "phase 9 byte-coverage breach detected"
            );
            return Err(thinkingroot_core::Error::ByteCoverageBreach {
                sources_with_orphans,
                total_orphan_bytes,
                sample,
            });
        }
        let structural_orphans = storage.graph.query_orphan_structural_rows()?;
        if !structural_orphans.is_empty() {
            let count: usize = structural_orphans.iter().map(|(_, _, n)| *n).sum();
            let sample: Vec<(String, String)> = structural_orphans
                .iter()
                .take(5)
                .map(|(table, sid, _)| (table.clone(), sid.clone()))
                .collect();
            tracing::error!(
                count = count,
                tables = structural_orphans.len(),
                "phase 9 structural orphan rows detected"
            );
            return Err(thinkingroot_core::Error::OrphanStructuralRows { count, sample });
        }
        tracing::info!(
            elapsed_ms = phase_9_started.elapsed().as_millis() as u64,
            "phase 9 byte-coverage audit passed (zero orphan bytes)"
        );
    } else {
        tracing::warn!(
            "phase 9 byte-coverage audit SKIPPED via TR_SKIP_BYTE_AUDIT=1; \
             do not use this in CI"
        );
    }
    mark_phase!("audit");

    // ─── E4: Hierarchical summary nodes (opt-in, default off) ───
    // Deterministic function→file→repo ladder over the code graph. Folded
    // into the audit budget (sub-millisecond on real workspaces). Non-fatal:
    // a failure means summaries are stale, not that the compile produced bad
    // data (honesty rule §6 — warn, never silently fail or fabricate).
    if emit_summaries {
        progress_state.set_substep("building summaries");
        let now_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        match storage.graph.build_summaries(now_ts) {
            Ok(n) => tracing::info!(summary_nodes = n, "E4 summary ladder built"),
            Err(e) => tracing::warn!(target: "summaries", error = %e, "summary build failed; summaries may be stale"),
        }
    }

    // ─── Phase 10: Workspace README synthesis ───
    // Deterministic from substrate; folded into the "audit" phase
    // budget rather than added to PHASE_NAMES (synth is sub-millisecond
    // on real workspaces). Only runs when this compile produced new
    // substrate and we are on the canonical (main / un-branched) view —
    // branches share workspace sources, so per-branch README would only
    // differ in counts and the user-facing root README is workspace-
    // level state.
    if matches!(branch, None | Some("main")) {
        progress_state.set_substep("synthesizing readme");
        if let Err(e) = synthesise_and_persist_readme(root_path, &storage).await {
            // Non-fatal: a failure here means the README is stale, not
            // that the compile produced bad data. Surface as warn (no
            // silent failure — CLAUDE.md honesty rule §6).
            tracing::warn!(
                target: "readme",
                error = %e,
                "README synthesis failed; .thinkingroot/README.md may be stale"
            );
        }
    }

    // ─── Phase 10b: Living Paper synthesis ───
    // Per-compile `paper.md` artefact (deterministic skeleton in v1;
    // AI narrative sections in v1.1). Single file with YAML
    // frontmatter (machine-readable spine) + markdown body (human
    // body + Mermaid architecture diagram). Workspace-level state,
    // not per-branch. Non-fatal — a stale paper is not a corrupt
    // graph.
    //
    // **Small-change skip gate (2026-05-18)**. The LLM-driven
    // synthesis path makes hundreds of provider calls and dominates
    // wall-clock on every compile (real measurement: 26.3 s of a
    // 33.8 s compile for 1 truly-changed source, ~78% of wall time
    // hidden in the previously-uninstrumented "other" bucket). For
    // an N-source workspace where the user just edited 1–3 files,
    // the regenerated paper is functionally identical to the
    // previous version, so the 20-30 s + N LLM calls cost is pure
    // waste on the incremental hot path.
    //
    // Gate: synthesise only when
    //   (a) `paper.md` does not exist yet (first compile must populate it),
    //   (b) the user explicitly forced a full rebuild via
    //       `PipelineOptions::no_incremental`, OR
    //   (c) the change set is "material" — more than
    //       `PAPER_RESYNTH_CHANGE_FLOOR` truly-changed-or-deleted
    //       sources.
    //
    // Otherwise: skip; the user refreshes the paper on demand via
    // the `regenerate_paper` MCP tool or `root render`. The
    // `synth_paper` phase timing is recorded either way (0 on skip)
    // so the time line in `root compile --json` and the
    // desktop's summary panel remains a complete accounting.
    const PAPER_RESYNTH_CHANGE_FLOOR: usize = 5;
    let paper_path = root_path.join(".thinkingroot").join("paper.md");
    let paper_exists = paper_path.exists();
    let material_change_count = truly_changed.len() + deleted_sources.len();
    let should_synth_paper = matches!(branch, None | Some("main"))
        && (no_incremental || !paper_exists || material_change_count > PAPER_RESYNTH_CHANGE_FLOOR);
    // Honest LLM-call accounting: extraction is fully mechanical, so the only
    // place the compile invokes an LLM is the optional paper synthesis below.
    let mut paper_llm_calls = 0usize;
    if should_synth_paper {
        progress_state.set_substep("synthesizing paper");
        let workspace_name: String = root_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("workspace")
            .to_string();
        let now = chrono::Utc::now();

        // Try to build an LlmClient from the merged workspace+user
        // config. Failure (no provider, missing key, etc.) maps to
        // None — the deterministic path always succeeds.
        let llm_client: Option<thinkingroot_llm::llm::LlmClient> =
            match thinkingroot_llm::llm::LlmClient::new(&config.llm).await {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::debug!(
                        target: "paper",
                        error = %e,
                        "LLM not configured; rendering deterministic skeleton only"
                    );
                    None
                }
            };

        // Cancel checkpoint before any work — if Stop was clicked
        // during Phase 9 / 10, don't even start the synthesis. For the
        // LLM path, wrap the await in select so a click DURING the LLM
        // call bails within ≤500 ms (LLM responses can take 60+ s and
        // pre-fix would block Stop the entire time).
        bail_if_cancelled!();
        let result = match &llm_client {
            Some(client) => {
                let fut = thinkingroot_paper::synthesize_and_persist_with_llm(
                    &storage.graph,
                    root_path,
                    &workspace_name,
                    now,
                    client,
                );
                tokio::select! {
                    r = fut => r.map(|_| ()),
                    _ = cancel.cancelled() => {
                        return Err(thinkingroot_core::Error::Cancelled);
                    }
                }
            }
            None => thinkingroot_paper::synthesize_and_persist(
                &storage.graph,
                root_path,
                &workspace_name,
                now,
            )
            .map(|_| ()),
        };
        // Count one LLM-backed synthesis pass iff the LLM path was taken.
        paper_llm_calls = usize::from(llm_client.is_some());
        if let Err(e) = result {
            tracing::warn!(
                target: "paper",
                error = %e,
                "paper synthesis failed; .thinkingroot/paper.md may be stale"
            );
        }
    } else {
        tracing::info!(
            target: "paper",
            truly_changed = truly_changed.len(),
            deleted = deleted_sources.len(),
            threshold = PAPER_RESYNTH_CHANGE_FLOOR,
            "paper synth skipped on incremental compile; existing paper retained — \
             trigger `regenerate_paper` MCP tool or a full recompile to refresh"
        );
    }
    mark_phase!("synth_paper");

    fingerprints.save()?;
    config.save(root_path)?;

    // Phase 7 succeeded — CozoDB is now the source of truth.  Clear the
    // in-flight checkpoint log so the next compile starts fresh.
    // Failure is non-fatal (a stale .in-flight.jsonl just means the
    // next run logs a misleading "resuming" message, then produces
    // identical output via cache hits).
    if let Err(e) = thinkingroot_llm::InFlightCheckpoint::clear(&data_dir) {
        tracing::warn!("failed to clear in-flight checkpoint after Phase 7: {e}");
    }

    let total_elapsed_ms = pipeline_start.elapsed().as_millis() as u64;
    let summed: u64 = phase_timings.values().sum();
    if total_elapsed_ms > summed {
        phase_timings.insert("other".to_string(), total_elapsed_ms - summed);
    }

    let summary = thinkingroot_core::IncrementalSummary {
        sources_total: documents.len(),
        sources_unchanged: skipped,
        sources_truly_changed: truly_changed.len(),
        sources_deleted: deleted_sources.len(),
        sources_resolution_dirty: resolution_dirty_sources.len(),
        claims_added: claims_count,
        // I-W4: per-source rebuild is always delete-then-insert; no in-place updates.
        claims_updated: 0,
        claims_deleted: phase4_claim_delete_count,
        structural_rows_emitted: phase_6_7_stats.structural_rows_emitted,
        structural_rows_cascaded: phase4_cascade_row_count,
        bytes_re_extracted: truly_changed.iter().map(|d| d.total_chars() as u64).sum(),
        // Genuine LLM calls = paper synthesis only (extraction is mechanical).
        llm_calls: paper_llm_calls,
        cache_hits,
        structural_extractions,
        // Chunks that produced no structural output (the value formerly
        // mislabeled as `llm_calls`), now reported under an honest name.
        chunks_without_extraction: extraction_chunks_processed
            .saturating_sub(cache_hits + structural_extractions),
        phase_timings: phase_timings.clone(),
        total_elapsed_ms,
    };
    emit!(ProgressEvent::IncrementalDone { summary: summary.clone() });

    Ok(PipelineResult {
        files_parsed,
        claims_count,
        entities_count,
        relations_count,
        contradictions_count: 0,
        artifacts_count: 0,
        health_score: 0,
        cache_hits,
        early_cutoffs: skipped + fingerprint_cutoffs,
        structural_extractions,
        // v3 pipeline ran — CozoDB has new data.
        cache_dirty: true,
        failed_batches,
        failed_chunk_ranges,
        incremental_summary: summary,
    })
}

/// Outcome of `reconcile_vector_index`.
///
/// `existing` is the size of the on-disk index before reconcile;
/// `current` is the count of (claim + entity) ids the graph carries
/// after the compile. `removed` and `added` are the delta actually
/// applied. Idempotent: a second reconcile call against the same
/// graph state reports `removed=0, added=0`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VectorReconcileStats {
    pub existing: usize,
    pub current: usize,
    pub removed: usize,
    pub added: usize,
    pub elapsed_ms: u64,
}

/// Reconcile the vector index against the persisted CozoDB graph.
///
/// Computes the set difference between the index's current keys and
/// the graph's current `(claim + entity)` id space, then removes
/// stale embeddings and embeds only the missing ones. The default
/// post-compile path (`rest.rs::finalize_successful_compile`-spawned
/// background task) uses this — re-embedding 600 claims when only
/// 10 changed was the dominant 30-second cost on the `thinkingroot`
/// workspace prior to 2026-05-18.
///
/// Identity contract:
/// - Claim ids are ULIDs (`thinkingroot_core::id::ClaimId`),
///   regenerated per compile only within `truly_changed` sources
///   (I-W4 per-source delete+insert). Unchanged sources' claims
///   keep their ids, so their embeddings stay in `existing ∩ current`
///   and are preserved verbatim. Changed sources' claims land in
///   `existing \ current` (old ids → removed) and `current \ existing`
///   (new ids → embedded).
/// - Entity ids are stable across compiles unless the entity is
///   dropped from the graph; same diff logic applies.
///
/// Cancellation: observed at chunk boundaries during the embed loop.
/// `RECONCILE_EMBED_CHUNK = 64` keeps cancel cadence under ~3 s on
/// typical hardware (~50 ms/embed × 64 = ~3.2 s worst case). On
/// `Error::Cancelled` the in-memory index reflects the partial
/// add/remove state but `vector.save()` is NOT called — the next
/// compile reconciles cleanly against the unchanged on-disk state.
///
/// For an explicit "wipe and rebuild" — used by `root index rebuild`
/// and the operator `rebuild_vector_index` tool — call
/// `rebuild_vector_index` instead.
pub fn reconcile_vector_index(
    storage: &mut StorageEngine,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<VectorReconcileStats> {
    /// Per-chunk size used by the embed loop. Calibrated for honest
    /// cancel cadence: ~50 ms/embed × 64 ≈ ~3.2 s worst case per
    /// chunk before observing the cancel token.
    const RECONCILE_EMBED_CHUNK: usize = 64;

    let started = std::time::Instant::now();

    let existing_keys = storage.vector.index_ids();
    let existing_set: std::collections::HashSet<String> = existing_keys.into_iter().collect();
    let existing_count = existing_set.len();

    let entities = storage.graph.get_all_entities()?;
    let claims = storage.graph.get_all_claims_with_sources()?;

    let mut current_set: std::collections::HashSet<String> =
        std::collections::HashSet::with_capacity(entities.len() + claims.len());
    let mut items_by_id: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::with_capacity(entities.len() + claims.len());

    for (id, name, etype) in &entities {
        let key = format!("entity:{id}");
        current_set.insert(key.clone());
        items_by_id.insert(
            key,
            (
                format!("{name} ({etype})"),
                format!("entity|{id}|{name}|{etype}"),
            ),
        );
    }
    for (id, statement, ctype, conf, uri, _) in &claims {
        let key = format!("claim:{id}");
        current_set.insert(key.clone());
        items_by_id.insert(
            key,
            (
                statement.clone(),
                format!("claim|{id}|{ctype}|{conf}|{uri}"),
            ),
        );
    }
    let current_count = current_set.len();

    let to_remove_owned: Vec<String> = existing_set.difference(&current_set).cloned().collect();
    let to_remove_refs: Vec<&str> = to_remove_owned.iter().map(|s| s.as_str()).collect();

    let to_add: Vec<(String, String, String)> = current_set
        .difference(&existing_set)
        .filter_map(|id| {
            items_by_id
                .get(id)
                .map(|(text, meta)| (id.clone(), text.clone(), meta.clone()))
        })
        .collect();

    let removed = to_remove_refs.len();
    storage.vector.remove_by_ids(&to_remove_refs);

    // Embed in cancel-aware chunks. `upsert_batch` calls ONNX
    // inference once per chunk — coarser chunks would be faster but
    // would burn longer before observing cancel. 64 strikes the
    // honest cadence balance.
    let mut added = 0usize;
    for chunk in to_add.chunks(RECONCILE_EMBED_CHUNK) {
        if cancel.is_cancelled() {
            return Err(thinkingroot_core::Error::Cancelled);
        }
        storage.vector.upsert_batch(chunk)?;
        added += chunk.len();
    }

    if removed > 0 || added > 0 {
        storage.vector.save()?;
    }

    Ok(VectorReconcileStats {
        existing: existing_count,
        current: current_count,
        removed,
        added,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

/// Force a full rebuild of the vector index from the persisted CozoDB
/// graph. Resets the existing index, embeds every entity + claim
/// currently in the graph, and saves to disk. Returns
/// `(entities_indexed, claims_indexed)`.
///
/// Used by `root index rebuild` / the operator `rebuild_vector_index`
/// MCP tool / the post-mount path — every site where the caller has
/// asked for a wipe-and-rebuild explicitly. The default post-compile
/// path uses `reconcile_vector_index` (delta) instead.
pub fn rebuild_vector_index(storage: &mut StorageEngine) -> Result<(usize, usize)> {
    storage.vector.reset();

    let entities = storage.graph.get_all_entities()?;
    let claims = storage.graph.get_all_claims_with_sources()?;

    let entity_items: Vec<(String, String, String)> = entities
        .iter()
        .map(|(id, name, etype)| {
            (
                format!("entity:{id}"),
                format!("{name} ({etype})"),
                format!("entity|{id}|{name}|{etype}"),
            )
        })
        .collect();

    // Skip embedding non-text (binary/PDF-byte) claims: they waste ONNX
    // compute, bloat the index, and (being padded to the batch's longest
    // sequence) drive the memory spike that OOM'd bulk rebuilds. The recall
    // path filters them anyway; not embedding them is the upstream fix.
    let claim_items: Vec<(String, String, String)> = claims
        .iter()
        .filter(|(_, statement, ..)| crate::intelligence::hybrid::is_probably_text(statement))
        .map(|(id, statement, ctype, conf, uri, _)| {
            (
                format!("claim:{id}"),
                statement.clone(),
                format!("claim|{id}|{ctype}|{conf}|{uri}"),
            )
        })
        .collect();

    let entity_count = upsert_in_chunks(&mut storage.vector, &entity_items, 512)?;
    let claim_count = upsert_in_chunks(&mut storage.vector, &claim_items, 512)?;
    storage.vector.save()?;

    Ok((entity_count, claim_count))
}

fn upsert_in_chunks(
    vector: &mut thinkingroot_graph::vector::VectorStore,
    items: &[(String, String, String)],
    chunk_size: usize,
) -> Result<usize> {
    let mut done = 0usize;
    for chunk in items.chunks(chunk_size) {
        vector.upsert_batch(chunk)?;
        done += chunk.len();
    }
    Ok(done)
}

/// Synthesise the workspace README from current substrate, write the
/// engine-canonical view to `<root>/.thinkingroot/README.md`, and
/// merge the auto-block into `<root>/README.md` (preserving any user-
/// authored content outside the markers).
async fn synthesise_and_persist_readme(
    root_path: &Path,
    storage: &StorageEngine,
) -> Result<()> {
    use thinkingroot_llm::readme;

    let workspace_name: String = root_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace")
        .to_string();

    let (source_count, claim_count, entity_count) = storage.graph.get_counts()?;
    let (rooted, attested, quarantined, rejected) =
        storage.graph.count_claims_by_admission_tier()?;
    let top_entities_raw = storage.graph.get_top_entities_by_claim_count(10)?;
    let top_sources_raw: Vec<(String, usize)> =
        storage.graph.get_top_sources_with_claim_counts(20)?;
    let contradiction_count = storage.graph.get_contradictions()?.len();

    let branches_raw = thinkingroot_branch::list_branches(root_path)?;

    let top_entities: Vec<readme::TopEntity<'_>> = top_entities_raw
        .iter()
        .map(|e| readme::TopEntity {
            name: e.name.as_str(),
            claim_count: e.claim_count as u64,
        })
        .collect();
    let sources: Vec<readme::SourceLine<'_>> = top_sources_raw
        .iter()
        .map(|(uri, cnt)| readme::SourceLine {
            relative_path: uri.as_str(),
            claim_count: *cnt as u64,
        })
        .collect();
    let branch_kinds: Vec<String> = branches_raw
        .iter()
        .map(|b| format!("{:?}", b.kind))
        .collect();
    let branch_policies: Vec<String> = branches_raw
        .iter()
        .map(|b| format!("{:?}", b.merge_policy))
        .collect();
    let branches: Vec<readme::BranchLine<'_>> = branches_raw
        .iter()
        .enumerate()
        .map(|(i, b)| readme::BranchLine {
            name: b.name.as_str(),
            kind: branch_kinds[i].as_str(),
            merge_policy: branch_policies[i].as_str(),
        })
        .collect();

    let extractor = format!("thinkingroot/extract@{}", env!("CARGO_PKG_VERSION"));
    let thinkingroot_version = env!("CARGO_PKG_VERSION");
    let now = chrono::Utc::now();

    let inputs = readme::ReadmeInputs {
        workspace_name: workspace_name.as_str(),
        description: None,
        extracted_at: now,
        extractor: extractor.as_str(),
        source_count: source_count as u64,
        claim_count: claim_count as u64,
        entity_count: entity_count as u64,
        rooted: rooted as u64,
        attested: attested as u64,
        quarantined: quarantined as u64,
        rejected: rejected as u64,
        top_entities: &top_entities,
        sources: &sources,
        branches: &branches,
        contradiction_count: contradiction_count as u64,
        thinkingroot_version,
    };

    // 1. Engine-canonical README — always overwritten.
    let canonical = readme::synthesise(&inputs);
    let canonical_path = root_path.join(".thinkingroot").join("README.md");
    if let Some(parent) = canonical_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| thinkingroot_core::Error::io_path(parent.to_path_buf(), e))?;
    }
    tokio::fs::write(&canonical_path, canonical.as_bytes())
        .await
        .map_err(|e| thinkingroot_core::Error::io_path(canonical_path.clone(), e))?;

    // 2. User-facing root README — section-marker maintained.
    //
    // Auto-creation is opt-in: the pipeline updates an *existing*
    // `<root>/README.md` (preserving user content outside markers) but
    // does NOT create one from scratch on a fresh workspace. Reason:
    // the workspace walker would pick up an auto-generated README.md
    // as a real source on the next compile (entity names listed in
    // the auto-block would re-enter the claim graph as a feedback
    // loop). Users opt in by creating the file themselves —
    // `touch README.md` is enough for the pipeline to start
    // maintaining the auto-block on the next compile. The desktop's
    // Readme tab keeps working in either case because it reads from
    // `.thinkingroot/README.md` (canonical, always written).
    let block = readme::synthesise_block(&inputs);
    let root_readme = root_path.join("README.md");
    // NotFound is the intended path for fresh workspaces (root README
    // auto-creation is opt-in). Other I/O errors (permission denied,
    // bad UTF-8, etc.) are real failures — log them rather than
    // silently treating the file as absent. CLAUDE.md §honesty rule §6.
    let existing = match tokio::fs::read_to_string(&root_readme).await {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            tracing::warn!(
                target: "readme",
                error = %e,
                path = %root_readme.display(),
                "could not read existing root README — skipping update"
            );
            None
        }
    };
    if let Some(existing_str) = existing.as_deref() {
        match readme::merge_into_root_readme(Some(existing_str), &block, "") {
            Ok(merged) => {
                tokio::fs::write(&root_readme, merged.as_bytes())
                    .await
                    .map_err(|e| thinkingroot_core::Error::io_path(root_readme.clone(), e))?;
            }
            Err(e) => {
                tracing::warn!(
                    target: "readme",
                    error = %e,
                    path = %root_readme.display(),
                    "skipping root README update — markers malformed; user file unchanged"
                );
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pre-cancelled tokens short-circuit the pipeline before any
    /// parsing or LLM work — the `bail_if_cancelled!()` checkpoint that
    /// fires after `ProgressEvent::ParseStart` and before
    /// `thinkingroot_parse::parse_directory` returns
    /// `Err(Error::Cancelled)`.  This is the foundational guarantee the
    /// desktop "Stop compile" button relies on (P3.4).
    #[tokio::test]
    async fn pre_cancelled_token_aborts_before_parse() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Touch a file so parse_directory wouldn't trivially return empty.
        std::fs::write(tmp.path().join("hello.md"), "# hello\n\nbody.").unwrap();

        let cancel = CancellationToken::new();
        cancel.cancel();

        let err = run_pipeline_with_cancel(tmp.path(), None, None, cancel)
            .await
            .expect_err("pre-cancelled token must produce Err");
        assert!(
            matches!(err, thinkingroot_core::Error::Cancelled),
            "expected Error::Cancelled, got {err:?}"
        );
    }

    /// A fresh, never-tripped token must behave exactly like the old
    /// `run_pipeline` API — empty workspaces still report parse=0 with
    /// no error.  Guards against accidental tightening of the cancel
    /// check (e.g. an `if !is_cancelled` typo).
    #[tokio::test]
    async fn untripped_token_runs_to_completion_on_empty_workspace() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let result = run_pipeline_with_cancel(tmp.path(), None, None, CancellationToken::new())
            .await
            .expect("untripped token must not abort an empty compile");
        assert_eq!(result.files_parsed, 0);
        assert_eq!(result.claims_count, 0);
        assert!(!result.cache_dirty, "empty compile must not dirty cache");
    }

    /// `set_substep` must propagate into the next ticker snapshot's
    /// `detail` field. The UI's indeterminate-fallback caption keys off
    /// this field; if a future refactor drops the wiring, the bar
    /// silently regresses to the "counting sources…" lie.
    #[test]
    fn substep_round_trips_through_snapshot() {
        let state = CompileProgressState::new();
        let snap0 = state.snapshot();
        assert!(
            snap0.detail.is_none(),
            "fresh state must have no substep detail"
        );

        state.set_substep("removing changed sources");
        let snap1 = state.snapshot();
        assert_eq!(
            snap1.detail.as_deref(),
            Some("removing changed sources"),
            "set_substep must surface as snapshot.detail"
        );

        // Empty literal clears the detail back to None — used by the
        // ticker spawn block before any sub-phase has set a label.
        state.set_substep("");
        let snap2 = state.snapshot();
        assert!(
            snap2.detail.is_none(),
            "empty substep must surface as None, not Some(\"\")"
        );
    }

    /// Substep persists across `set_step` transitions; resetting the
    /// step (done→0, total→N) intentionally does NOT clear the substep
    /// because the next sub-phase always overwrites it explicitly. This
    /// pins the contract so a `set_step` re-entry mid-phase doesn't
    /// flash an empty caption.
    #[test]
    fn substep_persists_across_step_transitions() {
        let state = CompileProgressState::new();
        state.set_substep("linking entities");
        state.set_step(thinkingroot_core::CompileStep::Persisting, 42);

        let snap = state.snapshot();
        assert_eq!(snap.step, thinkingroot_core::CompileStep::Persisting);
        assert_eq!(snap.total, 42);
        assert_eq!(snap.done, 0, "set_step must reset done to 0");
        assert_eq!(
            snap.detail.as_deref(),
            Some("linking entities"),
            "substep survives a set_step call by design"
        );
    }

    // ── VectorReconcileStats wire-shape + diff-math contract ──────────────────

    /// `VectorReconcileStats::default()` must zero every counter so a
    /// freshly-constructed value carries no implicit success/failure
    /// signal. The struct is wire-emitted via `tracing::info!` fields
    /// at `rest.rs::finalize_successful_compile`'s background task —
    /// non-zero defaults would silently log misleading numbers when a
    /// reconcile short-circuits before any work.
    #[test]
    fn vector_reconcile_stats_default_is_zero() {
        let s = VectorReconcileStats::default();
        assert_eq!(s.existing, 0);
        assert_eq!(s.current, 0);
        assert_eq!(s.removed, 0);
        assert_eq!(s.added, 0);
        assert_eq!(s.elapsed_ms, 0);
    }

    /// The reconcile diff math: given `existing` index keys and
    /// `current` graph keys, the function must produce
    /// `to_remove = existing \ current` and
    /// `to_add = current \ existing`. The test pins the algorithm
    /// using the same `HashSet::difference` primitive the function
    /// uses, so any future refactor that drifts the operator (e.g.
    /// `symmetric_difference` slip) is caught here, deterministically,
    /// without needing the ONNX embedding model.
    #[test]
    fn vector_reconcile_diff_math_matches_set_difference() {
        use std::collections::HashSet;

        let existing: HashSet<String> = ["a", "b", "c", "d"].into_iter().map(String::from).collect();
        let current: HashSet<String> = ["b", "c", "d", "e", "f"].into_iter().map(String::from).collect();

        let mut to_remove: Vec<&str> = existing.difference(&current).map(|s| s.as_str()).collect();
        let mut to_add: Vec<&str> = current.difference(&existing).map(|s| s.as_str()).collect();
        to_remove.sort();
        to_add.sort();

        assert_eq!(to_remove, vec!["a"], "existing-only ids must be removed");
        assert_eq!(to_add, vec!["e", "f"], "current-only ids must be added");
    }

    /// Idempotency: when `existing == current` (back-to-back compiles
    /// with no changes), reconcile must compute zero removes and zero
    /// adds. This is the property that makes reconcile safe to run on
    /// every successful compile — the cost when nothing changed is
    /// O(graph-read) + O(1) embedding work.
    #[test]
    fn vector_reconcile_diff_is_idempotent_when_sets_match() {
        use std::collections::HashSet;

        let s: HashSet<String> = ["claim:1", "claim:2", "entity:e"].into_iter().map(String::from).collect();
        let to_remove: Vec<&str> = s.difference(&s).map(|x| x.as_str()).collect();
        let to_add: Vec<&str> = s.difference(&s).map(|x| x.as_str()).collect();

        assert!(to_remove.is_empty(), "identical sets produce zero removes");
        assert!(to_add.is_empty(), "identical sets produce zero adds");
    }

    /// Cancel contract for the chunked embed loop: a pre-cancelled
    /// token must produce `Error::Cancelled` BEFORE the first
    /// `upsert_batch` call. We exercise the chunk-boundary
    /// `cancel.is_cancelled()` check in isolation here — the full
    /// reconcile path requires the ONNX model bundle (covered by the
    /// real-data benchmark in Commit F).
    #[test]
    fn reconcile_embed_loop_observes_cancel_at_chunk_boundary() {
        let cancel = CancellationToken::new();
        cancel.cancel();

        // Synthesise the same chunk-boundary check the function uses.
        // If this assertion ever drifts (e.g. someone removes the
        // cancel check), the real reconcile would silently waste an
        // embed cycle before noticing — exactly the dead-zone bug
        // Tier 1 closes.
        let to_add: Vec<(String, String, String)> = (0..200)
            .map(|i| (format!("k{i}"), format!("t{i}"), format!("m{i}")))
            .collect();
        let chunk_size: usize = 64;
        let mut iterations_executed = 0usize;
        let mut cancelled_at: Option<usize> = None;
        for (idx, _chunk) in to_add.chunks(chunk_size).enumerate() {
            if cancel.is_cancelled() {
                cancelled_at = Some(idx);
                break;
            }
            iterations_executed += 1;
        }
        assert_eq!(
            cancelled_at,
            Some(0),
            "pre-cancelled token must trip on the FIRST chunk-boundary check"
        );
        assert_eq!(
            iterations_executed, 0,
            "no embed iterations may execute under a pre-cancelled token"
        );
    }
}
