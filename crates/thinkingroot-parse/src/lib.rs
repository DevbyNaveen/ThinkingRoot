pub mod code;
pub mod git;
pub mod markdown;
pub mod pdf;
pub mod walker;

use std::path::Path;

use thinkingroot_core::ir::DocumentIR;
use thinkingroot_core::{Error, Result};

// Re-export for external use.
pub use git::parse_git_log;

/// Parse a single file into a DocumentIR based on its extension.
pub fn parse_file(path: &Path) -> Result<DocumentIR> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        "md" | "markdown" | "mdx" => markdown::parse(path),
        "rs" => code::parse(path, "rust"),
        "py" | "pyi" => code::parse(path, "python"),
        "js" | "jsx" | "mjs" | "cjs" => code::parse(path, "javascript"),
        "ts" | "tsx" => code::parse(path, "typescript"),
        "go" => code::parse(path, "go"),
        "pdf" => pdf::parse(path),
        // Treat unknown text files as plain markdown for basic extraction.
        "txt" | "toml" | "yaml" | "yml" | "json" | "cfg" | "ini" | "env" => {
            markdown::parse_as_text(path)
        }
        _ => Err(Error::UnsupportedFileType {
            extension: ext.to_string(),
        }),
    }
}

/// Parse all supported files in a directory tree.
/// Also ingests recent git history if the directory is a git repo.
pub fn parse_directory(
    root: &Path,
    config: &thinkingroot_core::config::ParserConfig,
) -> Result<Vec<DocumentIR>> {
    let files = walker::walk(root, config)?;
    let mut documents = Vec::new();

    for file_path in &files {
        match parse_file(file_path) {
            Ok(doc) => documents.push(doc),
            Err(Error::UnsupportedFileType { .. }) => {
                tracing::debug!("skipping unsupported file: {}", file_path.display());
            }
            Err(e) => {
                tracing::warn!("failed to parse {}: {e}", file_path.display());
            }
        }
    }

    // Also parse recent git commits if this is a git repo.
    match git::parse_git_log(root, 50) {
        Ok(git_docs) => {
            if !git_docs.is_empty() {
                tracing::info!("parsed {} git commits", git_docs.len());
                documents.extend(git_docs);
            }
        }
        Err(e) => {
            tracing::debug!("git parsing skipped: {e}");
        }
    }

    tracing::info!("parsed {} files from {}", documents.len(), root.display());
    Ok(documents)
}
