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
