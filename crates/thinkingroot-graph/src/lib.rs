pub mod aep_queries;
pub mod graph;
pub mod hybrid_queries;
pub mod per_source_rows;
pub mod row_blake3;
pub mod rows;
pub mod storage;
pub mod structural_inserts;
pub mod vector;

pub use per_source_rows::PerSourceRows;

pub use row_blake3::{row_blake3, Blake3Cache};
pub use rows::{
    CodeLink, CodeMarker, CodeMetric, CodeSignature, ConfigTreeNode, DataRowRow, DocTagRow,
    FunctionCall, GitBlameRow, GitCommit, HeadingRow, QuantityRow, ResidualChunk, SourceAnnotation,
    SourceReference, TestAnnotation,
};
pub use storage::StorageEngine;

// Re-export core error types so `aep_queries.rs` (and downstream consumers)
// can refer to them as `crate::Error` / `crate::Result` without needing to
// import `thinkingroot_core` directly.
pub use thinkingroot_core::{Error, Result};
