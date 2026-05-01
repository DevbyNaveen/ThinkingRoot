use std::path::{Path, PathBuf};

use thinkingroot_core::config::ParserConfig;
use thinkingroot_core::{Error, Result};

/// Walk a directory tree and return all parseable files, respecting
/// gitignore rules and the user's exclude patterns.
pub fn walk(root: &Path, config: &ParserConfig) -> Result<Vec<PathBuf>> {
    let mut builder = ignore::WalkBuilder::new(root);

    builder
        .hidden(true) // skip hidden files by default
        .git_ignore(config.respect_gitignore)
        .git_global(config.respect_gitignore)
        .git_exclude(config.respect_gitignore);

    // Add exclude patterns as overrides.
    let mut overrides = ignore::overrides::OverrideBuilder::new(root);
    for pattern in &config.exclude_patterns {
        // Negate patterns: "!pattern" means exclude.
        overrides
            .add(&format!("!{pattern}"))
            .map_err(|e| Error::Config(format!("invalid exclude pattern '{pattern}': {e}")))?;
    }
    let overrides = overrides
        .build()
        .map_err(|e| Error::Config(format!("failed to build overrides: {e}")))?;
    builder.overrides(overrides);

    let mut files = Vec::new();

    for entry in builder.build() {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                // Permission errors on system directories (e.g. macOS ~/Library) are
                // expected when compiling a broad path like $HOME. Skip and continue.
                if let Some(io_err) = e.io_error()
                    && io_err.kind() == std::io::ErrorKind::PermissionDenied
                {
                    tracing::warn!("skipping inaccessible path: {e}");
                    continue;
                }
                tracing::debug!("walk error (skipping): {e}");
                continue;
            }
        };

        let path = entry.path();

        // Skip directories.
        if !path.is_file() {
            continue;
        }

        // Check file size limit.  metadata() can fail on macOS APFS clones,
        // some FUSE mounts, and racy unlinks.  Pre-fix the failure branch
        // silently *passed* the file through, which then got read in full
        // regardless of `max_file_size`.  Treat any metadata failure as a
        // skip with a warning so a 2 GB log file behind a flaky FUSE mount
        // can never blow the parser's budget.
        match path.metadata() {
            Ok(meta) if meta.len() > config.max_file_size => {
                tracing::debug!(
                    "skipping large file: {} ({} bytes)",
                    path.display(),
                    meta.len()
                );
                continue;
            }
            Ok(_) => {} // size OK
            Err(e) => {
                tracing::warn!(
                    "skipping {} (metadata failed: {e})",
                    path.display(),
                );
                continue;
            }
        }

        // If include_extensions is set, filter by extension.
        if !config.include_extensions.is_empty() {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            if !config
                .include_extensions
                .iter()
                .any(|e| e.to_lowercase() == ext)
            {
                continue;
            }
        }

        files.push(path.to_path_buf());
    }

    files.sort();
    tracing::info!("found {} files in {}", files.len(), root.display());
    Ok(files)
}
