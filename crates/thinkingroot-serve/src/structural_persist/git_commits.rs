//! Git-commit emitter — Compile Completeness Contract §4.7.
//!
//! Emits one row per git-typed source. Reads `SourceMetadata` git
//! fields (extended in Block C: `commit_email`, `commit_timestamp`,
//! `commit_message`, `parent_sha`, `changed_files_json`) plus the
//! pre-existing `commit_sha` and `DocumentIR.author`.
//!
//! Composite key on `(source_id, commit_sha)`. The byte range is the
//! full doc — git commits don't have a meaningful sub-range and the
//! schema requires the I-2 triple to be present.

use thinkingroot_core::ir::DocumentIR;
use thinkingroot_core::types::SourceType;
use thinkingroot_graph::{Blake3Cache, rows::GitCommit};

pub(super) fn emit(
    doc: &DocumentIR,
    bytes: &[u8],
    source_id: &str,
    cache: &mut Blake3Cache,
    out: &mut Vec<GitCommit>,
) {
    if !matches!(doc.source_type, SourceType::GitCommit) {
        return;
    }
    let Some(sha) = &doc.metadata.commit_sha else {
        return;
    };
    if sha.is_empty() {
        return;
    }

    let byte_end = bytes.len() as u64;
    let blake3_str = cache.get(0, byte_end).to_string();

    out.push(GitCommit {
        source_id: source_id.to_string(),
        commit_sha: sha.clone(),
        commit_author: doc.author.clone().unwrap_or_default(),
        commit_email: doc.metadata.commit_email.clone().unwrap_or_default(),
        commit_timestamp: doc.metadata.commit_timestamp.unwrap_or(0.0),
        changed_files_json: doc
            .metadata
            .changed_files_json
            .clone()
            .unwrap_or_else(|| "[]".to_string()),
        message: doc.metadata.commit_message.clone().unwrap_or_default(),
        parent_sha: doc.metadata.parent_sha.clone().unwrap_or_default(),
        byte_start: 0,
        byte_end,
        content_blake3: blake3_str,
    });
}
