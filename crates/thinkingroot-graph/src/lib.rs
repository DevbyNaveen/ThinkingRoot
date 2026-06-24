pub mod aep_queries;
pub mod agents;
pub mod answer_cache;
pub mod artifact_nodes;
pub mod atomic_fact_inserts;
pub mod capsule;
pub mod codegraph;
pub mod concept_inserts;
pub mod cognition_inserts;
pub mod cognition_merge;
pub mod graph;
pub mod hybrid_queries;
pub mod ort_session;
pub mod spreading_activation;
pub mod per_source_rows;
pub mod prompt;
pub mod root_function;
pub mod rerank;
pub mod row_blake3;
pub mod rows;
pub mod source_store;
pub mod spine_inserts;
pub mod storage;
pub mod structural_inserts;
pub mod summaries;
pub mod vector_quant;
pub mod vector;
pub mod witness_inserts;

pub use per_source_rows::PerSourceRows;

pub use source_store::{FileSystemSourceStore, SourceByteStore, SourceBytes};

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
