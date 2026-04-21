//! ThinkingRoot — Phase 9 Reflect.
//!
//! A knowledge graph that observes its own structure, discovers
//! co-occurrence patterns, and surfaces "what knowledge SHOULD exist
//! but doesn't" as first-class queryable records.
//!
//! # Pipeline position
//!
//! Runs after `Verify`, treating the verified graph as input:
//!
//! ```text
//! Parse → Extract → Ground → Fingerprint → Link → Index → Compile → Verify → Reflect
//! ```
//!
//! # Model
//!
//! - `structural_patterns` — per (entity_type, condition_claim_type,
//!   expected_claim_type) co-occurrence frequencies. Re-derived in full
//!   every run.
//! - `known_unknowns` — per (entity, expected_claim_type) gaps implied by
//!   patterns. Persistent with lifecycle: `open → resolved` (or
//!   `dismissed` via user action). Persists across runs so callers can
//!   see gap age and resolution history.
//!
//! # Boundary
//!
//! This crate owns no cozo types. All database access goes through
//! helper methods on `GraphStore` (see `reflect_*` methods in
//! `thinkingroot-graph`).

pub mod engine;
pub mod types;

pub use engine::{
    count_open_gaps, dismiss_gap, list_open_gaps, reflect_across_graphs, ReflectConfig,
    ReflectEngine,
};
pub use types::{
    CrossReflectResult, GapReport, GapStatus, KnownUnknown, ReflectResult, StructuralPattern,
};
