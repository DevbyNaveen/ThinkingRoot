// crates/thinkingroot-core/src/types/branch.rs
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A reference to a knowledge branch, tracking its lifecycle status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchRef {
    /// Human-readable branch name, e.g. "feature/add-auth-docs".
    pub name: String,
    /// URL-safe slug derived from the name, e.g. "feature-add-auth-docs".
    pub slug: String,
    /// The parent branch this was forked from, e.g. "main".
    pub parent: String,
    /// When the branch was created.
    pub created_at: DateTime<Utc>,
    /// Current lifecycle status of the branch.
    pub status: BranchStatus,
    /// Optional human-readable description of the branch's purpose.
    pub description: Option<String>,
}

/// Lifecycle status of a knowledge branch.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum BranchStatus {
    /// Branch is active and accepting changes.
    Active,
    /// Branch has been merged into its parent.
    Merged {
        merged_at: DateTime<Utc>,
        merged_by: MergedBy,
    },
    /// Branch was abandoned without merging.
    Abandoned { abandoned_at: DateTime<Utc> },
}

/// Records who or what performed a branch merge.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MergedBy {
    /// A human user performed the merge.
    Human { user: String },
    /// An AI agent performed the merge.
    Agent { agent_id: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn branch_ref_roundtrip() {
        let b = BranchRef {
            name: "feature/x".to_string(),
            slug: "feature-x".to_string(),
            parent: "main".to_string(),
            created_at: Utc::now(),
            status: BranchStatus::Active,
            description: Some("test branch".to_string()),
        };
        assert_eq!(b.name, "feature/x");
        assert!(matches!(b.status, BranchStatus::Active));
    }

    #[test]
    fn merged_by_agent() {
        let mb = MergedBy::Agent {
            agent_id: "claude".to_string(),
        };
        assert!(matches!(mb, MergedBy::Agent { .. }));
    }
}
