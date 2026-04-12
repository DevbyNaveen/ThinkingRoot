//! Tier router — classifies chunks as Structural (Tier 0) vs LLM (Tier 2).
//!
//! Chunks carrying rich AST metadata (function names, type names, import paths)
//! can be extracted deterministically by the structural extractor with zero LLM
//! calls.  Everything else is forwarded to the LLM extraction path.

use thinkingroot_core::ir::{Chunk, ChunkType};

// ── Tier ─────────────────────────────────────────────────────────────────────

/// Which extraction path a chunk should follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Zero-LLM deterministic extraction via AST metadata.
    Structural,
    /// LLM-powered extraction (fallback for all other chunks).
    Llm,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Classify a single chunk into a [`Tier`].
///
/// Rules:
/// - `FunctionDef` with a non-empty `function_name`   → [`Tier::Structural`]
/// - `TypeDef`     with a non-empty `type_name`        → [`Tier::Structural`]
/// - `Import`      with a non-empty `import_path`      → [`Tier::Structural`]
/// - `ManifestDependency` always                       → [`Tier::Structural`]
/// - `Heading`     always                              → [`Tier::Structural`]
/// - `Prose`       with `commit_author` or non-empty `links` → [`Tier::Structural`]
/// - Everything else → [`Tier::Llm`]
///
/// Note: `ManifestDependency`, `Heading`, and git/link `Prose` chunks are routed
/// Structural here. Their extractors are implemented in `structural.rs` and fall
/// through to LLM only when metadata is absent (e.g., a Heading chunk with no content).
pub fn classify(chunk: &Chunk) -> Tier {
    match chunk.chunk_type {
        ChunkType::FunctionDef => {
            if chunk.metadata.function_name.as_deref().is_some_and(|n| !n.is_empty()) {
                Tier::Structural
            } else {
                Tier::Llm
            }
        }
        ChunkType::TypeDef => {
            if chunk.metadata.type_name.as_deref().is_some_and(|n| !n.is_empty()) {
                Tier::Structural
            } else {
                Tier::Llm
            }
        }
        ChunkType::Import => {
            if chunk.metadata.import_path.as_deref().is_some_and(|p| !p.is_empty()) {
                Tier::Structural
            } else {
                Tier::Llm
            }
        }
        // ManifestDependency always carries type_name + import_path (set by manifest parser).
        ChunkType::ManifestDependency => Tier::Structural,
        // Heading always carries heading_level (set by markdown parser).
        ChunkType::Heading => Tier::Structural,
        // Git commit Prose (has commit_author) and link-bearing Prose are structurally extractable.
        ChunkType::Prose => {
            if chunk.metadata.commit_author.is_some() || !chunk.metadata.links.is_empty() {
                Tier::Structural
            } else {
                Tier::Llm
            }
        }
        _ => Tier::Llm,
    }
}

/// Split a slice of chunks into two index lists: `(structural_indices, llm_indices)`.
///
/// The indices reference positions in the original `chunks` slice and are
/// returned in the order they were encountered.
pub fn route_chunks(chunks: &[Chunk]) -> (Vec<usize>, Vec<usize>) {
    let mut structural = Vec::new();
    let mut llm = Vec::new();

    for (i, chunk) in chunks.iter().enumerate() {
        match classify(chunk) {
            Tier::Structural => structural.push(i),
            Tier::Llm => llm.push(i),
        }
    }

    (structural, llm)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use thinkingroot_core::ir::{Chunk, ChunkMetadata, ChunkType};

    use super::*;

    fn chunk(chunk_type: ChunkType, meta: ChunkMetadata) -> Chunk {
        Chunk {
            content: "test".to_string(),
            chunk_type,
            start_line: 1,
            end_line: 1,
            heading: None,
            language: None,
            metadata: meta,
        }
    }

    #[test]
    fn function_def_with_name_is_structural() {
        let c = chunk(
            ChunkType::FunctionDef,
            ChunkMetadata {
                function_name: Some("my_fn".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(classify(&c), Tier::Structural);
    }

    #[test]
    fn function_def_without_name_is_llm() {
        let c = chunk(ChunkType::FunctionDef, ChunkMetadata::default());
        assert_eq!(classify(&c), Tier::Llm);
    }

    #[test]
    fn import_with_path_is_structural() {
        let c = chunk(
            ChunkType::Import,
            ChunkMetadata {
                import_path: Some("std::collections::HashMap".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(classify(&c), Tier::Structural);
    }

    #[test]
    fn type_def_with_name_is_structural() {
        let c = chunk(
            ChunkType::TypeDef,
            ChunkMetadata {
                type_name: Some("MyStruct".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(classify(&c), Tier::Structural);
    }

    #[test]
    fn code_chunk_is_llm() {
        let c = chunk(ChunkType::Code, ChunkMetadata::default());
        assert_eq!(classify(&c), Tier::Llm);
    }

    #[test]
    fn manifest_dependency_is_structural() {
        let c = chunk(ChunkType::ManifestDependency, ChunkMetadata::default());
        assert_eq!(classify(&c), Tier::Structural);
    }

    #[test]
    fn heading_is_structural() {
        let c = chunk(ChunkType::Heading, ChunkMetadata::default());
        assert_eq!(classify(&c), Tier::Structural);
    }

    #[test]
    fn prose_with_commit_author_is_structural() {
        let c = chunk(
            ChunkType::Prose,
            ChunkMetadata {
                commit_author: Some("Alice".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(classify(&c), Tier::Structural);
    }

    #[test]
    fn prose_with_links_is_structural() {
        let c = chunk(
            ChunkType::Prose,
            ChunkMetadata {
                links: vec!["./foo.md".to_string()],
                ..Default::default()
            },
        );
        assert_eq!(classify(&c), Tier::Structural);
    }

    #[test]
    fn prose_without_commit_author_or_links_is_llm() {
        let c = chunk(ChunkType::Prose, ChunkMetadata::default());
        assert_eq!(classify(&c), Tier::Llm);
    }

    #[test]
    fn route_chunks_splits_correctly() {
        // 3 chunks: FunctionDef+name (structural), Prose (llm), Import+path (structural)
        let chunks = vec![
            chunk(
                ChunkType::FunctionDef,
                ChunkMetadata {
                    function_name: Some("do_thing".to_string()),
                    ..Default::default()
                },
            ),
            chunk(ChunkType::Prose, ChunkMetadata::default()),
            chunk(
                ChunkType::Import,
                ChunkMetadata {
                    import_path: Some("crate::graph::GraphStore".to_string()),
                    ..Default::default()
                },
            ),
        ];

        let (structural, llm) = route_chunks(&chunks);

        assert_eq!(structural, vec![0, 2], "expected indices 0 and 2 in structural");
        assert_eq!(llm, vec![1], "expected index 1 in llm");
    }
}
