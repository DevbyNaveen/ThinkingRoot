use std::path::PathBuf;

/// Central error type for the ThinkingRoot engine.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    // --- IO ---
    #[error("io error at {path:?}: {source}")]
    Io {
        path: Option<PathBuf>,
        source: std::io::Error,
    },

    // --- Parsing ---
    #[error("parse error in {source_path}: {message}")]
    Parse {
        source_path: PathBuf,
        message: String,
    },

    #[error("unsupported file type: {extension}")]
    UnsupportedFileType { extension: String },

    // --- Graph / Storage ---
    #[error("graph storage error: {0}")]
    GraphStorage(String),

    #[error("vector storage error: {0}")]
    VectorStorage(String),

    #[error("entity not found: {0}")]
    EntityNotFound(String),

    #[error("claim not found: {0}")]
    ClaimNotFound(String),

    #[error("branch not found: {0}")]
    BranchNotFound(String),

    #[error("branch already exists: {0}")]
    BranchAlreadyExists(String),

    #[error("merge blocked: {0}")]
    MergeBlocked(String),

    // --- LLM / Extraction ---
    #[error("llm provider error: {provider}: {message}")]
    LlmProvider { provider: String, message: String },

    #[error("rate limited by {provider} (retry after {retry_after_ms}ms)")]
    RateLimited {
        provider: String,
        retry_after_ms: u64,
    },

    #[error("extraction failed for source {source_id}: {message}")]
    Extraction { source_id: String, message: String },

    #[error("structured output parse error: {message}")]
    StructuredOutput { message: String },

    #[error("llm output truncated by {provider} (hit output token limit): {model}")]
    TruncatedOutput { provider: String, model: String },

    // --- Compilation ---
    #[error("template error: {0}")]
    Template(String),

    #[error("compilation failed for artifact {artifact_type}: {message}")]
    Compilation {
        artifact_type: String,
        message: String,
    },

    // --- Verification ---
    #[error("verification failed: {0}")]
    Verification(String),

    // --- Config ---
    #[error("config error: {0}")]
    Config(String),

    #[error("missing config field: {0}")]
    MissingConfig(String),

    // --- Serialization ---
    #[error("serialization error: {0}")]
    Serialization(String),

    // --- Safety ---
    #[error("permission denied: {actor} cannot {action}")]
    PermissionDenied { actor: String, action: String },

    #[error("claim quarantined: {reason}")]
    Quarantined { reason: String },

    // --- Pipeline cancellation ---
    /// Surfaced when a `CancellationToken` was tripped mid-pipeline (e.g.
    /// the desktop "Stop compile" button or a CLI Ctrl-C handler).  Distinct
    /// from real failures so callers can render a "cancelled" state rather
    /// than a red error toast.  Partial state already persisted by Phase 4
    /// (source removal) and any per-batch checkpoint flushes is preserved
    /// on disk; the next run resumes from those.
    #[error("pipeline cancelled by caller")]
    Cancelled,

    // --- Security (path traversal, identifier validation) ---
    /// Returned by [`crate::safe_path`] helpers when an untrusted
    /// input would escape its trust boundary: a tar entry containing
    /// `..`, an absolute path passed through a JS bridge, a control
    /// character in a conversation ID, etc. Treat as a hard refusal
    /// at the call site — never silently fall back to an "unsafe"
    /// path. Distinct from [`Error::Io`] so security audits and
    /// telemetry can count traversal attempts separately.
    #[error("security: {0}")]
    SecurityViolation(String),

    // --- Compile Completeness Contract §I-3 ---
    /// Returned by Phase 9 (Byte-Coverage Audit) when one or more
    /// source bytes are not covered by any structural row at end of
    /// compile. Carries the count of affected sources and total
    /// orphan bytes plus a sample for diagnostic rendering — see
    /// `docs/2026-05-02-compile-completeness-contract.md` §7.3.
    /// `TR_SKIP_BYTE_AUDIT=1` is the per-compile escape hatch.
    #[error(
        "byte-coverage breach: {total_orphan_bytes} orphan bytes across \
         {sources_with_orphans} sources (run with TR_SKIP_BYTE_AUDIT=1 to \
         override; see https://docs.thinkingroot.dev/byte-coverage)"
    )]
    ByteCoverageBreach {
        sources_with_orphans: usize,
        total_orphan_bytes: usize,
        /// Sample of `(source_id, [(byte_start, byte_end), …])` for the
        /// first ~5 affected sources. CLI rendering shows these to the
        /// developer with file:offset hints so they can fix the gap.
        sample: Vec<(String, Vec<(u64, u64)>)>,
    },

    /// Phase 9 detected structural rows whose source_id has no row in
    /// `sources`.  Indicates a missing cascade — see CLAUDE.md
    /// "Incremental compile water-flow invariants" §I-W2.
    #[error("graph corruption: {count} structural rows reference deleted sources (sample: {sample:?}). Run `root migrate --to-water-flow` to clean up.")]
    OrphanStructuralRows {
        count: usize,
        sample: Vec<(String, String)>,
    },

    /// Slice 3 — surfaced by serve handlers when the FS-event watcher
    /// has flagged the active workspace's `.thinkingroot/` as missing.
    /// CLAUDE.md §honesty rule §1 forbids silent recovery; the user
    /// must `root mount` again or `root compile --rebuild` after the
    /// substrate disappears mid-run.
    #[error(
        "workspace orphaned: `.thinkingroot/` is missing under `{workspace_root}` — \
         re-mount the workspace before continuing"
    )]
    WorkspaceOrphaned { workspace_root: String },
}

pub type Result<T> = std::result::Result<T, Error>;

// --- Convenient From impls ---

impl Error {
    pub fn io(source: std::io::Error) -> Self {
        Self::Io { path: None, source }
    }

    pub fn io_path(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: Some(path.into()),
            source,
        }
    }

    /// True when this error represents a clean caller-initiated cancellation
    /// rather than a failure.  Lets the desktop UI render a "stopped"
    /// state and lets pipeline orchestrators short-circuit cleanup work
    /// that would otherwise fight the cancellation.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }

    /// True when the error is non-transient and retrying is wasted
    /// effort: HTTP 401 / 403 / 404, missing config, unsupported file
    /// type.  Used by [`crate::Result`] callers (notably the LLM retry
    /// loop in `thinkingroot-extract`) to short-circuit instead of
    /// burning `max_retries` attempts on a stale API key.  Pre-fix a
    /// 401 retried `max_retries` times eating quota and wall-time.
    pub fn is_permanent(&self) -> bool {
        match self {
            Self::MissingConfig(_) => true,
            // Structurally-broken config (typo in `api_version`,
            // missing `deployment` field, malformed TOML) re-fails
            // identically on every retry — burning the retry budget
            // on it just delays the user-visible error.
            Self::Config(_) => true,
            // Path traversal / identifier-validation refusals are
            // never going to succeed on retry; surface immediately.
            Self::SecurityViolation(_) => true,
            Self::UnsupportedFileType { .. } => true,
            // LlmProvider errors carry a free-form message; recognise the
            // common HTTP-status fingerprints upstream surfaces emit.
            // Anything we don't explicitly recognise is treated as
            // transient (the existing default).
            //
            // Match shapes seen in the wild:
            //   "got HTTP 401 from openai"            (CLI-readable form)
            //   `{"error":{"code":"401","message":…}` (JSON-embedded — Azure)
            //   "Access denied due to invalid subscription key" (Azure prose)
            //   "invalid_api_key"                     (OpenAI machine code)
            // The space-prefix check (`" 401"`) misses the JSON-embedded
            // form; we now also accept the quoted form (`"401"`).
            Self::LlmProvider { message, .. } => {
                let m = message.to_ascii_lowercase();
                m.contains(" 401")
                    || m.contains(" 403")
                    || m.contains(" 404")
                    || m.contains("\"401\"")
                    || m.contains("\"403\"")
                    || m.contains("\"404\"")
                    || m.contains("unauthorized")
                    || m.contains("forbidden")
                    || m.contains("not found")
                    || m.contains("invalid api key")
                    || m.contains("invalid_api_key")
                    || m.contains("invalid subscription key")
                    || m.contains("access denied")
                    || m.contains("authentication")
                    || m.contains("api endpoint")
                    // 2026-05-07: Azure / OpenAI return `unsupported_parameter`
                    // when the deployment is a reasoning model (gpt-5.x, o-series)
                    // but the request body still carries the legacy `max_tokens`
                    // field — and vice-versa.  Same fix-the-config story as 401:
                    // every retry replays the same wrong parameter and gets the
                    // same 400 back, so retries burn quota.  Bail-fast surfaces
                    // the misconfiguration in seconds instead of grinding for
                    // hours.
                    || m.contains("unsupported_parameter")
                    || m.contains("unsupported parameter")
                    // OpenAI returns `model_not_found` (HTTP 404 dressed as a
                    // typed code) when the deployment / model id is wrong.
                    // Same family of fail-and-stop errors.
                    || m.contains("model_not_found")
                    || m.contains("deployment_not_found")
            }
            _ => false,
        }
    }

    /// True when the error is a rate-limit / throttle from any LLM provider.
    /// Also catches generic provider errors whose message mentions throttling.
    pub fn is_rate_limited(&self) -> bool {
        match self {
            Self::RateLimited { .. } => true,
            Self::LlmProvider { message, .. } => {
                let m = message.to_lowercase();
                m.contains("throttl")
                    || m.contains("rate")
                    || m.contains("too many requests")
                    || m.contains("429")
                    || m.contains("quota")
                    || m.contains("capacity")
                    || m.contains("overloaded")
                    || m.contains("service error")
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_cancelled_only_for_cancelled_variant() {
        assert!(Error::Cancelled.is_cancelled());
        assert!(!Error::Config("x".into()).is_cancelled());
        assert!(!Error::MissingConfig("x".into()).is_cancelled());
    }

    #[test]
    fn is_permanent_recognises_auth_error_fingerprints() {
        // M5: HTTP 401/403/404 status codes embedded in provider
        // error messages.  Pre-fix these retried max_retries times
        // eating quota on a stale API key.
        let cases = [
            ("got HTTP 401 from openai", true),
            ("got HTTP 403 from azure", true),
            ("got HTTP 404 from azure", true),
            ("Unauthorized: bad api key", true),
            ("Forbidden: scope mismatch", true),
            ("not found: deployment 'x'", true),
            ("invalid api key for project foo", true),
            ("authentication failed", true),
            ("network connection reset", false),
            ("read timeout after 90s", false),
            ("503 service unavailable", false),
            // 2026-05-07: Azure's JSON-embedded error wasn't caught by
            // the original space-prefix matcher.  These three strings
            // are verbatim from /tmp/tr-daemon.log when AZURE_OPENAI_API_KEY
            // was stale.
            (
                r#"azure: unexpected response: {"error":{"code":"401","message":"Access denied due to invalid subscription key or wrong API endpoint."}}"#,
                true,
            ),
            (
                "Access denied due to invalid subscription key or wrong API endpoint.",
                true,
            ),
            (
                r#"{"error":{"code":"403","message":"Forbidden"}}"#,
                true,
            ),
            // 2026-05-07: Azure rejects `max_tokens` against reasoning
            // models (gpt-5.x / o-series) — the deployment expects
            // `max_completion_tokens`. Pre-fix the extractor retried 3×
            // per batch then walked through every batch in the workspace,
            // turning a config-name mismatch into hours of wall-time.
            // Verbatim Azure body string.
            (
                r#"azure: unexpected response: {"error":{"code":"unsupported_parameter","message":"Unsupported parameter: 'max_tokens' is not supported with this model. Use 'max_completion_tokens' instead.","param":"max_tokens","type":"invalid_request_error"}}"#,
                true,
            ),
            (
                "Unsupported parameter: 'max_tokens' is not supported with this model.",
                true,
            ),
            (
                r#"{"error":{"code":"model_not_found","message":"The model 'gpt-foo' does not exist or you do not have access to it."}}"#,
                true,
            ),
            (
                r#"{"error":{"code":"deployment_not_found","message":"deployment not found"}}"#,
                true,
            ),
        ];
        for (msg, expected) in cases {
            let err = Error::LlmProvider {
                provider: "test".into(),
                message: msg.into(),
            };
            assert_eq!(
                err.is_permanent(),
                expected,
                "is_permanent({msg:?}) should be {expected}"
            );
        }
    }

    #[test]
    fn missing_config_is_permanent() {
        // No retry budget should be spent on a deployment-name typo.
        assert!(Error::MissingConfig("set [llm.providers.azure].deployment".into()).is_permanent());
    }

    #[test]
    fn rate_limited_is_not_permanent() {
        // Rate-limit errors have their own RL-attempts budget; the
        // `is_permanent` short-circuit must NOT swallow them.
        let err = Error::RateLimited {
            provider: "azure".into(),
            retry_after_ms: 30_000,
        };
        assert!(!err.is_permanent());
        assert!(err.is_rate_limited());
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::io(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::Serialization(e.to_string())
    }
}

impl From<rmp_serde::encode::Error> for Error {
    fn from(e: rmp_serde::encode::Error) -> Self {
        Self::Serialization(e.to_string())
    }
}

impl From<rmp_serde::decode::Error> for Error {
    fn from(e: rmp_serde::decode::Error) -> Self {
        Self::Serialization(e.to_string())
    }
}

impl From<toml::de::Error> for Error {
    fn from(e: toml::de::Error) -> Self {
        Self::Config(e.to_string())
    }
}

impl From<toml::ser::Error> for Error {
    fn from(e: toml::ser::Error) -> Self {
        Self::Config(e.to_string())
    }
}
