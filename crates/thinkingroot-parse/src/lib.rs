pub mod code;
pub mod git;
pub mod manifest;
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
        "java" => code::parse(path, "java"),
        "c" | "h" => code::parse(path, "c"),
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" => code::parse(path, "cpp"),
        "cs" => code::parse(path, "csharp"),
        "rb" => code::parse(path, "ruby"),
        "kt" | "kts" => code::parse(path, "kotlin"),
        "swift" => code::parse(path, "swift"),
        "php" => code::parse(path, "php"),
        "sh" | "bash" => code::parse(path, "bash"),
        "lua" => code::parse(path, "lua"),
        "scala" => code::parse(path, "scala"),
        "ex" | "exs" => code::parse(path, "elixir"),
        "hs" => code::parse(path, "haskell"),
        "r" => code::parse(path, "r"),
        "pdf" => pdf::parse(path),
        // Manifest files get structured dependency parsing.
        "toml"
            if path
                .file_name()
                .is_some_and(|n| n == "Cargo.toml" || n == "pyproject.toml") =>
        {
            manifest::parse(path)
        }
        "json" if path.file_name().is_some_and(|n| n == "package.json") => manifest::parse(path),
        "mod" if path.file_name().is_some_and(|n| n == "go.mod") => manifest::parse(path),
        "txt" if path.file_name().is_some_and(|n| n == "requirements.txt") => manifest::parse(path),
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
