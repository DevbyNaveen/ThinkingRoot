use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::SourceId;

/// Origin of knowledge — a file, URL, git commit, chat message, etc.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub id: SourceId,
    pub uri: String,
    pub source_type: SourceType,
    pub author: Option<String>,
    pub created_at: DateTime<Utc>,
    pub content_hash: ContentHash,
    pub trust_level: TrustLevel,
    pub byte_size: u64,
    pub metadata: SourceMetadata,
}

impl Source {
    pub fn new(uri: String, source_type: SourceType) -> Self {
        Self {
            id: SourceId::new(),
            uri,
            source_type,
            author: None,
            created_at: Utc::now(),
            content_hash: ContentHash::empty(),
            trust_level: TrustLevel::Unknown,
            byte_size: 0,
            metadata: SourceMetadata::default(),
        }
    }

    pub fn with_author(mut self, author: impl Into<String>) -> Self {
        self.author = Some(author.into());
        self
    }

    pub fn with_trust(mut self, trust: TrustLevel) -> Self {
        self.trust_level = trust;
        self
    }

    pub fn with_id(mut self, id: SourceId) -> Self {
        self.id = id;
        self
    }

    pub fn with_hash(mut self, hash: ContentHash) -> Self {
        self.content_hash = hash;
        self
    }

    pub fn with_size(mut self, size: u64) -> Self {
        self.byte_size = size;
        self
    }

    /// Returns true if the content has changed since last processing.
    pub fn content_changed(&self, new_hash: &ContentHash) -> bool {
        self.content_hash != *new_hash
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceType {
    File,
    GitCommit,
    GitDiff,
    Document,
    ChatMessage,
    WebPage,
    Api,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevel {
    Quarantined,
    Untrusted,
    Unknown,
    Trusted,
    Verified,
}

impl TrustLevel {
    pub fn is_at_least(&self, minimum: TrustLevel) -> bool {
        *self >= minimum
    }
}

/// A7-SECURITY ① — the ORIGIN CHANNEL a piece of knowledge entered through.
/// Orthogonal to [`TrustLevel`] (a quality/verification judgement): the
/// trust class answers *who could have authored this bytes-wise*, which is
/// what memory-poisoning defenses key on — a fluent, high-quality poison
/// record sails through quality gates but cannot fake its entry channel.
///
/// Derivable from the engine's canonical source-URI conventions, so it is
/// retroactively available for every existing claim with zero migration:
///   file:// / git*       → OwnerSource      (compiled by the workspace owner)
///   mcp://agent/…        → AuthenticatedUser (a keyed session's chat turns)
///   connector://…        → ToolOutput        (keyed connector ingest)
///   rootfn://…           → AgentGenerated    (a function's own writes)
///   http:// / https://   → FetchedWeb        (anyone on the internet)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustClass {
    OwnerSource,
    AuthenticatedUser,
    ToolOutput,
    AgentGenerated,
    FetchedWeb,
    Unknown,
}

impl TrustClass {
    /// Classify a source URI by the engine's canonical channel prefixes.
    /// Unrecognised schemes are `Unknown` — never silently promoted.
    pub fn from_uri(uri: &str) -> Self {
        let u = uri.trim();
        if u.starts_with("file://") || u.starts_with("git://") || u.starts_with("git+") {
            Self::OwnerSource
        } else if u.starts_with("mcp://agent/") {
            Self::AuthenticatedUser
        } else if u.starts_with("connector://") {
            Self::ToolOutput
        } else if u.starts_with("rootfn://") {
            Self::AgentGenerated
        } else if u.starts_with("http://") || u.starts_with("https://") {
            Self::FetchedWeb
        } else if !u.is_empty() && !u.contains("://") {
            // Bare relative paths are how the compile pipeline records
            // owner-tree files (e.g. `src/lib.rs`).
            Self::OwnerSource
        } else {
            Self::Unknown
        }
    }
}

/// BLAKE3 content hash for change detection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentHash(pub String);

impl ContentHash {
    pub fn from_bytes(data: &[u8]) -> Self {
        Self(blake3::hash(data).to_hex().to_string())
    }

    pub fn empty() -> Self {
        Self(String::new())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Optional metadata attached to a source.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceMetadata {
    /// For files: the file extension (e.g. "rs", "md").
    pub file_extension: Option<String>,
    /// For git: the commit SHA.
    pub commit_sha: Option<String>,
    /// For git: the branch name.
    pub branch: Option<String>,
    /// For web: the page title.
    pub title: Option<String>,
    /// Language of the content (for code files).
    pub language: Option<String>,
    /// Relative path within the repository.
    pub relative_path: Option<String>,
    // ─── Compile Completeness Contract §4.7 — git_commits emitter inputs ─
    /// For git: author email — populates `git_commits.commit_email`.
    #[serde(default)]
    pub commit_email: Option<String>,
    /// For git: commit timestamp as a Unix epoch second — populates
    /// `git_commits.commit_timestamp` for chronological queries.
    #[serde(default)]
    pub commit_timestamp: Option<f64>,
    /// For git: the commit message body — populates `git_commits.message`.
    #[serde(default)]
    pub commit_message: Option<String>,
    /// For git: parent commit SHA — populates `git_commits.parent_sha` so
    /// the call graph DAG can be reconstructed via `:by_commit` joins.
    #[serde(default)]
    pub parent_sha: Option<String>,
    /// For git: file paths changed in this commit, JSON-serialised — populates
    /// `git_commits.changed_files_json` for "files-most-changed-by-author"
    /// queries without re-walking history.
    #[serde(default)]
    pub changed_files_json: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_detects_change() {
        let h1 = ContentHash::from_bytes(b"hello");
        let h2 = ContentHash::from_bytes(b"world");
        let h3 = ContentHash::from_bytes(b"hello");
        assert_ne!(h1, h2);
        assert_eq!(h1, h3);
    }

    #[test]
    fn trust_level_ordering() {
        assert!(TrustLevel::Verified > TrustLevel::Unknown);
        assert!(TrustLevel::Quarantined < TrustLevel::Untrusted);
        assert!(TrustLevel::Trusted.is_at_least(TrustLevel::Unknown));
        assert!(!TrustLevel::Unknown.is_at_least(TrustLevel::Trusted));
    }

    #[test]
    fn source_builder_pattern() {
        let src = Source::new("file:///test.rs".into(), SourceType::File)
            .with_author("naveen")
            .with_trust(TrustLevel::Verified)
            .with_size(1024);

        assert_eq!(src.author.as_deref(), Some("naveen"));
        assert_eq!(src.trust_level, TrustLevel::Verified);
        assert_eq!(src.byte_size, 1024);
    }
}
