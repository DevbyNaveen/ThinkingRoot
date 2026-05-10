//! Slice 0 — unified workspace status: one source of truth across CLI, daemon, desktop.
//!
//! Pre-Slice 0 the codebase had **five independent probes** answering "is
//! this workspace ready?", each picking a different axis and calling it
//! "compiled":
//!
//! - `RightRail.tsx` badge → `<workspace>/.thinkingroot/graph.db` exists
//! - `pack_estimate` Tauri cmd → `claim_count > 0`
//! - `llm_health` Tauri cmd → `/llm/health` reports mounted
//! - `mcp_list_connected` → `/livez` 200
//! - `workspace_compile_state` → registry has-substrate flag
//!
//! All five could legitimately disagree at the same time and produced
//! contradictory UI ("COMPILED" badge + "not compiled" warning + "not
//! mounted" banner all on one screen). This module is the structural
//! fix: workspace state is a **product of six orthogonal axes**, not a
//! single boolean.
//!
//! # The six axes
//!
//! | Axis        | Lives where                 | Captures                                                  |
//! |-------------|-----------------------------|-----------------------------------------------------------|
//! | [`SubstrateState`] | `<root>/.thinkingroot/`   | Does the substrate exist, is it empty, populated, orphaned, corrupt? |
//! | [`SourcesState`]   | `<root>/` filesystem walk | How many source files; do fingerprints match the last compile?       |
//! | [`MountState`]     | Daemon process            | Is the workspace currently loaded into the engine?                   |
//! | [`LlmState`]       | Network probe             | Is the configured LLM provider reachable?                            |
//! | [`CompileState`]   | Daemon job runner         | Is a compile in flight; how did the last one finish?                 |
//! | [`BranchState`]    | Branch engine             | What branch is active; are there uncommitted writes?                 |
//!
//! # Honesty enforcement (CLAUDE.md §honesty rule §1)
//!
//! - `claim_count` is set ONLY from a real `?[count(*)] := *claims[...]`
//!   Cozo query at probe time. Never fabricated, never estimated.
//! - `source.file_count` is set ONLY from a real filesystem walker.
//!   Stored fields go stale; we re-walk on every probe.
//! - `mount.state == Mounted` requires the daemon to *currently* hold
//!   the Cozo handle. Set on successful open, cleared on close.
//! - `llm.state == Healthy` requires `<5min` since the last successful
//!   probe; older transitions back to `Configured`. Never reports green
//!   from a stale probe.
//! - [`Readiness`] flags are pure derivations of the six axes
//!   (see [`WorkspaceStatus::derive_readiness`]). They cannot be set
//!   independently of the underlying state, by construction.
//! - `as_of` is the wall-clock instant the snapshot was taken; consumers
//!   age it to detect "no events for N seconds → stream may be dead".
//!
//! # Wire shape
//!
//! All types serde with `rename_all = "snake_case"` (matches the rest
//! of the daemon's REST surface). The `WorkspaceStatus` JSON shape is
//! the contract between daemon (writer) and desktop / CLI / future
//! cloud dashboard (readers). Adding fields requires `#[serde(default)]`
//! on every consumer; renaming or removing fields is a wire break.

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Substrate axis — the state of `<workspace_root>/.thinkingroot/`.
///
/// "Compiled" collapses three legitimate sub-states (`Empty`,
/// `Populated`, `Orphaned`) into one boolean — exactly the bug Slice 0
/// fixes. UIs render five distinct affordances per variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SubstrateState {
    /// `.thinkingroot/` does not exist. Workspace was registered but
    /// `root compile` (or a pack mount) has not run yet. Compile is the
    /// only honest action; nothing else can succeed.
    Absent,
    /// `.thinkingroot/graph.db` exists with the schema but zero claims.
    /// Compile ran but extracted nothing — typically because the
    /// workspace has no source files, or extraction failed silently
    /// upstream of T1-T12. Right-rail must NOT show "COMPILED" green
    /// for this state — it shows `EMPTY` amber with a diagnostic.
    Empty {
        /// Bytes on disk. Useful for the UI to surface "the substrate
        /// is 12 KiB of schema; you've never compiled real content".
        graph_db_bytes: u64,
    },
    /// `.thinkingroot/graph.db` exists and contains claims + entities.
    /// The "real ready" state for query / chat / export.
    Populated {
        /// Bytes on disk.
        graph_db_bytes: u64,
        /// Live claim count from `?[count] := *claims[..]`. Recomputed
        /// on every probe.
        claim_count: u64,
        /// Live entity count.
        entity_count: u64,
        /// Snapshot of the source-file count at the time the last
        /// compile finished. The drift between this and
        /// [`SourcesState::Some::file_count`] is what surfaces "you've
        /// added files since compile" without a separate fingerprint
        /// query.
        source_count_at_last_compile: u64,
    },
    /// `.thinkingroot/` was deleted while the daemon held it open.
    /// Mirrors the existing `WorkspaceState::OrphanedSubstrate`
    /// transition; surfaced as a distinct status state so the UI can
    /// show "ORPHAN" with a one-click "Rebuild" / "Remove" affordance
    /// instead of a generic error.
    Orphaned {
        /// The root path the daemon was mounted at when the deletion
        /// occurred. Surfaced verbatim in the UI's "rebuild" prompt so
        /// the user knows which workspace to re-create.
        workspace_root: PathBuf,
    },
    /// `graph.db` exists but Cozo refuses to open it. Distinct from
    /// `Empty` (which is "schema present, no rows") because corruption
    /// means we cannot recover by simply running compile against it —
    /// the substrate must be rebuilt from sources.
    Corrupt {
        /// One-line reason from CozoDB; surfaced in the UI's diagnostic.
        reason: String,
    },
}

/// Sources axis — state of the workspace's actual content files (the
/// non-`.thinkingroot/` files inside `<workspace_root>`).
///
/// `None` is the surprise state — a registered workspace with zero
/// files (`CipherVault` in the screenshot the audit was triggered by).
/// Compile against `None` produces `Empty` substrate; the UI now
/// surfaces this gap honestly instead of hiding it behind a
/// "compiled" badge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourcesState {
    /// Zero source files in the workspace directory (after applying the
    /// same exclusion rules as the pack writer: `.thinkingroot/`,
    /// `cache/`, `config.toml`, `fingerprints.json`, `target/`,
    /// `node_modules/`, `.git/`).
    None,
    /// One or more source files present.
    Some {
        /// File count from a live walker — never a stored field.
        file_count: u64,
        /// Sum of file sizes in bytes.
        total_bytes: u64,
        /// Most recent mtime across all source files; `None` if none.
        last_changed_at: Option<DateTime<Utc>>,
        /// True iff the daemon's incremental-compile fingerprint ledger
        /// matches the current filesystem state. False means a compile
        /// would actually do work.
        fingerprint_match: bool,
    },
}

impl SourcesState {
    /// Convenience accessor. Returns `0` for [`SourcesState::None`].
    pub fn file_count(&self) -> u64 {
        match self {
            SourcesState::None => 0,
            SourcesState::Some { file_count, .. } => *file_count,
        }
    }

    /// Convenience: are there any source files at all?
    pub fn has_sources(&self) -> bool {
        matches!(self, SourcesState::Some { .. })
    }
}

/// Mount axis — is the workspace currently loaded into the daemon's
/// engine?
///
/// Distinct from [`SubstrateState`] because a workspace can have
/// populated substrate on disk without the daemon having opened it
/// (the daemon mounts lazily). Pre-Slice 0 the chat banner and the
/// MCP TOOLS panel could disagree on this for the same workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MountState {
    /// Daemon is not holding a Cozo handle for this workspace.
    NotMounted,
    /// `POST /workspaces` is in flight; the engine write-lock is held.
    /// Surfaced so the UI can show a transient indicator rather than
    /// flickering between `NotMounted` and `Mounted`.
    Mounting,
    /// Daemon owns the Cozo handle; queries route here.
    Mounted {
        /// When the mount succeeded.
        since: DateTime<Utc>,
    },
    /// Last mount attempt failed. Carries the reason for the UI's
    /// "Retry mount" affordance.
    Failed {
        /// One-line error from the mount handler.
        reason: String,
        /// When the failure occurred.
        at: DateTime<Utc>,
    },
}

/// LLM provider health axis.
///
/// Honesty: [`LlmState::Healthy`] requires a *recent* successful probe
/// (within [`LLM_HEALTH_WINDOW`]). After that, the state machine moves
/// it back to [`LlmState::Configured`] until the next probe re-confirms
/// — a stale 1-hour-old success can never read as green.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LlmState {
    /// No provider configured for this workspace.
    Unconfigured,
    /// Provider configured but never probed (or last probe is older
    /// than [`LLM_HEALTH_WINDOW`]). The chat banner says "configured;
    /// click to probe" rather than green.
    Configured {
        /// Provider id from `~/.thinkingroot/credentials.toml` or
        /// workspace `config.toml`.
        provider: String,
        /// Active model. Optional because some providers default to a
        /// fleet-managed model and don't pin one explicitly.
        model: Option<String>,
    },
    /// Last probe succeeded within [`LLM_HEALTH_WINDOW`].
    Healthy {
        /// Provider id.
        provider: String,
        /// Active model.
        model: Option<String>,
        /// When the successful probe completed.
        last_probed_at: DateTime<Utc>,
    },
    /// Last probe failed.
    Unreachable {
        /// Provider id.
        provider: String,
        /// One-line reason from the provider's HTTP response or
        /// transport-layer error.
        reason: String,
        /// When the failed probe completed.
        last_probed_at: DateTime<Utc>,
    },
}

/// Maximum age of a successful LLM probe before [`LlmState::Healthy`]
/// transitions back to [`LlmState::Configured`]. Five minutes mirrors
/// the daemon's existing per-request credential reload cadence.
pub const LLM_HEALTH_WINDOW: Duration = Duration::from_secs(5 * 60);

/// Compile job axis — what is happening (or last happened) on the
/// daemon's compile job runner for this workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CompileState {
    /// No compile in flight.
    Idle {
        /// When the most recent compile finished, regardless of
        /// outcome. `None` if compile has never run for this workspace.
        last_finished_at: Option<DateTime<Utc>>,
        /// Wall-clock duration of the most recent compile.
        last_duration_ms: Option<u64>,
        /// How the last compile ended.
        last_outcome: Option<CompileOutcome>,
    },
    /// Compile is currently executing.
    Running {
        /// Pipeline phase — matches the existing `IncrementalSummary`
        /// `phase_timings` keys (`parse`, `extract`, `link`, etc.).
        phase: String,
        /// Sources processed so far, when the engine reports it; `None`
        /// during phases that don't track per-source progress.
        progress: Option<CompileProgress>,
        /// When the compile started.
        started_at: DateTime<Utc>,
    },
    /// Cancellation requested (client disconnect or POST cancel) — the
    /// pipeline is unwinding. Distinct from `Idle` so the UI doesn't
    /// flash "ready to compile" between disconnect and final cleanup.
    Cancelling {
        /// When the cancel signal fired.
        since: DateTime<Utc>,
    },
}

/// Outcome of a finished compile run, surfaced under
/// [`CompileState::Idle::last_outcome`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CompileOutcome {
    /// All phases completed; substrate is consistent.
    Success {
        /// Total claims after the run.
        extracted_claims: u64,
        /// Sources successfully processed.
        sources_processed: u64,
    },
    /// One or more LLM batches exhausted retries and the pipeline
    /// surfaced `failed_batches`/`failed_chunk_ranges`. Per
    /// `engine-pipeline.md` we never hide partial success behind "ok".
    Partial {
        /// Total claims after the partial run.
        extracted_claims: u64,
        /// Number of failed batches.
        failed_batches: u64,
        /// Brief description of what failed (`"3 LLM batches"`,
        /// `"2 sources unparseable"`).
        summary: String,
    },
    /// The pipeline aborted before finishing.
    Failed {
        /// Phase the failure occurred in (parse, extract, link, etc.).
        phase: String,
        /// One-line reason from `Error::Display`.
        reason: String,
    },
    /// Cancelled at a phase boundary.
    Cancelled {
        /// Phase the cancellation took effect in.
        phase: String,
    },
}

/// Compile progress fragment surfaced under [`CompileState::Running`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompileProgress {
    /// Sources finished so far.
    pub sources_done: u64,
    /// Sources expected (set early in the pipeline; may grow if walker
    /// discovers new files mid-run).
    pub sources_total: u64,
    /// Optional human-readable detail, e.g. the current phase's status
    /// line.
    pub detail: Option<String>,
}

/// Branch axis — which branch is active and whether it has uncommitted
/// claim writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchState {
    /// Currently-active branch name. Defaults to `"main"`.
    pub current: String,
    /// True iff the active branch has writes that haven't been merged
    /// into the primary line. Read off the existing `BranchRef::dirty`
    /// flag.
    pub modified: bool,
}

impl Default for BranchState {
    fn default() -> Self {
        Self {
            current: "main".to_string(),
            modified: false,
        }
    }
}

/// Derived readiness flags — pure functions of the six axes.
///
/// Every flag's truth condition is documented inline; the
/// [`WorkspaceStatus::derive_readiness`] function is the single
/// implementation. Views never recompute; they read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Readiness {
    /// Compile can be invoked: workspace path exists and no compile is
    /// already running. `True` even when the substrate is empty —
    /// compile against an empty workspace is the legitimate way to
    /// initialise.
    pub for_compile: bool,
    /// Query / search will return useful results: substrate has
    /// claims, daemon has the workspace mounted, an LLM is reachable.
    pub for_query: bool,
    /// Chat / ask will produce a grounded answer. Same gate as
    /// `for_query` today; kept distinct so future divergence (e.g. a
    /// chat surface that doesn't strictly need substrate) doesn't break
    /// the contract.
    pub for_chat: bool,
    /// `.tr` pack export will produce a non-empty pack: substrate has
    /// claims AND the workspace has source files on disk. Without
    /// either, the export dialog disables its button + surfaces the
    /// specific reason via [`Diagnostic`].
    pub for_export: bool,
    /// Pack publish (push to a registry) is safe: same as `for_export`
    /// PLUS LLM healthy (compile-time dependency for catalogue
    /// rendering) PLUS clean default branch (no uncommitted writes).
    pub for_publish: bool,
}

/// Severity tag on [`Diagnostic`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    /// Informational; no action required but useful context.
    Info,
    /// Something is preventing one or more readiness flags from being
    /// `true`; the user should act.
    Warn,
    /// Hard error — the workspace cannot be used at all (e.g.
    /// `Substrate::Corrupt`).
    Error,
}

/// Stable machine-readable codes for diagnostics. Used by UI to wire
/// per-code actions without fragile string matching on
/// [`Diagnostic::message`].
///
/// Codes are append-only — renaming is a wire break.
pub mod diagnostic_codes {
    /// `<root>/.thinkingroot/` does not exist.
    pub const NO_SUBSTRATE: &str = "no_substrate";
    /// Substrate exists but has zero claims.
    pub const EMPTY_SUBSTRATE: &str = "empty_substrate";
    /// Workspace directory has no parseable source files.
    pub const NO_SOURCES: &str = "no_sources";
    /// Substrate was orphaned (workspace dir deleted under a mounted daemon).
    pub const ORPHANED: &str = "orphaned";
    /// Substrate is corrupt (Cozo refused to open).
    pub const CORRUPT: &str = "corrupt";
    /// Daemon has not mounted this workspace yet.
    pub const NOT_MOUNTED: &str = "not_mounted";
    /// Last mount attempt failed.
    pub const MOUNT_FAILED: &str = "mount_failed";
    /// No LLM provider configured.
    pub const NO_PROVIDER: &str = "no_provider";
    /// LLM provider unreachable (last probe failed).
    pub const PROVIDER_UNREACHABLE: &str = "provider_unreachable";
    /// LLM provider configured but not probed within
    /// [`super::LLM_HEALTH_WINDOW`].
    pub const PROVIDER_STALE: &str = "provider_stale";
    /// Compile is currently running — most write actions disabled.
    pub const COMPILE_RUNNING: &str = "compile_running";
    /// Last compile failed.
    pub const COMPILE_FAILED: &str = "compile_failed";
    /// Last compile only partially succeeded (some batches exhausted
    /// retries — see `engine-pipeline.md` no-silent-partial-success).
    pub const COMPILE_PARTIAL: &str = "compile_partial";
    /// Source files have changed since last compile (fingerprint
    /// mismatch) — UI may suggest a re-compile.
    pub const SOURCES_STALE: &str = "sources_stale";
    /// Active branch has uncommitted writes.
    pub const BRANCH_DIRTY: &str = "branch_dirty";
    /// Workspace path on disk does not exist.
    pub const PATH_MISSING: &str = "path_missing";
}

/// Action affordance the UI may surface alongside a [`Diagnostic`].
/// `id` is stable; UIs map it to a specific button (e.g. `"compile"` →
/// "Run compile" button calling `workspace_compile`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticAction {
    /// Stable machine identifier — UI maps this to a click handler.
    pub id: String,
    /// Human-readable button label, e.g. "Run compile…", "Mount", "Re-auth in Settings".
    pub label: String,
}

/// Why a particular [`Readiness`] flag is `false`, in machine- AND
/// human-readable form.
///
/// `for` lists the readiness fields this diagnostic blocks. UIs filter
/// the diagnostic list per surface: the chat banner reads diagnostics
/// where `"for_chat"` is in `for`; the export dialog reads
/// `"for_export"`; etc. One source of warning text — never per-view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Stable machine-readable code from [`diagnostic_codes`].
    pub code: String,
    /// `info` / `warn` / `error`.
    pub severity: DiagnosticSeverity,
    /// One-line human-readable explanation.
    pub message: String,
    /// Which readiness flags this diagnostic blocks. Strings rather
    /// than an enum so adding a new readiness flag is forward-compatible.
    pub blocks: Vec<String>,
    /// Suggested actions the UI may render as buttons.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<DiagnosticAction>,
}

/// The atomic snapshot. One per workspace, broadcast on every state
/// transition. The wire shape consumed by every UI surface.
///
/// Adding fields requires `#[serde(default)]` on the consumer so old
/// readers don't break. Renaming or removing fields is a wire break
/// and must coincide with desktop + CLI bumps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceStatus {
    /// Workspace registry name (the `--name` from `root mount` or the
    /// directory name fallback).
    pub name: String,
    /// Absolute path on disk.
    pub path: PathBuf,
    /// When this snapshot was produced. Consumers age this against
    /// wall-clock to detect "the SSE stream went away" or "this cached
    /// snapshot is stale".
    pub as_of: DateTime<Utc>,
    /// Substrate axis.
    pub substrate: SubstrateState,
    /// Sources axis.
    pub sources: SourcesState,
    /// Mount axis.
    pub mount: MountState,
    /// LLM provider axis.
    pub llm: LlmState,
    /// Compile axis.
    pub compile: CompileState,
    /// Branch axis.
    pub branch: BranchState,
    /// Pure-derived readiness flags. Always reflects the six axes —
    /// the state machine refuses to publish a snapshot where this
    /// disagrees with [`WorkspaceStatus::derive_readiness`].
    pub readiness: Readiness,
    /// Why each false readiness is false. Empty when everything is
    /// green.
    #[serde(default)]
    pub diagnostics: Vec<Diagnostic>,
}

impl WorkspaceStatus {
    /// Compute [`Readiness`] from the six axes. Single source of truth;
    /// the state machine calls this on every transition and stamps the
    /// result onto [`WorkspaceStatus::readiness`].
    pub fn derive_readiness(
        substrate: &SubstrateState,
        sources: &SourcesState,
        mount: &MountState,
        llm: &LlmState,
        compile: &CompileState,
        branch: &BranchState,
        path_exists: bool,
    ) -> Readiness {
        let substrate_populated = matches!(substrate, SubstrateState::Populated { .. });
        let mounted = matches!(mount, MountState::Mounted { .. });
        let llm_healthy = matches!(llm, LlmState::Healthy { .. });
        // Bugfix 2026-05-10 — for_query/for_chat accept `Configured` as
        // well as `Healthy`. A workspace with a constructed LlmClient
        // and a populated substrate is chat-ready: the user has a
        // provider in shape, the substrate has claims to ground on,
        // and a momentarily-down provider produces the same UX as a
        // failed pre-flight (a single error toast). Pre-fix the gate
        // required `Healthy` which only the compile path produces, so
        // every workspace that hadn't been recompiled in this daemon
        // session was silently chat-blocked. `for_publish` still
        // requires `Healthy` — publishing warrants the stricter bar.
        let llm_ready = matches!(
            llm,
            LlmState::Healthy { .. } | LlmState::Configured { .. }
        );
        let compile_running = matches!(
            compile,
            CompileState::Running { .. } | CompileState::Cancelling { .. }
        );

        let for_compile = path_exists && !compile_running && !matches!(substrate, SubstrateState::Corrupt { .. } | SubstrateState::Orphaned { .. });
        let for_query = substrate_populated && mounted && llm_ready;
        let for_chat = for_query;
        let for_export = substrate_populated && sources.has_sources();
        let for_publish = for_export && llm_healthy && branch.current == "main" && !branch.modified;

        Readiness {
            for_compile,
            for_query,
            for_chat,
            for_export,
            for_publish,
        }
    }

    /// Derive the diagnostics list from the six axes. Pure function;
    /// the state machine stamps the result onto
    /// [`WorkspaceStatus::diagnostics`]. UI never re-derives.
    pub fn derive_diagnostics(
        substrate: &SubstrateState,
        sources: &SourcesState,
        mount: &MountState,
        llm: &LlmState,
        compile: &CompileState,
        branch: &BranchState,
        path_exists: bool,
    ) -> Vec<Diagnostic> {
        let mut out = Vec::new();

        if !path_exists {
            out.push(Diagnostic {
                code: diagnostic_codes::PATH_MISSING.into(),
                severity: DiagnosticSeverity::Error,
                message: "Workspace path does not exist on disk".into(),
                blocks: vec![
                    "for_compile".into(),
                    "for_query".into(),
                    "for_chat".into(),
                    "for_export".into(),
                    "for_publish".into(),
                ],
                actions: vec![
                    DiagnosticAction {
                        id: "locate".into(),
                        label: "Locate folder…".into(),
                    },
                    DiagnosticAction {
                        id: "remove_from_registry".into(),
                        label: "Remove from registry".into(),
                    },
                ],
            });
        }

        match substrate {
            SubstrateState::Absent => out.push(Diagnostic {
                code: diagnostic_codes::NO_SUBSTRATE.into(),
                severity: DiagnosticSeverity::Warn,
                message: ".thinkingroot/ not found — run compile to initialise".into(),
                blocks: vec!["for_query".into(), "for_chat".into(), "for_export".into(), "for_publish".into()],
                actions: vec![DiagnosticAction {
                    id: "compile".into(),
                    label: "Run compile…".into(),
                }],
            }),
            SubstrateState::Empty { .. } => out.push(Diagnostic {
                code: diagnostic_codes::EMPTY_SUBSTRATE.into(),
                severity: DiagnosticSeverity::Warn,
                message: "Substrate has no claims yet — add sources and recompile".into(),
                blocks: vec!["for_query".into(), "for_chat".into(), "for_export".into(), "for_publish".into()],
                actions: vec![
                    DiagnosticAction {
                        id: "open_in_finder".into(),
                        label: "Reveal in Finder".into(),
                    },
                    DiagnosticAction {
                        id: "compile".into(),
                        label: "Re-run compile…".into(),
                    },
                ],
            }),
            SubstrateState::Orphaned { .. } => out.push(Diagnostic {
                code: diagnostic_codes::ORPHANED.into(),
                severity: DiagnosticSeverity::Error,
                message: ".thinkingroot/ was deleted while the daemon held it open".into(),
                blocks: vec!["for_query".into(), "for_chat".into(), "for_export".into(), "for_publish".into(), "for_compile".into()],
                actions: vec![
                    DiagnosticAction {
                        id: "rebuild".into(),
                        label: "Rebuild from sources…".into(),
                    },
                    DiagnosticAction {
                        id: "remove_from_registry".into(),
                        label: "Remove from registry".into(),
                    },
                ],
            }),
            SubstrateState::Corrupt { reason } => out.push(Diagnostic {
                code: diagnostic_codes::CORRUPT.into(),
                severity: DiagnosticSeverity::Error,
                message: format!("Substrate refused to open: {reason}"),
                blocks: vec!["for_query".into(), "for_chat".into(), "for_export".into(), "for_publish".into(), "for_compile".into()],
                actions: vec![DiagnosticAction {
                    id: "rebuild".into(),
                    label: "Rebuild from sources…".into(),
                }],
            }),
            SubstrateState::Populated { .. } => {}
        }

        if matches!(sources, SourcesState::None) && !matches!(substrate, SubstrateState::Orphaned { .. } | SubstrateState::Corrupt { .. }) {
            out.push(Diagnostic {
                code: diagnostic_codes::NO_SOURCES.into(),
                severity: DiagnosticSeverity::Warn,
                message: "0 source files in workspace directory".into(),
                blocks: vec!["for_export".into(), "for_publish".into()],
                actions: vec![
                    DiagnosticAction {
                        id: "open_in_finder".into(),
                        label: "Reveal in Finder".into(),
                    },
                    DiagnosticAction {
                        id: "add_sources".into(),
                        label: "Add files to workspace…".into(),
                    },
                ],
            });
        }

        if let SourcesState::Some {
            fingerprint_match: false,
            ..
        } = sources
        {
            out.push(Diagnostic {
                code: diagnostic_codes::SOURCES_STALE.into(),
                severity: DiagnosticSeverity::Info,
                message: "Sources have changed since last compile — re-run compile".into(),
                blocks: Vec::new(),
                actions: vec![DiagnosticAction {
                    id: "compile".into(),
                    label: "Run compile…".into(),
                }],
            });
        }

        match mount {
            MountState::NotMounted => {
                if matches!(substrate, SubstrateState::Populated { .. }) {
                    out.push(Diagnostic {
                        code: diagnostic_codes::NOT_MOUNTED.into(),
                        severity: DiagnosticSeverity::Info,
                        message: "Engine has not loaded this workspace yet".into(),
                        blocks: vec!["for_query".into(), "for_chat".into()],
                        actions: vec![DiagnosticAction {
                            id: "mount".into(),
                            label: "Mount".into(),
                        }],
                    });
                }
            }
            MountState::Failed { reason, .. } => out.push(Diagnostic {
                code: diagnostic_codes::MOUNT_FAILED.into(),
                severity: DiagnosticSeverity::Error,
                message: format!("Mount failed: {reason}"),
                blocks: vec!["for_query".into(), "for_chat".into()],
                actions: vec![DiagnosticAction {
                    id: "mount".into(),
                    label: "Retry mount".into(),
                }],
            }),
            MountState::Mounting | MountState::Mounted { .. } => {}
        }

        match llm {
            LlmState::Unconfigured => out.push(Diagnostic {
                code: diagnostic_codes::NO_PROVIDER.into(),
                severity: DiagnosticSeverity::Warn,
                message: "No LLM provider configured for this workspace".into(),
                blocks: vec!["for_query".into(), "for_chat".into(), "for_publish".into()],
                actions: vec![DiagnosticAction {
                    id: "configure_provider".into(),
                    label: "Configure provider…".into(),
                }],
            }),
            LlmState::Configured { .. } => {
                // Info-level only — paired with the readiness relaxation
                // at `derive_readiness`, `Configured` no longer blocks
                // `for_query`/`for_chat`. Publishing still requires a
                // recent successful probe (gated by `for_publish` →
                // `llm_healthy`), so we surface that in the blocks list
                // to drive the "Probe now" prompt for users on the
                // publish path.
                if matches!(substrate, SubstrateState::Populated { .. }) {
                    out.push(Diagnostic {
                        code: diagnostic_codes::PROVIDER_STALE.into(),
                        severity: DiagnosticSeverity::Info,
                        message: "Provider has not been health-checked this session".into(),
                        blocks: vec!["for_publish".into()],
                        actions: vec![DiagnosticAction {
                            id: "probe_provider".into(),
                            label: "Probe now".into(),
                        }],
                    });
                }
            }
            LlmState::Unreachable { reason, .. } => out.push(Diagnostic {
                code: diagnostic_codes::PROVIDER_UNREACHABLE.into(),
                severity: DiagnosticSeverity::Error,
                message: format!("LLM provider unreachable: {reason}"),
                blocks: vec!["for_query".into(), "for_chat".into(), "for_publish".into()],
                actions: vec![DiagnosticAction {
                    id: "configure_provider".into(),
                    label: "Re-auth in Settings…".into(),
                }],
            }),
            LlmState::Healthy { .. } => {}
        }

        match compile {
            CompileState::Running { phase, .. } => out.push(Diagnostic {
                code: diagnostic_codes::COMPILE_RUNNING.into(),
                severity: DiagnosticSeverity::Info,
                message: format!("Compile in progress (phase: {phase})"),
                blocks: vec!["for_compile".into()],
                actions: vec![DiagnosticAction {
                    id: "cancel_compile".into(),
                    label: "Cancel compile".into(),
                }],
            }),
            CompileState::Cancelling { .. } => out.push(Diagnostic {
                code: diagnostic_codes::COMPILE_RUNNING.into(),
                severity: DiagnosticSeverity::Info,
                message: "Compile cancelling…".into(),
                blocks: vec!["for_compile".into()],
                actions: Vec::new(),
            }),
            CompileState::Idle {
                last_outcome: Some(CompileOutcome::Failed { phase, reason }),
                ..
            } => out.push(Diagnostic {
                code: diagnostic_codes::COMPILE_FAILED.into(),
                severity: DiagnosticSeverity::Error,
                message: format!("Last compile failed at {phase}: {reason}"),
                blocks: Vec::new(),
                actions: vec![DiagnosticAction {
                    id: "compile".into(),
                    label: "Re-run compile".into(),
                }],
            }),
            CompileState::Idle {
                last_outcome: Some(CompileOutcome::Partial { summary, .. }),
                ..
            } => out.push(Diagnostic {
                code: diagnostic_codes::COMPILE_PARTIAL.into(),
                severity: DiagnosticSeverity::Warn,
                message: format!("Last compile partial: {summary}"),
                blocks: Vec::new(),
                actions: vec![DiagnosticAction {
                    id: "compile".into(),
                    label: "Re-run compile".into(),
                }],
            }),
            CompileState::Idle { .. } => {}
        }

        if branch.modified {
            out.push(Diagnostic {
                code: diagnostic_codes::BRANCH_DIRTY.into(),
                severity: DiagnosticSeverity::Info,
                message: format!("Branch '{}' has uncommitted writes", branch.current),
                blocks: vec!["for_publish".into()],
                actions: Vec::new(),
            });
        }

        out
    }

    /// Build a complete [`WorkspaceStatus`] from the six axes plus
    /// metadata. Stamps `as_of`, `readiness`, and `diagnostics`
    /// consistently. Single entry point — the state machine never
    /// constructs `WorkspaceStatus` directly.
    pub fn assemble(
        name: String,
        path: PathBuf,
        path_exists: bool,
        substrate: SubstrateState,
        sources: SourcesState,
        mount: MountState,
        llm: LlmState,
        compile: CompileState,
        branch: BranchState,
    ) -> Self {
        let readiness = Self::derive_readiness(
            &substrate, &sources, &mount, &llm, &compile, &branch, path_exists,
        );
        let diagnostics = Self::derive_diagnostics(
            &substrate, &sources, &mount, &llm, &compile, &branch, path_exists,
        );
        Self {
            name,
            path,
            as_of: Utc::now(),
            substrate,
            sources,
            mount,
            llm,
            compile,
            branch,
            readiness,
            diagnostics,
        }
    }

    /// True iff this snapshot is older than `max_age`. Consumers use
    /// this to grey out the UI when the SSE stream goes silent.
    pub fn is_stale(&self, max_age: Duration) -> bool {
        let age = Utc::now().signed_duration_since(self.as_of);
        age.to_std()
            .map(|d| d > max_age)
            .unwrap_or(false)
    }
}

/// Compact wire event emitted on the SSE `/status/stream` channel.
/// Distinct from the full [`WorkspaceStatus`] snapshot so the daemon
/// can also send a lightweight `Heartbeat` without re-emitting the
/// (potentially large) full status body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceStatusEvent {
    /// A new full snapshot. Consumers replace their cached state.
    Snapshot(WorkspaceStatus),
    /// Periodic liveness ping (every 30s). No state change; consumers
    /// use this to detect "stream alive, just nothing happening".
    Heartbeat {
        /// Workspace name the stream is subscribed to. Lets a multi-
        /// workspace consumer disambiguate when subscribing through a
        /// single multiplex.
        name: String,
        /// Wall-clock at the daemon when the heartbeat fired.
        at: DateTime<Utc>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn populated() -> SubstrateState {
        SubstrateState::Populated {
            graph_db_bytes: 65_536,
            claim_count: 42,
            entity_count: 17,
            source_count_at_last_compile: 5,
        }
    }

    fn some_sources() -> SourcesState {
        SourcesState::Some {
            file_count: 5,
            total_bytes: 1024,
            last_changed_at: Some(Utc::now()),
            fingerprint_match: true,
        }
    }

    fn healthy_llm() -> LlmState {
        LlmState::Healthy {
            provider: "anthropic".into(),
            model: Some("claude-opus-4-7".into()),
            last_probed_at: Utc::now(),
        }
    }

    fn idle_compile() -> CompileState {
        CompileState::Idle {
            last_finished_at: Some(Utc::now()),
            last_duration_ms: Some(1840),
            last_outcome: Some(CompileOutcome::Success {
                extracted_claims: 42,
                sources_processed: 5,
            }),
        }
    }

    #[test]
    fn ready_workspace_has_all_readiness_flags() {
        let r = WorkspaceStatus::derive_readiness(
            &populated(),
            &some_sources(),
            &MountState::Mounted { since: Utc::now() },
            &healthy_llm(),
            &idle_compile(),
            &BranchState::default(),
            true,
        );
        assert!(r.for_compile);
        assert!(r.for_query);
        assert!(r.for_chat);
        assert!(r.for_export);
        assert!(r.for_publish);
    }

    #[test]
    fn configured_llm_with_populated_substrate_unblocks_chat_but_not_publish() {
        // Bugfix 2026-05-10 regression test — pre-fix, a workspace with
        // `LlmState::Configured` (no recent probe) had `for_chat=false`
        // because the gate required `Healthy`. Post-fix the readiness
        // gate accepts `Configured` for chat/query (a constructed
        // LlmClient + populated substrate is chat-ready), but `Healthy`
        // is still required for `for_publish` (the stricter bar).
        let r = WorkspaceStatus::derive_readiness(
            &populated(),
            &some_sources(),
            &MountState::Mounted { since: Utc::now() },
            &LlmState::Configured {
                provider: "azure".into(),
                model: Some("gpt-5.4".into()),
            },
            &idle_compile(),
            &BranchState::default(),
            true,
        );
        // Chat-readiness paths green — this is the user-visible fix.
        assert!(r.for_query);
        assert!(r.for_chat);
        assert!(r.for_export);
        assert!(r.for_compile);
        // Publish still requires a fresh probe.
        assert!(!r.for_publish);
    }

    #[test]
    fn empty_substrate_blocks_query_export_publish_only() {
        // CipherVault's actual state: substrate empty, no sources, not
        // mounted, llm configured, compile idle.
        let r = WorkspaceStatus::derive_readiness(
            &SubstrateState::Empty {
                graph_db_bytes: 12_288,
            },
            &SourcesState::None,
            &MountState::NotMounted,
            &LlmState::Configured {
                provider: "anthropic".into(),
                model: Some("claude-opus-4-7".into()),
            },
            &idle_compile(),
            &BranchState::default(),
            true,
        );
        // Compile is the only honest action.
        assert!(r.for_compile);
        // All four user-facing readiness flags are false — no more
        // contradictions across views.
        assert!(!r.for_query);
        assert!(!r.for_chat);
        assert!(!r.for_export);
        assert!(!r.for_publish);
    }

    #[test]
    fn orphaned_blocks_compile_too() {
        let r = WorkspaceStatus::derive_readiness(
            &SubstrateState::Orphaned {
                workspace_root: PathBuf::from("/tmp/ws"),
            },
            &SourcesState::None,
            &MountState::NotMounted,
            &LlmState::Unconfigured,
            &idle_compile(),
            &BranchState::default(),
            true,
        );
        // Orphan must block compile — re-running compile against a
        // missing substrate would just recreate the orphan condition.
        assert!(!r.for_compile);
        assert!(!r.for_query);
        assert!(!r.for_export);
    }

    #[test]
    fn missing_path_blocks_everything_including_compile() {
        let r = WorkspaceStatus::derive_readiness(
            &populated(),
            &some_sources(),
            &MountState::Mounted { since: Utc::now() },
            &healthy_llm(),
            &idle_compile(),
            &BranchState::default(),
            false, // path missing
        );
        assert!(!r.for_compile);
    }

    #[test]
    fn running_compile_disables_for_compile() {
        let r = WorkspaceStatus::derive_readiness(
            &populated(),
            &some_sources(),
            &MountState::Mounted { since: Utc::now() },
            &healthy_llm(),
            &CompileState::Running {
                phase: "extract".into(),
                progress: None,
                started_at: Utc::now(),
            },
            &BranchState::default(),
            true,
        );
        // Can't compile if a compile is already running.
        assert!(!r.for_compile);
        // But query / chat still work — substrate is mid-update but
        // still readable; the engine is single-writer multi-reader.
        // Actually wait — during Running the snapshot may be in flux.
        // The honest answer: while running, substrate is whatever the
        // last snapshot said; query continues to read from the prior
        // committed state in Cozo. So `for_query` stays true if all
        // other axes are happy.
        assert!(r.for_query);
    }

    #[test]
    fn dirty_branch_blocks_publish_only() {
        let r = WorkspaceStatus::derive_readiness(
            &populated(),
            &some_sources(),
            &MountState::Mounted { since: Utc::now() },
            &healthy_llm(),
            &idle_compile(),
            &BranchState {
                current: "main".into(),
                modified: true,
            },
            true,
        );
        assert!(r.for_compile);
        assert!(r.for_query);
        assert!(r.for_export);
        assert!(!r.for_publish);
    }

    #[test]
    fn non_main_branch_blocks_publish() {
        let r = WorkspaceStatus::derive_readiness(
            &populated(),
            &some_sources(),
            &MountState::Mounted { since: Utc::now() },
            &healthy_llm(),
            &idle_compile(),
            &BranchState {
                current: "feature/xyz".into(),
                modified: false,
            },
            true,
        );
        assert!(r.for_compile);
        assert!(r.for_query);
        assert!(r.for_export);
        assert!(!r.for_publish);
    }

    #[test]
    fn empty_substrate_emits_specific_diagnostic() {
        let diags = WorkspaceStatus::derive_diagnostics(
            &SubstrateState::Empty {
                graph_db_bytes: 12_288,
            },
            &SourcesState::None,
            &MountState::NotMounted,
            &LlmState::Configured {
                provider: "anthropic".into(),
                model: None,
            },
            &idle_compile(),
            &BranchState::default(),
            true,
        );
        let codes: Vec<_> = diags.iter().map(|d| d.code.as_str()).collect();
        assert!(codes.contains(&diagnostic_codes::EMPTY_SUBSTRATE));
        assert!(codes.contains(&diagnostic_codes::NO_SOURCES));
        // Right-rail no longer says "compiled" for this state — the
        // diagnostic is the source of warning text.
        let empty = diags
            .iter()
            .find(|d| d.code == diagnostic_codes::EMPTY_SUBSTRATE)
            .unwrap();
        assert_eq!(empty.severity, DiagnosticSeverity::Warn);
        assert!(empty.blocks.contains(&"for_query".to_string()));
        assert!(empty.blocks.contains(&"for_export".to_string()));
        // Action ids must be wired so UI can map clicks.
        assert!(empty.actions.iter().any(|a| a.id == "compile"));
    }

    #[test]
    fn assemble_round_trips_through_serde() {
        let s = WorkspaceStatus::assemble(
            "demo".into(),
            PathBuf::from("/tmp/demo"),
            true,
            populated(),
            some_sources(),
            MountState::Mounted { since: Utc::now() },
            healthy_llm(),
            idle_compile(),
            BranchState::default(),
        );
        let json = serde_json::to_string(&s).unwrap();
        let parsed: WorkspaceStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "demo");
        assert_eq!(parsed.readiness, s.readiness);
        assert_eq!(parsed.diagnostics, s.diagnostics);
    }

    #[test]
    fn diagnostic_for_query_filter() {
        let diags = WorkspaceStatus::derive_diagnostics(
            &SubstrateState::Empty {
                graph_db_bytes: 0,
            },
            &SourcesState::None,
            &MountState::NotMounted,
            &LlmState::Unreachable {
                provider: "anthropic".into(),
                reason: "401".into(),
                last_probed_at: Utc::now(),
            },
            &idle_compile(),
            &BranchState::default(),
            true,
        );
        let chat_blocking: Vec<_> = diags
            .iter()
            .filter(|d| d.blocks.iter().any(|b| b == "for_chat"))
            .map(|d| d.code.as_str())
            .collect();
        // Chat is blocked by both the empty substrate and the LLM
        // unreachable — UI shows both as separate banners or merges.
        assert!(chat_blocking.contains(&diagnostic_codes::EMPTY_SUBSTRATE));
        assert!(chat_blocking.contains(&diagnostic_codes::PROVIDER_UNREACHABLE));
    }

    #[test]
    fn stale_detection_window() {
        let mut s = WorkspaceStatus::assemble(
            "demo".into(),
            PathBuf::from("/tmp/demo"),
            true,
            populated(),
            some_sources(),
            MountState::Mounted { since: Utc::now() },
            healthy_llm(),
            idle_compile(),
            BranchState::default(),
        );
        // Fresh snapshot is not stale.
        assert!(!s.is_stale(Duration::from_secs(60)));
        // Backdate by 2 minutes.
        s.as_of = Utc::now() - chrono::Duration::seconds(120);
        assert!(s.is_stale(Duration::from_secs(60)));
        assert!(!s.is_stale(Duration::from_secs(300)));
    }

    #[test]
    fn substrate_state_serializes_with_kind_tag() {
        let json = serde_json::to_string(&SubstrateState::Empty {
            graph_db_bytes: 100,
        })
        .unwrap();
        assert!(json.contains("\"kind\":\"empty\""), "got {json}");
        // round-trip
        let parsed: SubstrateState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, SubstrateState::Empty { graph_db_bytes: 100 });
    }

    #[test]
    fn workspace_status_event_round_trips() {
        let snap = WorkspaceStatus::assemble(
            "demo".into(),
            PathBuf::from("/tmp/demo"),
            true,
            populated(),
            some_sources(),
            MountState::Mounted { since: Utc::now() },
            healthy_llm(),
            idle_compile(),
            BranchState::default(),
        );
        let ev = WorkspaceStatusEvent::Snapshot(snap.clone());
        let json = serde_json::to_string(&ev).unwrap();
        let parsed: WorkspaceStatusEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            WorkspaceStatusEvent::Snapshot(s) => assert_eq!(s.name, "demo"),
            _ => panic!("expected Snapshot variant"),
        }

        let hb = WorkspaceStatusEvent::Heartbeat {
            name: "demo".into(),
            at: Utc::now(),
        };
        let json = serde_json::to_string(&hb).unwrap();
        assert!(json.contains("\"kind\":\"heartbeat\""), "got {json}");
        let parsed: WorkspaceStatusEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, WorkspaceStatusEvent::Heartbeat { .. }));
    }

    #[test]
    fn diagnostic_codes_are_stable_strings() {
        // Sanity check: codes never accidentally collide.
        let codes = [
            diagnostic_codes::NO_SUBSTRATE,
            diagnostic_codes::EMPTY_SUBSTRATE,
            diagnostic_codes::NO_SOURCES,
            diagnostic_codes::ORPHANED,
            diagnostic_codes::CORRUPT,
            diagnostic_codes::NOT_MOUNTED,
            diagnostic_codes::MOUNT_FAILED,
            diagnostic_codes::NO_PROVIDER,
            diagnostic_codes::PROVIDER_UNREACHABLE,
            diagnostic_codes::PROVIDER_STALE,
            diagnostic_codes::COMPILE_RUNNING,
            diagnostic_codes::COMPILE_FAILED,
            diagnostic_codes::COMPILE_PARTIAL,
            diagnostic_codes::SOURCES_STALE,
            diagnostic_codes::BRANCH_DIRTY,
            diagnostic_codes::PATH_MISSING,
        ];
        let mut sorted = codes.to_vec();
        sorted.sort();
        let len_before = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), len_before, "duplicate diagnostic codes");
    }
}
