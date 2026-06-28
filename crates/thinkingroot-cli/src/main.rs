use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use console::style;
use tracing_subscriber::EnvFilter;
use thinkingroot_cli::summary_printer;

mod brain_cmd;
mod brain_code;
mod branch_cmd;
mod branch_data_cmd;
mod branch_extras_cmd;
mod branch_template_cmd;
mod claims_cmd;
mod cloud;
mod compliance_cmd;
mod cortex_client;
mod cortex_remote;
mod doctor;
mod engram_cmd;
mod eval_cmd;
mod function_cmd;
mod prompt_cmd;
mod secrets_cmd;
mod mcp_cmd;
mod mcp_config;
mod mount_cmd;
mod otel;
mod pack_cmd;
mod pipeline;
mod progress;
mod proposal_cmd;
mod provider_cmd;
mod reflect_cmd;
mod render_cmd;
mod resolver;
mod retrieve_cmd;
// `rooting_cmd` deleted in Witness Mesh cutover — there is no admission
// gate to inspect when every Witness is admitted by construction. The
// surviving "verification" surface is `tr-verify` (pack-level anchor
// check) + `witness_verifier::verify_witness_anchor` (per-witness BLAKE3
// re-check), both of which already have their own CLI surfaces.
mod serve;
mod service;
mod setup;
mod status_cmd;
mod tag_cmd;
mod update_cmd;
mod watch;
mod workspace;

#[derive(Parser)]
#[command(
    name = "root",
    about = "ThinkingRoot — Compiled knowledge infrastructure for AI agents",
    version,
    long_about = "ThinkingRoot compiles anything — codebases, docs, PDFs, notes, git history — into typed, verified, source-locked knowledge. Agents query it in <1ms instead of re-reading 50K tokens every session. 91.2% on LongMemEval."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Path to compile (shorthand for `root compile <path>`)
    #[arg(value_name = "PATH")]
    path: Option<PathBuf>,

    /// Enable verbose logging
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Cortex Protocol escape hatch: force the in-process call path
    /// instead of delegating to the daemon. Required for hermetic CI
    /// (no background daemon survives the test job) and for
    /// air-gapped scenarios where the daemon is intentionally not
    /// running. When set, the CLI emits a WARN log so accidental
    /// uses leave a breadcrumb back to the standard path.
    #[arg(long, global = true)]
    in_process: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Compile a directory through the v3 knowledge pipeline
    /// (Parse → Extract+Ground+Rooting+Link+SVO → CozoDB persist).
    /// Vector indexing, markdown artifacts, and the post-compile
    /// health score are NOT part of this command — invoke
    /// `root query`/`root ask` (auto-builds the index on first call),
    /// `root render`, and `root health` for those.
    Compile {
        /// Path to the directory to compile
        path: PathBuf,
        /// Compile into a specific branch instead of main
        #[arg(long)]
        branch: Option<String>,
        /// Skip the Rooting admission gate (Phase 6.5). All admitted claims
        /// stay in the `attested` tier — same as pre-Rooting behavior.
        #[arg(long)]
        no_rooting: bool,
        /// Emit the IncrementalSummary as JSON (one line, `serde_json`
        /// canonical) instead of the formatted summary table. Useful
        /// for piping into `jq` or driving CI dashboards.
        #[arg(long)]
        json: bool,
        /// Stay running and re-compile when files change. Uses notify-rs
        /// file watcher with `--debounce` ms quiet window. Press Ctrl-C
        /// to stop.
        #[arg(long)]
        watch: bool,
        /// Debounce window for `--watch` in milliseconds (default 200).
        /// Lower values trip more compiles on bursty saves; higher values
        /// delay the first compile after a final keystroke.
        #[arg(long, default_value = "200")]
        debounce: u64,
        /// Disable all incremental cutoffs; force the full pipeline. Phase
        /// 1 diff still runs but the fingerprint check is bypassed so every
        /// potentially-changed source proceeds through Phase 4+. Useful
        /// when the workspace is in a known-bad state and a clean rebuild
        /// is wanted without nuking `.thinkingroot/` first.
        #[arg(long)]
        no_incremental: bool,
        /// Offload the compile to ThinkingRoot Cloud GPUs. Requires a
        /// signed-in session (`root login`). Streams progress; downloads
        /// the result pack; mounts locally. Internally delegates to the
        /// `publish` flow with visibility: Private. Mutually exclusive
        /// with `--watch` (cloud compile is one-shot).
        #[arg(long)]
        cloud: bool,
    },
    /// Show the knowledge health score
    Health {
        /// Path to the compiled knowledge base
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Emit an EU AI Act technical-documentation bundle (Article 11 +
    /// Annex IV) for a compiled workspace. Produces eight files plus a
    /// BLAKE3 manifest under `--out`, optionally signed via Sigstore-
    /// keyless DSSE.  CLAUDE.md §honesty rule §1 is enforced: the
    /// training-data section uses an allow-list of provider-published
    /// URLs and refuses to fabricate lineage for unknown providers.
    Compliance {
        /// Emit the EU AI Act bundle. Required.
        #[arg(long = "eu-ai-act")]
        eu_ai_act: bool,
        /// Output directory. Bundle lands as a timestamped sub-dir.
        #[arg(long, value_name = "DIR")]
        out: Option<PathBuf>,
        /// Sign the manifest via Sigstore-keyless DSSE (browser OIDC,
        /// or `$TR_OIDC_TOKEN` if set).
        #[arg(long)]
        sign: bool,
        /// Workspace root.
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
    },
    /// Diagnose setup + health. Returns structured JSON with
    /// `--json`; runs interactive fix wizard with `--fix --interactive`;
    /// runs no-op auto-fix with `--fix`; silences output with `--quiet`.
    Doctor {
        /// Emit machine-readable JSON to stdout.
        #[arg(long)]
        json: bool,
        /// Run fix actions for failing checks.
        #[arg(long)]
        fix: bool,
        /// Prompt y/n per fix (no-op without --fix).
        #[arg(long)]
        interactive: bool,
        /// Silence stdout, exit-code only. For `install.sh` tail.
        #[arg(long)]
        quiet: bool,
    },
    /// Show unified workspace status (Slice 0).
    ///
    /// Reads the daemon's `/api/v1/workspaces/{name}/status` endpoint
    /// — the same source of truth the desktop's right-rail badge,
    /// chat banner, and export dialog all consume. With `--watch`,
    /// streams every state change from the SSE companion endpoint.
    ///
    /// Distinct from the legacy `root status` (which reports branch +
    /// filesystem state of a path); `workspace-status` reports the
    /// daemon-tracked unified status across substrate, sources,
    /// mount, LLM, compile, and branch axes.
    WorkspaceStatus {
        /// Workspace name (defaults to the active workspace from the
        /// registry when omitted).
        name: Option<String>,
        /// Emit the JSON snapshot on stdout instead of formatted
        /// human prose. Useful for piping into `jq` or scripting.
        #[arg(long)]
        json: bool,
        /// Subscribe to the SSE stream and print every snapshot until
        /// Ctrl-C. Honours the same JSON / human-prose flag.
        #[arg(long)]
        watch: bool,
    },
    /// Initialize a new ThinkingRoot workspace
    Init {
        /// Path to initialize
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Migrate an existing workspace to a contract version.
    ///
    /// Runs the schema upgrades for that contract (idempotent) and
    /// retroactively emits structural rows for legacy sources via
    /// `backfill_structural`. Required after upgrading to a release
    /// that ships a new compile contract — `root compile` auto-runs
    /// this when it detects the `compile_schema_version` mismatch,
    /// but you can run it manually here for visibility on a dry-run
    /// or when migrating a CI cache.
    Migrate {
        /// Path to the workspace to migrate
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Migrate to the Compile Completeness Contract (v2). Today
        /// the only supported migration target — the flag exists so
        /// future contracts can pin a target version explicitly.
        #[arg(long)]
        to_completeness_contract: bool,
        /// Migrate to the water-flow incremental schema (v3). Purges orphan
        /// structural rows left by source deletions that occurred before the
        /// water-flow cascade fix, re-resets dangling Phase 7e callee pointers,
        /// and bumps `compile_schema_version` to "3". Idempotent — safe to
        /// re-run. `root compile` auto-triggers this on first compile after
        /// an engine upgrade.
        #[arg(long)]
        to_water_flow: bool,
        /// Migrate to the Witness Mesh substrate (witness_schema_version "2").
        /// Reads the legacy `claims` table and synthesises one Witness per
        /// byte-anchored claim into the new `witnesses` table. Idempotent —
        /// a workspace already at schema v2 returns a zero-counts report.
        /// User-driven only (the pipeline does not auto-run this) because
        /// Witness ids are content-derived BLAKE3 hashes, not legacy ULIDs;
        /// engram pointers referencing old claim ids would silently dangle.
        #[arg(long)]
        to_witness_mesh: bool,
        /// Report what the migration would do without writing anything.
        /// Valid with `--to-water-flow` or `--to-witness-mesh`.
        #[arg(long)]
        dry_run: bool,
    },
    /// Query the compiled knowledge base (raw vector search)
    Query {
        /// The query string
        query: String,
        /// Path to the compiled knowledge base
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
        /// Number of results to show
        #[arg(short = 'n', long, default_value = "10")]
        top_k: usize,
    },
    /// Ask a question using the full hybrid intelligence pipeline (91.2% accuracy).
    /// Handles factual recall, counting, temporal reasoning, preferences — everything.
    /// Usage: root ask "what did I buy last week?"
    ///        root ask llm "what happened last Saturday?" --date "2023/05/30"
    Ask {
        /// 'llm' keyword (optional) or your question directly.
        /// Examples:
        ///   root ask "what did I buy last week?"
        ///   root ask llm "what did I buy last week?"
        first: String,
        /// Your question when 'llm' is the first argument
        rest: Vec<String>,
        /// Path to the compiled knowledge base
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
        /// Reference date for temporal questions (e.g. "2023/05/30").
        /// Auto-detected as today's date when omitted.
        #[arg(long)]
        date: Option<String>,
    },
    /// Open the interactive knowledge graph in your browser
    Graph {
        /// Path to the compiled knowledge base
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Port to bind
        #[arg(long, default_value = "3001")]
        port: u16,
    },
    /// Start the REST API and MCP server
    Serve {
        /// Port to bind. Cortex Protocol canonical port is 31760
        /// (shared with the desktop sidecar so a single-tenant user
        /// running both surfaces still hits one daemon). Override
        /// with `--port 3000` to keep the legacy default for existing
        /// scripts.
        #[arg(long, default_value = "31760")]
        port: u16,
        /// Host to bind
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Optional API key for bearer authentication
        #[arg(long)]
        api_key: Option<String>,
        /// Workspace paths to mount (repeatable; if omitted, reads from registry)
        #[arg(long = "path")]
        paths: Vec<PathBuf>,
        /// Mount a single workspace by registry name
        #[arg(long)]
        name: Option<String>,
        /// Run as MCP stdio server (single workspace, no HTTP)
        #[arg(long)]
        mcp_stdio: bool,
        /// Disable REST API (MCP only)
        #[arg(long)]
        no_rest: bool,
        /// Disable MCP endpoints (REST only)
        #[arg(long)]
        no_mcp: bool,
        /// Generate and install an OS-native service file (launchd/systemd/Windows)
        #[arg(long)]
        install_service: bool,
        /// Serve a specific branch instead of main
        #[arg(long)]
        branch: Option<String>,
    },
    /// First-time guided setup wizard
    Setup,
    /// Print the BLAKE3 hex digest of a file.  Hidden — used by
    /// `install.sh` to populate the install manifest at install
    /// time.  No stable contract for external callers.
    #[command(hide = true)]
    HashFile {
        /// Path to the file to hash.
        path: std::path::PathBuf,
    },
    /// One-time, LOSSLESS SQLite→RocksDB storage migration: walk a data dir
    /// (e.g. the per-project `/workspace` volume) and convert every workspace /
    /// branch / user-brain `graph.db` from a SQLite FILE to a RocksDB directory
    /// via cozo backup→restore. Idempotent (already-RocksDB dirs are skipped);
    /// the old SQLite file is kept beside it as `graph.db.sqlite.bak`.
    #[command(hide = true)]
    StorageMigrate {
        /// Data dir to walk recursively for `graph.db` files.
        dir: std::path::PathBuf,
    },
    /// Restore a workspace graph from a portable backup file (cozo
    /// `restore_backup`) into a FRESH RocksDB dir — disaster recovery from an
    /// object-storage backup. Any existing `graph.db` is moved aside first.
    #[command(hide = true)]
    StorageRestore {
        /// The workspace graph dir (e.g. …/.thinkingroot/graph).
        graph_dir: std::path::PathBuf,
        /// The portable `.bak` file (a cozo backup) to restore from.
        backup: std::path::PathBuf,
    },
    /// Multi-agent flow orchestration (C19, 2026-05-22).
    ///
    /// Declare YAML/TOML flow definitions and run them locally
    /// against per-workspace storage. For agent-driven runs
    /// (Claude Code / Cursor / etc.), use the `flow_run` MCP tool.
    Flow {
        #[command(subcommand)]
        action: thinkingroot_cli::flow_cmd::FlowAction,
    },
    /// Manage registered workspaces
    Workspace {
        #[command(subcommand)]
        action: WorkspaceAction,
    },
    /// Manage the login-agent that auto-starts `root serve`
    /// on macOS (launchd), Linux (systemd --user), and Windows
    /// (Task Scheduler at-logon).
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Write MCP configuration to detected AI tools
    Connect {
        /// Only connect this specific tool (e.g. "claude", "cursor")
        #[arg(long)]
        tool: Option<String>,
        /// Port the ThinkingRoot server is running on. Defaults to the
        /// Cortex Protocol canonical port 31760; override only if you
        /// run the daemon on a non-default port.
        #[arg(long, default_value = "31760")]
        port: u16,
        /// Show what would be written without changing any files
        #[arg(long)]
        dry_run: bool,
        /// Remove ThinkingRoot entry from all tool configs
        #[arg(long)]
        remove: bool,
    },
    /// Watch for changes and recompile incrementally
    Watch {
        /// Path to the directory to watch
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Create or manage knowledge branches
    Branch {
        /// Branch name to create
        name: Option<String>,
        /// List all active branches
        #[arg(long)]
        list: bool,
        /// Delete (abandon) a branch — keeps data directory
        #[arg(long)]
        delete: Option<String>,
        /// Hard-delete a branch and remove its data directory
        #[arg(long)]
        purge: Option<String>,
        /// Remove all abandoned branch data directories (garbage collect)
        #[arg(long)]
        gc: bool,
        /// Optional description for the new branch
        #[arg(long)]
        description: Option<String>,
        /// Path to workspace root
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
    },
    /// Set the active branch (update HEAD)
    Checkout {
        /// Branch name to check out
        name: String,
        /// Path to workspace root
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
    },
    /// Show semantic diff between a branch and main (Knowledge PR)
    Diff {
        /// Branch name to diff against main
        branch: String,
        /// Path to workspace root
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
    },
    /// Merge a branch into main (runs health CI gate)
    Merge {
        /// Branch name to merge
        branch: String,
        /// Path to workspace root
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
        /// Skip health CI gate
        #[arg(long)]
        force: bool,
        /// Apply claim deletions from branch to main
        #[arg(long)]
        propagate_deletions: bool,
        /// Restore main to its state before this branch was merged
        #[arg(long)]
        rollback: bool,
        /// Manually resolve a contradiction (format: <index>=keep-main|keep-branch).
        /// Index refers to the numbered list shown by `root diff`. Repeatable.
        #[arg(long = "resolve", value_name = "N=RESOLUTION")]
        resolutions: Vec<String>,
    },
    /// Show current branch and workspace status
    Status {
        /// Path to workspace root
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
    },
    /// Create an immutable named snapshot of the current branch
    Snapshot {
        /// Snapshot name
        name: String,
        /// Path to workspace root
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
    },
    /// T1.4 — Import a `.tr` knowledge pack as a new branch in the
    /// current workspace.  Forks main, replays the pack's claims +
    /// entities + sources into the branch's graph.db, and registers
    /// the branch.  Round-trip pair to `root pack --branch <name>`.
    BranchImport {
        /// Path to the `.tr` pack to import.
        pack: PathBuf,
        /// Name to assign the new branch (must be unique in the
        /// workspace).
        branch: String,
        /// Path to the destination workspace root.  Must already
        /// contain a `.thinkingroot/` from a prior `root compile`
        /// (an empty graph is fine — the import path forks off main
        /// regardless of whether main has data).
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
        /// Skip Sigstore signature + revocation checks on the pack.
        /// The pack-hash chain is still verified — `--no-verify`
        /// skips trust, not integrity.
        #[arg(long)]
        no_verify: bool,
    },
    /// Manage and switch LLM providers
    Provider {
        #[command(subcommand)]
        action: Option<ProviderAction>,
    },
    /// Update root to the latest version
    Update,
    /// Run the LongMemEval benchmark against a compiled workspace
    Eval {
        /// Path to the LongMemEval JSONL dataset file
        #[arg(long)]
        dataset: PathBuf,
        /// Path to the compiled workspace to evaluate
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
        /// Limit number of questions to evaluate (0 = all)
        #[arg(long, default_value = "0")]
        limit: usize,
        /// Filter by category (e.g. "TR", "SSP", "MS") — empty = all
        #[arg(long)]
        category: Option<String>,
        /// Azure deployment name for the GPT-4o judge LLM.
        /// When set, synthesis uses the workspace's configured model (e.g. GPT-4.1)
        /// while grading uses this deployment (e.g. "gpt-4o-deployment").
        /// Requires the workspace to use the azure provider.
        /// If omitted, the workspace's model is used for both synthesis and judging.
        #[arg(long)]
        judge_deployment: Option<String>,
        /// Rooting ablation mode. `on` filters Rejected-tier claims out of
        /// retrieval (what a production consumer with `trust=rooted` sees);
        /// `off` and `advisory` leave retrieval unchanged. Use `on` vs
        /// `off` as the ablation pair.
        #[arg(long, value_parser = ["on", "off", "advisory"])]
        rooting_mode: Option<String>,
        /// Honest mode: do NOT inject the gold answer sessions (no oracle leak).
        /// Retrieval is scoped to the full haystack only, and the reader sees
        /// solely retrieved evidence. Reports retrieval recall@k + the true
        /// end-to-end accuracy (vs. the default reading-ceiling number).
        #[arg(long)]
        no_leak: bool,
    },
    /// Render markdown artifacts (entity pages, architecture map,
    /// decision log, agent brief, runbook, health report) from the
    /// compiled CozoDB graph. Per v3 spec §11, the build pipeline no
    /// longer runs Compile Artifacts by default — agents synthesise
    /// on demand from claims + source. Users wanting pre-rendered
    /// markdown invoke this explicitly.
    Render {
        /// Path to the compiled workspace.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Run a Reflect cycle over the compiled graph and surface
    /// known-unknowns. Use `--json <path>` to write a stable artifact
    /// the cloud's compile-worker ingests into the federation
    /// `pack_reflect_gaps` table.
    ///
    /// Aliased as `root audit` per the v3 spec §11 — the v3 build
    /// pipeline doesn't run reflect by default; users invoke it
    /// explicitly via either name.
    #[command(alias = "audit")]
    Reflect {
        /// Path to the compiled workspace
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Write the open-gap list as JSON to this path. Output schema
        /// is documented in `reflect_cmd.rs`.
        #[arg(long, value_name = "FILE")]
        json: Option<PathBuf>,
    },
    /// Package a compiled workspace into a portable `.tr` knowledge
    /// pack. Reads metadata from `<workspace>/Pack.toml`; CLI flags
    /// override individual fields. The output `.tr` is a v3
    /// (`tr/3`) outer tar containing `manifest.toml` +
    /// `source.tar.zst` + `claims.jsonl` (and `signature.sig` when
    /// `--sign` or `--sign-keyless` was passed). Skips three
    /// local-only paths from the workspace: `cache/` (recompute
    /// artefact, contains workstation paths), `config.toml`
    /// (workspace-local, may carry provider keys),
    /// `fingerprints.json` (incremental-compile mtime ledger).
    Pack {
        /// Path to the workspace root (must contain `.thinkingroot/`).
        #[arg(default_value = ".")]
        workspace: PathBuf,
        /// Output `.tr` path. Defaults to
        /// `<workspace>/<owner>-<slug>-<version>.tr`.
        #[arg(short, long, value_name = "FILE")]
        out: Option<PathBuf>,
        /// Pack name in `owner/slug` form. Overrides `Pack.toml`.
        #[arg(long)]
        name: Option<String>,
        /// SemVer pack version. Overrides `Pack.toml`.
        #[arg(long)]
        version: Option<String>,
        /// SPDX license expression. Overrides `Pack.toml`.
        #[arg(long)]
        license: Option<String>,
        /// One-line description. Overrides `Pack.toml`.
        #[arg(long)]
        description: Option<String>,
        /// Path to an Ed25519 signing key (32 raw bytes). When set,
        /// the pack is signed inline and emitted with `signature.sig`
        /// as the 4th outer-tar entry. The Phase F design
        /// (`docs/2026-04-29-phase-f-trust-verify-spec.md`) covers
        /// the wire format. This is the air-gapped / self-signed
        /// path; Sigstore-public-good keyless signing uses
        /// `--sign-keyless` instead.
        #[arg(long, value_name = "KEY_FILE", conflicts_with = "sign_keyless")]
        sign: Option<PathBuf>,
        /// Sign the pack via Sigstore-public-good keyless DSSE.
        /// The CLI obtains an OIDC id_token (preferring `$TR_OIDC_TOKEN`
        /// if set; otherwise opening the default browser to
        /// `https://oauth2.sigstore.dev/auth`), requests an ephemeral
        /// ECDSA P-256 cert from Fulcio, signs the DSSE PAE with the
        /// ephemeral key, submits the entry to Rekor, and embeds the
        /// resulting Sigstore Bundle as `signature.sig` in the outer
        /// tar. The signing key never touches disk. See
        /// `crates/tr-sigstore/src/live.rs` for the flow.
        #[arg(long, conflicts_with = "sign")]
        sign_keyless: bool,
        /// T1.4 — pack a specific branch's claim graph instead of the
        /// workspace's main graph.  When set, the pack opens
        /// `<workspace>/.thinkingroot/branches/<slug>/graph/graph.db`
        /// and emits a pack containing only claims that live on that
        /// branch.  The byte store at `<workspace>/.thinkingroot/`
        /// remains the source-of-truth for source bytes (branches
        /// share the byte store with main); content hashes referenced
        /// by branch-only claims that don't exist in the main store
        /// are skipped with a warning rather than failing the pack.
        #[arg(long)]
        branch: Option<String>,
    },
    /// Verify a v3 `.tr` pack's integrity and signature without
    /// installing it. Runs the offline verification chain from spec
    /// §7.6: recompute the pack hash, check it matches the manifest's
    /// declared `pack_hash`, then verify the embedded Sigstore bundle
    /// (DSSE signature + in-toto statement subject digest). Exit
    /// codes match the install verification surface
    /// (`pack_cmd::EXIT_*`): 0 verified, 70 unsigned (without
    /// `--allow-unsigned`), 71 tampered, 72 revoked.
    Verify {
        /// Path to the `.tr` pack file.
        pack: PathBuf,
        /// Accept Unsigned packs as success. Without this flag, an
        /// unsigned pack exits 2.
        #[arg(long)]
        allow_unsigned: bool,
        /// Skip the revocation deny-list check. Use only for fully
        /// air-gapped workflows or when the cached snapshot is known
        /// to be current and offline. Without this flag, the verifier
        /// consults the cached deny-list (refreshing if stale, falling
        /// back to the cache on network failure).
        #[arg(long)]
        no_revocation_check: bool,
        /// Registry URL serving `/api/v1/revoked` (for snapshot
        /// refreshes). Defaults to the configured production registry.
        /// Override for testing or for self-hosted / on-prem mirror
        /// deployments. Ignored when `--no-revocation-check` is set.
        #[arg(long, value_name = "URL")]
        registry: Option<String>,
    },
    /// Install a `.tr` knowledge pack — extract its contents to a
    /// target directory's `.thinkingroot/` so the engine can mount it
    /// for `root query` / `root serve`. The reference may be:
    ///
    /// • a local path: `./pack.tr`, `/abs/path.tr`
    /// • a direct URL: `https://example.com/pack.tr`
    /// • a registry coordinate: `owner/slug@version` (or `@latest`),
    ///   resolved via the configured registry's discovery doc.
    ///
    /// Always verifies the manifest's canonical-bytes hash on read;
    /// for registry installs, also cross-checks the BLAKE3 of the
    /// downloaded body against the `x-tr-content-hash` response
    /// header before unpacking.
    Install {
        /// Local path, https URL, or `owner/slug@version` coordinate.
        reference: String,
        /// Target directory. Defaults to
        /// `~/.thinkingroot/packs/<owner>/<slug>/<version>/`.
        #[arg(short, long, value_name = "DIR")]
        target: Option<PathBuf>,
        /// Override the configured registry for this invocation.
        /// Otherwise resolved via `$TR_REGISTRY_URL`, then
        /// `~/.config/thinkingroot/registry.toml`, then a built-in
        /// default of `https://thinkingroot.dev`.
        #[arg(long, value_name = "URL")]
        registry: Option<String>,
        /// Allow installing unsigned (T0) packs from remote sources.
        /// Local installs accept T0 by default; this flag is required
        /// only for `https://` URLs and `owner/slug@version`.
        #[arg(long)]
        allow_unsigned: bool,
        /// Render a human-readable preview without extracting or
        /// modifying anything on disk. Skips trust verification —
        /// the goal is to inspect what a pack contains before
        /// deciding to install it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Mount a `.tr` knowledge pack as a live, cortex-attached
    /// workspace. The pack's claims become queryable through the
    /// daemon's REST + MCP endpoints in one command — the canonical
    /// "secondary brain" entry point per
    /// `docs/2026-05-03-secondary-brain-quickstart.md`.
    ///
    /// On success, prints a JSON `MountSummary` to stdout containing
    /// the workspace name, REST URL, MCP URL, and substrate counts
    /// (sources, claims, entities). The Python and TS SDKs parse
    /// this verbatim.
    Mount {
        /// Path to a `.tr` pack on the local filesystem.
        pack: PathBuf,
        /// Override the auto-derived workspace name (default:
        /// `manifest.name` with `/` replaced by `-`).
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// Skip Sigstore signature verification when the pack is
        /// signed. The pack-hash chain is always verified — this
        /// flag only governs the cert-chain + Rekor inclusion check
        /// (network-dependent, slower).
        #[arg(long)]
        no_verify: bool,
        /// After replaying claims, drive the daemon to fully recompile
        /// against the unpacked sources. Rebuilds the 33-table
        /// structural substrate (function calls, headings, doc tags,
        /// etc.) at the cost of one LLM extraction pass. Defaults
        /// off — replay-only mounts are queryable instantly.
        #[arg(long)]
        recompile: bool,
    },

    // -------------------------------------------------------------------------
    // Cloud subcommands (Phase G consolidation — replace legacy `tr` binary).
    //
    // All five share an optional `--server <url>` flag and consult
    // `~/.config/thinkingroot/auth.json` for the API token. Default
    // server is `https://api.thinkingroot.dev`; override per-command
    // or via `TR_SERVER` env var.
    // -------------------------------------------------------------------------
    /// Save your cloud API token. Replaces the legacy `tr login`.
    Login {
        /// Bearer token from the hub's `/settings/api-tokens`.
        /// Omit to use the browser-flow login (recommended).
        #[arg(long, env = "THINKINGROOT_API_TOKEN")]
        token: Option<String>,
        /// Cloud server URL. Defaults to https://api.thinkingroot.dev.
        #[arg(long, env = "THINKINGROOT_API_SERVER")]
        server: Option<String>,
        /// Skip the browser flow — only use `--token <T>` when set.
        /// Useful in headless CI environments.
        #[arg(long)]
        no_browser: bool,
    },
    /// Sign out of ThinkingRoot Cloud — wipes the local auth file.
    Logout,
    /// Print the cloud identity associated with your saved token.
    /// Replaces the legacy `tr whoami`.
    Whoami {
        /// Override the configured cloud server.
        #[arg(long, env = "TR_SERVER")]
        server: Option<String>,
    },
    /// Scaffold `tr-pack.toml` in the current directory so `root
    /// publish` can upload the workspace. Replaces `tr init`.
    PackInit {
        /// Pack slug (also the published name). Defaults to the
        /// current directory name.
        #[arg(long)]
        slug: Option<String>,
        /// Owner handle. Defaults to the logged-in user's handle.
        #[arg(long)]
        owner: Option<String>,
        /// Override the configured cloud server.
        #[arg(long, env = "TR_SERVER")]
        server: Option<String>,
    },
    /// Tarball the current workspace and submit a cloud compile job.
    /// Replaces the legacy `tr publish`.
    Publish {
        /// Path to the workspace root.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Don't poll the compile job — return immediately after
        /// enqueue.
        #[arg(long)]
        no_wait: bool,
        /// Wait timeout in seconds when polling.
        #[arg(long, default_value = "300")]
        timeout: u64,
        /// Override the configured cloud server.
        #[arg(long, env = "TR_SERVER")]
        server: Option<String>,
        /// Override the manifest's `pack.visibility` value. The hub's
        /// private-packs gate (Pro-tier only) is enforced server-side.
        #[arg(long, value_parser = clap::value_parser!(cloud::publish::Visibility))]
        visibility: Option<cloud::publish::Visibility>,
    },
    /// Push this workspace as a pack to ThinkingRoot Cloud. Alias
    /// for `root publish` with GitHub-feel UX.
    Push {
        /// Path to the workspace root.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Don't poll the compile job — return immediately after
        /// enqueue.
        #[arg(long)]
        no_wait: bool,
        /// Override the configured cloud server.
        #[arg(long, env = "TR_SERVER")]
        server: Option<String>,
        /// Wait timeout in seconds when polling.
        #[arg(long, default_value = "300")]
        timeout: u64,
        /// Override the manifest's `pack.visibility` value.
        #[arg(long, value_parser = clap::value_parser!(cloud::publish::Visibility))]
        visibility: Option<cloud::publish::Visibility>,
    },
    /// Pull a pack from ThinkingRoot Cloud into the local workspace.
    /// Alias for `root install owner/slug[@version]`.
    Pull {
        /// Pack reference in the form `owner/slug` or `owner/slug@version`.
        pack_ref: String,
        /// Target directory. Defaults to
        /// `~/.thinkingroot/packs/<owner>/<slug>/<version>/`.
        #[arg(long)]
        target: Option<PathBuf>,
    },
    /// Clone a pack into a new directory. Alias for `root pull`
    /// with a required `target`.
    Clone {
        /// Pack reference in the form `owner/slug` or `owner/slug@version`.
        pack_ref: String,
        /// Destination directory for the clone.
        target: PathBuf,
    },
    /// List your recent cloud compile jobs. Replaces `tr status` —
    /// the existing `root status` continues to show local branch
    /// state.
    Jobs {
        /// Maximum jobs to return.
        #[arg(long, default_value = "10")]
        limit: u32,
        /// Override the configured cloud server.
        #[arg(long, env = "TR_SERVER")]
        server: Option<String>,
    },

    /// Knowledge Proposal (T0.4) operations.
    ///
    /// Knowledge Proposals gate `MergePolicy::RequiresProposal` merges.
    /// `root proposal open` against the source branch, gather reviews,
    /// then merge once the policy's `min_reviewers` threshold is met.
    Proposal {
        #[command(subcommand)]
        action: ProposalAction,
    },

    /// Tag operations (T2.5 immutable snapshot tags).
    Tag {
        #[command(subcommand)]
        action: TagAction,
    },

    /// Extra branch operations (events, stats, lineage, rebase, rollback,
    /// contribute-bulk, redaction-set). The base `root branch` subcommand
    /// still handles list/create/delete.
    BranchOp {
        #[command(subcommand)]
        action: BranchOpAction,
    },

    /// Workspace orientation — token-efficient summary of counts, top
    /// entities, and recent decisions. Parity with the MCP `brief` tool.
    Brief {
        /// Path to the compiled workspace.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Branch name to scope the summary against (defaults to main).
        #[arg(long)]
        branch: Option<String>,
        /// Emit raw JSON instead of the formatted summary.
        #[arg(long)]
        json: bool,
    },

    /// Full graph context for one entity — relations (both directions),
    /// claims with provenance, and active contradictions. Parity with
    /// the MCP `investigate` tool.
    Investigate {
        /// Entity name to investigate (case-sensitive).
        entity: String,
        /// Path to the compiled workspace.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Branch name to scope the lookup against (defaults to main).
        #[arg(long)]
        branch: Option<String>,
        /// Emit raw JSON instead of the formatted context.
        #[arg(long)]
        json: bool,
    },

    /// Brain-as-Code — sync a brain to/from a git-versionable `./brain/`
    /// source folder (prompts, functions, routes). The OSS distribution
    /// convention; `pull` exports the live brain to disk.
    Brain {
        #[command(subcommand)]
        action: BrainAction,
    },

    /// Hybrid retrieve — vector recall fused with 11-component score
    /// across the typed Datalog substrate, with per-row BLAKE3 verification.
    Retrieve {
        /// The query string.
        query: String,
        /// Path to the compiled workspace.
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
        /// Number of hits to return.
        #[arg(short = 'n', long, default_value = "20")]
        top_k: usize,
        /// Branch name to scope the retrieve against (defaults to main).
        #[arg(long)]
        branch: Option<String>,
        /// Scoring profile name. `default` or `compliance`.
        #[arg(long)]
        profile: Option<String>,
        /// Emit raw JSON.
        #[arg(long)]
        json: bool,
    },

    /// Inspect compiled claims — list, filter, or query as-of a moment.
    Claims {
        /// Path to the compiled workspace.
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
        /// T2.4 — return claims that existed at or before this ISO-8601
        /// timestamp (e.g. `2026-04-15T00:00:00Z`).
        #[arg(long, value_name = "TIMESTAMP")]
        as_of: Option<String>,
        /// Restrict to trust-Rooted claims (the production-consumer view).
        #[arg(long, conflicts_with = "as_of")]
        rooted: bool,
        /// Branch name to scope against.
        #[arg(long)]
        branch: Option<String>,
        /// Filter by claim_type (Decision, Fact, Quantity, etc.).
        #[arg(long, conflicts_with_all = ["as_of", "rooted"])]
        r#type: Option<String>,
        /// Filter by entity name.
        #[arg(long, conflicts_with_all = ["as_of", "rooted"])]
        entity: Option<String>,
        /// Filter by minimum confidence in [0.0, 1.0].
        #[arg(long, conflicts_with_all = ["as_of", "rooted"])]
        min_confidence: Option<f64>,
        /// Maximum claims to return.
        #[arg(long, conflicts_with_all = ["as_of", "rooted"])]
        limit: Option<u32>,
        /// Pagination offset.
        #[arg(long, conflicts_with_all = ["as_of", "rooted"])]
        offset: Option<u32>,
        /// Emit raw JSON.
        #[arg(long)]
        json: bool,
    },

    /// Branch templates (T3.7) — pre-baked merge policy / kind / TTL
    /// bundles. Apply with `branch-template apply <template> --to <branch>`.
    BranchTemplate {
        #[command(subcommand)]
        action: BranchTemplateAction,
    },

    /// Active Engram Protocol — RARP lifecycle (materialize, list, probe,
    /// expire). Each invocation mints a fresh session id unless
    /// `--session <id>` ties multiple calls together.
    Engram {
        #[command(subcommand)]
        action: EngramAction,
    },

    /// Phase E.5 (2026-05-17) — manage external MCP servers bridged
    /// into ThinkingRoot's `tools/list`. Per-workspace config lives
    /// at `<workspace>/.thinkingroot/mcp-servers.toml`.
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },

    /// Manage Root Functions (deployed JS run in the engine's V8
    /// isolate). Talks to a running `root serve` over REST.
    Function {
        #[command(subcommand)]
        action: FunctionAction,
    },

    /// Manage workspace secrets in `~/.config/thinkingroot/secrets.toml`
    /// (mode 0600). Read by Root Functions via `ctx.env`.
    Secrets {
        #[command(subcommand)]
        action: SecretsAction,
    },

    /// Manage Compiled Prompt templates (versioned). Talks to a running
    /// `root serve` over REST.
    Prompt {
        #[command(subcommand)]
        action: PromptAction,
    },
}

#[derive(Subcommand)]
enum FunctionAction {
    /// Deploy (or version) a function from a JS file.
    Deploy {
        name: String,
        #[arg(long)]
        code: PathBuf,
        #[arg(long, default_value = "main")]
        workspace: String,
        #[arg(long, default_value = "http://127.0.0.1:31760")]
        url: String,
        #[arg(long)]
        api_key: Option<String>,
    },
    /// List deployed functions (latest version each).
    List {
        #[arg(long, default_value = "main")]
        workspace: String,
        #[arg(long, default_value = "http://127.0.0.1:31760")]
        url: String,
        #[arg(long)]
        api_key: Option<String>,
    },
    /// Invoke a function with a JSON input argument.
    Invoke {
        name: String,
        #[arg(long, default_value = "{}")]
        input: String,
        #[arg(long, default_value = "main")]
        workspace: String,
        #[arg(long, default_value = "http://127.0.0.1:31760")]
        url: String,
        #[arg(long)]
        api_key: Option<String>,
    },
}

#[derive(Subcommand)]
enum SecretsAction {
    /// Set a secret (value read from stdin if omitted).
    Set { name: String, value: Option<String> },
    /// List secret names (never values).
    List,
    /// Remove a secret.
    Unset { name: String },
}

#[derive(Subcommand)]
enum BrainAction {
    /// Export the live brain (prompts + functions) to `./brain/`.
    Pull {
        /// Path to the workspace (defaults to current dir).
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Branch to scope against (defaults to main).
        #[arg(long)]
        branch: Option<String>,
    },
    /// Apply a `./brain/` folder to the live brain (deploy prompts + functions).
    Push {
        /// Path to the workspace (defaults to current dir).
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Branch to scope against (defaults to main).
        #[arg(long)]
        branch: Option<String>,
    },
}

#[derive(Subcommand)]
enum PromptAction {
    /// Write a new version of a template from a file (or stdin).
    Edit {
        name: String,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long, default_value = "main")]
        workspace: String,
        #[arg(long, default_value = "http://127.0.0.1:31760")]
        url: String,
        #[arg(long)]
        api_key: Option<String>,
    },
    /// List templates (latest version each).
    List {
        #[arg(long, default_value = "main")]
        workspace: String,
        #[arg(long, default_value = "http://127.0.0.1:31760")]
        url: String,
        #[arg(long)]
        api_key: Option<String>,
    },
    /// Show a template's version history.
    Version {
        name: String,
        #[arg(long, default_value = "main")]
        workspace: String,
        #[arg(long, default_value = "http://127.0.0.1:31760")]
        url: String,
        #[arg(long)]
        api_key: Option<String>,
    },
}

#[derive(Subcommand)]
enum McpAction {
    /// Register an external MCP server (stdio transport).
    /// Example: `root mcp add filesystem -- npx -y @modelcontextprotocol/server-filesystem /Users/me/Documents`
    Add {
        /// Server name (used as the `<server>::<tool>` prefix).
        name: String,
        /// Workspace root path (defaults to current dir).
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
        /// Subprocess command + args; everything after `--` is the
        /// command to invoke.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command_and_args: Vec<String>,
    },
    /// List external MCP servers registered for a workspace.
    List {
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
    },
    /// Remove an external MCP server.
    Remove {
        name: String,
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
    },
}

#[derive(Subcommand)]
enum BranchTemplateAction {
    /// List every template registered in the workspace.
    List,
    /// Print full BranchTemplate JSON for one template.
    Get {
        /// Template name.
        name: String,
    },
    /// Create or overwrite a template by reading a BranchTemplate JSON
    /// blob from `<file>`. The `name` field inside the JSON is the
    /// template id.
    Upsert {
        /// Path to a JSON file describing the BranchTemplate.
        file: PathBuf,
    },
    /// Delete a template.
    Delete {
        /// Template name.
        name: String,
    },
    /// Materialise a new branch from the template.
    Apply {
        /// Template name to apply.
        template: String,
        /// New branch name.
        #[arg(long = "to", value_name = "BRANCH")]
        branch: String,
        /// Optional description for the new branch.
        #[arg(long)]
        description: Option<String>,
    },
}

#[derive(Subcommand)]
enum EngramAction {
    /// Materialise an engram for a topic. The daemon picks seed
    /// entities via vector search unless `--seed <id>` is passed
    /// (repeatable).
    Materialize {
        /// Free-text topic.
        topic: String,
        /// Path to the workspace.
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
        /// Explicit seed entity ids (repeatable).
        #[arg(long = "seed", value_name = "ENTITY_ID")]
        seeds: Vec<String>,
        /// Optional scope override (e.g. `1-cluster`, `5-cluster`).
        #[arg(long)]
        scope: Option<String>,
        /// Reuse an existing session id instead of minting a new one.
        #[arg(long)]
        session: Option<String>,
    },
    /// List engrams currently held by a session.
    List {
        /// Path to the workspace.
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
        /// Session id to query (required for any non-empty result).
        #[arg(long)]
        session: Option<String>,
    },
    /// Probe an engram with a question.
    Probe {
        /// Pointer (`0xXXXX`) returned by `materialize`.
        pointer: String,
        /// Free-text question.
        question: String,
        /// Path to the workspace.
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
        /// Clearance levels (Public/Internal/Confidential/Restricted).
        /// Repeatable.
        #[arg(long = "clearance")]
        clearance: Vec<String>,
        /// Force a probe-kind (overrides regex routing).
        #[arg(long)]
        probe_kind: Option<String>,
        /// Compose with hybrid scoring (reorders the answer rows in
        /// lockstep using the 11-component fused score).
        #[arg(long)]
        score_with_hybrid: bool,
        /// Reuse an existing session id.
        #[arg(long)]
        session: Option<String>,
    },
    /// Expire an engram (frees the pointer).
    Expire {
        /// Pointer to expire.
        pointer: String,
        /// Path to the workspace.
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
        /// Session id that owns the pointer.
        #[arg(long)]
        session: Option<String>,
    },
}

#[derive(Subcommand)]
enum ProposalAction {
    /// Open a Knowledge Proposal on a branch.
    Open {
        /// Source branch the proposal is opened on.
        branch: String,
        /// Target branch (defaults to `main`).
        #[arg(long, default_value = "main")]
        target: String,
        /// Optional human-readable description.
        #[arg(long)]
        description: Option<String>,
        /// Override the source branch's `min_reviewers` policy.
        #[arg(long)]
        min_reviewers: Option<u8>,
    },
    /// List proposals — workspace-wide unless `--branch` is set.
    List {
        /// Filter to one branch.
        #[arg(long)]
        branch: Option<String>,
    },
    /// Record a review on a proposal.
    Review {
        /// Proposal id (ULID).
        id: String,
        /// Approve the proposal.
        #[arg(long, conflicts_with_all = ["request_changes", "comment"])]
        approve: bool,
        /// Request changes (blocks merge until updated).
        #[arg(long, conflicts_with_all = ["approve", "comment"])]
        request_changes: bool,
        /// Leave a non-blocking comment.
        #[arg(long, conflicts_with_all = ["approve", "request_changes"])]
        comment: bool,
        /// Optional review note.
        #[arg(long)]
        note: Option<String>,
    },
    /// Author-initiated close (terminal — no further reviews accepted).
    Close {
        /// Proposal id (ULID).
        id: String,
    },
}

#[derive(Subcommand)]
enum TagAction {
    /// Create a tag from a branch's current state.
    Create {
        /// Tag name (immutable once created).
        name: String,
        /// Branch whose snapshot the tag points to.
        #[arg(long)]
        branch: String,
        /// Optional message attached to the tag.
        #[arg(long)]
        message: Option<String>,
    },
    /// List all tags in the workspace.
    List,
    /// Print full tag JSON for inspection.
    Get {
        /// Tag name.
        name: String,
    },
}

#[derive(Subcommand)]
enum BranchOpAction {
    /// Show audit-log events for a branch.
    Events {
        /// Branch name.
        branch: String,
    },
    /// Show claim/entity/source/event counts for a branch.
    Stats {
        /// Branch name.
        branch: String,
    },
    /// Print the fork/merge DAG across all branches as JSON.
    Lineage,
    /// Sync a branch with its parent (apply parent-only claims).
    Rebase {
        /// Branch name.
        branch: String,
    },
    /// Restore the parent from the pre-merge snapshot of `branch`.
    Rollback {
        /// Branch name whose merge should be rolled back.
        branch: String,
    },
    /// T0.7 — bulk-contribute claims under a Connector principal with
    /// idempotency. Reads the batch from a JSON file shaped as
    /// `{ session_id?, backfill?, workspace?, claims: [...] }`.
    ContributeBulk {
        /// Branch name to contribute into.
        branch: String,
        /// Path to the workspace whose name to compute (defaults to `.`)
        /// when the input file does not pin `workspace`.
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
        /// Connector identifier (`github`, `slack`, ...).
        #[arg(long = "connector-id", value_name = "ID")]
        connector_id: String,
        /// Per-install identifier (`alice-acme-prod`).
        #[arg(long = "install-id", value_name = "ID")]
        install_id: String,
        /// Idempotency key (typically the upstream event id).
        #[arg(long = "idempotency-key", value_name = "KEY")]
        idempotency_key: String,
        /// JSON file shaped as `BulkInputFile` (see branch_data_cmd.rs).
        #[arg(long, value_name = "FILE")]
        file: PathBuf,
    },
    /// T2.6 — set or clear a branch's outbound redaction policy. Reads
    /// the policy from a JSON file unless `--clear` is set.
    RedactionSet {
        /// Branch name to set the policy on.
        branch: String,
        /// Path to a JSON file shaped as `RedactionPolicy`. Required
        /// unless `--clear` is set.
        #[arg(long, value_name = "FILE", conflicts_with = "clear")]
        file: Option<PathBuf>,
        /// Clear the existing policy.
        #[arg(long, conflicts_with = "file")]
        clear: bool,
    },
}

// `RootingAction` deleted in Witness Mesh cutover.

#[derive(Subcommand)]
enum ProviderAction {
    /// List all available providers and show which is active (default)
    List {
        /// Workspace path to check for local overrides
        #[arg(short, long, default_value = ".", value_name = "PATH")]
        path: PathBuf,
    },
    /// Show active provider, model, and credential status
    Status {
        /// Workspace path to check for local overrides
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
    },
    /// Switch to a different provider
    Use {
        /// Provider name: openrouter, openai, azure, anthropic, bedrock, ollama,
        /// groq, together, deepseek, perplexity, litellm, custom
        name: String,
        /// Model ID (e.g. gpt-4o-mini). Prompted if not given.
        #[arg(long)]
        model: Option<String>,
        /// API key value. Skips interactive prompt; sets the provider's env var
        /// for this session. Ignored for bedrock and ollama.
        #[arg(long, value_name = "KEY")]
        key: Option<String>,
        /// Base URL override for self-hosted or custom endpoints.
        /// Required for the 'custom' provider.
        #[arg(long, value_name = "URL")]
        base_url: Option<String>,
        /// Write to .thinkingroot/config.toml instead of the global config.
        /// Overrides provider for this workspace only.
        #[arg(long)]
        local: bool,
        /// Workspace path (used with --local)
        #[arg(short, long, default_value = ".", value_name = "PATH")]
        path: PathBuf,
        /// Skip API key validation (useful in CI or offline environments)
        #[arg(long)]
        no_validate: bool,
        /// Azure resource name — skips the interactive prompt (azure only)
        #[arg(long, value_name = "NAME")]
        azure_resource: Option<String>,
        /// Azure deployment name — skips the interactive prompt (azure only)
        #[arg(long, value_name = "DEPLOYMENT")]
        azure_deployment: Option<String>,
        /// Azure API version — skips the interactive prompt (azure only)
        #[arg(long, value_name = "VERSION")]
        azure_api_version: Option<String>,
    },
    /// Change the extraction model without changing the provider
    #[command(name = "set-model")]
    SetModel {
        /// Model ID (e.g. gpt-4o, llama3, claude-3-haiku-20240307)
        model: String,
        /// Write to workspace config instead of global config
        #[arg(long)]
        local: bool,
        /// Workspace path (used with --local)
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
    },
}

#[derive(Subcommand)]
enum WorkspaceAction {
    /// Register a directory as a workspace
    Add {
        /// Path to the directory
        path: PathBuf,
        /// Workspace name (defaults to directory name)
        #[arg(long)]
        name: Option<String>,
        /// Port for this workspace's server (defaults to next available)
        #[arg(long)]
        port: Option<u16>,
    },
    /// List all registered workspaces
    List,
    /// Remove a workspace from the registry
    Remove {
        /// Workspace name to remove
        name: String,
    },
    /// Auto-discover workspaces by walking the filesystem.
    ///
    /// Walks each `--root <path>` (or sensible defaults: `~/Desktop`,
    /// `~/Documents`, `~/code`, `~/dev`, `~/projects`, `~/src`,
    /// `~/workspace`) up to depth 4, registering any directory that
    /// contains a `.thinkingroot/` marker. Existing entries are
    /// preserved. Same logic the desktop's auto-scan uses, exposed
    /// to the CLI for parity (Stream G).
    Scan {
        /// Override the scan roots. Repeatable.
        #[arg(long = "root", value_name = "PATH")]
        roots: Vec<PathBuf>,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Install + activate the login agent so `root serve` starts on
    /// every login (and right now). Idempotent.
    Install,
    /// Stop and remove the login agent. Idempotent.
    Uninstall,
}

fn main() {
    // Worker stack of 8 MB. Default is 2 MB but the synthesis path
    // transitively pulls in fastembed → ONNX Runtime, which can blow
    // a 2 MB tokio worker stack on first model load —
    // `/api/v1/ws/{ws}/ask` (and the new `/ask/stream`) both reproduce
    // this on a fresh process without the bump. `#[tokio::main]`
    // doesn't expose `thread_stack_size`, so we build the runtime
    // manually.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 * 1024 * 1024)
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: failed to build tokio runtime: {e}");
            std::process::exit(1);
        }
    };
    let result = runtime.block_on(async_main());
    match result {
        Ok(()) => {}
        Err(e) => {
            // Distinct exit code when the cortex daemon is unreachable
            // so wrappers and CI scripts can detect "your engine is
            // not running" without grepping stderr. Walk the anyhow
            // cause chain in case a higher-level handler attached
            // additional `with_context` frames above the marker.
            if e.downcast_ref::<cortex_remote::DaemonUnreachable>().is_some()
                || e.chain()
                    .any(|err| err.is::<cortex_remote::DaemonUnreachable>())
            {
                eprintln!("error: {e:#}");
                std::process::exit(pack_cmd::EXIT_DAEMON_UNREACHABLE);
            }
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
    }
}

/// Cortex Protocol entry point for stateful CLI commands.
///
/// Returns:
/// - `Some(Remote)` — daemon is running (or was just auto-spawned);
///   caller should use the cortex_remote::run_*_remote variant.
/// - `None` — `--in-process` was set OR resolve_engine errored OR
///   the connection came back as InProcess. Caller should fall
///   through to the legacy in-process path.
///
/// The decision tree is intentionally lenient: any failure in
/// resolve_engine logs a WARN and falls back to in-process, so a
/// transient daemon hiccup doesn't break the user's command.
async fn try_resolve_remote(
    in_process_flag: bool,
) -> Option<thinkingroot_core::cortex::EngineConnection> {
    if in_process_flag {
        tracing::warn!(
            "--in-process flag set; bypassing Cortex Protocol and opening CozoDB locally. \
             This is the legacy path; remove the flag to use the singleton daemon."
        );
        return None;
    }

    use thinkingroot_core::cortex::{EngineConnection, EngineIntent};
    match cortex_client::resolve_engine(EngineIntent::Command).await {
        Ok(conn @ EngineConnection::Remote { .. }) => Some(conn),
        Ok(EngineConnection::InProcess) | Ok(EngineConnection::Stdio) => None,
        Ok(EngineConnection::SpawnRequired { .. }) => {
            unreachable!("CLI resolve_engine never returns SpawnRequired (handled internally as detached spawn)");
        }
        Ok(EngineConnection::RepairNeeded { failing_check_ids }) => {
            eprintln!("error: ThinkingRoot engine cannot start.\n");
            for id in &failing_check_ids {
                eprintln!("  ✗ {id}");
            }
            eprintln!("\nRun `root doctor --fix` to repair, or `root doctor --json` for details.");
            std::process::exit(1);
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "cortex resolve_engine failed; falling back to in-process path"
            );
            None
        }
    }
}

/// Stream C — `try_resolve_remote` returns `None` when no daemon is
/// running. The new parity subcommands (proposals, tags, branch
/// extras) require a daemon because their REST routes are the only
/// implementation; falling back to in-process would silently mean
/// "did nothing." This wrapper surfaces a clear error tailored to
/// whether the user explicitly opted into `--in-process` or just hit
/// the daemon-not-running case.
async fn require_remote(
    in_process_flag: bool,
) -> anyhow::Result<thinkingroot_core::cortex::EngineConnection> {
    try_resolve_remote(in_process_flag).await.ok_or_else(|| {
        if in_process_flag {
            anyhow::anyhow!(
                "this subcommand has no in-process implementation — it is REST-only. \
                 Drop the `--in-process` flag and start a daemon with `root serve` in \
                 another terminal."
            )
        } else {
            anyhow::anyhow!(
                "this subcommand requires a running daemon. Start one with `root serve` \
                 in another terminal."
            )
        }
    })
}

async fn async_main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Detect TTY *before* initialising the subscriber — the filter depends on it.
    // Progress bars and tracing INFO both write to stderr; in TTY mode we suppress
    // INFO to avoid garbling the bars (same approach as `cargo build`).
    use std::io::IsTerminal as _;
    let use_progress = !cli.verbose && std::io::stderr().is_terminal();

    // Detect --mcp-stdio early so we can silence stdout logging.
    // MCP stdio protocol requires stdout to be pure JSON-RPC lines.
    // Any non-JSON line (INFO, WARN, etc.) sent to stdout will break every
    // MCP client (Claude Code, Cursor, Codex, Windsurf, Zed, VS Code).
    let is_mcp_stdio = matches!(
        &cli.command,
        Some(Commands::Serve {
            mcp_stdio: true,
            ..
        })
    );

    // Filter rule of thumb:
    //   * Use `info`/`debug` as the BASELINE so custom `target: "rerank"` /
    //     `"engram"` / `"observer"` / etc. tracing events emitted by our crates
    //     are visible — the old `thinkingroot=info,root=info` prefix filter
    //     silently dropped all 13 custom targets (rerank, ort_session,
    //     observer, paper, readme, reflect, mcp_bridge, fs_watch, witness_typed_edges,
    //     workspace_state, ttl_cleanup, stream_cleanup, cognition_commit).
    //   * Mute known-noisy third-party crates explicitly so the baseline
    //     stays useful without an `RUST_LOG=info,h2=warn,…` incantation.
    //   * `RUST_LOG` takes priority when set — operator escape hatch.
    const NOISE_MUTE: &str = "h2=warn,hyper=warn,hyper_util=warn,reqwest=warn,\
        rustls=warn,tokio_util=warn,tower=warn,want=warn,mio=warn,\
        cozo=warn,sqlx=warn";
    let default_directive = if cli.verbose {
        format!("debug,{NOISE_MUTE}")
    } else if is_mcp_stdio {
        // MCP stdio: only WARN/ERROR to stderr; stdout must stay pure JSON-RPC.
        "warn".to_string()
    } else if use_progress {
        // TTY + no --verbose: suppress everything below ERROR so progress bars
        // own stderr cleanly. WARN/INFO mixed with indicatif garbles the display.
        "error".to_string()
    } else {
        // Pipe / CI / detached daemon: full INFO for clean log output.
        format!("info,{NOISE_MUTE}")
    };
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(&default_directive));
    // stderr fmt (unchanged) + optional OTLP export to OpenObserve when
    // OTEL_EXPORTER_OTLP_ENDPOINT is set (the provisioner sets it per engine);
    // unset → stderr-only, zero cost. See `otel` module.
    otel::init_tracing("tr-engine", filter);

    let in_process_flag = cli.in_process;

    match cli.command {
        Some(Commands::Compile {
            path,
            branch,
            no_rooting,
            json,
            watch,
            debounce,
            no_incremental,
            cloud,
        }) => {
            if cloud {
                // `--cloud` offloads to the hub's compile-worker. The
                // current implementation delegates to `publish::run`
                // with visibility: Private. A future enhancement may
                // add `register_pack: false` so cloud compile downloads
                // the result without registering a publicly-visible
                // pack (spec §2 punch-list item).
                if watch {
                    anyhow::bail!(
                        "--cloud is mutually exclusive with --watch (cloud compile is one-shot)"
                    );
                }
                println!(
                    "{} offloading compile to ThinkingRoot Cloud",
                    console::style("→").cyan()
                );
                cloud::publish::run(
                    path,
                    /* wait */ true,
                    /* timeout */ 600,
                    /* server */ None,
                    /* visibility */ Some(cloud::publish::Visibility::Private),
                )
                .await?;
                return Ok(());
            }
            // Cortex Protocol: prefer the daemon. Falls back to
            // in-process on `--in-process` or daemon error.
            if let Some(conn) = try_resolve_remote(in_process_flag).await {
                cortex_remote::run_compile_remote(&conn, &path, branch.as_deref(), no_rooting, json)
                    .await?;
            } else {
                run_compile(&path, branch.as_deref(), use_progress, no_rooting, json, no_incremental).await?;
            }

            if watch {
                let watch_path = path.clone();
                let watch_branch = branch.clone();
                let options = watch::WatchOptions {
                    debounce_ms: debounce,
                    max_ticks: None,
                };

                eprintln!(
                    "[watch] watching {} (debounce {}ms) — Ctrl-C to stop",
                    watch_path.display(),
                    debounce
                );

                watch::run_watch_loop(
                    watch_path.clone(),
                    options,
                    move |_changed| {
                        let p = watch_path.clone();
                        let b = watch_branch.clone();
                        let in_process = in_process_flag;
                        async move {
                            if let Some(conn) = try_resolve_remote(in_process).await {
                                cortex_remote::run_compile_remote(
                                    &conn,
                                    &p,
                                    b.as_deref(),
                                    no_rooting,
                                    json,
                                )
                                .await?;
                            } else {
                                run_compile(
                                    &p,
                                    b.as_deref(),
                                    use_progress,
                                    no_rooting,
                                    json,
                                    no_incremental,
                                )
                                .await?;
                            }
                            Ok(())
                        }
                    },
                )
                .await?;

                eprintln!("\n[watch] stopped");
            }
        }
        Some(Commands::Health { path }) => {
            if let Some(conn) = try_resolve_remote(in_process_flag).await {
                cortex_remote::run_health_remote(&conn, &path).await?;
            } else {
                run_health(&path).await?;
            }
        }
        Some(Commands::Compliance {
            eu_ai_act,
            out,
            sign,
            workspace,
        }) => {
            // `compliance` is intentionally NOT routed through the
            // cortex — it reads CozoDB directly via GraphStore for a
            // deterministic snapshot; the daemon doesn't expose a
            // bundle-rendering route and proxying through a streaming
            // route would only add latency and a serialization roundtrip.
            compliance_cmd::run_compliance(compliance_cmd::ComplianceOpts {
                eu_ai_act,
                out,
                sign,
                workspace,
            })
            .await?;
        }
        Some(Commands::Doctor { json, fix, interactive, quiet }) => {
            // `doctor` is intentionally NOT routed through the cortex
            // — it inspects local state (lockfile, cache, disk) and
            // probes the daemon as one of its checks. Routing would
            // mean "ask the daemon if it's healthy via the daemon"
            // which is circular.
            // `--fix --json` is the Desktop blocking-panel surface
            // (Slice D): apply available fixes non-interactively,
            // re-run the checks, emit the post-fix JSON report.  Order
            // matters — check `fix && json` before plain `json` so the
            // combined flag is routed to the post-fix flow rather than
            // the read-only Json mode.
            let mode = if fix && json {
                doctor::DoctorMode::FixJson
            } else if json {
                doctor::DoctorMode::Json
            } else if fix && interactive {
                doctor::DoctorMode::FixInteractive
            } else if fix {
                doctor::DoctorMode::Fix
            } else if quiet {
                doctor::DoctorMode::Quiet
            } else {
                doctor::DoctorMode::Default
            };
            let report = doctor::run_doctor(mode).await?;
            let final_report = match mode {
                doctor::DoctorMode::Json => {
                    println!("{}", doctor::format::to_json(&report));
                    report
                }
                doctor::DoctorMode::Quiet => report,
                doctor::DoctorMode::Fix | doctor::DoctorMode::FixInteractive => {
                    print!("{}", doctor::format::to_terminal(&report));
                    let outcomes = doctor::fix::apply_all(
                        &report.checks,
                        mode == doctor::DoctorMode::FixInteractive,
                    );
                    if outcomes.is_empty() {
                        eprintln!("no fixable failures.");
                    } else {
                        for (check, outcome) in &outcomes {
                            eprintln!("  {} → {:?}", check.id.0, outcome);
                        }
                    }
                    report
                }
                doctor::DoctorMode::FixJson => {
                    // Apply all available fixes silently, re-run the
                    // full check matrix, then emit the post-fix JSON.
                    // stdout stays clean for the calling Tauri command
                    // to parse; fix progress noise goes to stderr.
                    let outcomes = doctor::fix::apply_all(&report.checks, false);
                    for (check, outcome) in &outcomes {
                        eprintln!("  {} → {:?}", check.id.0, outcome);
                    }
                    let post_fix = doctor::run_doctor(doctor::DoctorMode::Quiet).await?;
                    println!("{}", doctor::format::to_json(&post_fix));
                    post_fix
                }
                doctor::DoctorMode::Default => {
                    print!("{}", doctor::format::to_terminal(&report));
                    report
                }
            };
            std::process::exit(final_report.summary.exit_code());
        }
        Some(Commands::WorkspaceStatus { name, json, watch }) => {
            // The unified workspace-status command always goes through
            // the cortex — the snapshot lives in the daemon's
            // per-workspace state-machine actor. Without a daemon,
            // there is nothing to show.
            status_cmd::run_status(status_cmd::StatusOpts {
                name,
                json,
                watch,
            })
            .await?;
        }
        Some(Commands::Init { path }) => {
            // `init` is stateless — it creates a `.thinkingroot/`
            // directory and writes config. No CozoDB touched, no
            // cortex routing.
            run_init(&path)?;
        }
        Some(Commands::Migrate {
            path,
            to_completeness_contract,
            to_water_flow,
            to_witness_mesh,
            dry_run,
        }) => {
            // `migrate` opens CozoDB to run schema upgrades; today
            // it's safe to run in-process even when the daemon is
            // up because the migration helpers take the workspace
            // lock and wait. Future hardening: route through the
            // daemon to centralise schema mutations.
            run_migrate(
                &path,
                to_completeness_contract,
                to_water_flow,
                to_witness_mesh,
                dry_run,
            )?;
        }
        Some(Commands::Query { query, path, top_k }) => {
            if let Some(conn) = try_resolve_remote(in_process_flag).await {
                cortex_remote::run_query_remote(&conn, &path, &query, top_k).await?;
            } else {
                run_query(&path, &query, top_k).await?;
            }
        }
        Some(Commands::Ask {
            first,
            rest,
            path,
            date,
        }) => {
            // Accept both:
            //   root ask "question"
            //   root ask llm "question"
            let question = if first.to_lowercase() == "llm" {
                rest.join(" ")
            } else if rest.is_empty() {
                first.clone()
            } else {
                format!("{} {}", first, rest.join(" "))
            };
            if question.trim().is_empty() {
                anyhow::bail!(
                    "Please provide a question. Example: root ask \"what did I do last week?\""
                );
            }
            if let Some(conn) = try_resolve_remote(in_process_flag).await {
                cortex_remote::run_ask_remote(&conn, &path, &question, date.as_deref()).await?;
            } else {
                run_query_llm(&path, &question, date.as_deref()).await?;
            }
        }
        Some(Commands::Graph { path, port }) => {
            serve::run_graph(port, path).await?;
        }
        Some(Commands::Serve {
            port,
            host,
            api_key,
            paths,
            name,
            mcp_stdio,
            no_rest,
            no_mcp,
            install_service,
            branch,
        }) => {
            if install_service {
                serve::install_service()?;
                return Ok(());
            }
            serve::run_serve(
                port, host, api_key, paths, name, mcp_stdio, no_rest, no_mcp, branch,
            )
            .await?;
        }
        Some(Commands::Setup) => {
            setup::run_setup().await?;
        }
        Some(Commands::HashFile { path }) => {
            let mut file = std::fs::File::open(&path)
                .with_context(|| format!("opening {}", path.display()))?;
            let mut hasher = blake3::Hasher::new();
            std::io::copy(&mut file, &mut hasher)?;
            println!("{}", hasher.finalize().to_hex());
            return Ok(());
        }
        Some(Commands::StorageMigrate { dir }) => {
            // Walk for every `graph.db` (a FILE = SQLite) under the data dir and
            // migrate each workspace/branch/user brain to RocksDB. A `graph.db`
            // that is a DIRECTORY is already RocksDB — skip it and don't descend.
            let mut stack = vec![dir.clone()];
            let (mut migrated, mut skipped, mut failed) = (0usize, 0usize, 0usize);
            while let Some(d) = stack.pop() {
                let Ok(rd) = std::fs::read_dir(&d) else { continue };
                for e in rd.flatten() {
                    let p = e.path();
                    let is_graph_db = p.file_name().and_then(|n| n.to_str()) == Some("graph.db");
                    let Ok(ft) = e.file_type() else { continue };
                    if ft.is_dir() {
                        if !is_graph_db {
                            stack.push(p);
                        }
                    } else if is_graph_db {
                        if let Some(parent) = p.parent() {
                            match thinkingroot_graph::graph::GraphStore::migrate_sqlite_to_rocksdb(
                                parent,
                            ) {
                                Ok(msg) => {
                                    println!("{msg}");
                                    if msg.starts_with("migrated") {
                                        migrated += 1;
                                    } else {
                                        skipped += 1;
                                    }
                                }
                                Err(e) => {
                                    eprintln!("FAILED {}: {e}", parent.display());
                                    failed += 1;
                                }
                            }
                        }
                    }
                }
            }
            println!(
                "storage migrate done: {migrated} migrated, {skipped} skipped, {failed} failed"
            );
            if failed > 0 {
                std::process::exit(1);
            }
            return Ok(());
        }
        Some(Commands::StorageRestore { graph_dir, backup }) => {
            let db = graph_dir.join("graph.db");
            if db.exists() {
                let ts = chrono::Utc::now().timestamp();
                let aside = graph_dir.join(format!("graph.db.pre-restore-{ts}"));
                std::fs::rename(&db, &aside)
                    .with_context(|| format!("moving existing graph.db aside to {}", aside.display()))?;
                println!("moved existing graph.db -> {}", aside.display());
            }
            let store =
                thinkingroot_graph::graph::GraphStore::init_from_backup(&graph_dir, &backup)
                    .with_context(|| {
                        format!(
                            "restoring {} from {}",
                            graph_dir.display(),
                            backup.display()
                        )
                    })?;
            let n = store
                .raw_db()
                .run_default("?[id] := *claims{id}")
                .map(|r| r.rows.len())
                .unwrap_or(0);
            println!(
                "restored {} ({} claims) from {}",
                graph_dir.display(),
                n,
                backup.display()
            );
            return Ok(());
        }
        Some(Commands::Flow { action }) => {
            let code = thinkingroot_cli::flow_cmd::run(action).await?;
            std::process::exit(code);
        }
        Some(Commands::Workspace { action }) => match action {
            WorkspaceAction::Add { path, name, port } => {
                workspace::run_workspace_add(path, name, port)?;
            }
            WorkspaceAction::List => {
                workspace::run_workspace_list()?;
            }
            WorkspaceAction::Remove { name } => {
                workspace::run_workspace_remove(&name)?;
            }
            WorkspaceAction::Scan { roots } => {
                workspace::run_workspace_scan(roots)?;
            }
        },
        Some(Commands::Service { action }) => match action {
            ServiceAction::Install => {
                let outcome = service::install()
                    .map_err(|e| anyhow::anyhow!("install login agent: {e}"))?;
                service::print_outcome(&outcome, service::OutcomeKind::Install)?;
            }
            ServiceAction::Uninstall => {
                let outcome = service::uninstall()
                    .map_err(|e| anyhow::anyhow!("uninstall login agent: {e}"))?;
                service::print_outcome(&outcome, service::OutcomeKind::Uninstall)?;
            }
        },
        Some(Commands::Connect {
            tool,
            port,
            dry_run,
            remove,
        }) => {
            mcp_config::run_connect(tool.as_deref(), port, dry_run, remove)?;
        }
        Some(Commands::Watch { path }) => {
            let path = std::fs::canonicalize(&path)
                .with_context(|| format!("path not found: {}", path.display()))?;
            run_watch_standalone(&path).await?;
        }
        Some(Commands::Branch {
            name,
            list,
            delete,
            purge,
            gc,
            description,
            path,
        }) => {
            branch_cmd::handle_branch(
                &path,
                name.as_deref(),
                list,
                delete.as_deref(),
                purge.as_deref(),
                gc,
                description,
            )
            .await?;
        }
        Some(Commands::Checkout { name, path }) => {
            branch_cmd::handle_checkout(&path, &name).await?;
        }
        Some(Commands::Diff { branch, path }) => {
            branch_cmd::handle_diff(&path, &branch).await?;
        }
        Some(Commands::Merge {
            branch,
            path,
            force,
            propagate_deletions,
            rollback,
            resolutions,
        }) => {
            if rollback {
                branch_cmd::handle_rollback(&path, &branch)?;
            } else {
                branch_cmd::handle_merge(&path, &branch, force, propagate_deletions, &resolutions)
                    .await?;
            }
        }
        Some(Commands::Status { path }) => {
            // Stream B — try the daemon first per cortex protocol;
            // fall back to in-process when --in-process is set or no
            // daemon is running. The daemon path POSTs the workspace
            // mount + GETs its source list (with content hashes); the
            // CLI walks the filesystem locally for the diff.
            if let Some(conn) = try_resolve_remote(in_process_flag).await {
                cortex_remote::run_status_remote(&conn, &path).await?;
            } else {
                branch_cmd::handle_status(&path).await?;
            }
        }
        Some(Commands::Snapshot { name, path }) => {
            branch_cmd::handle_snapshot(&path, &name).await?;
        }
        Some(Commands::BranchImport {
            pack,
            branch,
            path,
            no_verify,
        }) => {
            mount_cmd::run_import_as_branch(&pack, &path, &branch, no_verify).await?;
        }
        Some(Commands::Provider { action }) => match action {
            None => {
                provider_cmd::run_provider_list(Path::new(".")).await?;
            }
            Some(ProviderAction::List { path }) => {
                provider_cmd::run_provider_list(&path).await?;
            }
            Some(ProviderAction::Status { path }) => {
                provider_cmd::run_provider_status(&path).await?;
            }
            Some(ProviderAction::Use {
                name,
                model,
                key,
                base_url,
                local,
                path,
                no_validate,
                azure_resource,
                azure_deployment,
                azure_api_version,
            }) => {
                provider_cmd::run_provider_use(
                    &name,
                    model.as_deref(),
                    key.as_deref(),
                    base_url.as_deref(),
                    local,
                    &path,
                    no_validate,
                    azure_resource.as_deref(),
                    azure_deployment.as_deref(),
                    azure_api_version.as_deref(),
                )
                .await?;
            }
            Some(ProviderAction::SetModel { model, local, path }) => {
                provider_cmd::run_provider_set_model(&model, local, &path)?;
            }
        },
        Some(Commands::Update) => {
            update_cmd::run_update().await?;
        }
        Some(Commands::Eval {
            dataset,
            path,
            limit,
            category,
            judge_deployment,
            rooting_mode,
            no_leak,
        }) => {
            eval_cmd::run_eval(
                &dataset,
                &path,
                limit,
                category.as_deref(),
                judge_deployment.as_deref(),
                rooting_mode.as_deref(),
                no_leak,
            )
            .await?;
        }
        Some(Commands::Reflect { path, json }) => {
            // Stream B — both --json and the terminal pretty-print
            // path go through `try_resolve_remote` first.  The cortex
            // protocol single-writer rule applies regardless of
            // output format; only the rendering differs.  When no
            // daemon is running (CI without a sidecar, --in-process),
            // fall back to the local pipeline.
            if let Some(conn) = try_resolve_remote(in_process_flag).await {
                cortex_remote::run_reflect_remote(&conn, &path).await?;
            } else {
                reflect_cmd::run(&path, json.as_ref())?;
            }
        }
        Some(Commands::Render { path }) => {
            if let Some(conn) = try_resolve_remote(in_process_flag).await {
                cortex_remote::run_render_remote(&conn, &path).await?;
            } else {
                render_cmd::run(&path)?;
            }
        }
        Some(Commands::Pack {
            workspace,
            out,
            name,
            version,
            license,
            description,
            sign,
            sign_keyless,
            branch,
        }) => {
            pack_cmd::run_pack(
                &workspace,
                out,
                name,
                version,
                license,
                description,
                sign.as_deref(),
                sign_keyless,
                branch.as_deref(),
            )?;
        }
        Some(Commands::Verify {
            pack,
            allow_unsigned,
            no_revocation_check,
            registry,
        }) => {
            let exit_code =
                pack_cmd::run_verify(&pack, allow_unsigned, !no_revocation_check, registry)?;
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
        }
        Some(Commands::Install {
            reference,
            target,
            registry,
            allow_unsigned,
            dry_run,
        }) => {
            if dry_run {
                pack_cmd::run_install_dry_run(&reference, registry).await?;
            } else if let Err(e) =
                pack_cmd::run_install(&reference, target, registry, allow_unsigned).await
            {
                if let Some(refused) = e.downcast_ref::<pack_cmd::InstallRefused>() {
                    eprintln!("{}", refused.message);
                    std::process::exit(refused.exit_code);
                }
                return Err(e);
            }
        }
        Some(Commands::Mount {
            pack,
            name,
            no_verify,
            recompile,
        }) => {
            mount_cmd::run_mount(pack, name, no_verify, recompile).await?;
        }
        Some(Commands::Login { token, server, no_browser }) => {
            cloud::login::run(token, server, no_browser).await?;
        }
        Some(Commands::Logout) => {
            cloud::logout::run().await?;
        }
        Some(Commands::Whoami { server }) => {
            cloud::whoami::run(server).await?;
        }
        Some(Commands::PackInit {
            slug,
            owner,
            server,
        }) => {
            cloud::init::run(slug, owner, server).await?;
        }
        Some(Commands::Publish {
            path,
            no_wait,
            timeout,
            server,
            visibility,
        }) => {
            cloud::publish::run(path, !no_wait, timeout, server, visibility).await?;
        }
        Some(Commands::Push {
            path,
            no_wait,
            server,
            timeout,
            visibility,
        }) => {
            cloud::push::run(path, !no_wait, timeout, server, visibility).await?;
        }
        Some(Commands::Pull { pack_ref, target }) => {
            cloud::pull::run(pack_ref, target).await?;
        }
        Some(Commands::Clone { pack_ref, target }) => {
            cloud::pull::run(pack_ref, Some(target)).await?;
        }
        Some(Commands::Jobs { limit, server }) => {
            cloud::status::run(limit, server).await?;
        }
        Some(Commands::Proposal { action }) => {
            let conn = require_remote(in_process_flag).await?;
            match action {
                ProposalAction::Open {
                    branch,
                    target,
                    description,
                    min_reviewers,
                } => {
                    proposal_cmd::run_open(
                        &conn,
                        &branch,
                        &target,
                        description,
                        min_reviewers,
                    )
                    .await?;
                }
                ProposalAction::List { branch } => {
                    proposal_cmd::run_list(&conn, branch.as_deref()).await?;
                }
                ProposalAction::Review {
                    id,
                    approve,
                    request_changes,
                    comment,
                    note,
                } => {
                    let decision = if approve {
                        "approve"
                    } else if request_changes {
                        "request_changes"
                    } else if comment {
                        "comment"
                    } else {
                        anyhow::bail!(
                            "review requires --approve, --request-changes, or --comment"
                        );
                    };
                    proposal_cmd::run_review(&conn, &id, decision, note).await?;
                }
                ProposalAction::Close { id } => {
                    proposal_cmd::run_close(&conn, &id).await?;
                }
            }
        }
        Some(Commands::Tag { action }) => {
            let conn = require_remote(in_process_flag).await?;
            match action {
                TagAction::Create {
                    name,
                    branch,
                    message,
                } => {
                    tag_cmd::run_create(&conn, &name, &branch, message).await?;
                }
                TagAction::List => tag_cmd::run_list(&conn).await?,
                TagAction::Get { name } => tag_cmd::run_get(&conn, &name).await?,
            }
        }
        Some(Commands::BranchOp { action }) => {
            let conn = require_remote(in_process_flag).await?;
            match action {
                BranchOpAction::Events { branch } => {
                    branch_extras_cmd::run_events(&conn, &branch).await?;
                }
                BranchOpAction::Stats { branch } => {
                    branch_extras_cmd::run_stats(&conn, &branch).await?;
                }
                BranchOpAction::Lineage => {
                    branch_extras_cmd::run_lineage(&conn).await?;
                }
                BranchOpAction::Rebase { branch } => {
                    branch_extras_cmd::run_rebase(&conn, &branch).await?;
                }
                BranchOpAction::Rollback { branch } => {
                    branch_extras_cmd::run_rollback(&conn, &branch).await?;
                }
                BranchOpAction::ContributeBulk {
                    branch,
                    path,
                    connector_id,
                    install_id,
                    idempotency_key,
                    file,
                } => {
                    branch_data_cmd::run_contribute_bulk(
                        &conn,
                        &path,
                        &branch,
                        &connector_id,
                        &install_id,
                        &idempotency_key,
                        &file,
                    )
                    .await?;
                }
                BranchOpAction::RedactionSet { branch, file, clear } => {
                    branch_data_cmd::run_redaction_set(
                        &conn,
                        &branch,
                        file.as_deref(),
                        clear,
                    )
                    .await?;
                }
            }
        }
        Some(Commands::Brief { path, branch, json }) => {
            let conn = require_remote(in_process_flag).await?;
            brain_cmd::run_brief(&conn, &path, branch.as_deref(), json).await?;
        }
        Some(Commands::Brain { action }) => match action {
            BrainAction::Pull { path, branch } => {
                let conn = require_remote(in_process_flag).await?;
                brain_code::run_pull(&conn, &path, branch.as_deref()).await?;
            }
            BrainAction::Push { path, branch } => {
                let conn = require_remote(in_process_flag).await?;
                brain_code::run_push(&conn, &path, branch.as_deref()).await?;
            }
        },
        Some(Commands::Investigate {
            entity,
            path,
            branch,
            json,
        }) => {
            let conn = require_remote(in_process_flag).await?;
            brain_cmd::run_investigate(&conn, &path, &entity, branch.as_deref(), json).await?;
        }
        Some(Commands::Retrieve {
            query,
            path,
            top_k,
            branch,
            profile,
            json,
        }) => {
            let conn = require_remote(in_process_flag).await?;
            retrieve_cmd::run_retrieve(
                &conn,
                &path,
                &query,
                top_k,
                branch.as_deref(),
                profile.as_deref(),
                json,
            )
            .await?;
        }
        Some(Commands::Claims {
            path,
            as_of,
            rooted,
            branch,
            r#type,
            entity,
            min_confidence,
            limit,
            offset,
            json,
        }) => {
            let conn = require_remote(in_process_flag).await?;
            claims_cmd::run(
                &conn,
                &path,
                as_of.as_deref(),
                rooted,
                branch.as_deref(),
                r#type.as_deref(),
                entity.as_deref(),
                min_confidence,
                limit,
                offset,
                json,
            )
            .await?;
        }
        Some(Commands::BranchTemplate { action }) => {
            let conn = require_remote(in_process_flag).await?;
            match action {
                BranchTemplateAction::List => {
                    branch_template_cmd::run_list(&conn).await?;
                }
                BranchTemplateAction::Get { name } => {
                    branch_template_cmd::run_get(&conn, &name).await?;
                }
                BranchTemplateAction::Upsert { file } => {
                    branch_template_cmd::run_upsert(&conn, &file).await?;
                }
                BranchTemplateAction::Delete { name } => {
                    branch_template_cmd::run_delete(&conn, &name).await?;
                }
                BranchTemplateAction::Apply {
                    template,
                    branch,
                    description,
                } => {
                    branch_template_cmd::run_apply(&conn, &template, &branch, description).await?;
                }
            }
        }
        Some(Commands::Engram { action }) => {
            let conn = require_remote(in_process_flag).await?;
            match action {
                EngramAction::Materialize {
                    topic,
                    path,
                    seeds,
                    scope,
                    session,
                } => {
                    engram_cmd::run_materialize(
                        &conn, &path, &topic, seeds, scope, session,
                    )
                    .await?;
                }
                EngramAction::List { path, session } => {
                    engram_cmd::run_list(&conn, &path, session).await?;
                }
                EngramAction::Probe {
                    pointer,
                    question,
                    path,
                    clearance,
                    probe_kind,
                    score_with_hybrid,
                    session,
                } => {
                    engram_cmd::run_probe(
                        &conn,
                        &path,
                        &pointer,
                        &question,
                        clearance,
                        probe_kind,
                        score_with_hybrid,
                        session,
                    )
                    .await?;
                }
                EngramAction::Expire {
                    pointer,
                    path,
                    session,
                } => {
                    engram_cmd::run_expire(&conn, &path, &pointer, session).await?;
                }
            }
        }
        // Phase E.5 (2026-05-17) — external MCP server management.
        // Purely config-file manipulation: writes to
        // `<workspace>/.thinkingroot/mcp-servers.toml`. The daemon
        // picks up the new entries on next workspace mount.
        Some(Commands::Mcp { action }) => match action {
            McpAction::Add {
                name,
                workspace,
                command_and_args,
            } => {
                mcp_cmd::add(&name, &workspace, &command_and_args)?;
            }
            McpAction::List { workspace } => {
                mcp_cmd::list(&workspace)?;
            }
            McpAction::Remove { name, workspace } => {
                mcp_cmd::remove(&name, &workspace)?;
            }
        },
        Some(Commands::Function { action }) => match action {
            FunctionAction::Deploy { name, code, workspace, url, api_key } => {
                function_cmd::deploy(&name, &code, &workspace, &url, api_key.as_deref()).await?;
            }
            FunctionAction::List { workspace, url, api_key } => {
                function_cmd::list(&workspace, &url, api_key.as_deref()).await?;
            }
            FunctionAction::Invoke { name, input, workspace, url, api_key } => {
                function_cmd::invoke(&name, &input, &workspace, &url, api_key.as_deref()).await?;
            }
        },
        Some(Commands::Secrets { action }) => match action {
            SecretsAction::Set { name, value } => secrets_cmd::set(&name, value.as_deref())?,
            SecretsAction::List => secrets_cmd::list()?,
            SecretsAction::Unset { name } => secrets_cmd::unset(&name)?,
        },
        Some(Commands::Prompt { action }) => match action {
            PromptAction::Edit { name, file, workspace, url, api_key } => {
                prompt_cmd::edit(&name, file.as_deref(), &workspace, &url, api_key.as_deref()).await?;
            }
            PromptAction::List { workspace, url, api_key } => {
                prompt_cmd::list(&workspace, &url, api_key.as_deref()).await?;
            }
            PromptAction::Version { name, workspace, url, api_key } => {
                prompt_cmd::version(&name, &workspace, &url, api_key.as_deref()).await?;
            }
        },
        None => {
            // `root ./path` shorthand — same as `root compile ./path`.
            let path = cli.path.unwrap_or_else(|| PathBuf::from("."));
            if let Some(conn) = try_resolve_remote(in_process_flag).await {
                cortex_remote::run_compile_remote(&conn, &path, None, false, false).await?;
            } else {
                run_compile(&path, None, use_progress, false, false, false).await?;
            }
        }
    }

    Ok(())
}

async fn run_compile(
    path: &PathBuf,
    branch: Option<&str>,
    use_progress: bool,
    no_rooting: bool,
    json: bool,
    no_incremental: bool,
) -> anyhow::Result<()> {
    // C2: When --json is set, suppress TTY progress bars so the JSON line
    // is the sole stdout output. Pair with `2>/dev/null` to silence stderr
    // entirely if you need a clean pipe.
    let use_progress = use_progress && !json;
    if !path.exists() {
        let name = path.display().to_string();
        anyhow::bail!(
            "Unknown command or path not found: '{}'\n\nRun 'root --help' to see available commands.",
            style(name).yellow().bold()
        );
    }
    let path = std::fs::canonicalize(path)
        .with_context(|| format!("failed to canonicalize path: {}", path.display()))?;

    print_banner();
    println!(
        "  {} {}\n",
        style("Compiling").cyan().bold(),
        style(path.display()).white()
    );

    let start = Instant::now();

    // M3: `--no-rooting` is plumbed through PipelineOptions instead of
    // an `unsafe { std::env::set_var(...) }`. The legacy
    // `TR_ROOTING_DISABLED=1` env-var fallback is gone — no code reads
    // it (race hazard with concurrent thread reads). External scripts
    // must pass `--no-rooting` on the CLI invocation.
    let result = if use_progress {
        progress::run_compile_progress(&path, branch, no_rooting).await?
    } else {
        pipeline::run_pipeline_with_options(
            &path,
            branch,
            None,
            pipeline::PipelineOptions {
                no_rooting,
                no_incremental,
                ..Default::default()
            },
        )
        .await?
    };

    let elapsed = start.elapsed();
    // In TTY mode the progress bars write to stderr (indicatif default).
    // Using eprintln! here keeps the summary on the same stream so it
    // appears in correct order after the bars, not interleaved with them.
    let out = |s: String| {
        if use_progress {
            eprintln!("{s}");
        } else {
            println!("{s}");
        }
    };

    if json {
        let line = serde_json::to_string(&result.incremental_summary)
            .context("failed to serialize IncrementalSummary as JSON")?;
        println!("{line}");
    } else {
        out(String::new());
        out(format!(
            "  {} compiled {} files in {:.1}s",
            style("ThinkingRoot").green().bold(),
            style(result.files_parsed).white().bold(),
            elapsed.as_secs_f64()
        ));
        out(format!(
            "  {} {}%",
            style("Knowledge Health:").white().bold(),
            style(result.health_score).green().bold()
        ));
        out(format!(
            "  {} {} claims extracted",
            style("  ├──").dim(),
            style(result.claims_count).cyan()
        ));
        out(format!(
            "  {} {} entities identified",
            style("  ├──").dim(),
            style(result.entities_count).cyan()
        ));
        out(format!(
            "  {} {} relations mapped",
            style("  ├──").dim(),
            style(result.relations_count).cyan()
        ));
        out(format!(
            "  {} {} contradictions found",
            style("  ├──").dim(),
            style(result.contradictions_count).yellow()
        ));
        out(format!(
            "  {} {} artifacts generated",
            style("  └──").dim(),
            style(result.artifacts_count).cyan()
        ));
        if result.cache_hits > 0 {
            out(format!(
                "  {} {} extraction cache hits",
                style("  ├──").dim(),
                style(result.cache_hits).green()
            ));
        }
        if result.early_cutoffs > 0 {
            out(format!(
                "  {} {} sources unchanged (early cutoff)",
                style("  └──").dim(),
                style(result.early_cutoffs).green()
            ));
        }
        if result.failed_batches > 0 {
            // Pre-fix these failures were silently dropped — the user only
            // saw "ok" while their compile was incomplete.  Always print
            // unconditionally (regardless of TTY filter) so users notice.
            let ranges = result
                .failed_chunk_ranges
                .iter()
                .map(|(s, e)| format!("{s}–{e}"))
                .collect::<Vec<_>>()
                .join(", ");
            out(format!(
                "  {} {} batch{} failed permanently (chunks {}) — claims for those chunks are missing; re-run to retry",
                style("  ⚠").yellow().bold(),
                style(result.failed_batches).yellow().bold(),
                if result.failed_batches == 1 { "" } else { "es" },
                style(ranges).yellow()
            ));
        }
        out(String::new());
        summary_printer::print(&result.incremental_summary, use_progress);
    }

    Ok(())
}

async fn run_health(path: &PathBuf) -> anyhow::Result<()> {
    let path = std::fs::canonicalize(path)
        .with_context(|| format!("path not found: {}", path.display()))?;
    let data_dir = path.join(".thinkingroot");

    if !data_dir.exists() {
        anyhow::bail!(
            "No ThinkingRoot data found at {}. Run `root compile {}` first.",
            data_dir.display(),
            path.display()
        );
    }

    let config = thinkingroot_core::Config::load_merged(&path)?;
    let storage = thinkingroot_graph::StorageEngine::init(&data_dir)
        .await
        .context("failed to open storage")?;
    let verifier = thinkingroot_health::Verifier::new(&config);
    let result = verifier.verify(&storage.graph)?;

    print_banner();
    println!(
        "  {} {}%\n",
        style("Knowledge Health:").white().bold(),
        style(result.health_score.as_percentage()).green().bold()
    );

    if !result.warnings.is_empty() {
        println!("  {}", style("Warnings:").yellow().bold());
        for w in &result.warnings {
            println!("    {} {}", style("!").yellow(), w);
        }
    }

    Ok(())
}

/// Lazy-build the vector index from the persisted graph if it's
/// empty. v3 `root compile` does not embed; the index is materialised
/// on first `root query` / `root ask` and reused thereafter (per v3
/// final plan §13.1 — consumers choose their own embedding model).
///
/// **Race semantics.** Two concurrent `root ask` invocations on the
/// same workspace previously each observed the empty index, both
/// embedded the entire claim set, and the second writer overwrote
/// the first.  The second-writer cost is non-trivial — embedding a
/// 100k-claim graph can take 5+ minutes — and the racy double-write
/// can corrupt the index if two threads ever interleave RocksDB
/// batches mid-commit.
///
/// The fix is a cross-process advisory lock at
/// `<data_dir>/.thinkingroot/.vector_index.lock`.  Pattern:
///   1. Open the lockfile (creating it if absent).
///   2. Take an exclusive `fs2` lock — blocks until any concurrent
///      builder finishes and releases.
///   3. Re-check `is_empty()` AFTER acquiring the lock (the
///      double-checked-locking pattern: another process may have
///      finished while we were waiting, so we'd skip the rebuild
///      entirely).
///   4. Build under the held lock.
///   5. Release on `Drop` (RAII).  fs2 also auto-releases on process
///      death so a crashed builder doesn't leave a stale lock.
fn ensure_vector_index(
    storage: &mut thinkingroot_graph::StorageEngine,
    path: &Path,
) -> anyhow::Result<()> {
    use fs2::FileExt;

    // Fast path: index already populated, no lock acquisition needed.
    // Reading `is_empty()` is cheap and lock-free — only contend on
    // the lockfile when we'd actually rebuild.
    if !storage.vector.is_empty() {
        return Ok(());
    }

    let data_dir = path.join(".thinkingroot");
    // The lockfile lives under .thinkingroot/ (already created by
    // `root compile`) so `root ask` doesn't need to mkdir it.
    let lock_path = data_dir.join(".vector_index.lock");
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("failed to open vector-index lockfile at {}", lock_path.display()))?;

    // `lock_exclusive` blocks until acquired (no busy-wait).  fs2
    // delegates to flock/LockFileEx; the OS releases on process
    // exit even if the process panics or is SIGKILL'd, so a stale
    // lockfile can never deadlock a future `root ask`.
    #[allow(clippy::incompatible_msrv)]
    lock_file
        .lock_exclusive()
        .with_context(|| format!("failed to acquire vector-index lock at {}", lock_path.display()))?;

    // RAII guard releases the OS lock on scope exit (success OR
    // panic).  The file itself is left on disk so subsequent
    // invocations can re-lock it without re-creating; the file is
    // 0 bytes so retention is harmless.
    struct LockGuard(std::fs::File);
    impl Drop for LockGuard {
        fn drop(&mut self) {
            #[allow(clippy::incompatible_msrv)]
            let _ = self.0.unlock();
        }
    }
    let _guard = LockGuard(lock_file);

    // Double-checked locking: another process may have built the
    // index while we waited.  Reload the vector store's emptiness
    // signal — fastembed/cozo persist the rows on disk, so the
    // freshly-acquired storage handle sees the post-build state on
    // its next read.
    if !storage.vector.is_empty() {
        return Ok(());
    }

    let claim_count = storage.graph.get_all_claims_with_sources()?.len();
    if claim_count == 0 {
        anyhow::bail!(
            "No claims in the compiled graph. Run `root compile {}` first.",
            path.display()
        );
    }
    println!(
        "  {} building search index from {} claims (one-time, ~10s for 1k claims)…",
        style("Indexing:").cyan().bold(),
        style(claim_count).white()
    );
    let (entities, claims) = thinkingroot_serve::pipeline::rebuild_vector_index(storage)?;
    println!(
        "  {} {} entities + {} claims indexed",
        style("Indexed:").green().bold(),
        entities,
        claims
    );
    Ok(())
}

async fn run_query(path: &PathBuf, query: &str, top_k: usize) -> anyhow::Result<()> {
    let path = std::fs::canonicalize(path)
        .with_context(|| format!("path not found: {}", path.display()))?;
    let data_dir = path.join(".thinkingroot");

    if !data_dir.exists() {
        anyhow::bail!(
            "No ThinkingRoot data found. Run `root compile {}` first.",
            path.display()
        );
    }

    let mut storage = thinkingroot_graph::StorageEngine::init(&data_dir)
        .await
        .context("failed to open storage")?;

    ensure_vector_index(&mut storage, &path)?;

    println!();
    println!(
        "  {} \"{}\"",
        style("Searching:").cyan().bold(),
        style(query).white()
    );
    println!();

    let results = storage.vector.search(query, top_k)?;

    if results.is_empty() {
        println!("  {} No results found.", style("!").yellow());
        return Ok(());
    }

    for (i, (_id, metadata, score)) in results.iter().enumerate() {
        if *score < 0.1 {
            break; // Skip very low relevance results.
        }

        let parts: Vec<&str> = metadata.splitn(5, '|').collect();
        match parts.first() {
            Some(&"entity") if parts.len() >= 4 => {
                let name = parts[2];
                let etype = parts[3];
                println!(
                    "  {} {} {} ({})",
                    style(format!("{}.", i + 1)).dim(),
                    style("Entity:").green().bold(),
                    style(name).white().bold(),
                    style(etype).dim()
                );
                // Show claims for this entity.
                let entity_id = parts[1];
                if let Ok(claims) = storage.graph.get_claims_with_sources_for_entity(entity_id) {
                    for (_, stmt, _, uri, conf) in claims.iter().take(3) {
                        println!(
                            "      {} {} {} [{}]",
                            style("·").dim(),
                            stmt,
                            style(format!("({:.0}%)", conf * 100.0)).dim(),
                            style(uri).dim()
                        );
                    }
                }
                println!("      {} {:.0}%", style("relevance:").dim(), score * 100.0);
                println!();
            }
            Some(&"claim") if parts.len() >= 5 => {
                let ctype = parts[2];
                let uri = parts[4];
                // The statement isn't in metadata — use the ID to look it up or
                // show what we have.
                println!(
                    "  {} {} [{}] [{}]",
                    style(format!("{}.", i + 1)).dim(),
                    style(format!("Claim ({ctype}):")).blue().bold(),
                    style(uri).dim(),
                    style(format!("{:.0}% relevance", score * 100.0)).dim(),
                );
                // Get the actual claim statement from graph.
                let claim_id = parts[1];
                if let Ok(claims) = storage.graph.get_claims_for_entity(claim_id) {
                    for (_, stmt, _) in claims.iter().take(1) {
                        println!("      {} {}", style("·").dim(), stmt);
                    }
                }
                println!();
            }
            _ => {
                println!(
                    "  {} {} (relevance: {:.0}%)",
                    style(format!("{}.", i + 1)).dim(),
                    metadata,
                    score * 100.0
                );
                println!();
            }
        }
    }

    Ok(())
}

/// Full hybrid intelligence pipeline — same 91.2%-accuracy path as POST /ask.
/// Multi-pass scoped retrieval + transcript loading + temporal anchors + LLM synthesis.
/// Temporal anchors are always computed (uses today's date when --date is not supplied).
async fn run_query_llm(path: &PathBuf, query: &str, date: Option<&str>) -> anyhow::Result<()> {
    use std::collections::{HashMap, HashSet};
    use thinkingroot_core::Config;
    use thinkingroot_llm::llm::LlmClient;
    use thinkingroot_serve::engine::QueryEngine;
    use thinkingroot_serve::intelligence::router::{QueryPath, classify_query};
    use thinkingroot_serve::intelligence::session::SessionContext;
    use thinkingroot_serve::intelligence::synthesizer::{AskRequest, ask};

    let path = std::fs::canonicalize(path)
        .with_context(|| format!("path not found: {}", path.display()))?;
    let data_dir = path.join(".thinkingroot");

    if !data_dir.exists() {
        anyhow::bail!(
            "No ThinkingRoot data found. Run `root compile {}` first.",
            path.display()
        );
    }

    // Lazy-build the vector index BEFORE mounting QueryEngine — once
    // QueryEngine takes ownership of storage behind a Mutex, rebuilding
    // is harder to drive from the CLI. v3 packs deliberately don't ship
    // an in-pack vector index (final plan §13.1).
    {
        let mut storage = thinkingroot_graph::StorageEngine::init(&data_dir)
            .await
            .context("failed to open storage")?;
        ensure_vector_index(&mut storage, &path)?;
    }

    println!();
    println!(
        "  {} \"{}\"",
        style("Thinking:").cyan().bold(),
        style(query).white()
    );

    // Mount engine
    let mut engine = QueryEngine::new();
    engine
        .mount("default".to_string(), path.clone())
        .await
        .context("failed to mount workspace")?;

    // Load LLM
    let config = Config::load_merged(&path).unwrap_or_default();
    let llm = match LlmClient::new(&config.llm).await {
        Ok(c) => {
            println!(
                "  {} {} / {}",
                style("LLM:").dim(),
                config.llm.default_provider,
                config.llm.extraction_model
            );
            Some(std::sync::Arc::new(c))
        }
        Err(e) => {
            println!(
                "  {} LLM unavailable ({}), using best claim fallback",
                style("Warning:").yellow(),
                e
            );
            None
        }
    };

    // Auto-detect category from query
    let tmp_session = SessionContext::new("cli", "default");
    let category = match classify_query(query, &tmp_session) {
        QueryPath::Agentic => {
            let q = query.to_lowercase();
            if q.contains(" ago")
                || q.contains("last ")
                || q.contains("when ")
                || q.contains("how many days")
                || q.contains("how many weeks")
                || q.contains("how many months")
                || q.contains("what day")
                || q.contains("what date")
                || q.contains("yesterday")
            {
                "temporal-reasoning"
            } else if q.contains("prefer")
                || q.contains("recommend")
                || q.contains("favourite")
                || q.contains("favorite")
                || q.contains("gift")
                || q.contains("enjoy")
            {
                "single-session-preference"
            } else {
                "multi-session"
            }
        }
        QueryPath::Fast => "single-session-user",
    };

    // Always provide a date for temporal anchoring.
    // Use --date if supplied, otherwise today's local date (YYYY/MM/DD format).
    let today_str;
    let question_date = match date {
        Some(d) => d,
        None => {
            let now = chrono::Local::now();
            today_str = now.format("%Y/%m/%d").to_string();
            &today_str
        }
    };

    let sessions_dir = path.join("sessions");

    // Workspace identity / persona for the `root ask` CLI surface — same
    // pattern as the HTTP /ask handler so the CLI and the desktop chat
    // get the same workspace-grounded answers. The engine has only one
    // workspace mounted here ("default"), so the snapshot is cheap.
    let snapshot = engine.workspace_chat_snapshot("default").await;
    let chat = snapshot
        .as_ref()
        .map(|s| s.config.chat.resolve(&s.source_kinds))
        .unwrap_or_else(AskRequest::default_chat);
    let identity_owned = snapshot.as_ref().map(|s| {
        thinkingroot_serve::intelligence::identity::build_workspace_identity(s, &s.config.chat)
    });
    let today_iso = chrono::Local::now().format("%Y-%m-%d").to_string();

    let req = AskRequest {
        workspace: "default",
        question: query,
        category,
        allowed_sources: &HashSet::new(),
        question_date,
        session_dates: &HashMap::new(),
        answer_sids: &[],
        sessions_dir: &sessions_dir,
        excluded_claim_ids: &HashSet::new(),
        chat,
        identity: identity_owned.as_ref(),
        today: Some(&today_iso),
        history: thinkingroot_serve::intelligence::synthesizer::NO_HISTORY,
        persona_override: None,
    };

    let spinner_msg = format!(
        "  {} Running hybrid retrieval [{}]...",
        style("·").dim(),
        style(category).cyan()
    );
    print!("{spinner_msg}");
    let _ = std::io::Write::flush(&mut std::io::stdout());

    let result = ask(&engine, llm, &req).await;

    // Clear spinner line
    print!("\r{}\r", " ".repeat(spinner_msg.len() + 4));

    println!();
    println!("  {}", style("Answer").green().bold());
    println!();
    for line in result.answer.lines() {
        println!("  {line}");
    }
    println!();
    println!(
        "  {} claims · {} · date ref: {}",
        style(result.claims_used).dim(),
        style(&result.category).dim(),
        style(question_date).dim(),
    );
    println!();

    Ok(())
}

fn run_migrate(
    path: &Path,
    to_completeness_contract: bool,
    to_water_flow: bool,
    to_witness_mesh: bool,
    dry_run: bool,
) -> anyhow::Result<()> {
    if !to_completeness_contract && !to_water_flow && !to_witness_mesh {
        anyhow::bail!(
            "no migration target specified. Use --to-completeness-contract for v2, \
             --to-water-flow for v3, or --to-witness-mesh for the Witness Mesh substrate."
        );
    }

    let data_dir = path.join(".thinkingroot");
    if !data_dir.exists() {
        anyhow::bail!(
            "no ThinkingRoot workspace at {} — run `root init` first",
            path.display()
        );
    }

    if to_witness_mesh {
        if dry_run {
            println!(
                "  {} [dry-run] scanning workspace at {} for Witness Mesh migration",
                style("→").cyan().bold(),
                path.display()
            );
            let report = thinkingroot_serve::backfill::backfill_witness_mesh_at_path(
                &data_dir, true,
            )
            .map_err(|e| anyhow::anyhow!("witness mesh dry-run failed: {e}"))?;
            println!(
                "  {} would migrate {} witness(es) from {} claim(s); {} claim(s) skipped (missing byte anchor)",
                style("→").cyan().bold(),
                report.witnesses_emitted,
                report.claims_scanned,
                report.claims_missing_anchor,
            );
            if let Some(v) = &report.schema_version_before {
                println!(
                    "  {} current witness_schema_version: {v}",
                    style("→").cyan().bold()
                );
            }
            return Ok(());
        }
        println!(
            "  {} migrating workspace at {} to Witness Mesh substrate (witness_schema_version v2)",
            style("→").cyan().bold(),
            path.display()
        );
        let report = thinkingroot_serve::backfill::backfill_witness_mesh_at_path(
            &data_dir, false,
        )
        .map_err(|e| anyhow::anyhow!("witness mesh migration failed: {e}"))?;
        println!(
            "  {} migrated {} witness(es) from {} claim(s); {} claim(s) skipped (missing byte anchor).",
            style("✓").green().bold(),
            report.witnesses_emitted,
            report.claims_scanned,
            report.claims_missing_anchor,
        );
        println!(
            "  {} workspace is now on witness_schema_version=2.",
            style("✓").green().bold()
        );
        return Ok(());
    }

    if to_water_flow {
        if dry_run {
            let store = thinkingroot_graph::graph::GraphStore::init(&data_dir)
                .map_err(|e| anyhow::anyhow!("failed to open workspace: {e}"))?;
            let orphans = store
                .query_orphan_structural_rows()
                .map_err(|e| anyhow::anyhow!("orphan query failed: {e}"))?;
            let total: usize = orphans.iter().map(|(_, _, n)| *n).sum();
            println!(
                "  {} [dry-run] would purge {total} orphan structural rows across {} group(s)",
                style("→").cyan().bold(),
                orphans.len()
            );
            for (table, sid, count) in orphans.iter().take(10) {
                println!("    {table:30} source_id={sid:40} {count} rows");
            }
            if orphans.len() > 10 {
                println!("    … and {} more group(s) not shown", orphans.len() - 10);
            }
            return Ok(());
        }

        println!(
            "  {} migrating workspace at {} to water-flow incremental schema (v3)",
            style("→").cyan().bold(),
            path.display()
        );
        thinkingroot_serve::backfill::backfill_water_flow_v3_at_path(&data_dir)
            .map_err(|e| anyhow::anyhow!("water-flow migration failed: {e}"))?;
        println!(
            "  {} migration complete: workspace is on water-flow schema (v3).",
            style("✓").green().bold()
        );
        return Ok(());
    }

    // to_completeness_contract path (v2).
    println!(
        "  {} migrating workspace at {} to Compile Completeness Contract (v2)",
        style("→").cyan().bold(),
        path.display()
    );

    let report = thinkingroot_serve::backfill::backfill_structural(&data_dir)
        .map_err(|e| anyhow::anyhow!("backfill failed: {e}"))?;

    println!(
        "  {} {} sources backfilled, {} skipped (already migrated), {} missing bytes, {} re-parse failures",
        style("✓").green().bold(),
        report.sources_backfilled,
        report.sources_skipped,
        report.sources_missing_bytes,
        report.sources_parse_failed,
    );
    println!(
        "  {} {} structural rows emitted ({} chunks_residual fall-through)",
        style("✓").green().bold(),
        report.rows_emitted,
        report.residual_emitted,
    );
    if report.orphan_bytes_after > 0 {
        println!(
            "  {} {} orphan bytes remain on legacy data — re-compile affected sources to clear",
            style("!").yellow().bold(),
            report.orphan_bytes_after,
        );
    } else {
        println!(
            "  {} byte-coverage audit clean (zero orphans)",
            style("✓").green().bold()
        );
    }
    println!(
        "  {} compile_schema_version = {}",
        style("✓").green().bold(),
        report.schema_version_after,
    );
    Ok(())
}

/// Default `.rootignore` written into a fresh workspace by `root init`.
/// Precedence (`.rootignore` > `.gitignore`) keeps secrets and personal
/// files out of compiled cognition independently of git tracking.
/// Mirrors `.dockerignore` / `.npmignore` semantics; honoured by
/// `thinkingroot_parse::walker::walk` via `add_custom_ignore_filename`.
const DEFAULT_ROOTIGNORE: &str = "\
# .rootignore — files ThinkingRoot will skip during compile.
# Same syntax as .gitignore. Loaded ahead of .gitignore so anything
# matched here is excluded even if git would otherwise track it.

# Secrets — never compile credentials into cognition
*.env
*.env.*
credentials*
*.key
*.pem
*.p12
id_rsa*

# Personal / private folders
personal/
private/
financial/

# Heavy binaries / archives (waste compile time, no useful extraction)
*.zip
*.tar.gz
*.iso
*.dmg
*.bin

# Build / cache / vendor
.cache/
node_modules/
target/
dist/
build/
venv/
.venv/
__pycache__/
";

fn run_init(path: &Path) -> anyhow::Result<()> {
    let data_dir = path.join(".thinkingroot");

    if data_dir.exists() {
        println!(
            "  {} already initialized at {}",
            style("ThinkingRoot").green().bold(),
            data_dir.display()
        );
        return Ok(());
    }

    // Only create the data directory — no local config.toml.
    // LLM settings are inherited from the global config (~/.config/thinkingroot/config.toml).
    // Users who need per-workspace overrides can create .thinkingroot/config.toml manually.
    std::fs::create_dir_all(&data_dir)
        .map_err(|e| anyhow::anyhow!("could not create {}: {e}", data_dir.display()))?;

    // Write a sensible-default `.rootignore` so a fresh workspace
    // immediately keeps secrets, personal files, and build artefacts
    // out of the compiled cognition. The walker honours this on the
    // very next `root compile`. Skipped silently if the user already
    // dropped their own .rootignore into the workspace.
    let rootignore_path = path.join(".rootignore");
    if !rootignore_path.exists() {
        if let Err(e) = std::fs::write(&rootignore_path, DEFAULT_ROOTIGNORE) {
            // Non-fatal: workspace still works, just without the default
            // privacy guard. Warn loudly so the user can spot it.
            eprintln!(
                "  {} could not write {}: {e}",
                style("warning:").yellow().bold(),
                rootignore_path.display()
            );
        }
    }

    println!(
        "  {} initialized at {}",
        style("ThinkingRoot").green().bold(),
        data_dir.display()
    );

    let global_exists = thinkingroot_core::GlobalConfig::path()
        .map(|p| p.exists())
        .unwrap_or(false);
    if !global_exists {
        println!(
            "  {} No global config found — run {} first to configure your LLM provider.",
            style("Note:").yellow().bold(),
            style("root setup").cyan()
        );
    }

    println!(
        "  Run `root compile {}` to compile your knowledge.",
        path.display()
    );

    Ok(())
}

/// `root watch <path>` — standalone watch subcommand.
///
/// Watches a directory for changes and runs incremental compilation.
/// Respects `.gitignore` and `exclude_patterns` from config.  This
/// function lives in `main.rs` (not `watch.rs`) so it can call
/// `pipeline` without exposing it through the lib crate.
async fn run_watch_standalone(root_path: &std::path::Path) -> anyhow::Result<()> {
    use std::time::Instant;
    use notify::RecursiveMode;
    use notify_debouncer_mini::{DebouncedEventKind, DebounceEventResult, new_debouncer};
    use thinkingroot_core::config::Config;

    let config = Config::load_merged(root_path)?;

    let mut builder = ignore::gitignore::GitignoreBuilder::new(root_path);
    if config.parsers.respect_gitignore {
        let gitignore_path = root_path.join(".gitignore");
        if gitignore_path.exists() {
            let _ = builder.add(&gitignore_path);
        }
    }
    for pattern in &config.parsers.exclude_patterns {
        let _ = builder.add_line(None, pattern);
    }
    let gitignore = builder.build().unwrap_or_else(|_| {
        ignore::gitignore::GitignoreBuilder::new(root_path)
            .build()
            .expect("empty gitignore builder must succeed")
    });

    const ALWAYS_IGNORE: &[&str] = &[
        ".thinkingroot", ".git", "target", "node_modules",
        ".next", "dist", "build", "__pycache__", ".tox", ".venv",
    ];
    let root_canon = root_path.canonicalize().unwrap_or_else(|_| root_path.to_path_buf());
    let should_ignore = move |path: &std::path::Path| -> bool {
        for component in path.components() {
            if ALWAYS_IGNORE.iter().any(|&b| component.as_os_str() == b) {
                return true;
            }
        }
        match path.canonicalize() {
            Ok(canon) if canon.starts_with(&root_canon) => {}
            _ => return true,
        }
        gitignore.matched_path_or_any_parents(path, path.is_dir()).is_ignore()
    };

    println!(
        "\n  {} watching {} for changes (Ctrl+C to stop)\n",
        style("ThinkingRoot").green().bold(),
        style(root_path.display()).white()
    );

    println!("  {} initial compile...", style(">>").cyan().bold());
    let start = Instant::now();
    match pipeline::run_pipeline(root_path, None, None).await {
        Ok(result) => {
            println!(
                "  {} compiled {} files in {:.1}s (health: {}%)\n",
                style("OK").green().bold(),
                result.files_parsed,
                start.elapsed().as_secs_f64(),
                result.health_score,
            );
        }
        Err(e) => {
            println!("  {} {e}\n", style("ERR").red().bold());
        }
    }

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DebounceEventResult>();
    let mut debouncer = new_debouncer(
        std::time::Duration::from_millis(500),
        move |result: DebounceEventResult| {
            let _ = tx.send(result);
        },
    )?;
    debouncer.watcher().watch(root_path, RecursiveMode::Recursive)?;

    println!("  {} waiting for changes...\n", style("--").dim());

    loop {
        match rx.recv().await {
            Some(Ok(events)) => {
                let relevant: Vec<_> = events
                    .iter()
                    .filter(|e| e.kind == DebouncedEventKind::Any && !should_ignore(&e.path))
                    .collect();

                if relevant.is_empty() {
                    continue;
                }

                let changed_count = relevant.len();
                let sample = relevant
                    .first()
                    .map(|e| {
                        e.path
                            .strip_prefix(root_path)
                            .unwrap_or(&e.path)
                            .display()
                            .to_string()
                    })
                    .unwrap_or_default();
                let extra = if changed_count > 1 {
                    format!(" (+{} more)", changed_count - 1)
                } else {
                    String::new()
                };

                println!(
                    "  {} {}{}",
                    style(">>").cyan().bold(),
                    style(&sample).white(),
                    style(&extra).dim(),
                );

                let start = Instant::now();
                match pipeline::run_pipeline(root_path, None, None).await {
                    Ok(result) => {
                        println!(
                            "  {} {:.1}s | {} claims, {} entities, health {}%\n",
                            style("OK").green().bold(),
                            start.elapsed().as_secs_f64(),
                            result.claims_count,
                            result.entities_count,
                            result.health_score,
                        );
                    }
                    Err(e) => {
                        println!("  {} {e}\n", style("ERR").red().bold());
                    }
                }

                println!("  {} waiting for changes...\n", style("--").dim());
            }
            Some(Err(e)) => {
                eprintln!("  {} watch error: {e}", style("ERR").red().bold());
                tracing::warn!("watch error: {e:?}");
            }
            None => {
                break;
            }
        }
    }

    Ok(())
}

fn print_banner() {
    println!();
    println!("  {}", style("ThinkingRoot").green().bold());
    println!(
        "  {}",
        style("Compiled knowledge infrastructure for AI agents — works like a secondary brain.")
            .dim()
    );
    println!();
}
