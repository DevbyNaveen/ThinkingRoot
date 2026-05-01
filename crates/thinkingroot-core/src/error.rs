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
    #[error("permission denied: agent {agent_id} cannot {action}")]
    PermissionDenied { agent_id: String, action: String },

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
            Self::UnsupportedFileType { .. } => true,
            // LlmProvider errors carry a free-form message; recognise the
            // common HTTP-status fingerprints upstream surfaces emit.
            // Anything we don't explicitly recognise is treated as
            // transient (the existing default).
            Self::LlmProvider { message, .. } => {
                let m = message.to_ascii_lowercase();
                m.contains(" 401")
                    || m.contains(" 403")
                    || m.contains(" 404")
                    || m.contains("unauthorized")
                    || m.contains("forbidden")
                    || m.contains("not found")
                    || m.contains("invalid api key")
                    || m.contains("invalid_api_key")
                    || m.contains("authentication")
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
        assert!(Error::MissingConfig("set [llm.providers.azure].deployment".into())
            .is_permanent());
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
