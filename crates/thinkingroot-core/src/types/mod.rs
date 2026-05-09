mod artifact;
mod branch;
mod claim;
pub mod claim_migration;
mod contradiction;
mod diff;
mod entity;
mod event;
pub mod incremental;
mod predicate;
mod relation;
mod source;
mod workspace;
mod workspace_event;

pub use artifact::*;
pub use branch::*;
pub use claim::*;
pub use claim_migration::{
    CLAIM_SCHEMA_VERSION_META_KEY, CURRENT_CLAIM_SCHEMA_VERSION, ClaimMigration,
    MigrationRegistry, clear_global_registry_for_test, migrate_claim, register_migration,
};
pub use contradiction::*;
pub use diff::*;
pub use entity::*;
pub use event::*;
pub use incremental::{format_bytes, IncrementalSummary, PHASE_NAMES};
pub use predicate::*;
pub use relation::*;
pub use source::*;
pub use workspace::*;
pub use workspace_event::*;

// --- Type-safe ID aliases ---

use crate::id::Id;

/// Marker types for type-safe IDs.
pub mod markers {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
    pub struct SourceMarker;
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
    pub struct ClaimMarker;
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
    pub struct EntityMarker;
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
    pub struct RelationMarker;
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
    pub struct ContradictionMarker;
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
    pub struct ArtifactMarker;
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
    pub struct WorkspaceMarker;
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
    pub struct AgentMarker;
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
    pub struct UserMarker;
}

pub type SourceId = Id<markers::SourceMarker>;
pub type ClaimId = Id<markers::ClaimMarker>;
pub type EntityId = Id<markers::EntityMarker>;
pub type RelationId = Id<markers::RelationMarker>;
pub type ContradictionId = Id<markers::ContradictionMarker>;
pub type ArtifactId = Id<markers::ArtifactMarker>;
pub type WorkspaceId = Id<markers::WorkspaceMarker>;
pub type AgentId = Id<markers::AgentMarker>;
pub type UserId = Id<markers::UserMarker>;
